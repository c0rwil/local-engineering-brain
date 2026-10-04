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

/// One checkout path re-indexed after a merge.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ReindexResult {
    pub path: String,
    /// `indexed`, `missing` or `failed`.
    pub state: String,
    pub scan_id: Option<String>,
    pub detail: Option<String>,
}

/// What the steps after a merge transaction did.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct FollowUpReport {
    pub pins: Vec<PinRewrite>,
    pub reindex: Vec<ReindexResult>,
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

/// Finish a merge from the client: rewrite pins, re-index the survivor over
/// every checkout path in the outcome's order, then finalize. Anchors are
/// resolved only when every existing checkout was re-indexed; otherwise they
/// keep waiting for a repeated merge.
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
    let mut reindex = Vec::new();
    for path in &outcome.reindex_paths {
        let checkout = Path::new(path);
        if !checkout.exists() {
            reindex.push(ReindexResult {
                path: path.clone(),
                state: "missing".to_owned(),
                scan_id: None,
                detail: None,
            });
            continue;
        }
        reindex.push(match client.index_project(checkout, None).await {
            Ok(response) if response.resolved.project.id == outcome.survivor => ReindexResult {
                path: path.clone(),
                state: "indexed".to_owned(),
                scan_id: Some(response.index.scan_id),
                detail: None,
            },
            Ok(response) => ReindexResult {
                path: path.clone(),
                state: "failed".to_owned(),
                scan_id: None,
                detail: Some(format!(
                    "the checkout resolves to {} ({}), not the survivor",
                    response.resolved.project.name, response.resolved.project.id
                )),
            },
            Err(error) => ReindexResult {
                path: path.clone(),
                state: "failed".to_owned(),
                scan_id: None,
                detail: Some(error.to_string()),
            },
        });
    }
    let indexed = reindex.iter().all(|result| result.state != "failed");
    let finalize = client
        .finalize_merge(&MergeFinalizeRequest {
            project_id: outcome.survivor.clone(),
            resolve_anchors: indexed,
        })
        .await?;
    let pins_done = pins.iter().all(|pin| {
        matches!(
            pin.state,
            PinState::Rewritten
                | PinState::AlreadySurvivor
                | PinState::OtherProject
                | PinState::NoPin
                | PinState::Missing
        )
    });
    Ok(FollowUpReport {
        complete: pins_done && indexed && finalize.pending_anchors == 0,
        pins,
        reindex,
        finalize,
    })
}
