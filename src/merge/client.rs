//! Client-side steps of a merge: pins, re-index and finalize.
//!
//! These run in the CLI process. A daemon auto-started by an app may lack
//! privacy access to a checkout that the person's terminal has, so the pin
//! files are written from here.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{FinalizeOutcome, MergeOutcome};
use crate::daemon::{DaemonClient, MergeFinalizeRequest};
use crate::error::Result;
use crate::ids::ProjectId;
use crate::projects::{PinRewrite, PinState, rewrite_merged_pins};

/// The survivor's re-index after a merge.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ReindexResult {
    pub path: Option<String>,
    /// `indexed`, `skipped` (a completed follow-up already did it),
    /// `missing`, `none` (no checkout exists) or `failed`.
    pub state: String,
    pub scan_id: Option<String>,
    /// Memories the re-index quarantined because a symbol they are anchored
    /// to changed in the indexed tree (ordinary symbol drift).
    #[serde(default)]
    pub quarantined: u64,
    pub detail: Option<String>,
}

/// What the steps after a merge transaction did.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct FollowUpReport {
    pub pins: Vec<PinRewrite>,
    pub reindex: ReindexResult,
    pub finalize: FinalizeOutcome,
    /// Every step finished; a repeated merge has nothing left to do.
    pub complete: bool,
}

/// Rewrite (or, with `dry_run`, inspect) the pins of the retired project's
/// checkouts. Checkouts that share a primary worktree are visited once.
#[must_use]
pub fn rewrite_pins(
    checkouts: &[String],
    retired: &ProjectId,
    survivor: &ProjectId,
    survivor_name: &str,
    dry_run: bool,
) -> Vec<PinRewrite> {
    let mut seen = std::collections::BTreeSet::new();
    let mut results = Vec::new();
    for checkout in checkouts {
        for pin in rewrite_merged_pins(
            Path::new(checkout),
            retired,
            survivor,
            survivor_name,
            dry_run,
        ) {
            let key = pin
                .file
                .clone()
                .unwrap_or_else(|| PathBuf::from(&pin.checkout));
            if seen.insert(key) {
                results.push(pin);
            }
        }
    }
    results
}

/// Whether a pin needs no further work.
#[must_use]
pub const fn pin_done(state: PinState) -> bool {
    matches!(
        state,
        PinState::Rewritten
            | PinState::AlreadySurvivor
            | PinState::OtherProject
            | PinState::NoPin
            | PinState::Missing
    )
}

/// Finish a merge from the client: rewrite the pins that still hold the
/// retired id, re-index the survivor from its primary checkout (skipped when
/// a completed follow-up already did), then finalize. Waiting anchors are
/// resolved only after a re-index; otherwise they wait for a repeated merge.
///
/// # Errors
///
/// Returns daemon errors from the finalize step. Pin and index failures are
/// reported in the result instead.
pub async fn follow_up(client: &DaemonClient, outcome: &MergeOutcome) -> Result<FollowUpReport> {
    let pins = rewrite_pins(
        &outcome.checkouts_to_repin,
        &outcome.retired,
        &outcome.survivor,
        &outcome.survivor_name,
        false,
    );
    let reindex = reindex(client, outcome).await;
    let indexed = matches!(reindex.state.as_str(), "indexed" | "skipped");
    let finalize = client
        .finalize_merge(&MergeFinalizeRequest {
            project_id: outcome.survivor.clone(),
            resolve_anchors: indexed,
            reindexed: (reindex.state == "indexed")
                .then(|| reindex.path.clone())
                .flatten(),
        })
        .await?;
    let pins_done = pins.iter().all(|pin| pin_done(pin.state));
    Ok(FollowUpReport {
        complete: pins_done && indexed && finalize.pending_anchors == 0,
        pins,
        reindex,
        finalize,
    })
}

async fn reindex(client: &DaemonClient, outcome: &MergeOutcome) -> ReindexResult {
    let result = |path: Option<&String>, state: &str| ReindexResult {
        path: path.cloned(),
        state: state.to_owned(),
        scan_id: None,
        quarantined: 0,
        detail: None,
    };
    if let Some(done) = &outcome.reindexed {
        return ReindexResult {
            detail: Some("a completed follow-up re-indexed it after the merge".to_owned()),
            ..result(Some(done), "skipped")
        };
    }
    let Some(path) = &outcome.reindex_path else {
        return ReindexResult {
            detail: Some("no checkout of the survivor exists".to_owned()),
            ..result(None, "none")
        };
    };
    if !Path::new(path).exists() {
        return result(Some(path), "missing");
    }
    match client.index_project(Path::new(path), None).await {
        Ok(response) if response.resolved.project.id == outcome.survivor => ReindexResult {
            scan_id: Some(response.index.scan_id),
            quarantined: response.invalidated_memories,
            ..result(Some(path), "indexed")
        },
        Ok(response) => ReindexResult {
            detail: Some(format!(
                "the checkout resolves to {} ({}), not the survivor",
                response.resolved.project.name, response.resolved.project.id
            )),
            ..result(Some(path), "failed")
        },
        Err(error) => ReindexResult {
            detail: Some(error.to_string()),
            ..result(Some(path), "failed")
        },
    }
}
