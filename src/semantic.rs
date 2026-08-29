//! Optional deterministic semantic retrieval: local embeddings and a
//! rebuildable `SQLite` vector sidecar.
//!
//! The default [`EmbeddingProvider`] is a dependency-free hashed
//! character-n-gram / token term-frequency projection into a fixed
//! [`EMBEDDING_DIMENSION`]-dimensional, L2-normalized vector. It is a
//! *lexical-semantic approximation*, not a learned model: two texts score
//! close when they share tokens and character n-grams, which is enough to
//! exercise the full semantic retrieval path deterministically, offline, and
//! with zero model downloads. Per ADR-008 the traits below are the seam where
//! a real local embedding model can be swapped in later without touching
//! canonical storage, and per Appendix E/G the whole feature stays disabled
//! by default until a measured benchmark win justifies it.
//!
//! The vector sidecar (`memory_vectors`) is **rebuildable derived state**,
//! never canonical truth. It is safe to drop at any time and rebuild from the
//! canonical `memories` table (`leb semantic rebuild <path>`); losing it
//! degrades retrieval to lexical + structural, exactly as spec §27.2 demands.

use rusqlite::{OptionalExtension, Transaction, params};

use crate::error::{Error, Result};
use crate::ids::{MemoryId, ProjectId};
use crate::storage::Database;

/// Fixed dimensionality of every stored embedding.
pub const EMBEDDING_DIMENSION: usize = 256;
const EMBEDDING_DIMENSION_U64: u64 = 256;

/// Stable identity of the built-in embedder; stored per row so a future model
/// swap can detect and rebuild incompatible vectors.
pub const EMBEDDER_ID: &str = "leb-hashed-ngram-v1-d256";

/// Stable, wall-clock-free seed for feature hashing.
const FEATURE_SEED: u64 = 0x1eb5_ed00_c0de_0001;

const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Maps batches of texts to fixed-dimension `f32` vectors (spec Appendix B).
pub trait EmbeddingProvider {
    /// Dimensionality of every vector this provider returns.
    fn dimension(&self) -> usize;

    /// Embed a batch of texts into one vector per text.
    ///
    /// # Errors
    ///
    /// Returns an error when the provider cannot produce a vector for every
    /// input text.
    fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>>;
}

/// One vector to store for a memory (spec Appendix B `VectorItem`).
#[derive(Clone, Debug, PartialEq)]
pub struct VectorItem {
    pub memory_id: MemoryId,
    pub project_id: ProjectId,
    pub vector: Vec<f32>,
}

/// One scored search result (spec Appendix B `Hit`). `score` is raw cosine
/// similarity in `[-1, 1]`; retrieval clamps it to `[0, 1]`.
#[derive(Clone, Debug, PartialEq)]
pub struct VectorHit {
    pub memory_id: MemoryId,
    pub score: f64,
}

/// Project-filtered vector store over derived, rebuildable state
/// (spec Appendix B `VectorIndex`).
pub trait VectorIndex {
    /// Insert or replace vectors for the given memories.
    ///
    /// # Errors
    ///
    /// Returns an error for a dimension mismatch or a failed canonical
    /// transaction.
    fn upsert(&self, items: &[VectorItem]) -> Result<()>;

    /// Return up to `k` hits for `query`, restricted to one project and to
    /// active validated memories, with a deterministic tie-break.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid query vector or failed storage reads.
    fn search(&self, project_id: &ProjectId, query: &[f32], k: usize) -> Result<Vec<VectorHit>>;

    /// Remove vectors for the given memories; missing rows are ignored.
    ///
    /// # Errors
    ///
    /// Returns an error for a failed canonical transaction.
    fn delete(&self, memory_ids: &[MemoryId]) -> Result<()>;

    /// Drop and regenerate derived vectors from canonical memories, for one
    /// project or for every project, returning the number of vectors written.
    ///
    /// # Errors
    ///
    /// Returns an error for a failed canonical transaction.
    fn rebuild(&self, project_id: Option<&ProjectId>) -> Result<u64>;
}

/// Bounds threaded from `[semantic]` configuration into retrieval.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SemanticOptions {
    /// Maximum semantic candidates merged into hybrid retrieval.
    pub max_candidates: usize,
    /// Hard bound on vectors scanned per brute-force search.
    pub max_scanned: usize,
}

/// Deterministic hashed token + character-trigram embedder.
///
/// Same text always produces the same vector: hashing is seeded by a stable
/// constant, never wall-clock time or process randomness.
#[derive(Clone, Copy, Debug, Default)]
pub struct HashedNGramEmbedder;

