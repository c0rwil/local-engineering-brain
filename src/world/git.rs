//! Bounded, deterministic Git commit-history ingestion.

use std::path::Path;
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::events::{DomainEvent, append_generated_in_transaction};
use crate::ids::ProjectId;
use crate::projects::ResolvedProject;
use crate::storage::Database;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GitCommit {
    pub hash: String,
    pub parents: Vec<String>,
    pub authored_at_ms: Option<i64>,
    pub subject: Option<String>,
}

#[derive(Clone, Debug)]
pub struct PreparedGitHistory {
    project_id: ProjectId,
    head: Option<String>,
    commits: Vec<GitCommit>,
    truncated: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GitIngestReport {
    pub head: Option<String>,
    pub commits_seen: u64,
    pub commits_upserted: u64,
    pub truncated: bool,
}

#[derive(Clone, Debug)]
pub struct GitHistory {
    database: Database,
}

impl GitHistory {
    #[must_use]
    pub const fn new(database: Database) -> Self {
        Self { database }
    }

    /// Read a bounded history snapshot outside the canonical writer.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid bounds, a non-Git project, Git failures, or
    /// malformed command output.
    pub fn prepare(
        &self,
        project: &ResolvedProject,
        max_commits: usize,
    ) -> Result<PreparedGitHistory> {
        if max_commits == 0 || max_commits > 100_000 {
            return Err(Error::Config(
                "Git history limit must be between 1 and 100000".to_owned(),
            ));
        }
        if project.git.is_none() {
            return Err(Error::Git("project is not a Git repository".to_owned()));
        }
        let requested = max_commits.saturating_add(1);
        let output = run_git(
            &project.root,
            &[
                "log",
                "--all",
                "--topo-order",
                &format!("--max-count={requested}"),
                "--format=%H%x00%P%x00%at%x00%s%x00",
            ],
        )?;
        let mut commits = parse_log(&output.stdout)?;
        let truncated = commits.len() > max_commits;
        commits.truncate(max_commits);
        Ok(PreparedGitHistory {
            project_id: project.project.id.clone(),
            head: project.git.as_ref().and_then(|git| git.head.clone()),
            commits,
            truncated,
        })
    }

    /// Persist a prepared history and its immutable ingestion event atomically.
    ///
    /// # Errors
    ///
    /// Returns an error for project-scope mismatch or canonical storage failure.
    pub fn apply_prepared(
        &self,
        project: &ResolvedProject,
        prepared: PreparedGitHistory,
    ) -> Result<GitIngestReport> {
        if prepared.project_id != project.project.id {
            return Err(Error::Git(
                "prepared Git history belongs to another project".to_owned(),
            ));
        }
        let now = unix_time_ms();
        let event = DomainEvent::GitHistoryIngested {
            project_id: project.project.id.clone(),
            head: prepared.head.clone(),
            commit_count: u64::try_from(prepared.commits.len()).unwrap_or(u64::MAX),
            truncated: prepared.truncated,
        };
        self.database.write_immediate(|transaction| {
            for commit in &prepared.commits {
                let parents = serde_json::to_string(&commit.parents)?;
                transaction.execute(
                    "INSERT INTO commits
                     (project_id, hash, parents_json, authored_at_ms, subject)
                     VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(project_id, hash) DO UPDATE SET
                        parents_json = excluded.parents_json,
                        authored_at_ms = excluded.authored_at_ms,
                        subject = excluded.subject",
                    params![
                        project.project.id.as_str(),
                        commit.hash,
                        parents,
                        commit.authored_at_ms,
                        commit.subject
                    ],
                )?;
            }
            append_generated_in_transaction(transaction, &event, now)
        })?;
        Ok(GitIngestReport {
            head: prepared.head,
            commits_seen: u64::try_from(prepared.commits.len()).unwrap_or(u64::MAX),
            commits_upserted: u64::try_from(prepared.commits.len()).unwrap_or(u64::MAX),
            truncated: prepared.truncated,
        })
    }

