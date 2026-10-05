//! Operator-run expiry of pending lesson reviews that only new verification
//! work can unblock.
//!
//! The judge defers a proposal while a deterministic gate holds and checks it
//! again on every pass. Three gates never clear while the proposal stays as it
//! is ([`GateClass`]): no readable, independently verified evidence; a touched
//! file without an index anchor whose hook digest is missing or no longer
//! matches; and a recorded revision whose touched files have changed since.
//! `engram review expire` closes pending reviews that have not changed for a
//! threshold (14 days by default) and that one of these gates holds, unless a
//! runnable re-verification could clear the gate. The decision reads the
//! judge's gate and the re-verification plan ([`crate::reverify::plan`]); this
//! module only selects, counts and applies.
//!
//! It is never automatic: planning reads only, and nothing changes without
//! `--yes`. Expired is a terminal status, distinct from a rejection. The judge
//! and the pending inbox skip expired reviews, every audit row stays, and
//! `engram review reopen` returns a review to pending.

use std::collections::BTreeMap;

use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::ids::ProjectId;
use crate::judge::{GateClass, JudgeConfig, ReverificationGate, gate_label};
use crate::product::lessons::PlannedExpiry;
use crate::reverify::{self, PlanRequest, ReviewPlan, ReviewStatus};
use crate::security::Redactor;
use crate::storage::Database;

const DAY_MS: i64 = 86_400_000;
/// Default threshold: reviews unchanged for 14 days.
pub const DEFAULT_OLDER_THAN_MS: i64 = 14 * DAY_MS;
/// Largest threshold: ten years.
const MAX_OLDER_THAN_MS: i64 = 3_650 * DAY_MS;
/// Review ids listed per project without `verbose`.
const SAMPLE_IDS: usize = 5;
/// Reviews per re-verification plan; the planner accepts at most 500.
const PLAN_BATCH: usize = 250;

/// Why a considered review stays pending.
const KEPT_UNAVAILABLE: &str = "unavailable";
const KEPT_JUDGE_ELIGIBLE: &str = "judge_eligible";
const KEPT_OTHER_GATE: &str = "other_gate";
const KEPT_REVERIFY: &str = "reverify_may_clear";
const KEPT_NOT_SELECTED: &str = "class_not_selected";

/// Which pending reviews to consider, and whether to expire them.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ExpiryRequest {
    /// Only this project's reviews.
    pub project_id: Option<ProjectId>,
    /// Consider reviews unchanged for at least this long.
    pub older_than_ms: i64,
    /// Only reviews that these gate classes hold; every class when empty.
    pub classes: Vec<GateClass>,
    /// Expire the reviews; otherwise only report.
    pub apply: bool,
    /// List every expirable review with its gate and the start of its claim.
    pub verbose: bool,
}

impl Default for ExpiryRequest {
    fn default() -> Self {
        Self {
            project_id: None,
            older_than_ms: DEFAULT_OLDER_THAN_MS,
            classes: Vec::new(),
            apply: false,
            verbose: false,
        }
    }
}

/// One review that the request expires or would expire.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ExpiryCandidate {
    pub review_id: String,
    pub project_id: Option<ProjectId>,
    pub gate_class: GateClass,
    /// The judge's gate that holds the review.
    pub gate_reason: String,
    /// Whole days since the review last changed.
    pub unchanged_days: i64,
    /// The start of the claim.
    pub claim: String,
}

/// The expirable reviews of one project.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProjectExpiry {
    pub project_id: ProjectId,
    pub project_name: Option<String>,
    pub expirable: usize,
    pub by_class: BTreeMap<GateClass, usize>,
    /// Up to five review ids.
    pub sample_review_ids: Vec<String>,
}

/// What an expiry request found and, when applied, changed.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ExpiryReport {
    pub applied: bool,
    pub as_of_ms: i64,
    pub older_than_ms: i64,
    /// Pending reviews in scope.
    pub pending: usize,
    /// Of those, the reviews unchanged for at least `older_than_ms`.
    pub considered: usize,
    /// Considered reviews the request expires or would expire.
    pub expirable: usize,
    /// Reviews this request expired. A review that changed after the plan
    /// stays pending, so this can be lower than `expirable`.
    pub expired: usize,
    /// Expirable reviews by the class of the gate that holds them.
    pub by_class: BTreeMap<GateClass, usize>,
    /// Considered reviews by the judge's current gate, as fixed labels.
    pub by_gate: BTreeMap<String, usize>,
    /// Why considered reviews stay pending.
    pub kept: BTreeMap<String, usize>,
    pub projects: Vec<ProjectExpiry>,
    /// Every expirable review with the start of its claim; only on request.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reviews: Vec<ExpiryCandidate>,
}

