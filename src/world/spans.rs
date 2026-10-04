//! Line ranges for indexed source pointers.
//!
//! The structural index stores a byte span for every definition and one
//! content digest per file. A range is reported only after the checkout file
//! has been read and its digest equals the indexed digest. A file that changed
//! after indexing, or that cannot be read safely, yields no range, so callers
//! fall back to a file-level pointer instead of emitting stale line numbers.

use std::collections::BTreeMap;
use std::fmt;
use std::io::Read as _;
use std::path::{Component, Path, PathBuf};

use rusqlite::{OptionalExtension, params};
use sha2::{Digest, Sha256};

use crate::error::Result;
use crate::storage::Database;

/// Largest single file read for a range check. Indexed files are already
/// bounded by `structural.max_file_bytes`; this is an independent bound.
const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
/// Total bytes read for range checks by one [`LiveSpans`] instance.
const MAX_TOTAL_BYTES: u64 = 32 * 1024 * 1024;
/// Contiguous attribute, decorator or comment lines folded into a definition.
const MAX_LEADING_LINES: usize = 16;

/// A 1-based, inclusive line range.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LineRange {
    pub start: u32,
    pub end: u32,
}

impl fmt::Display for LineRange {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}-{}", self.start, self.end)
    }
}

/// Outcome of checking one indexed node against its checkout file.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SpanCheck {
    /// The checkout file still has the indexed digest; the range is current.
    Current(LineRange),
    /// The checkout file differs from the indexed version.
    Changed,
    /// No range applies: a whole-file node, a node without a usable span,
    /// another repository, no checkout root, or an unreadable file.
    Unavailable,
}

enum FileState {
    Current(VerifiedFile),
    Changed,
    Unavailable,
}

struct VerifiedFile {
    bytes: Vec<u8>,
    /// Byte offset of every line start; the first entry is always zero.
    line_starts: Vec<usize>,
}

struct IndexedNode {
    project_id: String,
    source_kind: String,
    kind: String,
    file_path: Option<String>,
    language: Option<String>,
    start_byte: Option<i64>,
    end_byte: Option<i64>,
    label: String,
    content_digest: Option<String>,
    size_bytes: Option<i64>,
}

/// Per-compilation range checker for one repository checkout. Each file is
/// read and hashed at most once; nothing is cached across compilations.
pub struct LiveSpans<'a> {
    database: &'a Database,
    project_id: String,
    root: Option<PathBuf>,
    files: BTreeMap<String, FileState>,
    read_bytes: u64,
}

impl<'a> LiveSpans<'a> {
    /// A checker for nodes of `project_id` whose files live under `root`.
    /// `None` (or an unusable root) disables ranges entirely.
    #[must_use]
    pub fn new(database: &'a Database, project_id: &str, root: Option<&Path>) -> Self {
        Self {
            database,
            project_id: project_id.to_owned(),
            root: root
                .filter(|root| root.is_absolute())
                .and_then(|root| root.canonicalize().ok()),
            files: BTreeMap::new(),
            read_bytes: 0,
        }
    }

    /// A checker that never reports ranges.
    #[must_use]
    pub fn disabled(database: &'a Database) -> Self {
        Self::new(database, "", None)
    }

