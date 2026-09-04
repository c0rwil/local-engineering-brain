//! Bounded-memory newline-delimited project export.

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::blobs::BlobStore;
use crate::error::{Error, Result, io_error};
use crate::ids::ProjectId;
use crate::storage::Database;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ExportReport {
    pub project_id: ProjectId,
    pub destination: PathBuf,
    pub records: u64,
    pub bytes: u64,
}

/// Export canonical project state as versioned NDJSON without materializing it in RAM.
///
/// Evidence artifacts are exported in full: blob-backed artifacts are resolved
/// through the blob store so the NDJSON file stays self-contained.
///
/// # Errors
///
/// Returns an error if the destination exists, the project is unknown, storage
/// is corrupt, a referenced blob is missing, or the atomic output cannot be
/// completed.
pub fn export_project(
    database: &Database,
    project_id: &ProjectId,
    destination: &Path,
) -> Result<ExportReport> {
    if destination.exists() {
        return Err(Error::Config(format!(
            "export destination already exists: {}",
            destination.display()
        )));
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(|source| io_error(parent, source))?;
    }
    let temporary = destination.with_extension(format!("engram-tmp-{}", std::process::id()));
    if temporary.exists() {
        return Err(Error::Config(format!(
            "temporary export path already exists: {}",
            temporary.display()
        )));
    }
    let file = File::create(&temporary).map_err(|source| io_error(&temporary, source))?;
    restrict_file(&temporary)?;
    let mut writer = BufWriter::new(file);
    let mut records = 0_u64;
    let blobs = BlobStore::new(database.clone());
    database.with_reader(|connection| {
        let project: String = connection
            .query_row(
                "SELECT json_object(
                    'id', id, 'name', name, 'canonical_remote', canonical_remote,
                    'root_commit', root_commit, 'parent_project_id', parent_project_id,
                    'created_at_ms', created_at_ms, 'tombstoned_at_ms', tombstoned_at_ms)
                 FROM projects WHERE id = ?1",
                [project_id.as_str()],
                |row| row.get(0),
            )
            .map_err(|error| {
                if matches!(error, rusqlite::Error::QueryReturnedNoRows) {
                    Error::Config(format!("unknown project `{project_id}`"))
                } else {
                    Error::from(error)
                }
            })?;
        write_envelope(&mut writer, "project", &project)?;
        records += 1;
        for (kind, query) in export_queries() {
            let mut statement = connection.prepare(query)?;
            if *kind == EVIDENCE_KIND {
                // Evidence rows carry either inline artifact text or a blob
                // reference. Resolve the blob here: an export must never depend
                // on files outside the NDJSON it produced.
                let rows = statement.query_map(params![project_id.as_str()], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
                })?;
                for row in rows {
                    let (raw_json, blob_digest) = row?;
                    let mut data: serde_json::Value = serde_json::from_str(&raw_json)?;
                    if let Some(digest) = blob_digest {
                        data["artifact_text"] =
                            serde_json::Value::String(blobs.read_text(&digest)?);
                    }
                    write_value_envelope(&mut writer, kind, &data)?;
                    records = records.saturating_add(1);
                }
                continue;
            }
            let rows =
                statement.query_map(params![project_id.as_str()], |row| row.get::<_, String>(0))?;
            for row in rows {
                write_envelope(&mut writer, kind, &row?)?;
                records = records.saturating_add(1);
            }
        }
        Ok(())
    })?;
    writer
        .flush()
        .map_err(|source| io_error(&temporary, source))?;
    let file = writer
        .into_inner()
        .map_err(|error| io_error(&temporary, error.into_error()))?;
    file.sync_all()
        .map_err(|source| io_error(&temporary, source))?;
    let bytes = file
        .metadata()
        .map_err(|source| io_error(&temporary, source))?
        .len();
    drop(file);
    fs::rename(&temporary, destination).map_err(|source| io_error(destination, source))?;
    restrict_file(destination)?;
    Ok(ExportReport {
        project_id: project_id.clone(),
        destination: destination.to_path_buf(),
        records,
        bytes,
    })
}

fn write_envelope(writer: &mut impl Write, kind: &str, raw_json: &str) -> Result<()> {
    let data: serde_json::Value = serde_json::from_str(raw_json)?;
    write_value_envelope(writer, kind, &data)
}

fn write_value_envelope(
    writer: &mut impl Write,
    kind: &str,
    data: &serde_json::Value,
) -> Result<()> {
    serde_json::to_writer(
        &mut *writer,
        &json!({"format": "engram-project-ndjson-v1", "type": kind, "data": data}),
    )?;
    writer.write_all(b"\n").map_err(|source| Error::Io {
        path: PathBuf::from("project export"),
        source,
    })
}

/// Export kind whose artifact payload may live in the blob store.
const EVIDENCE_KIND: &str = "evidence";

