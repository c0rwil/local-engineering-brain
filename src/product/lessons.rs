//! Low-ceremony episode completion and explicit, evidence-gated lesson review.

use std::collections::BTreeSet;

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{ProductStore, invalid, now_ms};
use crate::error::Result;
use crate::experience::{
    AttemptOutcome, EvidenceStrength, ExperienceStore, TaskOutcome, TaskRecord,
};
use crate::ids::{EvidenceId, MemoryId, ProjectId, TaskId};
use crate::learning::{DistillationRequest, LearningStore};
use crate::memory::{EpistemicState, MemoryDraft, MemoryStatus, MemoryStore};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Verification {
    pub kind: String,
    pub strength: EvidenceStrength,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub exit_code: Option<i32>,
    pub artifact: String,
    #[serde(default)]
    pub revision: Option<String>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LessonDraft {
    pub claim: String,
    #[serde(default)]
    pub rationale: String,
    #[serde(default)]
    pub evidence_ids: Vec<EvidenceId>,
    /// Optional lifetime for environment-specific lessons. Expired lessons stop injecting.
    #[serde(default)]
    pub expires_at_ms: Option<i64>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Completion {
    pub task_id: TaskId,
    pub outcome: TaskOutcome,
    #[serde(default)]
    pub accepted_by_user: Option<bool>,
    #[serde(default)]
    pub final_commit: Option<String>,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub verifications: Vec<Verification>,
    #[serde(default)]
    pub lessons: Vec<LessonDraft>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CompletionResult {
    pub task: TaskRecord,
    pub evidence_ids: Vec<EvidenceId>,
    pub reviews: Vec<LessonReview>,
    pub next_step: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LessonReview {
    pub id: String,
    pub task_id: TaskId,
    pub project_id: ProjectId,
    pub claim: String,
    pub rationale: String,
    pub evidence_ids: Vec<EvidenceId>,
    pub status: String,
    pub memory_id: Option<MemoryId>,
    pub expires_at_ms: Option<i64>,
    pub updated_at_ms: i64,
    /// Lexical overlap is a review hint, not a claim of logical contradiction.
    pub related_memories: Vec<RelatedMemory>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RelatedMemory {
    pub id: MemoryId,
    pub claim: String,
    pub exact: bool,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Accept,
    Reject,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewRequest {
    pub id: String,
    pub expected_updated_at_ms: i64,
    pub decision: Decision,
    #[serde(default)]
    pub claim: Option<String>,
    #[serde(default)]
    pub rationale: Option<String>,
    /// Explicitly identified contradictions; never inferred from similarity.
    #[serde(default)]
    pub contradicts: Vec<MemoryId>,
    #[serde(default)]
    pub reviewed_related: bool,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalKind {
    Verification,
    CommitLanded,
    PrMerged,
    UserAcceptance,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Signal {
    pub task_id: TaskId,
    pub kind: SignalKind,
    pub reference: String,
    pub evidence_id: EvidenceId,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum LessonMutation {
    Complete(Completion),
    Review(ReviewRequest),
    Signal(Signal),
}

impl ProductStore {
    /// Dispatch episode and explicit review actions through one scheduler call.
    /// # Errors
    /// Returns validation or storage errors.
    pub fn lesson_mutate(&self, request: &LessonMutation) -> Result<serde_json::Value> {
        Ok(match request {
            LessonMutation::Complete(r) => serde_json::to_value(self.complete(r)?)?,
            LessonMutation::Review(r) => serde_json::to_value(self.review(r)?)?,
            LessonMutation::Signal(r) => {
                self.signal(r)?;
                serde_json::json!({"recorded":true,"task_acceptance_changed":false})
            }
        })
    }

    /// Record verification, close an episode and queue a few reviewable lessons.
    /// Retries reuse evidence and normalized task-scoped proposals. Each stage
    /// persists independently; a failure can be retried without relabeling work.
    /// # Errors
    /// Returns invalid evidence/scope/outcome, storage or redaction errors.
    #[allow(clippy::too_many_lines)]
    pub fn complete(&self, request: &Completion) -> Result<CompletionResult> {
        if request.verifications.len() > 16
            || request.lessons.len() > 5
            || request.files.len() > 128
            || request.summary.len() > 16_384
        {
            return Err(invalid(
                "completion accepts up to 16 verifications, 5 lessons, 128 files and a 16 KiB summary",
            ));
        }
        let experience =
            ExperienceStore::with_redactor(self.database.clone(), self.redactor.clone());
        let task = experience
            .get_task(&request.task_id)?
            .ok_or_else(|| invalid("unknown task"))?;
        if task.status != "open" && TaskOutcome::parse(&task.status)? != request.outcome {
            return Err(invalid(
                "task already closed with a different outcome; do not relabel failed work",
            ));
        }
        for v in &request.verifications {
            crate::experience::validate_evidence_kind(&v.kind, v.command.as_deref(), v.exit_code)?;
            if v.artifact.is_empty() || v.artifact.len() > 262_144 {
                return Err(invalid(
                    "verification requires 1–262144 bytes of actual output",
                ));
            }
        }
        for lesson in &request.lessons {
            validate_lesson(lesson)?;
        }
        if request.outcome != TaskOutcome::Success && !request.lessons.is_empty() {
            return Err(invalid(
                "lessons require a successful task; record the unsuccessful outcome without lessons",
            ));
        }
        let mut evidence_ids = Vec::new();
        for v in &request.verifications {
            let artifact_digest = crate::experience::sha256(v.artifact.as_bytes());
            let command = v.command.as_ref().map(|s| self.redactor.redact_text(s));
            let existing: Option<String>=self.database.with_reader(|c|Ok(c.query_row("SELECT id FROM evidence WHERE task_id=?1 AND kind=?2 AND strength=?3 AND artifact_digest=?4 AND command IS ?5 AND exit_code IS ?6 AND commit_hash IS ?7 ORDER BY observed_at_ms,id LIMIT 1",params![task.id.as_str(),v.kind,v.strength.as_str(),artifact_digest,command,v.exit_code,v.revision],|r|r.get(0)).optional()?))?;
            let id = if let Some(id) = existing {
                EvidenceId::parse(&id)?
            } else {
                experience
                    .record_evidence(
                        &task.project_id,
                        Some(&task.id),
                        None,
                        &v.kind,
                        v.strength,
                        v.revision.as_deref(),
                        v.command.as_deref(),
                        v.exit_code,
                        v.artifact.as_bytes(),
                        &serde_json::json!({"capture":"completion"}),
                    )?
                    .id
            };
            evidence_ids.push(id);
        }
        if task.status == "open" && !request.summary.trim().is_empty() {
            let summary = self.redactor.redact_text(&request.summary);
            let exists=self.database.with_reader(|c|Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM attempts WHERE task_id=?1 AND strategy_summary=?2)",params![task.id.as_str(),summary],|r|r.get::<_,bool>(0))?))?;
            if !exists {
                experience.record_attempt(
                    &task.id,
                    &summary,
                    match request.outcome {
                        TaskOutcome::Success => AttemptOutcome::Success,
                        TaskOutcome::Failure => AttemptOutcome::Failure,
                        _ => AttemptOutcome::Partial,
                    },
                    &request.files,
                )?;
            }
        }
        let task = experience.finish_task(
            &task.id,
            request.outcome,
            request.accepted_by_user,
            request.final_commit.as_deref(),
        )?;
        let mut lessons = request.lessons.clone();
        if task.status == "success" && lessons.is_empty() && !request.summary.trim().is_empty() {
            lessons.push(LessonDraft { claim:request.summary.clone(),rationale:"Proposed from the completion summary; edit this into a reusable rule before accepting.".to_owned(),evidence_ids:Vec::new(),expires_at_ms:None });
        }
        let eligible_ids:Vec<EvidenceId>=self.database.with_reader(|c| {
            let mut s=c.prepare("SELECT evidence_id FROM verified_supporting_evidence WHERE task_id=?1 AND project_id=?2 ORDER BY evidence_id LIMIT 32")?;
            let ids=s.query_map(params![task.id.as_str(),task.project_id.as_str()],|r|r.get::<_,String>(0))?.collect::<std::result::Result<Vec<_>,_>>()?;
            ids.iter().map(|id|EvidenceId::parse(id)).collect()
        })?;
        for lesson in &mut lessons {
            if lesson.evidence_ids.is_empty() {
                lesson.evidence_ids.clone_from(&eligible_ids);
            }
            // A proposal without eligible evidence remains a proposal. Review
            // acceptance still passes through the existing distillation gate.
            self.propose(&task, lesson)?;
        }
        let reviews = self
            .reviews()?
            .into_iter()
            .filter(|r| r.task_id == task.id)
            .collect();
        Ok(CompletionResult { task,evidence_ids,reviews,next_step:"Proposed lessons await evidence review. An enabled judge reviews them automatically; Knowledge also supports human review. Lesson approval is separate from user acceptance of the task. Unsupported proposals cannot become validated memories.".to_owned() })
    }

    fn propose(&self, task: &TaskRecord, lesson: &LessonDraft) -> Result<()> {
        validate_lesson(lesson)?;
        let claim = self.redactor.redact_text(&lesson.claim);
        let rationale = self.redactor.redact_text(&lesson.rationale);
        let normalized = normalize(&claim);
        let now = now_ms();
        self.database.write_immediate(|tx| {
            for id in &lesson.evidence_ids {
                let valid=tx.query_row("SELECT EXISTS(SELECT 1 FROM evidence WHERE id=?1 AND task_id=?2 AND project_id=?3)",params![id.as_str(),task.id.as_str(),task.project_id.as_str()],|r|r.get::<_,bool>(0))?;
                if !valid { return Err(invalid("lesson evidence must come from this source task and project")); }
            }
            let existing:Option<(String,String,String)>=tx.query_row("SELECT id,evidence_ids_json,status FROM lesson_reviews WHERE task_id=?1 AND normalized_claim=?2",params![task.id.as_str(),normalized],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
            if let Some((id,raw,status))=existing {
                if status=="pending" {
                    let mut ids:Vec<EvidenceId>=serde_json::from_str(&raw)?;
                    for new in &lesson.evidence_ids { if !ids.contains(new) {ids.push(new.clone());} }
                    let merged=serde_json::to_string(&ids)?;
                    if merged!=raw { tx.execute("UPDATE lesson_reviews SET evidence_ids_json=?2,updated_at_ms=MAX(updated_at_ms+1,?3) WHERE id=?1",params![id,merged,now])?; }
                }
                return Ok(());
            }
            tx.execute("INSERT INTO lesson_reviews(id,task_id,project_id,claim,normalized_claim,rationale,evidence_ids_json,status,expires_at_ms,created_at_ms,updated_at_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,'pending',?8,?9,?9) ON CONFLICT(task_id,normalized_claim) DO NOTHING",params![format!("review_{}",Uuid::new_v4()),task.id.as_str(),task.project_id.as_str(),claim,normalized,rationale,serde_json::to_string(&lesson.evidence_ids)?,lesson.expires_at_ms,now])?;
            Ok(())
        })
    }

    /// Read the bounded review inbox including possible overlaps.
    /// # Errors
    /// Returns storage or persisted-data errors.
    pub fn reviews(&self) -> Result<Vec<LessonReview>> {
        self.database.with_reader(|c| {
            let mut s=c.prepare("SELECT id,task_id,project_id,claim,rationale,evidence_ids_json,status,memory_id,expires_at_ms,updated_at_ms FROM lesson_reviews ORDER BY CASE status WHEN 'pending' THEN 0 ELSE 1 END,updated_at_ms DESC,id LIMIT 200")?;
            let rows=s.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?,r.get::<_,String>(5)?,r.get::<_,String>(6)?,r.get::<_,Option<String>>(7)?,r.get::<_,Option<i64>>(8)?,r.get::<_,i64>(9)?)))?.collect::<std::result::Result<Vec<_>,_>>()?;
            rows.into_iter().map(|row| {
                let mut s=c.prepare("SELECT id,claim FROM memories WHERE project_id=?1 AND status IN ('active','suspect') ORDER BY updated_at_ms DESC LIMIT 500")?;
                let candidates=s.query_map([&row.2],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?.collect::<std::result::Result<Vec<_>,_>>()?;
                let mut related=Vec::new();
                for (id,claim) in candidates {
                    if similar(&row.3,&claim) { related.push(RelatedMemory { id:MemoryId::parse(&id)?,exact:normalize(&claim)==normalize(&row.3),claim }); }
                    if related.len()>=8 { break; }
                }
                Ok(LessonReview { id:row.0,task_id:TaskId::parse(&row.1)?,project_id:ProjectId::parse(&row.2)?,claim:row.3,rationale:row.4,evidence_ids:serde_json::from_str(&row.5)?,status:row.6,memory_id:row.7.as_deref().map(MemoryId::parse).transpose()?,expires_at_ms:row.8,updated_at_ms:row.9,related_memories:related })
            }).collect()
        })
    }

    /// Read one proposal by identity, independently of the bounded UI inbox.
    /// # Errors
    /// Returns storage or persisted-data errors.
    pub fn review_by_id(&self, id: &str) -> Result<Option<LessonReview>> {
        let raw = self.database.with_reader(|connection| {
            Ok(connection
                .query_row(
                    "SELECT json_object('id',id,'task_id',task_id,'project_id',project_id,
                 'claim',claim,'rationale',rationale,'evidence_ids',json(evidence_ids_json),
                 'status',status,'memory_id',memory_id,'expires_at_ms',expires_at_ms,
                 'updated_at_ms',updated_at_ms,'related_memories',json('[]'))
                 FROM lesson_reviews WHERE id=?1",
                    [id],
                    |row| row.get::<_, String>(0),
                )
                .optional()?)
        })?;
        let Some(raw) = raw else {
            return Ok(None);
        };
        let mut review: LessonReview = serde_json::from_str(&raw)?;
        review.related_memories = self.related_memories(&review.project_id, &review.claim)?;
        Ok(Some(review))
    }

    /// Read one bounded page of pending identities in stable oldest-first order.
    /// The caller may continue after the last `(updated_at_ms, id)` key.
    /// # Errors
    /// Returns storage errors.
    pub(crate) fn pending_review_page(
        &self,
        after: Option<(i64, &str)>,
        as_of_ms: i64,
    ) -> Result<Vec<(i64, String)>> {
        self.database.with_reader(|connection| {
            let mut statement = connection.prepare(
                "SELECT updated_at_ms,id FROM lesson_reviews WHERE status='pending'
                 AND (?1 IS NULL OR (updated_at_ms,id)>(?1,?2)) AND updated_at_ms<=?3
                 ORDER BY updated_at_ms,id LIMIT 64",
            )?;
            Ok(statement
                .query_map(
                    params![after.map(|key| key.0), after.map(|key| key.1), as_of_ms],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?
                .collect::<std::result::Result<Vec<_>, _>>()?)
        })
    }

    fn related_memories(&self, project_id: &ProjectId, claim: &str) -> Result<Vec<RelatedMemory>> {
        self.database.with_reader(|c| {
            let mut s=c.prepare("SELECT id,claim FROM memories WHERE project_id=?1 AND status IN ('active','suspect') ORDER BY updated_at_ms DESC LIMIT 500")?;
            let candidates=s.query_map([project_id.as_str()],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?.collect::<std::result::Result<Vec<_>,_>>()?;
            candidates.into_iter().filter(|(_,other)|similar(claim,other)).take(8).map(|(id,other)|Ok(RelatedMemory{id:MemoryId::parse(&id)?,exact:normalize(claim)==normalize(&other),claim:other})).collect()
        })
    }

    /// Apply an explicit decision; accepting uses normal evidence gates.
    /// # Errors
    /// Returns stale-review, unsupported-claim, scope or storage errors.
    pub fn review(&self, request: &ReviewRequest) -> Result<LessonReview> {
        self.review_with_evidence(request, None)
    }

    /// Apply a review using only the cited subset of the original source evidence.
    /// The judge cannot add evidence; normal distillation and promotion still gate it.
    /// # Errors
    /// Returns invalid citations, stale review, or normal review validation errors.
    #[allow(clippy::too_many_lines)]
    pub(crate) fn review_with_evidence(
        &self,
        request: &ReviewRequest,
        cited_evidence: Option<&[EvidenceId]>,
    ) -> Result<LessonReview> {
        let mut review = self
            .review_by_id(&request.id)?
            .ok_or_else(|| invalid("review not found"))?;
        if review.status != "pending" {
            return Ok(review);
        }
        if review.updated_at_ms != request.expected_updated_at_ms {
            return Err(invalid("review changed; reload before deciding"));
        }
        if let Some(cited) = cited_evidence {
            if cited.is_empty() || cited.iter().any(|id| !review.evidence_ids.contains(id)) {
                return Err(invalid(
                    "a review can cite only a nonempty subset of its source evidence",
                ));
            }
            review.evidence_ids = cited.to_vec();
        }
        if let Some(claim) = &request.claim {
            review.claim = self.redactor.redact_text(claim);
        }
        if let Some(rationale) = &request.rationale {
            review.rationale = self.redactor.redact_text(rationale);
        }
        validate_lesson(&LessonDraft {
            claim: review.claim.clone(),
            rationale: review.rationale.clone(),
            evidence_ids: review.evidence_ids.clone(),
            expires_at_ms: review.expires_at_ms,
        })?;
        let memory = if matches!(request.decision, Decision::Accept) {
            if review
                .expires_at_ms
                .is_some_and(|expiry| expiry <= now_ms())
            {
                return Err(invalid(
                    "proposal has expired; verify again before learning",
                ));
            }
            if request.claim.is_some() {
                let related = self.related_memories(&review.project_id, &review.claim)?;
                if cited_evidence.is_some()
                    && related.iter().any(|memory| {
                        !memory.exact
                            && !review
                                .related_memories
                                .iter()
                                .any(|old| old.id == memory.id)
                    })
                {
                    return Err(invalid(
                        "narrowed automatic claim overlaps additional memories; re-review the proposal",
                    ));
                }
                review.related_memories = related;
            }
            if !request.reviewed_related && review.related_memories.iter().any(|m| !m.exact) {
                return Err(invalid(
                    "review similar existing memories and set reviewed_related=true before accepting",
                ));
            }
            let memories = MemoryStore::with_redactor(self.database.clone(), self.redactor.clone());
            for id in &request.contradicts {
                let old = memories
                    .get(id)?
                    .ok_or_else(|| invalid("contradicted memory does not exist"))?;
                if old.project_id != review.project_id {
                    return Err(invalid("contradictions must be in the same project"));
                }
                if normalize(&old.claim) == normalize(&review.claim) {
                    return Err(invalid("a duplicate cannot contradict itself"));
                }
            }
            let learned =
                LearningStore::with_redactor(self.database.clone(), self.redactor.clone())
                    .distill(&DistillationRequest {
                        project_id: review.project_id.clone(),
                        memory: MemoryDraft {
                            scope_type: "project".to_owned(),
                            scope_id: review.project_id.to_string(),
                            kind: "lesson".to_owned(),
                            claim: review.claim.clone(),
                            rationale: review.rationale.clone(),
                            confidence: 0.5,
                            importance: 0.7,
                            tags: vec!["reviewed-lesson".to_owned()],
                            valid_from_commit: None,
                            valid_to_commit: None,
                            trust_source: "agent_claim".to_owned(),
                            evidence_ids: review.evidence_ids.clone(),
                            source_task_ids: vec![review.task_id.clone()],
                            node_ids: Vec::new(),
                        },
                    })?;
            let mut memory = learned.memory;
            if memory.status != MemoryStatus::Active
                || memory.epistemic_state == EpistemicState::Rejected
            {
                return Err(invalid(
                    "matching memory is archived, rejected or quarantined; explicitly revalidate it or revise the claim before accepting",
                ));
            }
            if review.expires_at_ms.is_none()
                && memories
                    .expired_ids(&review.project_id)?
                    .contains(&memory.id)
            {
                return Err(invalid(
                    "matching memory expired; a reverified lesson must specify a new expiry",
                ));
            }
            for state in [
                EpistemicState::Observed,
                EpistemicState::Supported,
                EpistemicState::Validated,
            ] {
                if memory.epistemic_state < state {
                    memory = memories.promote(&memory.id, state, false)?;
                }
            }
            for id in &request.contradicts {
                memories.mark_contradicted(id, &memory.id)?;
            }
            Some(memory.id)
        } else {
            None
        };
        let now = now_ms().max(review.updated_at_ms.saturating_add(1));
        self.database.write_immediate(|tx| {
            tx.execute("UPDATE lesson_reviews SET status=?2,claim=?3,rationale=?4,memory_id=?5,updated_at_ms=?6,evidence_ids_json=?7 WHERE id=?1 AND status='pending'",params![review.id,if memory.is_some(){"accepted"}else{"rejected"},review.claim,review.rationale,memory.as_ref().map(MemoryId::as_str),now,serde_json::to_string(&review.evidence_ids)?])?;
            if let (Some(id),Some(expiry))=(&memory,review.expires_at_ms) { tx.execute("INSERT INTO memory_expiry(memory_id,expires_at_ms) VALUES(?1,?2) ON CONFLICT(memory_id) DO UPDATE SET expires_at_ms=excluded.expires_at_ms",params![id.as_str(),expiry])?; }
            Ok(())
        })?;
        // A decision resolves an automatic conflict quarantine this proposal
        // caused: confirmed contradictions stay contradicted; the rest return.
        MemoryStore::with_redactor(self.database.clone(), self.redactor.clone())
            .release_review_conflicts(&review.project_id, &review.id)?;
        if memory.is_some() {
            "accepted"
        } else {
            "rejected"
        }
        .clone_into(&mut review.status);
        review.memory_id = memory;
        review.updated_at_ms = now;
        Ok(review)
    }

    /// Inspect delivery signals without conflating them with user acceptance.
    /// # Errors
    /// Returns unknown task or storage errors.
    pub fn task_signals(&self, task_id: &TaskId) -> Result<serde_json::Value> {
        let task = ExperienceStore::new(self.database.clone())
            .get_task(task_id)?
            .ok_or_else(|| invalid("unknown task"))?;
        let signals=self.database.with_reader(|c| {
            let mut s=c.prepare("SELECT kind,reference,evidence_id,recorded_at_ms FROM task_signals WHERE task_id=?1 ORDER BY recorded_at_ms,kind,reference")?;
            let rows=s.query_map([task_id.as_str()],|r|Ok(serde_json::json!({"kind":r.get::<_,String>(0)?,"reference":r.get::<_,String>(1)?,"evidence_id":r.get::<_,Option<String>>(2)?,"recorded_at_ms":r.get::<_,i64>(3)?})))?.collect::<std::result::Result<Vec<_>,_>>()?;
            Ok(rows)
        })?;
        Ok(
            serde_json::json!({"task_id":task.id,"status":task.status,"accepted_by_user":task.accepted_by_user,"final_commit":task.final_commit,"signals":signals}),
        )
    }

    /// Store an observed delivery signal separately from task acceptance.
    /// # Errors
    /// Returns scope/independence validation or storage errors.
    pub fn signal(&self, signal: &Signal) -> Result<()> {
        if signal.reference.trim().is_empty() || signal.reference.len() > 4096 {
            return Err(invalid("signal reference must contain 1–4096 bytes"));
        }
        self.database.write_immediate(|tx| {
            let valid=tx.query_row("SELECT EXISTS(SELECT 1 FROM evidence e JOIN tasks t ON t.id=e.task_id AND t.project_id=e.project_id WHERE e.id=?1 AND t.id=?2 AND e.strength IN ('deterministic','observational') AND (e.exit_code=0 OR e.exit_code IS NULL) AND (e.artifact_text IS NOT NULL OR e.artifact_blob_digest IS NOT NULL))",params![signal.evidence_id.as_str(),signal.task_id.as_str()],|r|r.get::<_,bool>(0))?;
            if !valid { return Err(invalid("signal needs independent evidence from the same task")); }
            let evidence_kind:String=tx.query_row("SELECT kind FROM evidence WHERE id=?1",[signal.evidence_id.as_str()],|r|r.get(0))?;
            let compatible=match signal.kind {
                SignalKind::Verification=>matches!(evidence_kind.as_str(),"test_result"|"build_result"|"lint_result"|"typecheck_result"|"benchmark_result"),
                SignalKind::UserAcceptance=>evidence_kind=="user_acceptance",
                SignalKind::CommitLanded|SignalKind::PrMerged=>matches!(evidence_kind.as_str(),"runtime_observation"|"trace"),
            };
            if !compatible {return Err(invalid("signal kind must match its evidence: verification result, merge/commit observation, or explicit user_acceptance"));}
            let kind=match signal.kind { SignalKind::Verification=>"verification",SignalKind::CommitLanded=>"commit_landed",SignalKind::PrMerged=>"pr_merged",SignalKind::UserAcceptance=>"user_acceptance" };
            tx.execute("INSERT INTO task_signals(task_id,kind,reference,evidence_id,recorded_at_ms) VALUES(?1,?2,?3,?4,?5) ON CONFLICT DO NOTHING",params![signal.task_id.as_str(),kind,self.redactor.redact_text(&signal.reference),signal.evidence_id.as_str(),now_ms()])?;
            Ok(())
        })
    }
}
fn validate_lesson(lesson: &LessonDraft) -> Result<()> {
    if lesson.claim.trim().is_empty()
        || lesson.claim.len() > 16_384
        || lesson.rationale.len() > 16_384
        || lesson.evidence_ids.len() > 32
    {
        return Err(invalid(
            "lesson requires a claim and rationale up to 16 KiB and at most 32 evidence ids",
        ));
    }
    Ok(())
}
fn normalize(claim: &str) -> String {
    claim
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}
fn similar(left: &str, right: &str) -> bool {
    let terms = |s: &str| {
        s.split(|c: char| !c.is_alphanumeric())
            .filter(|t| t.len() > 3)
            .map(str::to_lowercase)
            .collect::<BTreeSet<_>>()
    };
    let l = terms(left);
    let r = terms(right);
    let shared = l.intersection(&r).count();
    normalize(left) == normalize(right) || (shared >= 3 && shared * 3 >= l.union(&r).count() * 2)
}