    /// Resolve the current line range of an indexed node.
    ///
    /// # Errors
    ///
    /// Returns storage errors. File-system problems never fail the check;
    /// they produce [`SpanCheck::Unavailable`].
    pub fn check(&mut self, node_id: &str) -> Result<SpanCheck> {
        if self.root.is_none() {
            return Ok(SpanCheck::Unavailable);
        }
        let node = self.database.with_reader(|connection| {
            Ok(connection
                .prepare_cached(
                    "SELECT n.project_id,n.source_kind,n.kind,n.file_path,n.language,
                            n.start_byte,n.end_byte,n.label,f.content_digest,f.size_bytes
                     FROM nodes n LEFT JOIN indexed_files f
                       ON f.project_id=n.project_id AND f.path=n.file_path
                     WHERE n.id=?1 AND n.tombstoned_at_ms IS NULL",
                )?
                .query_row(params![node_id], |row| {
                    Ok(IndexedNode {
                        project_id: row.get(0)?,
                        source_kind: row.get(1)?,
                        kind: row.get(2)?,
                        file_path: row.get(3)?,
                        language: row.get(4)?,
                        start_byte: row.get(5)?,
                        end_byte: row.get(6)?,
                        label: row.get(7)?,
                        content_digest: row.get(8)?,
                        size_bytes: row.get(9)?,
                    })
                })
                .optional()?)
        })?;
        let Some(node) = node else {
            return Ok(SpanCheck::Unavailable);
        };
        // Only the native indexer records raw-content digests and byte spans;
        // imported graphs keep their exporter's own digest contract.
        if node.project_id != self.project_id
            || node.source_kind != "static_analysis"
            || matches!(node.kind.as_str(), "File" | "ExternalReference")
        {
            return Ok(SpanCheck::Unavailable);
        }
        let (Some(path), Some(start), Some(end), Some(digest), Some(size)) = (
            node.file_path.as_deref(),
            node.start_byte
                .and_then(|value| usize::try_from(value).ok()),
            node.end_byte.and_then(|value| usize::try_from(value).ok()),
            node.content_digest.as_deref(),
            node.size_bytes.and_then(|value| u64::try_from(value).ok()),
        ) else {
            return Ok(SpanCheck::Unavailable);
        };
        if end <= start {
            return Ok(SpanCheck::Unavailable);
        }
        if !self.files.contains_key(path) {
            let state = self.load(path, digest, size);
            self.files.insert(path.to_owned(), state);
        }
        Ok(match self.files.get(path) {
            Some(FileState::Current(file)) => {
                let range = if node.kind == "Section" {
                    section_range(file, start, &node.label)
                } else {
                    definition_range(file, start, end, node.language.as_deref())
                };
                range.map_or(SpanCheck::Unavailable, SpanCheck::Current)
            }
            Some(FileState::Changed) => SpanCheck::Changed,
            Some(FileState::Unavailable) | None => SpanCheck::Unavailable,
        })
    }

    fn load(&mut self, relative: &str, digest: &str, size: u64) -> FileState {
        let Some(root) = self.root.as_deref() else {
            return FileState::Unavailable;
        };
        let Some(path) = contained_path(root, relative) else {
            return FileState::Unavailable;
        };
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            return FileState::Unavailable;
        };
        if !metadata.file_type().is_file() || metadata.len() > MAX_FILE_BYTES {
            return FileState::Unavailable;
        }
        if metadata.len() != size {
            return FileState::Changed;
        }
        if self.read_bytes.saturating_add(metadata.len()) > MAX_TOTAL_BYTES {
            return FileState::Unavailable;
        }
        // Symlinked parent directories could leave the checkout; the indexer
        // never follows them either.
        if !path
            .canonicalize()
            .is_ok_and(|resolved| resolved.starts_with(root))
        {
            return FileState::Unavailable;
        }
        let mut bytes = Vec::new();
        let read = std::fs::File::open(&path)
            .and_then(|file| file.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes));
        self.read_bytes = self
            .read_bytes
            .saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        if read.is_err() || u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_FILE_BYTES {
            return FileState::Unavailable;
        }
        if hex_sha256(&bytes) != digest {
            return FileState::Changed;
        }
        let line_starts = std::iter::once(0)
            .chain(
                bytes
                    .iter()
                    .enumerate()
                    .filter(|(_, byte)| **byte == b'\n')
                    .map(|(offset, _)| offset + 1),
            )
            .collect();
        FileState::Current(VerifiedFile { bytes, line_starts })
    }
}

/// Join an indexed repository-relative path without escaping the root.
fn contained_path(root: &Path, relative: &str) -> Option<PathBuf> {
    let relative = Path::new(relative);
    if relative.as_os_str().is_empty()
        || !relative
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
    {
        return None;
    }
    Some(root.join(relative))
}

impl VerifiedFile {
    /// 1-based line containing `offset`, which must be inside the file.
    fn line_of(&self, offset: usize) -> usize {
        self.line_starts.partition_point(|start| *start <= offset)
    }

    /// Number of lines, not counting the empty remainder after a final newline.
    fn line_count(&self) -> usize {
        if self.bytes.last() == Some(&b'\n') {
            self.line_starts.len() - 1
        } else {
            self.line_starts.len()
        }
    }

    /// Bytes of a 1-based line, without its line terminator.
    fn line(&self, number: usize) -> &[u8] {
        let start = self.line_starts[number - 1];
        let end = self
            .line_starts
            .get(number)
            .map_or(self.bytes.len(), |next| next - 1);
        let line = &self.bytes[start..end.max(start)];
        line.strip_suffix(b"\r").unwrap_or(line)
    }
}