impl HashedNGramEmbedder {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    fn embed_one(text: &str) -> Vec<f32> {
        let mut vector = vec![0.0_f32; EMBEDDING_DIMENSION];
        for token in tokens(text) {
            accumulate_feature(&mut vector, "t:", &token);
            let characters = token.chars().collect::<Vec<_>>();
            for window in characters.windows(3) {
                let trigram = window.iter().collect::<String>();
                accumulate_feature(&mut vector, "g:", &trigram);
            }
        }
        l2_normalize(&mut vector);
        vector
    }
}

impl EmbeddingProvider for HashedNGramEmbedder {
    fn dimension(&self) -> usize {
        EMBEDDING_DIMENSION
    }

    fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|text| Self::embed_one(text)).collect())
    }
}

/// Brute-force cosine search over a `SQLite` sidecar table of vector BLOBs.
///
/// All writes reuse the canonical single-writer connection; searches use the
/// bounded read pool and scan at most `max_scanned` vectors per query.
#[derive(Clone, Debug)]
pub struct SqliteVectorIndex {
    database: Database,
    max_scanned: usize,
}

impl SqliteVectorIndex {
    #[must_use]
    pub const fn new(database: Database, max_scanned: usize) -> Self {
        Self {
            database,
            max_scanned,
        }
    }
}

impl VectorIndex for SqliteVectorIndex {
    fn upsert(&self, items: &[VectorItem]) -> Result<()> {
        for item in items {
            if item.vector.len() != EMBEDDING_DIMENSION {
                return Err(Error::Semantic(format!(
                    "vector for `{}` has dimension {} instead of {EMBEDDING_DIMENSION}",
                    item.memory_id,
                    item.vector.len()
                )));
            }
        }
        if items.is_empty() {
            return Ok(());
        }
        let now = unix_time_ms();
        self.database.write_immediate(|transaction| {
            for item in items {
                store_vector(
                    transaction,
                    &item.memory_id,
                    item.project_id.as_str(),
                    &item.vector,
                    now,
                )?;
            }
            Ok(())
        })
    }

    fn search(&self, project_id: &ProjectId, query: &[f32], k: usize) -> Result<Vec<VectorHit>> {
        if query.len() != EMBEDDING_DIMENSION {
            return Err(Error::Semantic(format!(
                "query vector has dimension {} instead of {EMBEDDING_DIMENSION}",
                query.len()
            )));
        }
        if k == 0 {
            return Ok(Vec::new());
        }
        let max_scanned = i64::try_from(self.max_scanned).unwrap_or(i64::MAX);
        let scanned = self.database.with_reader(|connection| {
            let mut statement = connection.prepare(
                "SELECT v.memory_id, v.vector FROM memory_vectors v
                 JOIN memories m ON m.id = v.memory_id
                 WHERE v.project_id = ?1 AND v.embedder = ?2
                   AND m.status = 'active' AND m.epistemic_state = 'validated'
                   AND NOT EXISTS (
                     SELECT 1 FROM memory_nodes mn
                     LEFT JOIN nodes n ON n.id = mn.node_id AND n.tombstoned_at_ms IS NULL
                     WHERE mn.memory_id = m.id
                       AND (n.id IS NULL OR n.current_digest <> mn.observed_digest)
                   )
                 ORDER BY v.memory_id LIMIT ?3",
            )?;
            let rows = statement.query_map(
                params![project_id.as_str(), EMBEDDER_ID, max_scanned],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)),
            )?;
            rows.collect::<std::result::Result<Vec<_>, _>>()
                .map_err(Error::from)
        })?;
        let mut hits = Vec::with_capacity(scanned.len());
        for (raw_id, blob) in scanned {
            let vector = decode_vector(&blob)?;
            hits.push(VectorHit {
                memory_id: MemoryId::parse(&raw_id)?,
                score: cosine_similarity(query, &vector),
            });
        }
        hits.sort_by(|left, right| {
            right
                .score
                .total_cmp(&left.score)
                .then_with(|| left.memory_id.cmp(&right.memory_id))
        });
        hits.truncate(k);
        Ok(hits)
    }

    fn delete(&self, memory_ids: &[MemoryId]) -> Result<()> {
        if memory_ids.is_empty() {
            return Ok(());
        }
        self.database.write_immediate(|transaction| {
            for memory_id in memory_ids {
                delete_memory_vector(transaction, memory_id)?;
            }
            Ok(())
        })
    }

    fn rebuild(&self, project_id: Option<&ProjectId>) -> Result<u64> {
        let now = unix_time_ms();
        self.database.write_immediate(|transaction| {
            transaction.execute(
                "DELETE FROM memory_vectors WHERE ?1 IS NULL OR project_id = ?1",
                [project_id.map(ProjectId::as_str)],
            )?;
            let sources = {
                let mut statement = transaction.prepare(
                    "SELECT id, project_id, claim, rationale, tags_json FROM memories
                     WHERE status = 'active' AND (?1 IS NULL OR project_id = ?1)
                     ORDER BY id",
                )?;
                let rows = statement.query_map([project_id.map(ProjectId::as_str)], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                })?;
                rows.collect::<std::result::Result<Vec<_>, _>>()?
            };
            let mut written = 0_u64;
            for (raw_id, raw_project, claim, rationale, tags_json) in sources {
                let memory_id = MemoryId::parse(&raw_id)?;
                let tags: Vec<String> = serde_json::from_str(&tags_json)?;
                let vector = memory_vector(&claim, &rationale, &tags);
                store_vector(transaction, &memory_id, &raw_project, &vector, now)?;
                written = written.saturating_add(1);
            }
            Ok(written)
        })
    }
}

