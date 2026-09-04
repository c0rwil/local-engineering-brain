//! Content-addressed filesystem blob store for large canonical artifacts.
//!
//! Large evidence artifacts would otherwise bloat `SQLite` rows, so their bytes
//! live on the filesystem under a `blobs/` directory beside the canonical
//! database while `SQLite` keeps only digest-addressed metadata:
//! `blobs(digest, size_bytes, content_type, created_at_ms, refcount)`.
//!
//! Files are sharded as `blobs/<first-2-hex>/<sha256-hex>` and are written with
//! the same owner-only permissions as the canonical database. Content is
//! redacted by the caller *before* it reaches this module: nothing here ever
//! inspects or rewrites bytes on their way to disk.
//!
//! Lifetime is refcounted. A reference is taken inside the same transaction
//! that persists the referencing row, and released inside the transaction that
//! removes it. Files are only unlinked *after* that transaction commits, so a
//! rollback can never orphan a metadata row whose bytes are already gone. The
//! reverse (a file with no row) is tolerated, reported by `engram doctor`, and
//! cleaned up by any later release of the same digest.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rusqlite::{OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{Error, Result, io_error};
use crate::storage::{Database, restrict_directory, restrict_file};

/// Directory holding content-addressed blobs, sibling to the database file.
const BLOB_DIRECTORY: &str = "blobs";

/// Distinguishes concurrent temporary files written by one process.
static TEMPORARY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Outcome of releasing one blob reference.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlobRelease {
    /// Other references remain; the file must stay.
    Retained,
    /// The last reference disappeared and the metadata row was deleted.
    /// The caller must unlink the file after the transaction commits.
    Removed,
    /// No metadata row existed. Purge retries land here; unlinking the file
    /// remains correct and is tolerant of an already-missing file.
    Unknown,
}

impl BlobRelease {
    /// Whether the caller should unlink the backing file after committing.
    #[must_use]
    pub const fn removes_file(self) -> bool {
        matches!(self, Self::Removed | Self::Unknown)
    }
}

/// Read-only blob-store health used by `engram doctor`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BlobHealth {
    pub root: PathBuf,
    pub blob_count: u64,
    pub total_bytes: u64,
    /// Files present on disk with no metadata row.
    pub orphan_files: u64,
    /// Metadata rows whose backing file is missing.
    pub missing_files: u64,
}

/// Content-addressed store for large artifacts referenced by canonical rows.
#[derive(Clone, Debug)]
pub struct BlobStore {
    database: Database,
    root: PathBuf,
}

impl BlobStore {
    #[must_use]
    pub(crate) const fn database(&self) -> &Database {
        &self.database
    }
}

impl BlobStore {
    /// Open the blob store that belongs to `database`.
    ///
    /// The root is derived from the database path exactly like the live
    /// `SQLite` sidecars are: a fixed name beside the database file.
    #[must_use]
    pub fn new(database: Database) -> Self {
        let root = blob_root(database.path());
        Self { database, root }
    }

    /// Directory holding sharded blob files.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Absolute path of the file backing `digest`.
    ///
    /// # Errors
    ///
    /// Returns an error when `digest` is not lowercase 64-character hex.
    pub fn path_for(&self, digest: &str) -> Result<PathBuf> {
        validate_digest(digest)?;
        Ok(self.root.join(&digest[..2]).join(digest))
    }

    /// Materialize `bytes` in the store and return their digest.
    ///
    /// Writing is idempotent: an existing file with the same digest is left
    /// untouched. This takes no reference; call [`Self::retain_in_transaction`]
    /// inside the transaction that persists the referencing row.
    ///
    /// # Errors
    ///
    /// Returns an error when the blob directory or file cannot be created with
    /// restrictive permissions.
    pub fn put(&self, bytes: &[u8]) -> Result<String> {
        let digest = digest_hex(bytes);
        let path = self.path_for(&digest)?;
        if path.exists() {
            // Refresh the mtime: orphan GC measures its grace window from the
            // file's last modification, so a reused crash-orphan must read as
            // freshly written or GC could unlink it before the referencing
            // row commits.
            let file = fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .map_err(|source| io_error(&path, source))?;
            file.set_modified(std::time::SystemTime::now())
                .map_err(|source| io_error(&path, source))?;
            return Ok(digest);
        }
        let shard = path
            .parent()
            .ok_or_else(|| Error::Config("blob path has no shard directory".to_owned()))?
            .to_path_buf();
        // The blob root and its shards belong exclusively to Engram, so they are
        // always restricted; only caller-owned parents are left alone.
        fs::create_dir_all(&self.root).map_err(|source| io_error(&self.root, source))?;
        restrict_directory(&self.root)?;
        fs::create_dir_all(&shard).map_err(|source| io_error(&shard, source))?;
        restrict_directory(&shard)?;

        let sequence = TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary = shard.join(format!("{digest}.tmp-{}-{sequence}", std::process::id()));
        fs::write(&temporary, bytes).map_err(|source| io_error(&temporary, source))?;
        restrict_file(&temporary)?;
        if let Err(source) = fs::rename(&temporary, &path) {
            let _ = fs::remove_file(&temporary);
            return Err(io_error(&path, source));
        }
        Ok(digest)
    }