fn definition_range(
    file: &VerifiedFile,
    start: usize,
    end: usize,
    language: Option<&str>,
) -> Option<LineRange> {
    if end > file.bytes.len() {
        return None;
    }
    let last = file.line_of(end - 1);
    let mut first = file.line_of(start);
    // Attributes, decorators and doc comments directly above a definition
    // are part of what a reader needs; the parser records them as siblings.
    for _ in 0..MAX_LEADING_LINES {
        if first <= 1 || !is_leading_line(file.line(first - 1), language) {
            break;
        }
        first -= 1;
    }
    Some(LineRange {
        start: u32::try_from(first).ok()?,
        end: u32::try_from(last).ok()?,
    })
}

fn is_leading_line(line: &[u8], language: Option<&str>) -> bool {
    let line = line.trim_ascii();
    if line.is_empty() {
        return false;
    }
    let prefixes: &[&[u8]] = match language {
        Some("python" | "ruby" | "bash") => &[b"#", b"@"],
        Some("csharp") => &[b"///", b"//", b"/*", b"*", b"["],
        _ => &[b"///", b"//", b"/*", b"*", b"#[", b"@"],
    };
    prefixes.iter().any(|prefix| line.starts_with(prefix))
}

/// A Markdown section spans from its heading to the line before the next
/// heading of the same or a higher level, outside fenced code blocks; this
/// mirrors the indexer's heading rules. The heading line must still carry the
/// indexed title, otherwise no range is reported.
fn section_range(file: &VerifiedFile, start: usize, label: &str) -> Option<LineRange> {
    if start >= file.bytes.len() {
        return None;
    }
    let first = file.line_of(start);
    if file.line_starts[first - 1] != start {
        return None;
    }
    let (level, title) = heading(file.line(first))?;
    if !title.starts_with(label) {
        return None;
    }
    let mut last = file.line_count();
    let mut in_fence = false;
    for number in first + 1..=file.line_count() {
        let line = String::from_utf8_lossy(file.line(number));
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if !in_fence && heading(file.line(number)).is_some_and(|(next, _)| next <= level) {
            last = number - 1;
            break;
        }
    }
    while last > first && file.line(last).trim_ascii().is_empty() {
        last -= 1;
    }
    Some(LineRange {
        start: u32::try_from(first).ok()?,
        end: u32::try_from(last).ok()?,
    })
}

fn heading(line: &[u8]) -> Option<(usize, String)> {
    let line = String::from_utf8_lossy(line);
    let rest = line.strip_prefix('#')?;
    let level = 1 + rest.chars().take_while(|c| *c == '#').count();
    let title = rest.trim_start_matches('#').trim();
    (level <= 6 && !title.is_empty()).then(|| (level, title.to_owned()))
}

