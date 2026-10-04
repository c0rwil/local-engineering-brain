//! Plain-text rendering of merge plans and outcomes. Output depends only on
//! its input, which lists everything in a fixed order, so the same database
//! state always renders the same text.

use std::fmt::Write as _;

use super::client::FollowUpReport;
use super::{MergeOutcome, MergeParty, MergePlan};
use crate::projects::{PinRewrite, PinState};

/// The dry-run report.
#[must_use]
pub fn plan_text(plan: &MergePlan, pins: &[PinRewrite]) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Merge plan: {} into {}",
        party_label(&plan.retired),
        party_label(&plan.survivor)
    );
    out.push('\n');
    identity_section(&mut out, plan);
    out.push('\n');
    let _ = writeln!(
        out,
        "Rows of {} (moved: re-scoped to {}; rebuilt: dropped, then rebuilt; retired: kept \
         with the retired id):",
        plan.retired.name, plan.survivor.name
    );
    let width = plan
        .tables
        .iter()
        .map(|table| table.table.len())
        .max()
        .unwrap_or(0);
    for table in &plan.tables {
        let _ = writeln!(
            out,
            "  {:<8} {:<width$} {:>8}  {}",
            table.class, table.table, table.rows, table.role
        );
    }
    out.push('\n');
    list_section(
        &mut out,
        "Key collisions",
        plan.collisions
            .iter()
            .map(|collision| {
                format!(
                    "{} {}: {}",
                    collision.table, collision.key, collision.resolution
                )
            })
            .collect(),
    );
    list_section(
        &mut out,
        "Memories with the same claim in both projects (both kept)",
        plan.duplicate_claims
            .iter()
            .map(|pair| format!("{} and {}", pair.survivor_memory_id, pair.retired_memory_id))
            .collect(),
    );
    list_section(
        &mut out,
        &format!("Facts of {}", plan.retired.name),
        plan.facts
            .iter()
            .map(|fact| match &fact.survivor_value {
                Some(kept) => format!(
                    "{}: {} (retired value {:?}, survivor value {kept:?})",
                    fact.key, fact.decision, fact.retired_value
                ),
                None => format!("{}: {} ({:?})", fact.key, fact.decision, fact.retired_value),
            })
            .collect(),
    );
    list_section(
        &mut out,
        "Links",
        plan.links
            .iter()
            .map(|link| {
                format!(
                    "{} -> {} ({}): {}",
                    link.project_id, link.linked_project_id, link.relation, link.decision
                )
            })
            .collect(),
    );
    let mut views = Vec::new();
    if let Some(view) = &plan.views.repaired {
        views.push(format!("{view}: re-paired with {}", plan.survivor.name));
    }
    for view in &plan.views.retired {
        views.push(format!("{view}: retired with {}", plan.retired.name));
    }
    list_section(&mut out, "Origin views", views);
    let _ = writeln!(
        out,
        "Memory anchors on {}'s symbols: {} ({} re-attached in the merge, {} after the re-index)",
        plan.retired.name, plan.anchors.total, plan.anchors.remap_now, plan.anchors.after_reindex
    );
    list_section(
        &mut out,
        "Product records re-anchored",
        plan.product_records.clone(),
    );
    list_section(
        &mut out,
        "Unfinished jobs cancelled",
        plan.cancelled_jobs.clone(),
    );
    list_section(&mut out, "Pins", pins.iter().map(pin_line).collect());
    list_section(
        &mut out,
        "Re-indexed after the merge, in order",
        plan.reindex_paths.clone(),
    );
    if let Some(refusal) = &plan.identity.refusal {
        let _ = writeln!(out, "Refused: {refusal}");
    } else {
        out.push_str(
            "A transactionally consistent backup is taken before anything changes.\n",
        );
    }
    out
}

fn identity_section(out: &mut String, plan: &MergePlan) {
    let identity = &plan.identity;
    if identity.same_repository {
        let _ = writeln!(
            out,
            "Same repository: yes ({} at root commit {})",
            identity.canonical_remote.as_deref().unwrap_or("?"),
            short(identity.root_commit.as_deref().unwrap_or("?"))
        );
    } else {
        out.push_str("Same repository: not proven\n");
    }
    for probe in &identity.checkouts {
        let name = if probe.project_id == plan.survivor.id {
            &plan.survivor.name
        } else {
            &plan.retired.name
        };
        let detail = probe
            .detail
            .as_deref()
            .map_or_else(String::new, |detail| format!(" ({detail})"));
        let _ = writeln!(
            out,
            "  {name}: {} {}{detail}",
            probe.path, probe.state
        );
    }
    let fill = &plan.identity_fill;
    if let Some(remote) = &fill.canonical_remote {
        let _ = writeln!(
            out,
            "{} records the canonical remote {remote} (it had none).",
            plan.survivor.name
        );
    }
    if let Some(root) = &fill.root_commit {
        let _ = writeln!(
            out,
            "{} records the root commit {} (it had none).",
            plan.survivor.name,
            short(root)
        );
    }
}