    /// Read the complete bytes backing `digest`.
    ///
    /// The content is verified against its own address, so silent corruption
    /// surfaces as an error instead of wrong canonical data.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid digest, a missing file, or content that
    /// does not hash to `digest`.
    pub fn read(&self, digest: &str) -> Result<Vec<u8>> {
        let path = self.path_for(digest)?;
        let bytes = fs::read(&path).map_err(|source| io_error(&path, source))?;
        if digest_hex(&bytes) != digest {
            return Err(Error::Config(format!(
                "blob {digest} does not match its stored content"
            )));
        }
        Ok(bytes)
    }

    /// Read the complete UTF-8 text backing `digest`.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid digest, a missing file, corrupt content,
    /// or bytes that are not valid UTF-8.
    pub fn read_text(&self, digest: &str) -> Result<String> {
        let bytes = self.read(digest)?;
        String::from_utf8(bytes)
            .map_err(|_| Error::Config(format!("blob {digest} is not valid UTF-8 text")))
    }

    /// Unlink the file backing `digest`, tolerating an already-missing file.
    ///
    /// Purge is retried freely, so a second removal is a success, not an error.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid digest or an unlink failure other than
    /// a missing file.
    pub fn remove_file(&self, digest: &str) -> Result<()> {
        let path = self.path_for(digest)?;
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(source) => return Err(io_error(&path, source)),
        }
        if let Some(shard) = path.parent() {
            // Best effort: an empty shard directory is noise, but a concurrent
            // writer may legitimately be repopulating it.
            let _ = fs::remove_dir(shard);
        }
        Ok(())
    }

    /// Take one reference to `digest`, inserting metadata on first use.
    ///
    /// Identical content deduplicates onto a single blob: the digest is the
    /// identity, and only the reference count grows.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid digest or a failed statement.
    pub(crate) fn retain_in_transaction(
        transaction: &Transaction<'_>,
        digest: &str,
        size_bytes: usize,
        content_type: &str,
        now_ms: i64,
    ) -> Result<()> {
        validate_digest(digest)?;
        let size = i64::try_from(size_bytes)
            .map_err(|_| Error::Config("blob size exceeds the canonical limit".to_owned()))?;
        transaction.execute(
            "INSERT INTO blobs(digest, size_bytes, content_type, created_at_ms, refcount)
             VALUES (?1, ?2, ?3, ?4, 1)
             ON CONFLICT(digest) DO UPDATE SET refcount = refcount + 1",
            params![digest, size, content_type, now_ms],
        )?;
        Ok(())
    }

    /// Release one reference to `digest`, deleting metadata at zero.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid digest or a failed statement.
    pub(crate) fn release_in_transaction(
        transaction: &Transaction<'_>,
        digest: &str,
    ) -> Result<BlobRelease> {
        validate_digest(digest)?;
        let refcount: Option<i64> = transaction
            .query_row(
                "SELECT refcount FROM blobs WHERE digest = ?1",
                [digest],
                |row| row.get(0),
            )
            .optional()?;
        let Some(refcount) = refcount else {
            return Ok(BlobRelease::Unknown);
        };
        if refcount <= 1 {
            transaction.execute("DELETE FROM blobs WHERE digest = ?1", [digest])?;
            return Ok(BlobRelease::Removed);
        }
        transaction.execute(
            "UPDATE blobs SET refcount = refcount - 1 WHERE digest = ?1",
            [digest],
        )?;
        Ok(BlobRelease::Retained)
    }

    /// Current reference count for `digest`, or `None` when unknown.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid digest or a storage failure.
    pub fn refcount(&self, digest: &str) -> Result<Option<i64>> {
        validate_digest(digest)?;
        self.database.with_reader(|connection| {
            connection
                .query_row(
                    "SELECT refcount FROM blobs WHERE digest = ?1",
                    [digest],
                    |row| row.get(0),
                )
                .optional()
                .map_err(Error::from)
        })
    }

    /// Summarize blob-store health without mutating anything on disk.
    ///
    /// # Errors
    ///
    /// Returns an error when metadata cannot be read or the blob directory
    /// cannot be listed.
    pub fn health(&self) -> Result<BlobHealth> {
        let rows: BTreeMap<String, i64> = self.database.with_reader(|connection| {
            let mut statement = connection.prepare("SELECT digest, size_bytes FROM blobs")?;
            let rows = statement.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?;
            let mut collected = BTreeMap::new();
            for row in rows {
                let (digest, size) = row?;
                collected.insert(digest, size);
            }
            Ok(collected)
        })?;
        let files = self.list_files()?;
        let total_bytes = rows
            .values()
            .map(|size| u64::try_from(*size).unwrap_or(0))
            .fold(0_u64, u64::saturating_add);
        let orphan_files = files
            .iter()
            .filter(|name| !rows.contains_key(*name))
            .count();
        let missing_files = rows
            .keys()
            .filter(|digest| !files.contains(*digest))
            .count();
        Ok(BlobHealth {
            root: self.root.clone(),
            blob_count: count(rows.len()),
            total_bytes,
            orphan_files: count(orphan_files),
            missing_files: count(missing_files),
        })
    }

    /// Digests of files present on disk with no metadata row, bounded to
    /// `limit`. This is the same discovery `engram doctor` reports as
    /// `orphan_files`; callers such as the `consolidation_gc` job may delete
    /// these files because nothing canonical references them. Names that are
    /// not valid digests (for example abandoned temporary files) are skipped.
    ///
    /// # Errors
    ///
    /// Returns an error when metadata cannot be read or the blob directory
    /// cannot be listed.
    pub fn orphaned_digests(&self, limit: usize) -> Result<Vec<String>> {
        let rows: BTreeSet<String> = self.database.with_reader(|connection| {
            let mut statement = connection.prepare("SELECT digest FROM blobs")?;
            let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
            rows.collect::<std::result::Result<BTreeSet<_>, _>>()
                .map_err(Error::from)
        })?;
        Ok(self
            .list_files()?
            .into_iter()
            .filter(|name| validate_digest(name).is_ok() && !rows.contains(name))
            .take(limit)
            .collect())
    }

    /// Names of every regular file under the sharded blob root.
    fn list_files(&self) -> Result<BTreeSet<String>> {
        let mut names = BTreeSet::new();
        let shards = match fs::read_dir(&self.root) {
            Ok(shards) => shards,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(names),
            Err(source) => return Err(io_error(&self.root, source)),
        };
        for shard in shards {
            let shard = shard.map_err(|source| io_error(&self.root, source))?;
            let shard_path = shard.path();
            if !shard_path.is_dir() {
                continue;
            }
            let entries =
                fs::read_dir(&shard_path).map_err(|source| io_error(&shard_path, source))?;
            for entry in entries {
                let entry = entry.map_err(|source| io_error(&shard_path, source))?;
                if entry.path().is_dir() {
                    continue;
                }
                names.insert(entry.file_name().to_string_lossy().into_owned());
            }
        }
        Ok(names)
    }
}