fn hex_sha256(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(output, "{byte:02x}");
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::fixtures::IndexedRepo;

    const SOURCE: &str = "use std::sync::Mutex;

/// Shared state.
#[derive(Debug)]
pub struct Engine {
    id: u32,
}

impl Engine {
    /// Run it.
    pub fn run(&self) -> u32 {
        helper()
    }
}

fn helper() -> u32 {
    1
}
";

    const GUIDE: &str = "# Guide
Intro text.

## Install
Step one.
```sh
# not a heading
```
More.

### Detail
Deep.

## Usage
Use it.
";

    fn check(repo: &IndexedRepo, label: &str) -> SpanCheck {
        LiveSpans::new(&repo.database, repo.id().as_str(), Some(&repo.root))
            .check(&repo.node(label))
            .unwrap()
    }

    fn current(start: u32, end: u32) -> SpanCheck {
        SpanCheck::Current(LineRange { start, end })
    }

    #[test]
    fn definitions_report_one_based_inclusive_ranges_with_leading_attributes() {
        let repo = IndexedRepo::new(&[("src/lib.rs", SOURCE)]);
        assert_eq!(check(&repo, "Engine"), current(3, 7));
        assert_eq!(check(&repo, "Engine::run"), current(10, 13));
        assert_eq!(check(&repo, "helper"), current(16, 18));
        assert_eq!(LineRange { start: 3, end: 7 }.to_string(), "3-7");
    }

    #[test]
    fn markdown_sections_end_before_the_next_heading_of_equal_or_higher_level() {
        let repo = IndexedRepo::new(&[("docs/guide.md", GUIDE)]);
        assert_eq!(check(&repo, "Guide"), current(1, 15));
        assert_eq!(check(&repo, "Install"), current(4, 12));
        assert_eq!(check(&repo, "Detail"), current(11, 12));
        assert_eq!(check(&repo, "Usage"), current(14, 15));
        // A whole-file pointer has no range.
        assert_eq!(check(&repo, "docs/guide.md"), SpanCheck::Unavailable);
    }

    #[test]
    fn changed_files_never_report_a_stale_range() {
        let repo = IndexedRepo::new(&[("src/lib.rs", SOURCE)]);
        assert_eq!(check(&repo, "helper"), current(16, 18));
        // Same size, different bytes: only the digest can tell.
        repo.write("src/lib.rs", &SOURCE.replace("1\n}", "2\n}"));
        assert_eq!(check(&repo, "helper"), SpanCheck::Changed);
        repo.write("src/lib.rs", &format!("// moved\n{SOURCE}"));
        assert_eq!(check(&repo, "helper"), SpanCheck::Changed);
        // A refreshed index makes the moved definition current again.
        repo.reindex();
        assert_eq!(check(&repo, "helper"), current(17, 19));
        std::fs::remove_file(repo.root.join("src/lib.rs")).unwrap();
        assert_eq!(check(&repo, "helper"), SpanCheck::Unavailable);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_or_escaping_files_are_never_read() {
        let repo = IndexedRepo::new(&[("src/lib.rs", SOURCE)]);
        let outside = repo.root.parent().unwrap().join("outside.rs");
        std::fs::write(&outside, SOURCE).unwrap();
        std::fs::remove_file(repo.root.join("src/lib.rs")).unwrap();
        std::os::unix::fs::symlink(&outside, repo.root.join("src/lib.rs")).unwrap();
        assert_eq!(check(&repo, "helper"), SpanCheck::Unavailable);
        // A corrupted indexed path cannot climb out of the checkout either.
        repo.database
            .write_immediate(|tx| {
                tx.execute(
                    "UPDATE indexed_files SET path='../outside.rs' WHERE path='src/lib.rs'",
                    [],
                )?;
                tx.execute(
                    "UPDATE nodes SET file_path='../outside.rs' WHERE file_path='src/lib.rs'",
                    [],
                )?;
                Ok(())
            })
            .unwrap();
        assert_eq!(check(&repo, "helper"), SpanCheck::Unavailable);
    }

    #[test]
    fn ranges_require_the_requests_own_checkout() {
        let repo = IndexedRepo::new(&[("src/lib.rs", SOURCE)]);
        let helper = repo.node("helper");
        for (project, root) in [
            ("another-project", Some(repo.root.as_path())),
            (repo.id().as_str(), None),
            (repo.id().as_str(), Some(Path::new("relative/root"))),
        ] {
            assert_eq!(
                LiveSpans::new(&repo.database, project, root)
                    .check(&helper)
                    .unwrap(),
                SpanCheck::Unavailable
            );
        }
        assert_eq!(
            LiveSpans::disabled(&repo.database).check(&helper).unwrap(),
            SpanCheck::Unavailable
        );
        assert_eq!(
            LiveSpans::new(&repo.database, repo.id().as_str(), Some(&repo.root))
                .check("missing-node")
                .unwrap(),
            SpanCheck::Unavailable
        );
        // Imported graphs keep their exporter's digest contract and spans.
        repo.database
            .write_immediate(|tx| {
                tx.execute(
                    "UPDATE nodes SET source_kind='import' WHERE id=?1",
                    [&helper],
                )?;
                Ok(())
            })
            .unwrap();
        assert_eq!(check(&repo, "helper"), SpanCheck::Unavailable);
    }

    #[test]
    fn misaligned_markdown_offsets_do_not_produce_ranges() {
        // The Markdown scanner counts one byte per line ending, so later
        // headings in CRLF files have drifted offsets; they get no range.
        let repo = IndexedRepo::new(&[("notes.md", "# Alpha\r\nx\r\n# Beta\r\ny\r\n")]);
        assert_eq!(check(&repo, "Alpha"), current(1, 2));
        assert_eq!(check(&repo, "Beta"), SpanCheck::Unavailable);
    }
}