/// Embed the retrievable text of one memory with the default embedder.
#[must_use]
pub fn memory_vector(claim: &str, rationale: &str, tags: &[String]) -> Vec<f32> {
    let text = format!("{claim}\n{rationale}\n{}", tags.join(" "));
    HashedNGramEmbedder::embed_one(&text)
}

/// Cosine similarity between two vectors; zero-norm inputs score 0.0.
#[must_use]
pub fn cosine_similarity(left: &[f32], right: &[f32]) -> f64 {
    let mut dot = 0.0_f64;
    let mut left_norm = 0.0_f64;
    let mut right_norm = 0.0_f64;
    for (a, b) in left.iter().zip(right.iter()) {
        dot += f64::from(*a) * f64::from(*b);
        left_norm += f64::from(*a) * f64::from(*a);
        right_norm += f64::from(*b) * f64::from(*b);
    }
    let denominator = left_norm.sqrt() * right_norm.sqrt();
    if denominator <= f64::EPSILON {
        0.0
    } else {
        dot / denominator
    }
}

/// Upsert a memory's derived vector inside an existing canonical transaction,
/// mirroring how `memories_fts` is maintained.
pub(crate) fn upsert_memory_vector(
    transaction: &Transaction<'_>,
    memory_id: &MemoryId,
    project_id: &ProjectId,
    claim: &str,
    rationale: &str,
    tags: &[String],
    now: i64,
) -> Result<()> {
    let vector = memory_vector(claim, rationale, tags);
    store_vector(transaction, memory_id, project_id.as_str(), &vector, now)
}

/// Remove a memory's derived vector inside an existing canonical transaction.
pub(crate) fn delete_memory_vector(
    transaction: &Transaction<'_>,
    memory_id: &MemoryId,
) -> Result<()> {
    transaction.execute(
        "DELETE FROM memory_vectors WHERE memory_id = ?1",
        [memory_id.as_str()],
    )?;
    Ok(())
}

