//! Live verification of the files a source episode touched.
//!
//! Agents record touched paths relative to the checkout they worked in, as
//! absolute paths, or occasionally as annotated text. A path is read only
//! inside a registered checkout of the review's project or of an explicitly
//! linked repository; everything else blocks automatic review. The task's
//! checkout is identified from absolute paths, then from the recorded task
//! revision, then from where the recorded files exist (primary checkout
//! first). Reads are bounded and never follow a symlink out of a checkout.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::{ErrorKind, Read as _};
use std::path::{Component, Path, PathBuf};

use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

use crate::error::Result;
use crate::ids::{ProjectId, TaskId};
use crate::storage::Database;

/// Touched files verified per episode.
const MAX_TOUCHED_FILES: usize = 64;
/// Registered checkout aliases considered for the project and for linked projects.
const MAX_ALIASES: i64 = 128;
/// Live bytes read per packet and per file.
const LIVE_READ_BUDGET: usize = 4 * 1024 * 1024;
const LIVE_FILE_LIMIT: usize = 512 * 1024;
/// Git metadata read while matching a checkout to a recorded revision.
const GIT_METADATA_LIMIT: u64 = 4 * 1024;
const PACKED_REFS_LIMIT: u64 = 8 * 1024 * 1024;

pub(super) const TOO_MANY_FILES: &str =
    "Source episode exceeds the 64-file live verification limit";
pub(super) const CHECKOUT_UNAVAILABLE: &str =
    "Source checkout unavailable; cannot establish current file contents";
pub(super) const OUTSIDE_CHECKOUTS: &str =
    "Touched path is outside every registered checkout of this project and its linked repositories";
pub(super) const INVALID_PATH: &str = "Touched path escapes its checkout or is not a file path";
pub(super) const RESOLVES_OUTSIDE: &str = "Touched source resolves outside the registered checkout";
pub(super) const TASK_REVISION_DIFFERS: &str =
    "Source task revision differs from the live checkout; verify current behavior again";
pub(super) const PERMISSION_DENIED: &str = "Touched source file is not readable by the daemon (permission denied); grant read access to the checkout";
pub(super) const READ_FAILED: &str =
    "Touched source file could not be read (I/O error); check the checkout and its storage";
pub(super) const NOT_REGULAR_FILE: &str = "Touched source is not a regular file";
pub(super) const OVER_BUDGET: &str = "Touched source exceeds bounded live verification budget";
pub(super) const MISSING_FILE: &str = "Touched source file is missing from the checkout (deleted, renamed or not a file path); verify current behavior again";
pub(super) const DIFFERS_FROM_INDEX: &str =
    "Touched file differs from indexed source; index and verify it again";
pub(super) const NOT_INDEXED: &str =
    "Touched file has no indexed anchor; index the repository and verify it again";

/// Reporting order: containment, checkout state, reads, then index agreement.
/// A Git HEAD failure has no fixed text and ranks with the revision check.
const ISSUE_ORDER: [&str; 13] = [
    TOO_MANY_FILES,
    OUTSIDE_CHECKOUTS,
    INVALID_PATH,
    RESOLVES_OUTSIDE,
    CHECKOUT_UNAVAILABLE,
    TASK_REVISION_DIFFERS,
    PERMISSION_DENIED,
    READ_FAILED,
    NOT_REGULAR_FILE,
    OVER_BUDGET,
    MISSING_FILE,
    DIFFERS_FROM_INDEX,
    NOT_INDEXED,
];
const HEAD_ISSUE_RANK: usize = 5;

/// Verified live state for one episode.
pub(super) struct LiveSources {
    /// Indexed anchors of touched files: path, content digest and index time.
    pub indexed: Vec<Value>,
    /// Live checkout state in a stable order, without index times or scan IDs.
    pub live: Value,
    /// The highest-priority reason this packet cannot be reviewed automatically.
    pub issue: Option<String>,
}

struct Checkout {
    root: PathBuf,
    project_id: ProjectId,
    linked: bool,
    primary: bool,
}

enum Location {
    /// Inside a known checkout at this normalized relative path.
    Checkout(usize, String),
    /// A repository-relative path, resolved against the task checkout.
    Relative(String),
    Outside,
    Invalid,
}

struct Recorded {
    touched: Vec<String>,
    own: Vec<String>,
    linked: Vec<(String, String)>,
    revisions: Vec<String>,
}

