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
    visibility_note(&mut out, &plan.survivor, &plan.retired);
    out.push('\n');
    identity_section(&mut out, plan);
    out.push('\n');
    rows_section(&mut out, plan);
    out.push('\n');
    decisions_section(&mut out, plan);
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
    match &plan.reindex_path {
        Some(path) => {
            let _ = writeln!(
                out,
                "Re-indexed after the merge: {path} (the survivor's primary checkout; its other \
                 checkouts stay recorded paths)"
            );
        }
        None => out.push_str("Re-indexed after the merge: nothing (no checkout exists)\n"),
    }
    if let Some(refusal) = &plan.identity.refusal {
        let _ = writeln!(out, "Refused: {refusal}");
    } else {
        out.push_str("A transactionally consistent backup is taken before anything changes.\n");
    }
    out
}

/// Every project-keyed table: moved first, then rebuilt, then retired.
fn rows_section(out: &mut String, plan: &MergePlan) {
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
    for class in ["moved", "rebuilt", "retired"] {
        for table in plan.tables.iter().filter(|table| table.class == class) {
            let _ = writeln!(
                out,
                "  {:<8} {:<width$} {:>8}  {}",
                table.class, table.table, table.rows, table.role
            );
        }
    }
}

fn decisions_section(out: &mut String, plan: &MergePlan) {
    list_section(
        out,
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
        out,
        "Memories with the same claim in both projects (both kept)",
        plan.duplicate_claims
            .iter()
            .map(|pair| format!("{} and {}", pair.survivor_memory_id, pair.retired_memory_id))
            .collect(),
    );
    list_section(
        out,
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
        out,
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
    list_section(out, "Origin views", views);
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
        let _ = writeln!(out, "  {name}: {} {}{detail}", probe.path, probe.state);
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
    reindex_line(&mut out, &follow_up.reindex);
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
        "Written under the retired id since the merge, then moved to the survivor",
        &finalize.stragglers_since_merge,
    );
    if follow_up.complete {
        out.push_str("Done.\n");
    } else {
        out.push_str(
            "Some steps did not finish (see above). Fix them, then run the same command again: \
             a repeated merge rewrites only pins that still hold the retired id, skips a \
             re-index that already finished, and resolves anchors that still wait.\n",
        );
    }
    out
}

fn reindex_line(out: &mut String, reindex: &super::client::ReindexResult) {
    let path = reindex.path.as_deref().unwrap_or("no checkout");
    let detail = reindex
        .detail
        .as_deref()
        .map_or_else(String::new, |detail| format!(": {detail}"));
    match reindex.state.as_str() {
        "indexed" => {
            let _ = writeln!(
                out,
                "Re-index: indexed {path} (scan {}); {} memories quarantined because a symbol \
                 they are anchored to changed in this tree (`engram memory list --path {path}` \
                 shows them as suspect)",
                reindex.scan_id.as_deref().unwrap_or("?"),
                reindex.quarantined
            );
        }
        state => {
            let _ = writeln!(out, "Re-index: {state} {path}{detail}");
        }
    }
}

fn party_label(party: &MergeParty) -> String {
    format!("{} ({})", party.name, party.id)
}

/// Say what hiding means after a merge: the survivor keeps its facts, so a
/// hidden survivor hides the merged project as a whole.
fn visibility_note(out: &mut String, survivor: &MergeParty, retired: &MergeParty) {
    if survivor.hidden {
        let _ = writeln!(
            out,
            "Survivor is hidden ({} has internal.hidden = true): the merged project will not \
             appear in workspace views (browser, vault, product catalog){}.",
            survivor.name,
            if retired.hidden {
                ""
            } else {
                "; the retired project is visible today"
            }
        );
    } else if retired.hidden {
        let _ = writeln!(
            out,
            "The retired project is hidden; the survivor stays visible in workspace views."
        );
    }
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
    let target = pin.file.as_ref().map_or_else(
        || pin.checkout.display().to_string(),
        |file| file.display().to_string(),
    );
    let detail = pin
        .detail
        .as_deref()
        .map_or_else(String::new, |detail| format!(": {detail}"));
    format!("{target}: {state}{detail}")
}

fn short(commit: &str) -> &str {
    &commit[..commit.len().min(12)]
}