/// Re-derive the vector row for a memory from its canonical text: active
/// memories keep an up-to-date vector, every other status drops it.
pub(crate) fn sync_memory_vector(
    transaction: &Transaction<'_>,
    memory_id: &MemoryId,
    active: bool,
    now: i64,
) -> Result<()> {
    if !active {
        return delete_memory_vector(transaction, memory_id);
    }
    let source = transaction
        .query_row(
            "SELECT project_id, claim, rationale, tags_json FROM memories WHERE id = ?1",
            [memory_id.as_str()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .optional()?;
    let Some((project_id, claim, rationale, tags_json)) = source else {
        return Ok(());
    };
    let tags: Vec<String> = serde_json::from_str(&tags_json)?;
    let vector = memory_vector(&claim, &rationale, &tags);
    store_vector(transaction, memory_id, &project_id, &vector, now)
}

fn store_vector(
    transaction: &Transaction<'_>,
    memory_id: &MemoryId,
    project_id: &str,
    vector: &[f32],
    now: i64,
) -> Result<()> {
    let dimension = i64::try_from(vector.len())
        .map_err(|_| Error::Semantic("vector dimension exceeds i64".to_owned()))?;
    transaction.execute(
        "INSERT INTO memory_vectors(memory_id, project_id, embedder, dimension, vector, updated_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(memory_id) DO UPDATE SET
             project_id = excluded.project_id,
             embedder = excluded.embedder,
             dimension = excluded.dimension,
             vector = excluded.vector,
             updated_at_ms = excluded.updated_at_ms",
        params![
            memory_id.as_str(),
            project_id,
            EMBEDDER_ID,
            dimension,
            encode_vector(vector),
            now
        ],
    )?;
    Ok(())
}

fn encode_vector(vector: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(vector.len() * 4);
    for value in vector {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

fn decode_vector(bytes: &[u8]) -> Result<Vec<f32>> {
    if bytes.len() % 4 != 0 {
        return Err(Error::Semantic(
            "stored vector BLOB is not a whole number of f32 values".to_owned(),
        ));
    }
    Ok(bytes
        .chunks_exact(4)
        .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect())
}

fn tokens(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|character: char| !character.is_alphanumeric() && character != '_')
        .filter(|token| !token.is_empty())
        .map(str::to_lowercase)
}

fn accumulate_feature(vector: &mut [f32], namespace: &str, feature: &str) {
    let mut hash = fnv1a(FEATURE_SEED, namespace.as_bytes());
    hash = fnv1a(hash, feature.as_bytes());
    let index = usize::try_from(hash % EMBEDDING_DIMENSION_U64).unwrap_or(0);
    let sign = if (hash >> 63) & 1 == 1 { -1.0 } else { 1.0 };
    vector[index] += sign;
}

fn fnv1a(seed: u64, bytes: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET_BASIS ^ seed;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

fn l2_normalize(vector: &mut [f32]) {
    let norm = vector
        .iter()
        .map(|value| f64::from(*value) * f64::from(*value))
        .sum::<f64>()
        .sqrt();
    if norm > f64::EPSILON {
        for value in vector.iter_mut() {
            #[allow(clippy::cast_possible_truncation)]
            {
                *value = (f64::from(*value) / norm) as f32;
            }
        }
    }
}

fn unix_time_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};

    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::experience::{EvidenceStrength, ExperienceStore, TaskOutcome};
    use crate::memory::{EpistemicState, MemoryDraft, MemoryStore};
    use crate::projects::ProjectRegistry;

    fn fixture() -> (TempDir, Database, ProjectId) {
        let temp = TempDir::new().expect("temp");
        let root = temp.path().join("project");
        std::fs::create_dir(&root).expect("project");
        let database = Database::open(temp.path().join("brain.sqlite")).expect("database");
        let project = ProjectRegistry::new(database.clone())
            .resolve_or_register(&root)
            .expect("project")
            .project
            .id;
        (temp, database, project)
    }

    fn validated_memory(database: &Database, project: &ProjectId, claim: &str) -> MemoryId {
        let experience = ExperienceStore::new(database.clone());
        let task = experience.start_task(project, claim, None).expect("task");
        let evidence = experience
            .record_evidence(
                project,
                Some(&task.id),
                None,
                "test_result",
                EvidenceStrength::Deterministic,
                None,
                Some("cargo test"),
                Some(0),
                b"pass",
                &serde_json::json!({}),
            )
            .expect("evidence");
        experience
            .finish_task(&task.id, TaskOutcome::Success, Some(true), None)
            .expect("finish task");
        let store = MemoryStore::new(database.clone());
        let memory = store
            .create_candidate(
                project,
                &MemoryDraft {
                    scope_type: "project".to_owned(),
                    scope_id: project.to_string(),
                    kind: "lesson".to_owned(),
                    claim: claim.to_owned(),
                    rationale: "Validated by regression test".to_owned(),
                    confidence: 0.5,
                    importance: 0.5,
                    tags: Vec::new(),
                    valid_from_commit: None,
                    valid_to_commit: None,
                    trust_source: "agent_inference".to_owned(),
                    evidence_ids: vec![evidence.id],
                    source_task_ids: vec![task.id],
                    node_ids: Vec::new(),
                },
            )
            .expect("candidate");
        store
            .promote(&memory.id, EpistemicState::Validated, true)
            .expect("validated");
        memory.id
    }

    #[test]
    fn embedder_is_deterministic_normalized_and_discriminative() {
        let embedder = HashedNGramEmbedder::new();
        let texts = [
            "release reservations during abort cleanup",
            "rotate log files weekly",
        ];
        let first = embedder.embed(&texts).expect("embed");
        let second = embedder.embed(&texts).expect("embed again");
        assert_eq!(first, second, "same text must map to the same vector");
        assert_ne!(first[0], first[1], "different texts must differ");
        for vector in &first {
            assert_eq!(vector.len(), EMBEDDING_DIMENSION);
            let norm = vector
                .iter()
                .map(|value| f64::from(*value) * f64::from(*value))
                .sum::<f64>()
                .sqrt();
            assert!((norm - 1.0).abs() < 1e-6, "vector must be L2-normalized");
        }
        assert!(cosine_similarity(&first[0], &first[0]) > 0.999_999);
        let related = embedder
            .embed(&["abort cleanup must release reservations"])
            .expect("related");
        assert!(
            cosine_similarity(&first[0], &related[0]) > cosine_similarity(&first[1], &related[0]),
            "shared tokens must score closer than unrelated text"
        );
        let empty = embedder.embed(&[""]).expect("empty");
        assert!(empty[0].iter().all(|value| *value == 0.0));
    }

    #[test]
    fn index_supports_upsert_search_delete_project_isolation_and_rebuild() {
        let (temp, database, project) = fixture();
        let other_root = temp.path().join("other");
        std::fs::create_dir(&other_root).expect("other root");
        let other_project = ProjectRegistry::new(database.clone())
            .resolve_or_register(&other_root)
            .expect("other project")
            .project
            .id;
        let abort_claim = "Release reservations during abort cleanup";
        let abort_id = validated_memory(&database, &project, abort_claim);
        let logging_id = validated_memory(&database, &project, "Rotate log files weekly");
        let foreign_id = validated_memory(&database, &other_project, abort_claim);

        let index = SqliteVectorIndex::new(database.clone(), 50_000);
        let query = memory_vector(abort_claim, "Validated by regression test", &[]);
        let hits = index.search(&project, &query, 10).expect("search");
        assert_eq!(hits[0].memory_id, abort_id);
        assert!(hits[0].score > 0.999, "self-similarity must be ~1.0");
        assert!(
            hits.iter().all(|hit| hit.memory_id != foreign_id),
            "search must never cross project boundaries"
        );
        let abort_score = hits[0].score;
        let logging_score = hits
            .iter()
            .find(|hit| hit.memory_id == logging_id)
            .map_or(0.0, |hit| hit.score);
        assert!(abort_score > logging_score);

        index
            .delete(std::slice::from_ref(&abort_id))
            .expect("delete");
        let after_delete = index.search(&project, &query, 10).expect("search");
        assert!(after_delete.iter().all(|hit| hit.memory_id != abort_id));

        // The sidecar is derived state: drop everything and rebuild from
        // canonical memories.
        database
            .write_immediate(|transaction| {
                transaction.execute("DELETE FROM memory_vectors", [])?;
                Ok(())
            })
            .expect("drop derived state");
        let rebuilt = index.rebuild(None).expect("rebuild");
        assert_eq!(rebuilt, 3);
        let after_rebuild = index.search(&project, &query, 10).expect("search");
        assert_eq!(after_rebuild[0].memory_id, abort_id);

        let scoped = index.rebuild(Some(&other_project)).expect("scoped rebuild");
        assert_eq!(scoped, 1);
        assert_eq!(
            index.search(&project, &query, 10).expect("search")[0].memory_id,
            abort_id,
            "scoped rebuild must not clear other projects"
        );
    }

    #[test]
    fn search_excludes_unvalidated_memories_and_rejects_bad_dimensions() {
        let (_temp, database, project) = fixture();
        let store = MemoryStore::new(database.clone());
        let candidate = store
            .create_candidate(
                &project,
                &MemoryDraft {
                    scope_type: "project".to_owned(),
                    scope_id: project.to_string(),
                    kind: "lesson".to_owned(),
                    claim: "Unvalidated abort cleanup lore".to_owned(),
                    rationale: "No supporting evidence yet".to_owned(),
                    confidence: 0.5,
                    importance: 0.5,
                    tags: Vec::new(),
                    valid_from_commit: None,
                    valid_to_commit: None,
                    trust_source: "agent_inference".to_owned(),
                    evidence_ids: Vec::new(),
                    source_task_ids: Vec::new(),
                    node_ids: Vec::new(),
                },
            )
            .expect("candidate");
        let index = SqliteVectorIndex::new(database, 50_000);
        let query = memory_vector("abort cleanup", "", &[]);
        let hits = index.search(&project, &query, 10).expect("search");
        assert!(
            hits.iter().all(|hit| hit.memory_id != candidate.id),
            "candidate memories must never be semantic candidates"
        );
        assert!(index.search(&project, &[0.0; 3], 10).is_err());
        assert!(
            index
                .upsert(&[VectorItem {
                    memory_id: candidate.id,
                    project_id: project,
                    vector: vec![0.0; 3],
                }])
                .is_err()
        );
    }
}