type Target = std::result::Result<(usize, String), &'static str>;
type IndexAnchor = Option<(String, i64)>;

/// Resolve, read and compare the episode's touched files with the index.
/// # Errors
/// Returns canonical-storage errors; filesystem problems become issues.
pub(super) fn live_sources(
    database: &Database,
    project_id: &ProjectId,
    task_id: &TaskId,
    final_commit: Option<&str>,
) -> Result<LiveSources> {
    let recorded = load(database, project_id, task_id)?;
    if recorded.touched.len() > MAX_TOUCHED_FILES {
        return Ok(LiveSources {
            indexed: Vec::new(),
            live: json!({"too_many_files": true}),
            issue: Some(TOO_MANY_FILES.to_owned()),
        });
    }
    let checkouts = checkouts(project_id, &recorded);
    let locations: Vec<Location> = recorded
        .touched
        .iter()
        .map(|path| locate(path, &checkouts))
        .collect();
    let mut revisions = recorded.revisions;
    revisions.extend(final_commit.map(str::to_owned));
    let task_checkout = task_checkout(&checkouts, &locations, &revisions);

    let mut issues = Issues::default();
    let mut head = None;
    if let Some(index) = task_checkout {
        match live_head(&checkouts[index].root) {
            Ok(current) => head = current,
            Err(error) => issues.add(&error),
        }
        if head
            .as_deref()
            .zip(final_commit)
            .is_some_and(|(current, recorded)| !same_revision(recorded, current))
        {
            issues.add(TASK_REVISION_DIFFERS);
        }
    }
    let targets: Vec<Target> = locations
        .into_iter()
        .map(|location| match location {
            Location::Checkout(index, relative) => Ok((index, relative)),
            Location::Relative(relative) => task_checkout
                .map(|index| (index, relative))
                .ok_or(CHECKOUT_UNAVAILABLE),
            Location::Outside => Err(OUTSIDE_CHECKOUTS),
            Location::Invalid => Err(INVALID_PATH),
        })
        .collect();
    let anchors = index_lookups(database, &checkouts, &targets)?;
    let files = verify_files(
        &checkouts,
        task_checkout,
        &recorded.touched,
        &targets,
        anchors,
        &mut issues,
    );
    let checkout = task_checkout.map_or_else(
        || json!("unavailable"),
        |index| json!(checkouts[index].root.to_string_lossy()),
    );
    Ok(LiveSources {
        indexed: files.indexed,
        live: json!({"checkout": checkout, "head": head, "files": files.live}),
        issue: issues.first(),
    })
}

#[derive(Default)]
struct VerifiedFiles {
    indexed: Vec<Value>,
    live: Vec<Value>,
}

/// Read each resolved file once and compare it with its project's index.
fn verify_files(
    checkouts: &[Checkout],
    task_checkout: Option<usize>,
    touched: &[String],
    targets: &[Target],
    anchors: Vec<IndexAnchor>,
    issues: &mut Issues,
) -> VerifiedFiles {
    let mut files = VerifiedFiles::default();
    let mut remaining = LIVE_READ_BUDGET;
    for ((recorded_path, target), anchor) in touched.iter().zip(targets).zip(anchors) {
        let (index, relative) = match target {
            Ok(target) => target,
            Err(error) => {
                issues.add(error);
                files
                    .live
                    .push(json!({"path": recorded_path, "error": error}));
                continue;
            }
        };
        let checkout = &checkouts[*index];
        let observed = read_live(&checkout.root, relative, &mut remaining);
        let indexed_digest = anchor.as_ref().map(|(digest, _)| digest);
        match &observed {
            Ok(digest) if indexed_digest == Some(digest) => {}
            Ok(_) if indexed_digest.is_some() => issues.add(DIFFERS_FROM_INDEX),
            Ok(_) => issues.add(NOT_INDEXED),
            Err(error) => issues.add(error),
        }
        let other_checkout =
            (Some(*index) != task_checkout).then(|| checkout.root.to_string_lossy().into_owned());
        let linked = checkout.linked.then(|| checkout.project_id.to_string());
        // The scan revision of an index row is omitted: unchanged files keep the
        // scan that last rewrote them, so it differs from HEAD without meaning.
        if let Some((digest, indexed_at_ms)) = &anchor {
            let mut indexed =
                json!({"path": relative, "content_digest": digest, "indexed_at_ms": indexed_at_ms});
            if let Some(project) = &linked {
                indexed["project_id"] = json!(project);
            }
            files.indexed.push(indexed);
        }
        let error = observed.as_ref().err().copied();
        let mut stamp = json!({"path": relative, "indexed_digest": indexed_digest,
            "live_digest": observed.as_ref().ok(), "error": error});
        if let Some(root) = &other_checkout {
            stamp["checkout"] = json!(root);
        }
        if let Some(project) = &linked {
            stamp["project_id"] = json!(project);
        }
        files.live.push(stamp);
    }
    for list in [&mut files.indexed, &mut files.live] {
        list.sort_by_cached_key(|value| {
            ["path", "checkout", "project_id"].map(|key| value[key].as_str().map(str::to_owned))
        });
    }
    files
}