/// The report after `--yes`.
#[must_use]
pub fn outcome_text(outcome: &MergeOutcome, already: bool, follow_up: &FollowUpReport) -> String {
    let mut out = String::new();
    let retired = format!("{} ({})", outcome.retired_name, outcome.retired);
    let survivor = format!("{} ({})", outcome.survivor_name, outcome.survivor);
    if already {
        let _ = writeln!(out, "Already merged: {retired} into {survivor}.");
    } else {
        let _ = writeln!(out, "Merged {retired} into {survivor}.");
    }
    if let Some(backup) = &outcome.audit.backup_path {
        let _ = writeln!(out, "Backup taken before the merge: {backup}");
    }
    let audit = &outcome.audit;
    counts_line(&mut out, "Moved to the survivor", &audit.moved);
    counts_line(&mut out, "Dropped for the rebuild", &audit.rebuilt);
    counts_line(&mut out, "Kept with the retired id", &audit.kept);
    let _ = writeln!(
        out,
        "Key collisions resolved: {}; memories with the same claim kept: {}",
        audit.collisions.len(),
        audit.duplicate_claims.len()
    );
    list_section(
        &mut out,
        "Pins",
        follow_up.pins.iter().map(pin_line).collect(),
    );
    list_section(
        &mut out,
        "Re-index",
        follow_up
            .reindex
            .iter()
            .map(|result| {
                let detail = result
                    .scan_id
                    .as_deref()
                    .or(result.detail.as_deref())
                    .map_or_else(String::new, |detail| format!(": {detail}"));
                format!("{} {}{detail}", result.state, result.path)
            })
            .collect(),
    );
    let finalize = &follow_up.finalize;
    let _ = writeln!(
        out,
        "Memory anchors: {} re-attached in the merge, {} after the re-index, {} memories marked \
         for revalidation, {} still waiting",
        audit.anchors_remapped,
        finalize.anchors_remapped,
        finalize.revalidate.len(),
        finalize.pending_anchors
    );
    counts_line(
        &mut out,
        "Written under the retired id since the merge, now moved",
        &finalize.stragglers,
    );
    if follow_up.complete {
        out.push_str("Done.\n");
    } else {
        out.push_str(
            "Some steps did not finish (see above). Fix them, then run the same command again: \
             a repeated merge only retries the remaining steps.\n",
        );
    }
    out
}

fn party_label(party: &MergeParty) -> String {
    let hidden = if party.hidden { ", hidden" } else { "" };
    format!("{} ({}{hidden})", party.name, party.id)
}

fn list_section(out: &mut String, title: &str, items: Vec<String>) {
    if items.is_empty() {
        let _ = writeln!(out, "{title}: none");
        return;
    }
    let _ = writeln!(out, "{title} ({}):", items.len());
    for item in items {
        let _ = writeln!(out, "  {item}");
    }
}

fn counts_line(out: &mut String, title: &str, counts: &std::collections::BTreeMap<String, u64>) {
    let parts = counts
        .iter()
        .filter(|(_, rows)| **rows > 0)
        .map(|(table, rows)| format!("{table} {rows}"))
        .collect::<Vec<_>>();
    if parts.is_empty() {
        let _ = writeln!(out, "{title}: none");
    } else {
        let _ = writeln!(out, "{title}: {}", parts.join(", "));
    }
}

fn pin_line(pin: &PinRewrite) -> String {
    let state = match pin.state {
        PinState::Rewritten => "rewritten",
        PinState::WouldRewrite => "would rewrite",
        PinState::AlreadySurvivor => "already the survivor",
        PinState::OtherProject => "pins another project; left alone",
        PinState::NoPin => "no pin",
        PinState::Missing => "checkout missing",
        PinState::Denied => {
            "denied (on macOS grant this terminal access to the folder, then run the merge again)"
        }
        PinState::Failed => "failed",
    };
    let target = pin
        .file
        .as_ref()
        .map_or_else(|| pin.checkout.display().to_string(), |file| file.display().to_string());
    let detail = pin
        .detail
        .as_deref()
        .map_or_else(String::new, |detail| format!(": {detail}"));
    format!("{target}: {state}{detail}")
}

fn short(commit: &str) -> &str {
    &commit[..commit.len().min(12)]
}