    /// Fetch recent canonical commit facts in deterministic authored order.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid limit or corrupt/storage state.
    pub fn recent(&self, project_id: &ProjectId, limit: usize) -> Result<Vec<GitCommit>> {
        if limit == 0 || limit > 10_000 {
            return Err(Error::Config(
                "recent Git commit limit must be between 1 and 10000".to_owned(),
            ));
        }
        self.database.with_reader(|connection| {
            let mut statement = connection.prepare(
                "SELECT hash, parents_json, authored_at_ms, subject
                 FROM commits WHERE project_id = ?1
                 ORDER BY authored_at_ms DESC, hash LIMIT ?2",
            )?;
            let rows = statement.query_map(
                params![
                    project_id.as_str(),
                    i64::try_from(limit).unwrap_or(i64::MAX)
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                },
            )?;
            rows.map(|row| {
                let (hash, parents, authored_at_ms, subject) = row?;
                Ok(GitCommit {
                    hash,
                    parents: serde_json::from_str(&parents)?,
                    authored_at_ms,
                    subject,
                })
            })
            .collect()
        })
    }
}

fn run_git(root: &Path, arguments: &[&str]) -> Result<Output> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(arguments)
        .output()
        .map_err(|error| Error::Git(format!("unable to execute git: {error}")))?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(Error::Git(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ))
    }
}

fn parse_log(bytes: &[u8]) -> Result<Vec<GitCommit>> {
    let mut fields = bytes.split(|byte| *byte == 0);
    let mut commits = Vec::new();
    while let Some(hash) = fields.next() {
        let hash = String::from_utf8_lossy(hash).trim().to_owned();
        if hash.is_empty() {
            continue;
        }
        let parents = next_utf8(&mut fields, "parents")?;
        let authored = next_utf8(&mut fields, "authored timestamp")?;
        let subject = next_utf8(&mut fields, "subject")?;
        validate_hash(&hash)?;
        let parents = parents
            .split_whitespace()
            .map(|parent| {
                validate_hash(parent)?;
                Ok(parent.to_owned())
            })
            .collect::<Result<Vec<_>>>()?;
        let authored_at_ms = authored
            .trim()
            .parse::<i64>()
            .ok()
            .and_then(|seconds| seconds.checked_mul(1_000));
        commits.push(GitCommit {
            hash,
            parents,
            authored_at_ms,
            subject: (!subject.is_empty()).then(|| truncate_utf8(&subject, 4_096)),
        });
    }
    Ok(commits)
}

fn next_utf8<'a>(fields: &mut impl Iterator<Item = &'a [u8]>, name: &str) -> Result<String> {
    fields
        .next()
        .map(|field| String::from_utf8_lossy(field).to_string())
        .ok_or_else(|| Error::Git(format!("malformed git log: missing {name}")))
}

fn validate_hash(hash: &str) -> Result<()> {
    if !(7..=128).contains(&hash.len()) || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(Error::Git(format!(
            "invalid commit hash in git log: {hash}"
        )));
    }
    Ok(())
}

fn truncate_utf8(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut boundary = max_bytes;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value[..boundary].to_owned()
}

fn unix_time_ms() -> i64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis());
    i64::try_from(millis).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;
    use crate::projects::ProjectRegistry;

    fn git(root: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(root)
            .args([
                "-c",
                "user.name=Engram Test",
                "-c",
                "user.email=engram@example.invalid",
            ])
            .args(args)
            .status()
            .expect("git command");
        assert!(status.success());
    }

    #[test]
    fn ingests_parent_relationships_with_a_hard_limit() {
        let temp = TempDir::new().expect("temp");
        let root = temp.path().join("repo");
        fs::create_dir(&root).expect("repo");
        git(&root, &["init", "-q"]);
        fs::write(root.join("a.txt"), "one").expect("file");
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "first"]);
        fs::write(root.join("a.txt"), "two").expect("file");
        git(&root, &["commit", "-qam", "second"]);

        let database = Database::open(temp.path().join("brain.sqlite")).expect("database");
        let project = ProjectRegistry::new(database.clone())
            .resolve_or_register(&root)
            .expect("project");
        let history = GitHistory::new(database);
        let prepared = history.prepare(&project, 1).expect("prepare");
        assert!(prepared.truncated);
        let report = history.apply_prepared(&project, prepared).expect("apply");
        assert_eq!(report.commits_upserted, 1);
        let recent = history.recent(&project.project.id, 10).expect("recent");
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].subject.as_deref(), Some("second"));
        assert_eq!(recent[0].parents.len(), 1);
    }
}