/// Whether a recorded revision names the live commit. Agents often record
/// abbreviated hashes; at least seven hexadecimal digits must match.
pub(super) fn same_revision(recorded: &str, live: &str) -> bool {
    let recorded = recorded.trim();
    recorded == live
        || (recorded.len() >= 7
            && recorded.bytes().all(|byte| byte.is_ascii_hexdigit())
            && live
                .get(..recorded.len())
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case(recorded)))
}

/// Read the live Git HEAD, or `None` outside Git.
/// # Errors
/// Returns a reviewer-facing reason when Git exists but HEAD cannot be read.
pub(super) fn live_head(root: &Path) -> std::result::Result<Option<String>, String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--verify", "HEAD"])
        .output()
        .map_err(|error| format!("Cannot read live Git HEAD: {error}"))?;
    if !output.status.success() {
        if root.join(".git").exists() {
            return Err("Cannot resolve live Git HEAD".to_owned());
        }
        return Ok(None);
    }
    let head = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if !(head.len() == 40 || head.len() == 64) || !head.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("Live Git HEAD is not a commit hash".to_owned());
    }
    Ok(Some(head))
}

#[derive(Default)]
struct Issues(Option<(usize, String)>);

impl Issues {
    fn add(&mut self, issue: &str) {
        let rank = ISSUE_ORDER
            .iter()
            .position(|known| *known == issue)
            .unwrap_or(HEAD_ISSUE_RANK);
        if self.0.as_ref().is_none_or(|(current, _)| rank < *current) {
            self.0 = Some((rank, issue.to_owned()));
        }
    }

    fn first(self) -> Option<String> {
        self.0.map(|(_, issue)| issue)
    }
}

