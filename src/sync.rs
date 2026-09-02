//! Cross-instance brain synchronization: hub-and-spoke knowledge flow with
//! an explicit trust boundary.
//!
//! DOWN (central → local): the central instance's VALIDATED memories, plus
//! curated facts and links, replicate into local brains and inject
//! immediately — they passed central's evidence gates. Replicas keep their
//! central identity (same memory id everywhere), so re-syncs update in
//! place and never duplicate.
//!
//! UP (local → central): locally validated memories are OFFERED to central,
//! where they land as CANDIDATES — local proof is a hypothesis until a
//! central curator or central evidence promotes it. Offers never echo back
//! down as duplicates because replicas are excluded from offering.
//!
//! Local decisions win locally: a memory forgotten on a laptop stays
//! forgotten there even if central still ships it. Projects match across
//! instances by their stable identity (canonical remote / project.toml),
//! which every clone already shares.

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::ids::ProjectId;
use crate::memory::MemoryStore;
use crate::projects::{ProjectFact, ProjectRegistry};
use crate::security::Redactor;
use crate::storage::Database;

/// Marker written into replicated memories' trust source.
pub const SYNCED_TRUST: &str = "synced";
/// Trust source for offers landing on central as candidates.
pub const OFFERED_TRUST: &str = "synced_hypothesis";