fn export_queries() -> &'static [(&'static str, &'static str)] {
    &[
        (
            "commit",
            "SELECT json_object('hash',hash,'parents',json(parents_json),'authored_at_ms',authored_at_ms,'subject',subject) FROM commits WHERE project_id=?1 ORDER BY authored_at_ms,hash",
        ),
        (
            "node",
            "SELECT json_object('id',id,'kind',kind,'stable_key',stable_key,'label',label,'file_path',file_path,'language',language,'current_digest',current_digest,'source_kind',source_kind,'source_ref',source_ref,'extractor_version',extractor_version,'observed_at_ms',observed_at_ms,'tombstoned_at_ms',tombstoned_at_ms) FROM nodes WHERE project_id=?1 ORDER BY id",
        ),
        (
            "edge",
            "SELECT json_object('id',id,'source_id',source_id,'relation',relation,'target_id',target_id,'confidence',confidence,'source_kind',source_kind,'source_ref',source_ref,'content_digest',content_digest,'extractor_version',extractor_version,'observed_at_ms',observed_at_ms,'tombstoned_at_ms',tombstoned_at_ms) FROM edges WHERE project_id=?1 ORDER BY id",
        ),
        (
            "task",
            "SELECT json_object('id',id,'session_id',session_id,'prompt',prompt,'status',status,'accepted_by_user',accepted_by_user,'started_at_ms',started_at_ms,'completed_at_ms',completed_at_ms,'final_commit',final_commit) FROM tasks WHERE project_id=?1 ORDER BY started_at_ms,id",
        ),
        (
            "attempt",
            "SELECT json_object('id',a.id,'task_id',a.task_id,'ordinal',a.ordinal,'strategy_summary',a.strategy_summary,'outcome',a.outcome,'files',json(a.files_json),'created_at_ms',a.created_at_ms) FROM attempts a JOIN tasks t ON t.id=a.task_id WHERE t.project_id=?1 ORDER BY a.created_at_ms,a.id",
        ),
        (
            EVIDENCE_KIND,
            "SELECT json_object('id',id,'task_id',task_id,'attempt_id',attempt_id,'kind',kind,'strength',strength,'commit_hash',commit_hash,'command',command,'exit_code',exit_code,'artifact_digest',artifact_digest,'artifact_text',COALESCE(artifact_text,''),'metadata',json(metadata_json),'observed_at_ms',observed_at_ms), artifact_blob_digest FROM evidence WHERE project_id=?1 ORDER BY observed_at_ms,id",
        ),
        (
            "memory",
            "SELECT json_object('id',id,'scope_type',scope_type,'scope_id',scope_id,'kind',kind,'epistemic_state',epistemic_state,'status',status,'claim',claim,'rationale',rationale,'confidence',confidence,'importance',importance,'tags',json(tags_json),'token_estimate',token_estimate,'valid_from_commit',valid_from_commit,'valid_to_commit',valid_to_commit,'known_from_ms',known_from_ms,'known_to_ms',known_to_ms,'trust_source',trust_source,'retrieved_count',retrieved_count,'injected_count',injected_count,'acknowledged_count',acknowledged_count,'successful_count',successful_count,'harmful_count',harmful_count,'updated_at_ms',updated_at_ms) FROM memories WHERE project_id=?1 ORDER BY known_from_ms,id",
        ),
        (
            "event",
            "SELECT json_object('sequence',sequence,'id',id,'event_type',event_type,'schema_version',schema_version,'payload',json(payload_json),'created_at_ms',created_at_ms) FROM events WHERE project_id=?1 ORDER BY sequence",
        ),
    ]
}

#[cfg(unix)]
fn restrict_file(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|source| io_error(path, source))
}

#[cfg(not(unix))]
fn restrict_file(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::experience::{EvidenceStrength, ExperienceStore};
    use crate::projects::ProjectRegistry;

    #[test]
    fn export_is_ndjson_atomic_and_refuses_overwrite() {
        let temp = TempDir::new().expect("temp");
        let root = temp.path().join("project");
        fs::create_dir(&root).expect("project");
        let database = Database::open(temp.path().join("brain.sqlite")).expect("database");
        let project = ProjectRegistry::new(database.clone())
            .resolve_or_register(&root)
            .expect("project");
        let path = temp.path().join("export.ndjson");
        let report = export_project(&database, &project.project.id, &path).expect("export");
        assert!(report.records >= 3);
        for line in fs::read_to_string(&path).expect("read").lines() {
            let value: serde_json::Value = serde_json::from_str(line).expect("JSON line");
            assert_eq!(value["format"], "engram-project-ndjson-v1");
        }
        assert!(export_project(&database, &project.project.id, &path).is_err());
    }

    #[test]
    fn exported_evidence_carries_full_artifact_text_from_the_blob_store() {
        let temp = TempDir::new().expect("temp");
        let root = temp.path().join("project");
        fs::create_dir(&root).expect("project");
        let database = Database::open(temp.path().join("brain.sqlite")).expect("database");
        let project = ProjectRegistry::new(database.clone())
            .resolve_or_register(&root)
            .expect("project")
            .project
            .id;
        let experience = ExperienceStore::new(database.clone()).with_blob_threshold(64);
        let inline_text = "short artifact";
        let blob_text = "exported artifact line\n".repeat(64);
        for artifact in [inline_text, blob_text.as_str()] {
            experience
                .record_evidence(
                    &project,
                    None,
                    None,
                    "log",
                    EvidenceStrength::Observational,
                    None,
                    None,
                    None,
                    artifact.as_bytes(),
                    &serde_json::json!({}),
                )
                .expect("evidence");
        }

        let path = temp.path().join("export.ndjson");
        export_project(&database, &project, &path).expect("export");
        let exported: Vec<String> = fs::read_to_string(&path)
            .expect("read")
            .lines()
            .map(str::to_owned)
            .collect();
        let artifacts: Vec<String> = exported
            .iter()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("JSON line"))
            .filter(|value| value["type"] == "evidence")
            .map(|value| {
                value["data"]["artifact_text"]
                    .as_str()
                    .unwrap_or("")
                    .to_owned()
            })
            .collect();
        assert_eq!(artifacts.len(), 2);
        assert!(artifacts.contains(&inline_text.to_owned()));
        assert!(
            artifacts.contains(&blob_text),
            "export must resolve blob-backed artifacts in full"
        );
    }
}