/// Saturating count conversion for report fields.
fn count(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

/// Blob directory for a canonical database path.
fn blob_root(database: &Path) -> PathBuf {
    database
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(BLOB_DIRECTORY)
}

/// Lowercase hexadecimal SHA-256 of `bytes`.
#[must_use]
pub fn digest_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

/// Reject anything that is not a canonical digest before it reaches a path.
fn validate_digest(digest: &str) -> Result<()> {
    if digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Ok(());
    }
    Err(Error::Config(format!(
        "blob digest must be 64 lowercase hexadecimal characters: `{digest}`"
    )))
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn store() -> (TempDir, BlobStore) {
        let temp = TempDir::new().expect("temp");
        let database = Database::open(temp.path().join("brain.sqlite")).expect("database");
        (temp, BlobStore::new(database))
    }

    #[test]
    fn blobs_are_sharded_beside_the_database_and_read_back_intact() {
        let (temp, blobs) = store();
        assert_eq!(blobs.root(), temp.path().join("blobs"));
        let digest = blobs.put(b"artifact bytes").expect("put");
        let path = blobs.path_for(&digest).expect("path");
        assert_eq!(
            path,
            temp.path().join("blobs").join(&digest[..2]).join(&digest)
        );
        assert!(path.exists());
        assert_eq!(blobs.read_text(&digest).expect("read"), "artifact bytes");
        // Re-writing identical content is a no-op that keeps one file.
        assert_eq!(blobs.put(b"artifact bytes").expect("put again"), digest);
        assert_eq!(blobs.health().expect("health").orphan_files, 1);
    }

    #[cfg(unix)]
    #[test]
    fn blob_files_and_directories_are_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;

        let (_temp, blobs) = store();
        let digest = blobs.put(b"private artifact").expect("put");
        let path = blobs.path_for(&digest).expect("path");
        let file_mode = fs::metadata(&path).expect("file").permissions().mode() & 0o777;
        assert_eq!(file_mode, 0o600);
        for directory in [blobs.root(), path.parent().expect("shard")] {
            let mode = fs::metadata(directory)
                .expect("directory")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o700, "unsafe permissions on {}", directory.display());
        }
    }

    #[test]
    fn traversal_shaped_digests_are_rejected() {
        let (_temp, blobs) = store();
        assert!(blobs.path_for("../../etc/passwd").is_err());
        assert!(blobs.read("NOTHEX").is_err());
        assert!(blobs.remove_file(&"A".repeat(64)).is_err());
    }

    #[test]
    fn removing_a_missing_file_is_not_an_error() {
        let (_temp, blobs) = store();
        let digest = digest_hex(b"never written");
        blobs.remove_file(&digest).expect("first removal");
        blobs.remove_file(&digest).expect("idempotent removal");
    }

    #[test]
    fn corrupted_content_fails_its_address_check() {
        let (_temp, blobs) = store();
        let digest = blobs.put(b"trustworthy").expect("put");
        fs::write(blobs.path_for(&digest).expect("path"), b"tampered").expect("tamper");
        let error = blobs.read(&digest).expect_err("corrupt blob");
        assert!(error.to_string().contains("does not match"));
    }
}