/// A portable memory record. Node anchors are deliberately absent: node ids
/// are instance-local (stable keys could map them later); commit-validity
/// bounds ARE portable because they reference the shared git history.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SyncMemory {
    pub id: String,
    pub kind: String,
    pub claim: String,
    pub rationale: String,
    pub confidence: f64,
    pub importance: f64,
    pub tags: Vec<String>,
    pub valid_from_commit: Option<String>,
    pub valid_to_commit: Option<String>,
    pub updated_at_ms: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SyncLink {
    pub target_project_id: ProjectId,
    pub relation: String,
    pub note: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProjectKnowledge {
    pub project_id: ProjectId,
    pub facts: Vec<ProjectFact>,
    pub links: Vec<SyncLink>,
    pub memories: Vec<SyncMemory>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct SyncKnowledge {
    pub projects: Vec<ProjectKnowledge>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct SyncApplyReport {
    pub projects_matched: u32,
    pub memories_added: u32,
    pub memories_updated: u32,
    pub memories_skipped: u32,
    pub facts_applied: u32,
    pub links_applied: u32,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct OfferReport {
    pub accepted_as_candidates: u32,
    pub deduplicated: u32,
    pub unknown_project: u32,
}

/// Export every visible project's promoted knowledge for down-sync.
///
/// # Errors
///
/// Returns storage errors.
pub fn export_knowledge(database: &Database) -> Result<SyncKnowledge> {
    let registry = ProjectRegistry::new(database.clone());
    let ids = visible_projects(database)?;
    let mut projects = Vec::new();
    for project_id in ids {
        let facts = registry
            .facts(&project_id)?
            .into_iter()
            .filter(|fact| !fact.key.starts_with("internal."))
            .collect::<Vec<_>>();
        let links = registry
            .links(&project_id)?
            .into_iter()
            .map(|link| SyncLink {
                target_project_id: link.project_id,
                relation: link.relation,
                note: link.note,
            })
            .collect::<Vec<_>>();
        let memories = validated_memories(database, &project_id)?;
        if facts.is_empty() && links.is_empty() && memories.is_empty() {
            continue;
        }
        projects.push(ProjectKnowledge {
            project_id,
            facts,
            links,
            memories,
        });
    }
    Ok(SyncKnowledge { projects })
}

/// Collect this instance's own validated memories to offer upstream.
/// Replicas (trust `synced`) and hypotheses are excluded so knowledge never
/// ping-pongs.
///
/// # Errors
///
/// Returns storage errors.
pub fn collect_offers(database: &Database) -> Result<SyncKnowledge> {
    let mut knowledge = export_knowledge(database)?;
    for project in &mut knowledge.projects {
        project.facts.clear();
        project.links.clear();
        project
            .memories
            .retain(|memory| !memory.tags.iter().any(|tag| tag == "synced"));
    }
    knowledge
        .projects
        .retain(|project| !project.memories.is_empty());
    Ok(knowledge)
}

/// Apply central knowledge locally. Only projects registered on THIS
/// instance are touched; local non-active statuses (forgotten, suspect)
/// always win over incoming replicas.
///
/// # Errors
///
/// Returns storage errors.
pub fn apply_knowledge(
    database: &Database,
    redactor: &Redactor,
    knowledge: &SyncKnowledge,
) -> Result<SyncApplyReport> {
    let registry = ProjectRegistry::new(database.clone());
    let memories = MemoryStore::with_redactor(database.clone(), redactor.clone());
    let mut report = SyncApplyReport::default();
    for project in &knowledge.projects {
        if registry.find_by_id(&project.project_id)?.is_none() {
            continue;
        }
        report.projects_matched += 1;
        for fact in &project.facts {
            if fact.key.starts_with("internal.") {
                continue;
            }
            registry.set_fact(
                &project.project_id,
                &fact.key,
                &fact.value,
                fact.note.as_deref(),
            )?;
            report.facts_applied += 1;
        }
        for link in &project.links {
            if registry.find_by_id(&link.target_project_id)?.is_some()
                && registry
                    .link(
                        &project.project_id,
                        &link.target_project_id,
                        &link.relation,
                        link.note.as_deref(),
                    )
                    .is_ok()
            {
                report.links_applied += 1;
            }
        }
        for memory in &project.memories {
            match memories.upsert_replica(&project.project_id, memory)? {
                crate::memory::ReplicaOutcome::Added => report.memories_added += 1,
                crate::memory::ReplicaOutcome::Updated => report.memories_updated += 1,
                crate::memory::ReplicaOutcome::Skipped => report.memories_skipped += 1,
            }
        }
    }
    Ok(report)
}

/// Accept up-sync offers on the central instance: each unknown validated
/// memory becomes a CANDIDATE for curation, never validated truth.
///
/// # Errors
///
/// Returns storage errors.
pub fn accept_offers(
    database: &Database,
    redactor: &Redactor,
    offers: &SyncKnowledge,
) -> Result<OfferReport> {
    let registry = ProjectRegistry::new(database.clone());
    let memories = MemoryStore::with_redactor(database.clone(), redactor.clone());
    let mut report = OfferReport::default();
    for project in &offers.projects {
        if registry.find_by_id(&project.project_id)?.is_none() {
            report.unknown_project += u32::try_from(project.memories.len()).unwrap_or(u32::MAX);
            continue;
        }
        for memory in &project.memories {
            if memories.replica_exists(&project.project_id, memory)? {
                report.deduplicated += 1;
                continue;
            }
            let mut tags = memory.tags.clone();
            tags.push("offered".to_owned());
            memories.create_candidate(
                &project.project_id,
                &crate::memory::MemoryDraft {
                    scope_type: "project".to_owned(),
                    scope_id: project.project_id.to_string(),
                    kind: memory.kind.clone(),
                    claim: memory.claim.clone(),
                    rationale: format!(
                        "Offered by a synced instance (validated there): {}",
                        memory.rationale
                    ),
                    confidence: memory.confidence.min(0.5),
                    importance: memory.importance,
                    tags,
                    valid_from_commit: memory.valid_from_commit.clone(),
                    valid_to_commit: memory.valid_to_commit.clone(),
                    trust_source: OFFERED_TRUST.to_owned(),
                    evidence_ids: Vec::new(),
                    source_task_ids: Vec::new(),
                    node_ids: Vec::new(),
                },
            )?;
            report.accepted_as_candidates += 1;
        }
    }
    Ok(report)
}

/// One full sync cycle against a central instance over HTTP.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct SyncCycleReport {
    pub pulled: Option<SyncApplyReport>,
    pub offered: Option<OfferReport>,
}

/// Pull central knowledge into the local daemon and/or offer local
/// validated memories upstream. HTTP errors are returned, not retried.
///
/// # Errors
///
/// Returns transport or storage errors.
pub async fn run_sync_cycle(
    local: &crate::daemon::DaemonClient,
    central_url: &str,
    token: Option<&str>,
    pull: bool,
    push: bool,
) -> Result<SyncCycleReport> {
    use crate::error::Error;
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .map_err(|error| Error::Config(format!("sync http client: {error}")))?;
    let base = central_url.trim_end_matches('/');
    let authorize = |request: reqwest::RequestBuilder| match token {
        Some(token) => request.bearer_auth(token),
        None => request,
    };
    let mut report = SyncCycleReport::default();
    if pull {
        let response = authorize(http.get(format!("{base}/api/sync/knowledge")))
            .send()
            .await
            .map_err(|error| Error::Config(format!("central pull: {error}")))?;
        if !response.status().is_success() {
            return Err(Error::Config(format!(
                "central pull returned {}",
                response.status()
            )));
        }
        let knowledge: SyncKnowledge = response
            .json()
            .await
            .map_err(|error| Error::Config(format!("central pull body: {error}")))?;
        report.pulled = Some(local.sync_apply(&knowledge).await?);
    }
    if push {
        let offers = local.sync_offers().await?;
        if !offers.projects.is_empty() {
            let response = authorize(http.post(format!("{base}/api/sync/offer")))
                .json(&offers)
                .send()
                .await
                .map_err(|error| Error::Config(format!("central offer: {error}")))?;
            if !response.status().is_success() {
                return Err(Error::Config(format!(
                    "central offer returned {}",
                    response.status()
                )));
            }
            report.offered = Some(
                response
                    .json()
                    .await
                    .map_err(|error| Error::Config(format!("central offer body: {error}")))?,
            );
        }
    }
    Ok(report)
}

fn visible_projects(database: &Database) -> Result<Vec<ProjectId>> {
    database.with_reader(|connection| {
        let mut statement = connection.prepare(
            "SELECT id FROM projects p WHERE p.tombstoned_at_ms IS NULL
               AND NOT EXISTS (SELECT 1 FROM project_facts pf WHERE pf.project_id = p.id
                       AND (pf.key = 'internal.view_of'
                            OR (pf.key = 'internal.hidden' AND pf.value = 'true')))
             ORDER BY p.name, p.id",
        )?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows.into_iter().map(|id| ProjectId::parse(&id)).collect()
    })
}

fn validated_memories(database: &Database, project_id: &ProjectId) -> Result<Vec<SyncMemory>> {
    database.with_reader(|connection| {
        let mut statement = connection.prepare(
            "SELECT id, kind, claim, rationale, confidence, importance, tags_json,
                    valid_from_commit, valid_to_commit, updated_at_ms
             FROM memories
             WHERE project_id = ?1 AND status = 'active'
               AND epistemic_state = 'validated'
             ORDER BY known_from_ms, id LIMIT 500",
        )?;
        let rows = statement
            .query_map([project_id.as_str()], |row| {
                Ok(SyncMemory {
                    id: row.get(0)?,
                    kind: row.get(1)?,
                    claim: row.get(2)?,
                    rationale: row.get(3)?,
                    confidence: row.get(4)?,
                    importance: row.get(5)?,
                    tags: serde_json::from_str(&row.get::<_, String>(6)?).unwrap_or_default(),
                    valid_from_commit: row.get(7)?,
                    valid_to_commit: row.get(8)?,
                    updated_at_ms: row.get(9)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    })
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::memory::{EpistemicState, MemoryDraft};

    /// Register the SAME project identity on two instances via pinned
    /// metadata — exactly how real clones share identity.
    fn register_pinned(database: &Database, root: &std::path::Path, id: &str) -> ProjectId {
        std::fs::create_dir_all(root.join(".leb")).expect("dirs");
        std::fs::write(
            root.join(".leb/project.toml"),
            format!("project_id = \"{id}\"\nname = \"svc\"\n"),
        )
        .expect("metadata");
        ProjectRegistry::new(database.clone())
            .resolve_or_register(root)
            .expect("register")
            .project
            .id
    }

    fn validated(database: &Database, project: &ProjectId, claim: &str) -> String {
        let store = MemoryStore::new(database.clone());
        let memory = store
            .create_candidate(
                project,
                &MemoryDraft {
                    scope_type: "project".to_owned(),
                    scope_id: project.to_string(),
                    kind: "lesson".to_owned(),
                    claim: claim.to_owned(),
                    rationale: "central evidence".to_owned(),
                    confidence: 0.5,
                    importance: 0.8,
                    tags: vec!["team".to_owned()],
                    valid_from_commit: None,
                    valid_to_commit: None,
                    trust_source: "human_instruction".to_owned(),
                    evidence_ids: Vec::new(),
                    source_task_ids: Vec::new(),
                    node_ids: Vec::new(),
                },
            )
            .expect("memory");
        store
            .promote(&memory.id, EpistemicState::Validated, true)
            .expect("promote");
        memory.id.to_string()
    }

    #[test]
    fn down_sync_replicates_updates_and_respects_local_forgets() {
        let temp = TempDir::new().expect("temp");
        let central = Database::open(temp.path().join("central.sqlite")).expect("central");
        let local = Database::open(temp.path().join("local.sqlite")).expect("local");
        let pinned = "prj_11111111-2222-4333-8444-555555555555";
        let central_project = register_pinned(&central, &temp.path().join("central-co"), pinned);
        let local_project = register_pinned(&local, &temp.path().join("local-co"), pinned);
        assert_eq!(central_project, local_project, "identity must match");
        ProjectRegistry::new(central.clone())
            .set_fact(
                &central_project,
                "hosting.base_url",
                "api.example.com",
                None,
            )
            .expect("fact");
        let memory_id = validated(&central, &central_project, "Deploys are blue-green only");

        let knowledge = export_knowledge(&central).expect("export");
        let redactor = Redactor::new(&[]).expect("redactor");
        let report = apply_knowledge(&local, &redactor, &knowledge).expect("apply");
        assert_eq!(report.projects_matched, 1);
        assert_eq!(report.memories_added, 1);
        assert_eq!(report.facts_applied, 1);

        // The replica is validated, keeps its id, and is tagged synced.
        let store = MemoryStore::new(local.clone());
        let replica = store
            .get(&crate::ids::MemoryId::parse(&memory_id).expect("id"))
            .expect("lookup")
            .expect("replica");
        assert_eq!(replica.epistemic_state, EpistemicState::Validated);
        assert!(replica.tags.contains(&"synced".to_owned()));

        // Idempotent re-apply.
        let again = apply_knowledge(&local, &redactor, &knowledge).expect("re-apply");
        assert_eq!(again.memories_added, 0);
        assert_eq!(again.memories_updated, 0);

        // Central edits flow down as updates...
        std::thread::sleep(std::time::Duration::from_millis(5));
        MemoryStore::new(central.clone())
            .mark_status(
                &crate::ids::MemoryId::parse(&memory_id).expect("id"),
                crate::memory::MemoryStatus::Archived,
                "user_forget",
            )
            .expect("central change");
        // (forget on central removes it from export entirely)
        let after_forget = export_knowledge(&central).expect("export2");
        assert!(
            after_forget
                .projects
                .iter()
                .all(|project| project.memories.is_empty()),
            "forgotten memories must not export"
        );

        // ...and a LOCAL forget survives future pulls of the same memory.
        let knowledge_with_memory = knowledge;
        MemoryStore::new(local.clone())
            .mark_status(
                &crate::ids::MemoryId::parse(&memory_id).expect("id"),
                crate::memory::MemoryStatus::Archived,
                "user_forget",
            )
            .expect("local forget");
        let post_forget =
            apply_knowledge(&local, &redactor, &knowledge_with_memory).expect("apply3");
        assert_eq!(post_forget.memories_added, 0);
        assert_eq!(post_forget.memories_updated, 0, "local forget wins");
    }

    #[test]
    fn up_sync_offers_become_candidates_and_never_echo() {
        let temp = TempDir::new().expect("temp");
        let central = Database::open(temp.path().join("central.sqlite")).expect("central");
        let local = Database::open(temp.path().join("local.sqlite")).expect("local");
        let pinned = "prj_aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
        let central_project = register_pinned(&central, &temp.path().join("central-co"), pinned);
        let local_project = register_pinned(&local, &temp.path().join("local-co"), pinned);
        validated(&local, &local_project, "Retry queues drain before deploys");

        let offers = collect_offers(&local).expect("offers");
        assert_eq!(offers.projects.len(), 1);
        let redactor = Redactor::new(&[]).expect("redactor");
        let report = accept_offers(&central, &redactor, &offers).expect("accept");
        assert_eq!(report.accepted_as_candidates, 1);

        // Central holds it as a CANDIDATE hypothesis, not validated truth.
        let pending = central
            .with_reader(|connection| {
                connection
                    .query_row(
                        "SELECT epistemic_state || ':' || trust_source FROM memories
                         WHERE project_id = ?1",
                        [central_project.as_str()],
                        |row| row.get::<_, String>(0),
                    )
                    .map_err(crate::error::Error::from)
            })
            .expect("central row");
        assert_eq!(pending, "candidate:synced_hypothesis");

        // Re-offering dedupes; and replicas pulled from central never offer.
        let again = accept_offers(&central, &redactor, &offers).expect("re-accept");
        assert_eq!(again.deduplicated, 1);
        let downstream = export_knowledge(&central).expect("central export");
        let pulled = apply_knowledge(&local, &redactor, &downstream).expect("pull");
        assert_eq!(pulled.memories_added, 0, "candidates never flow down");
        let offers_after = collect_offers(&local).expect("offers2");
        let echoes = offers_after
            .projects
            .iter()
            .flat_map(|project| &project.memories)
            .filter(|memory| memory.tags.contains(&"synced".to_owned()))
            .count();
        assert_eq!(echoes, 0, "replicas must not ping-pong");
    }
}