enum Decision {
    Expire(GateClass, String),
    Keep(&'static str),
}

/// Expire a review only when a gate that new work alone clears holds it, and
/// a re-verification cannot clear that gate.
fn decide(review: &ReviewPlan, classes: &[GateClass]) -> Decision {
    let Some(gate) = &review.gate else {
        return Decision::Keep(KEPT_UNAVAILABLE);
    };
    let Some(reason) = &gate.current else {
        return Decision::Keep(KEPT_JUDGE_ELIGIBLE);
    };
    let Some(class) = gate.current_class else {
        return Decision::Keep(KEPT_OTHER_GATE);
    };
    if reverification_may_clear(review.status, gate) {
        return Decision::Keep(KEPT_REVERIFY);
    }
    if !classes.is_empty() && !classes.contains(&class) {
        return Decision::Keep(KEPT_NOT_SELECTED);
    }
    Decision::Expire(class, reason.clone())
}

/// A runnable re-verification protects a review, unless the judge's preview
/// shows that even a passing run leaves a gate that only new work clears.
fn reverification_may_clear(status: ReviewStatus, gate: &ReverificationGate) -> bool {
    status == ReviewStatus::Runnable && !(gate.previewed && gate.if_passing_class.is_some())
}

/// Find the pending reviews the request would expire. Reads canonical storage
/// and the projects' checkouts (local-only Git), like a re-verification plan;
/// changes nothing. Returns the report and the reviews to expire.
///
/// # Errors
///
/// Returns an invalid threshold or storage errors.
pub fn plan(
    database: &Database,
    redactor: &Redactor,
    judge: &JudgeConfig,
    request: &ExpiryRequest,
) -> Result<(ExpiryReport, Vec<PlannedExpiry>)> {
    if !(0..=MAX_OLDER_THAN_MS).contains(&request.older_than_ms) {
        return Err(Error::Config(
            "the age threshold must be between 0 and 3650 days".to_owned(),
        ));
    }
    let as_of_ms = crate::product::now_ms();
    let cutoff = as_of_ms.saturating_sub(request.older_than_ms);
    let pending = pending_reviews(database, request.project_id.as_ref())?;
    let considered: Vec<&(String, i64)> = pending
        .iter()
        .filter(|(_, updated)| *updated <= cutoff)
        .collect();
    let mut tally = Tally {
        report: ExpiryReport {
            as_of_ms,
            older_than_ms: request.older_than_ms,
            pending: pending.len(),
            considered: considered.len(),
            ..ExpiryReport::default()
        },
        projects: BTreeMap::new(),
        expiries: Vec::new(),
    };
    for batch in considered.chunks(PLAN_BATCH) {
        let planned = reverify::plan(
            database,
            redactor,
            judge,
            &PlanRequest {
                review_ids: batch.iter().map(|(id, _)| id.clone()).collect(),
                project_id: None,
                all: false,
            },
        )?;
        for review in &planned.reviews {
            if let Some((_, updated_at_ms)) = batch.iter().find(|(id, _)| *id == review.review_id) {
                tally.add(review, *updated_at_ms, request);
            }
        }
    }
    let Tally {
        mut report,
        projects,
        expiries,
    } = tally;
    report.projects = projects.into_values().collect();
    report
        .projects
        .sort_by_key(|project| std::cmp::Reverse(project.expirable));
    Ok((report, expiries))
}

/// Pending reviews in scope with their last change, oldest first per project.
fn pending_reviews(database: &Database, project: Option<&ProjectId>) -> Result<Vec<(String, i64)>> {
    database.with_reader(|connection| {
        let mut statement = connection.prepare(
            "SELECT id,updated_at_ms FROM lesson_reviews WHERE status='pending'
             AND (?1 IS NULL OR project_id=?1) ORDER BY project_id,updated_at_ms,id",
        )?;
        Ok(statement
            .query_map(params![project.map(ProjectId::as_str)], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?)
    })
}

/// The report, its projects and the reviews to expire, as reviews are decided.
struct Tally {
    report: ExpiryReport,
    projects: BTreeMap<ProjectId, ProjectExpiry>,
    expiries: Vec<PlannedExpiry>,
}

impl Tally {
    fn add(&mut self, review: &ReviewPlan, updated_at_ms: i64, request: &ExpiryRequest) {
        let report = &mut self.report;
        if let Some(reason) = review
            .gate
            .as_ref()
            .and_then(|gate| gate.current.as_deref())
        {
            *report
                .by_gate
                .entry(gate_label(reason).to_owned())
                .or_default() += 1;
        }
        let (gate_class, gate_reason) = match decide(review, &request.classes) {
            Decision::Keep(why) => {
                *report.kept.entry(why.to_owned()).or_default() += 1;
                return;
            }
            Decision::Expire(class, reason) => (class, reason),
        };
        report.expirable += 1;
        *report.by_class.entry(gate_class).or_default() += 1;
        if let Some(project_id) = &review.project_id {
            let project =
                self.projects
                    .entry(project_id.clone())
                    .or_insert_with(|| ProjectExpiry {
                        project_id: project_id.clone(),
                        project_name: review.project_name.clone(),
                        expirable: 0,
                        by_class: BTreeMap::new(),
                        sample_review_ids: Vec::new(),
                    });
            project.expirable += 1;
            *project.by_class.entry(gate_class).or_default() += 1;
            if project.sample_review_ids.len() < SAMPLE_IDS {
                project.sample_review_ids.push(review.review_id.clone());
            }
        }
        if request.verbose {
            report.reviews.push(ExpiryCandidate {
                review_id: review.review_id.clone(),
                project_id: review.project_id.clone(),
                gate_class,
                gate_reason: gate_reason.clone(),
                unchanged_days: (report.as_of_ms - updated_at_ms) / DAY_MS,
                claim: review.claim.clone(),
            });
        }
        self.expiries.push(PlannedExpiry {
            review_id: review.review_id.clone(),
            updated_at_ms,
            gate_class,
            gate_reason,
        });
    }
}

impl ExpiryReport {
    /// Terminal lines: the totals, the kept reasons, then each project.
    #[must_use]
    pub fn lines(&self) -> Vec<String> {
        let considered = if self.older_than_ms == 0 {
            format!("{} considered at any age", self.considered)
        } else {
            format!(
                "{} unchanged for {} or longer",
                self.considered,
                duration(self.older_than_ms)
            )
        };
        let mut lines = vec![format!(
            "{} pending reviews; {considered}; {} {}.",
            self.pending,
            self.expirable,
            if self.applied {
                format!("can expire and {} expired", self.expired)
            } else {
                "can expire".to_owned()
            }
        )];
        if !self.by_class.is_empty() {
            lines.push(format!("  By gate: {}.", counts(&self.by_class)));
        }
        if !self.kept.is_empty() {
            lines.push(format!("  Kept pending: {}.", counts(&self.kept)));
        }
        for project in &self.projects {
            lines.push(format!(
                "  {}: {} ({}); for example {}",
                project
                    .project_name
                    .as_deref()
                    .unwrap_or(project.project_id.as_str()),
                project.expirable,
                counts(&project.by_class),
                project.sample_review_ids.join(", ")
            ));
        }
        for review in &self.reviews {
            lines.push(format!(
                "    {} · {} · unchanged {} days · {}",
                review.review_id, review.gate_class, review.unchanged_days, review.claim
            ));
        }
        if self.applied {
            lines.push(format!(
                "Expired {} reviews. `engram review reopen <id>` returns one to pending.",
                self.expired
            ));
        } else if self.expirable > 0 {
            lines.push(
                "Nothing changed. Re-run with --yes to expire them; \
                 `engram review reopen <id>` returns one to pending."
                    .to_owned(),
            );
        }
        lines
    }
}

fn counts<K: std::fmt::Display>(map: &BTreeMap<K, usize>) -> String {
    map.iter()
        .map(|(key, count)| format!("{key} {count}"))
        .collect::<Vec<_>>()
        .join(" · ")
}

/// A threshold in whole days, hours or minutes when it divides evenly.
fn duration(ms: i64) -> String {
    let unit =
        |count: i64, name: &str| format!("{count} {name}{}", if count == 1 { "" } else { "s" });
    match ms {
        0 => "any time".to_owned(),
        ms if ms % DAY_MS == 0 => unit(ms / DAY_MS, "day"),
        ms if ms % 3_600_000 == 0 => unit(ms / 3_600_000, "hour"),
        ms => unit(ms / 60_000, "minute"),
    }
}

/// Parse a threshold such as `14d`, `36h` or `90m`; a bare number is days.
///
/// # Errors
///
/// Returns an error for anything else or a threshold over 3650 days.
pub fn parse_age(value: &str) -> Result<i64> {
    let value = value.trim();
    let (digits, unit) = match value.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
        Some((index, _)) => value.split_at(index),
        None => (value, "d"),
    };
    let scale = match unit {
        "d" => DAY_MS,
        "h" => 3_600_000,
        "m" => 60_000,
        _ => 0,
    };
    let ms = digits
        .parse::<i64>()
        .ok()
        .filter(|_| scale > 0)
        .and_then(|count| count.checked_mul(scale))
        .filter(|ms| *ms <= MAX_OLDER_THAN_MS);
    ms.ok_or_else(|| {
        Error::Config(format!(
            "`{value}` is not an age such as 14d, 36h or 90m (at most 3650 days)"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reverify::ReviewStatus;

    fn review(status: ReviewStatus, gate: Option<ReverificationGate>) -> ReviewPlan {
        let gate = serde_json::to_value(gate).expect("gate");
        serde_json::from_value(serde_json::json!({
            "review_id": "review_one", "project_id": null, "project_name": null,
            "task_id": null, "claim": "", "status": status, "reason": "",
            "checkout": null, "head": null, "source_files": [], "excluded_files": 0,
            "files_absent_at_head": 0, "files_changed_since_head": 0, "commands": [],
            "gate": gate,
        }))
        .expect("plan")
    }

    fn gate(
        current: Option<&str>,
        current_class: Option<GateClass>,
        if_passing: Option<(&str, Option<GateClass>)>,
    ) -> ReverificationGate {
        ReverificationGate {
            current: current.map(str::to_owned),
            current_class,
            previewed: if_passing.is_some(),
            if_passing: if_passing.map(|(reason, _)| reason.to_owned()),
            if_passing_class: if_passing.and_then(|(_, class)| class),
        }
    }

    fn outcome(review: &ReviewPlan, classes: &[GateClass]) -> String {
        match decide(review, classes) {
            Decision::Expire(class, _) => format!("expire {class}"),
            Decision::Keep(why) => format!("keep {why}"),
        }
    }

    #[test]
    fn only_gates_that_new_work_alone_clears_expire() {
        use GateClass::{NoVerifiedEvidence, RevisionChanged, UnanchoredFiles};
        let no_evidence = Some(crate::judge::NO_VERIFIED_EVIDENCE);
        let cases = [
            (review(ReviewStatus::Unavailable, None), "keep unavailable"),
            (
                review(ReviewStatus::Refused, Some(gate(None, None, None))),
                "keep judge_eligible",
            ),
            (
                review(
                    ReviewStatus::Refused,
                    Some(gate(Some("Touched file differs"), None, None)),
                ),
                "keep other_gate",
            ),
            (
                review(
                    ReviewStatus::Refused,
                    Some(gate(no_evidence, Some(NoVerifiedEvidence), None)),
                ),
                "expire no_verified_evidence",
            ),
            (
                review(
                    ReviewStatus::NoCommands,
                    Some(gate(Some("unanchored"), Some(UnanchoredFiles), None)),
                ),
                "expire unanchored_files",
            ),
            // A passing run would clear the gate, or its preview failed.
            (
                review(
                    ReviewStatus::Runnable,
                    Some(gate(no_evidence, Some(NoVerifiedEvidence), None)),
                ),
                "keep reverify_may_clear",
            ),
            // A passing run would leave a gate that indexing can clear.
            (
                review(
                    ReviewStatus::Runnable,
                    Some(gate(
                        no_evidence,
                        Some(NoVerifiedEvidence),
                        Some(("Touched file differs from indexed source", None)),
                    )),
                ),
                "keep reverify_may_clear",
            ),
            // A passing run cannot certify the touched files.
            (
                review(
                    ReviewStatus::Runnable,
                    Some(gate(
                        no_evidence,
                        Some(NoVerifiedEvidence),
                        Some(("unanchored", Some(UnanchoredFiles))),
                    )),
                ),
                "expire no_verified_evidence",
            ),
            (
                review(
                    ReviewStatus::Refused,
                    Some(gate(Some("revision"), Some(RevisionChanged), None)),
                ),
                "expire revision_changed",
            ),
        ];
        for (review, expected) in &cases {
            assert_eq!(outcome(review, &[]), *expected, "{:?}", review.gate);
        }
        let selected = review(
            ReviewStatus::Refused,
            Some(gate(no_evidence, Some(NoVerifiedEvidence), None)),
        );
        assert_eq!(
            outcome(&selected, &[RevisionChanged]),
            "keep class_not_selected"
        );
        assert_eq!(
            outcome(&selected, &[NoVerifiedEvidence, RevisionChanged]),
            "expire no_verified_evidence"
        );
    }

    #[test]
    fn ages_parse_with_units_and_bounds() {
        assert_eq!(parse_age("14d").expect("days"), 14 * DAY_MS);
        assert_eq!(parse_age("14").expect("bare days"), 14 * DAY_MS);
        assert_eq!(parse_age("36h").expect("hours"), 36 * 3_600_000);
        assert_eq!(parse_age("90m").expect("minutes"), 90 * 60_000);
        assert_eq!(parse_age("0d").expect("zero"), 0);
        for bad in ["", "d", "14w", "-1d", "3651d", "1.5d", "14 d"] {
            assert!(parse_age(bad).is_err(), "{bad}");
        }
        assert_eq!(duration(14 * DAY_MS), "14 days");
        assert_eq!(duration(DAY_MS), "1 day");
        assert_eq!(duration(36 * 3_600_000), "36 hours");
        assert_eq!(duration(0), "any time");
    }
}