fn load(database: &Database, project_id: &ProjectId, task_id: &TaskId) -> Result<Recorded> {
    database.with_reader(|connection| {
        let mut statement = connection.prepare(
            "SELECT DISTINCT touched.value FROM attempts a JOIN json_each(a.files_json) touched
             WHERE a.task_id=?1 AND touched.type='text' ORDER BY touched.value LIMIT ?2",
        )?;
        let limit = i64::try_from(MAX_TOUCHED_FILES + 1).unwrap_or(i64::MAX);
        let touched = statement
            .query_map(params![task_id.as_str(), limit], |row| row.get(0))?
            .collect::<std::result::Result<Vec<String>, _>>()?;
        let mut statement = connection.prepare(
            "SELECT path FROM project_aliases WHERE project_id=?1
             ORDER BY observed_at_ms DESC,path LIMIT ?2",
        )?;
        let own = statement
            .query_map(params![project_id.as_str(), MAX_ALIASES], |row| row.get(0))?
            .collect::<std::result::Result<Vec<String>, _>>()?;
        // Links are explicit operator declarations; either direction qualifies.
        let mut statement = connection.prepare(
            "SELECT a.project_id,a.path FROM project_aliases a
             JOIN projects p ON p.id=a.project_id AND p.tombstoned_at_ms IS NULL
             WHERE a.project_id IN (
                 SELECT linked_project_id FROM project_links WHERE project_id=?1
                 UNION SELECT project_id FROM project_links WHERE linked_project_id=?1)
             ORDER BY a.observed_at_ms DESC,a.path LIMIT ?2",
        )?;
        let linked = statement
            .query_map(params![project_id.as_str(), MAX_ALIASES], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?
            .collect::<std::result::Result<Vec<(String, String)>, _>>()?;
        let mut statement = connection.prepare(
            "SELECT DISTINCT commit_hash FROM evidence
             WHERE task_id=?1 AND commit_hash IS NOT NULL ORDER BY commit_hash LIMIT 32",
        )?;
        let revisions = statement
            .query_map([task_id.as_str()], |row| row.get(0))?
            .collect::<std::result::Result<Vec<String>, _>>()?;
        Ok(Recorded {
            touched,
            own,
            linked,
            revisions,
        })
    })
}

/// Existing checkouts in preference order: this project's most recently
/// observed aliases, then explicitly linked repositories. Roots are canonical.
fn checkouts(project_id: &ProjectId, recorded: &Recorded) -> Vec<Checkout> {
    let own = recorded
        .own
        .iter()
        .map(|path| (Some(project_id.clone()), path, false));
    let linked = recorded
        .linked
        .iter()
        .map(|(project, path)| (ProjectId::parse(project).ok(), path, true));
    let mut checkouts: Vec<Checkout> = Vec::new();
    for (project, path, linked) in own.chain(linked) {
        let (Some(project_id), Ok(root)) = (project, Path::new(path).canonicalize()) else {
            continue;
        };
        if !root.is_dir() || checkouts.iter().any(|checkout| checkout.root == root) {
            continue;
        }
        let primary = root.join(".git").is_dir();
        checkouts.push(Checkout {
            root,
            project_id,
            linked,
            primary,
        });
    }
    checkouts
}

fn locate(recorded: &str, checkouts: &[Checkout]) -> Location {
    let path = Path::new(recorded);
    if !path.is_absolute() {
        return normalize_relative(path).map_or(Location::Invalid, Location::Relative);
    }
    let resolved = canonicalize_existing_prefix(path);
    let containing = checkouts
        .iter()
        .enumerate()
        .filter(|(_, checkout)| resolved.starts_with(&checkout.root))
        .max_by_key(|(_, checkout)| checkout.root.components().count());
    let Some((index, checkout)) = containing else {
        return Location::Outside;
    };
    resolved
        .strip_prefix(&checkout.root)
        .ok()
        .and_then(normalize_relative)
        .map_or(Location::Invalid, |relative| {
            Location::Checkout(index, relative)
        })
}

/// Canonicalize the longest existing ancestor and keep the missing tail, so
/// deleted files and `/tmp` versus `/private/tmp` spellings compare correctly.
fn canonicalize_existing_prefix(path: &Path) -> PathBuf {
    let mut tail: Vec<&OsStr> = Vec::new();
    let mut ancestor = path;
    loop {
        if let Ok(mut resolved) = ancestor.canonicalize() {
            for part in tail.iter().rev() {
                resolved.push(part);
            }
            return resolved;
        }
        match (ancestor.parent(), ancestor.file_name()) {
            (Some(parent), Some(name)) => {
                tail.push(name);
                ancestor = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// Lexically normalize a repository-relative path; `None` when it is empty,
/// absolute, escapes its root or is not UTF-8.
fn normalize_relative(path: &Path) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_str()?),
            Component::CurDir => {}
            Component::ParentDir => {
                parts.pop()?;
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// Choose the checkout the task worked in. Absolute paths name it directly.
/// Otherwise prefer checkouts at the recorded task revision, then the one
/// holding most recorded files, then a primary checkout, then recency.
fn task_checkout(
    checkouts: &[Checkout],
    locations: &[Location],
    revisions: &[String],
) -> Option<usize> {
    let own: Vec<usize> = (0..checkouts.len())
        .filter(|index| !checkouts[*index].linked)
        .collect();
    let mut votes: HashMap<usize, usize> = HashMap::new();
    for location in locations {
        if let Location::Checkout(index, _) = location {
            if !checkouts[*index].linked {
                *votes.entry(*index).or_default() += 1;
            }
        }
    }
    if let Some(index) = own
        .iter()
        .copied()
        .filter(|index| votes.contains_key(index))
        .max_by_key(|index| (votes[index], std::cmp::Reverse(*index)))
    {
        return Some(index);
    }
    let mut packed = HashMap::new();
    let at_revision: Vec<usize> = if revisions.is_empty() {
        Vec::new()
    } else {
        own.iter()
            .copied()
            .filter(|index| {
                checkout_head(&checkouts[*index].root, &mut packed).is_some_and(|head| {
                    revisions
                        .iter()
                        .any(|revision| same_revision(revision, &head))
                })
            })
            .collect()
    };
    let pool = if at_revision.is_empty() {
        own
    } else {
        at_revision
    };
    let relative: Vec<&str> = locations
        .iter()
        .filter_map(|location| match location {
            Location::Relative(path) => Some(path.as_str()),
            _ => None,
        })
        .collect();
    pool.into_iter().max_by_key(|index| {
        let checkout = &checkouts[*index];
        let present = relative
            .iter()
            .filter(|path| checkout.root.join(path).exists())
            .count();
        (present, checkout.primary, std::cmp::Reverse(*index))
    })
}

/// Read a checkout's HEAD commit from Git metadata without running Git.
/// Unfamiliar layouts, such as reftable, return `None`.
fn checkout_head(root: &Path, packed: &mut HashMap<PathBuf, Option<String>>) -> Option<String> {
    let dot_git = root.join(".git");
    let git_dir = if dot_git.is_dir() {
        dot_git
    } else {
        let pointer = read_limited(&dot_git, GIT_METADATA_LIMIT)?;
        resolve_from(root, pointer.trim().strip_prefix("gitdir:")?.trim())
    };
    let common_dir = read_limited(&git_dir.join("commondir"), GIT_METADATA_LIMIT).map_or_else(
        || git_dir.clone(),
        |text| resolve_from(&git_dir, text.trim()),
    );
    let head = read_limited(&git_dir.join("HEAD"), GIT_METADATA_LIMIT)?;
    let head = head.trim();
    let Some(reference) = head.strip_prefix("ref:").map(str::trim) else {
        return commit_hash(head);
    };
    if !reference.starts_with("refs/")
        || reference
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return None;
    }
    if let Some(value) = read_limited(&git_dir.join(reference), GIT_METADATA_LIMIT)
        .or_else(|| read_limited(&common_dir.join(reference), GIT_METADATA_LIMIT))
    {
        return commit_hash(value.trim());
    }
    packed
        .entry(common_dir.clone())
        .or_insert_with(|| read_limited(&common_dir.join("packed-refs"), PACKED_REFS_LIMIT))
        .as_deref()?
        .lines()
        .filter(|line| !line.starts_with(['#', '^']))
        .find_map(|line| {
            let (hash, name) = line.split_once(' ')?;
            if name.trim() == reference {
                commit_hash(hash)
            } else {
                None
            }
        })
}

fn resolve_from(base: &Path, value: &str) -> PathBuf {
    let path = Path::new(value);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

fn read_limited(path: &Path, limit: u64) -> Option<String> {
    let mut text = String::new();
    std::fs::File::open(path)
        .ok()?
        .take(limit.saturating_add(1))
        .read_to_string(&mut text)
        .ok()?;
    u64::try_from(text.len())
        .is_ok_and(|length| length <= limit)
        .then_some(text)
}

fn commit_hash(value: &str) -> Option<String> {
    ((value.len() == 40 || value.len() == 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| value.to_ascii_lowercase())
}

fn index_lookups(
    database: &Database,
    checkouts: &[Checkout],
    targets: &[Target],
) -> Result<Vec<IndexAnchor>> {
    database.with_reader(|connection| {
        let mut statement = connection.prepare(
            "SELECT content_digest,indexed_at_ms FROM indexed_files WHERE project_id=?1 AND path=?2",
        )?;
        targets
            .iter()
            .map(|target| {
                let Ok((index, relative)) = target else {
                    return Ok(None);
                };
                Ok(statement
                    .query_row(
                        params![checkouts[*index].project_id.as_str(), relative],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .optional()?)
            })
            .collect()
    })
}

/// Digest one touched file inside its checkout, with a precise failure reason.
fn read_live(
    root: &Path,
    relative: &str,
    remaining: &mut usize,
) -> std::result::Result<String, &'static str> {
    let canonical = root
        .join(relative)
        .canonicalize()
        .map_err(|error| io_reason(&error))?;
    if !canonical.starts_with(root) {
        return Err(RESOLVES_OUTSIDE);
    }
    if !canonical
        .metadata()
        .map_err(|error| io_reason(&error))?
        .is_file()
    {
        return Err(NOT_REGULAR_FILE);
    }
    let limit = (*remaining).min(LIVE_FILE_LIMIT);
    let mut bytes = Vec::new();
    std::fs::File::open(&canonical)
        .map_err(|error| io_reason(&error))?
        .take(u64::try_from(limit + 1).unwrap_or(u64::MAX))
        .read_to_end(&mut bytes)
        .map_err(|error| io_reason(&error))?;
    if bytes.len() > limit {
        return Err(OVER_BUDGET);
    }
    *remaining -= bytes.len();
    Ok(crate::experience::sha256(&bytes))
}

/// Deleted or renamed files are legitimately stale; permission and other
/// I/O failures are environment problems with their own remedies.
fn io_reason(error: &std::io::Error) -> &'static str {
    match error.kind() {
        ErrorKind::NotFound | ErrorKind::NotADirectory => MISSING_FILE,
        ErrorKind::PermissionDenied => PERMISSION_DENIED,
        _ => READ_FAILED,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abbreviated_revisions_match_only_a_real_prefix() {
        let live = "4dd913a0c3e6f0f9b0e3e9b1c2d3e4f5a6b7c8d9";
        assert!(same_revision(live, live));
        assert!(same_revision("4dd913a", live));
        assert!(same_revision(" 4DD913A0 ", live));
        assert!(!same_revision("4dd913", live), "fewer than seven digits");
        assert!(!same_revision("4dd913b", live));
        assert!(!same_revision("main", live));
        assert!(!same_revision(&format!("{live}0"), live));
    }

    #[test]
    fn relative_paths_normalize_without_escaping_their_root() {
        assert_eq!(
            normalize_relative(Path::new("./src/../src/lib.rs")).as_deref(),
            Some("src/lib.rs")
        );
        assert_eq!(normalize_relative(Path::new("../outside.rs")), None);
        assert_eq!(normalize_relative(Path::new("src/../../outside.rs")), None);
        assert_eq!(normalize_relative(Path::new("/etc/passwd")), None);
        assert_eq!(normalize_relative(Path::new(".")), None);
    }

    #[test]
    fn io_failures_have_distinct_reasons_and_a_stable_report_order() {
        let reason = |kind| io_reason(&std::io::Error::from(kind));
        assert_eq!(reason(ErrorKind::NotFound), MISSING_FILE);
        assert_eq!(reason(ErrorKind::NotADirectory), MISSING_FILE);
        assert_eq!(reason(ErrorKind::PermissionDenied), PERMISSION_DENIED);
        assert_eq!(reason(ErrorKind::Interrupted), READ_FAILED);
        let mut issues = Issues::default();
        for issue in [
            NOT_INDEXED,
            MISSING_FILE,
            PERMISSION_DENIED,
            DIFFERS_FROM_INDEX,
        ] {
            issues.add(issue);
        }
        assert_eq!(issues.first().as_deref(), Some(PERMISSION_DENIED));
    }

    #[test]
    fn head_is_read_from_loose_packed_and_linked_worktree_refs() {
        let temp = tempfile::TempDir::new().expect("temp");
        let primary = temp.path().join("primary");
        let git = primary.join(".git");
        std::fs::create_dir_all(git.join("refs/heads")).expect("refs");
        let loose = "a".repeat(40);
        let packed = "b".repeat(40);
        std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n").expect("head");
        std::fs::write(git.join("refs/heads/main"), format!("{loose}\n")).expect("ref");
        assert_eq!(checkout_head(&primary, &mut HashMap::new()), Some(loose));
        std::fs::remove_file(git.join("refs/heads/main")).expect("pack");
        std::fs::write(
            git.join("packed-refs"),
            format!("# pack-refs with: peeled\n{packed} refs/heads/main\n"),
        )
        .expect("packed");
        assert_eq!(checkout_head(&primary, &mut HashMap::new()), Some(packed));
        let worktree = temp.path().join("worktree");
        let admin = git.join("worktrees/feature");
        std::fs::create_dir_all(&admin).expect("admin");
        std::fs::create_dir_all(&worktree).expect("worktree");
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", admin.display()),
        )
        .expect("pointer");
        std::fs::write(admin.join("commondir"), "../..\n").expect("commondir");
        std::fs::write(admin.join("HEAD"), format!("{}\n", "c".repeat(40))).expect("detached");
        assert_eq!(
            checkout_head(&worktree, &mut HashMap::new()),
            Some("c".repeat(40))
        );
        std::fs::write(admin.join("HEAD"), "ref: refs/heads/../../escape\n").expect("escape");
        assert_eq!(checkout_head(&worktree, &mut HashMap::new()), None);
    }
}
