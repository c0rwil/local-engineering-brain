//! Canonical `SQLite` persistence and schema migrations.

use std::fmt;
use std::fs;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{Error, Result, io_error};

const MIGRATIONS: &[(i64, &str, &str)] = &[
    (1, "core", include_str!("migrations/0001_core.sql")),
    (
        2,
        "v1_domain",
        include_str!("migrations/0002_v1_domain.sql"),
    ),
    (
        3,
        "context_request_scope",
        include_str!("migrations/0003_context_request_scope.sql"),
    ),
    (
        4,
        "scan_changed_paths",
        include_str!("migrations/0004_scan_changed_paths.sql"),
    ),
    (
        5,
        "blob_store",
        include_str!("migrations/0005_blob_store.sql"),
    ),
    (
        6,
        "semantic_index",
        include_str!("migrations/0006_semantic_index.sql"),
    ),
    (7, "jobs", include_str!("migrations/0007_jobs.sql")),
    (
        8,
        "compact_injection",
        include_str!("migrations/0008_compact_injection.sql"),
    ),
    (
        9,
        "fts_porter",
        include_str!("migrations/0009_fts_porter.sql"),
    ),
    (
        10,
        "project_links",
        include_str!("migrations/0010_project_links.sql"),
    ),
    (
        11,
        "project_facts",
        include_str!("migrations/0011_project_facts.sql"),
    ),
    (
        12,
        "product_knowledge",
        include_str!("migrations/0012_product_knowledge.sql"),
    ),
    (
        13,
        "effectiveness",
        include_str!("migrations/0013_effectiveness.sql"),
    ),
    (
        14,
        "lesson_judgments",
        include_str!("migrations/0014_lesson_judgments.sql"),
    ),
    (
        15,
        "source_search",
        include_str!("migrations/0015_source_search.sql"),
    ),
    (
        16,
        "source_aliases_delivery",
        include_str!("migrations/0016_source_aliases_delivery.sql"),
    ),
    (
        19,
        "tool_observations",
        include_str!("migrations/0019_tool_observations.sql"),
    ),
];
/// Newest canonical schema this binary can safely read and write.
pub const LATEST_MIGRATION_VERSION: i64 = 19;

/// Reopenable handle to the canonical database.
///
/// Clones share one persistent canonical writer and a bounded reader pool.
#[derive(Clone)]
pub struct Database {
    inner: Arc<DatabaseInner>,
}

struct DatabaseInner {
    path: PathBuf,
    writer: Mutex<Connection>,
    readers: ReadPool,
}

struct ReadPool {
    available: Mutex<Vec<Connection>>,
    ready: Condvar,
    capacity: usize,
    wait_timeout: Duration,
}

struct ReadLease<'a> {
    pool: &'a ReadPool,
    connection: Option<Connection>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct IntegrityReport {
    pub integrity: String,
    pub journal_mode: String,
    pub foreign_keys: bool,
    pub migration_version: i64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReadPoolStats {
    pub capacity: usize,
    pub available: usize,
    pub in_use: usize,
}

impl Database {
    /// Open a database, enable required pragmas, and apply pending migrations.
    ///
    /// # Errors
    ///
    /// Returns an error when the path cannot be prepared or `SQLite` setup/migration fails.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_read_pool(path, 4)
    }

    /// Open canonical storage with a fixed reusable reader pool.
    ///
    /// # Errors
    ///
    /// Returns an error when `read_connections` is zero or any connection,
    /// permission, setup, or migration operation fails.
    pub fn open_with_read_pool(path: impl AsRef<Path>, read_connections: usize) -> Result<Self> {
        Self::open_with_options(path, read_connections, Duration::from_secs(5))
    }

    /// Open canonical storage with explicit bounded reader wait behavior.
    ///
    /// # Errors
    ///
    /// Returns an error when a limit is zero or any storage setup fails.
    pub fn open_with_options(
        path: impl AsRef<Path>,
        read_connections: usize,
        read_wait_timeout: Duration,
    ) -> Result<Self> {
        if read_connections == 0 {
            return Err(Error::Config(
                "storage read connection pool must be greater than zero".to_owned(),
            ));
        }
        if read_wait_timeout.is_zero() {
            return Err(Error::Config(
                "storage read wait timeout must be greater than zero".to_owned(),
            ));
        }
        let path = path.as_ref().to_path_buf();
        if path.as_os_str() == ":memory:" {
            return Err(Error::Config(
                "`:memory:` is unsupported because Engram requires reopenable canonical storage"
                    .to_owned(),
            ));
        }
        if let Some(parent) = path.parent() {
            let parent_existed = parent.exists();
            fs::create_dir_all(parent).map_err(|source| io_error(parent, source))?;
            // Never chmod an arbitrary caller-owned directory (for example `/tmp`).
            // A directory created specifically for this database is safe to restrict.
            if !parent_existed {
                restrict_directory(parent)?;
            }
        }

        let mut writer = open_configured_connection(&path)?;
        apply_migrations(&mut writer)?;
        restrict_database_files(&path)?;
        let mut readers = Vec::with_capacity(read_connections);
        for _ in 0..read_connections {
            readers.push(open_read_only_connection(&path)?);
        }
        let database = Self {
            inner: Arc::new(DatabaseInner {
                path,
                writer: Mutex::new(writer),
                readers: ReadPool {
                    available: Mutex::new(readers),
                    ready: Condvar::new(),
                    capacity: read_connections,
                    wait_timeout: read_wait_timeout,
                },
            }),
        };
        restrict_database_files(database.path())?;
        Ok(database)
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    /// Open an unmanaged configured connection for diagnostics and migrations tests.
    ///
    /// Normal production reads should use [`Self::with_reader`]; canonical
    /// mutations use the daemon's scheduler and the internal writer connection.
    ///
    /// # Errors
    ///
    /// Returns an error when `SQLite` cannot open/configure the connection.
    pub fn connect(&self) -> Result<Connection> {
        open_configured_connection(self.path())
    }

    /// Execute a bounded read using a reusable connection.
    ///
    /// # Errors
    ///
    /// Returns an error if the pool is poisoned or the operation fails.
    pub fn with_reader<T>(&self, operation: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let lease = self.inner.readers.acquire()?;
        operation(&lease)
    }

    /// Return a point-in-time view of bounded read-pool utilization.
    #[must_use]
    pub fn read_pool_stats(&self) -> ReadPoolStats {
        let available = self
            .inner
            .readers
            .available
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len();
        ReadPoolStats {
            capacity: self.inner.readers.capacity,
            available,
            in_use: self.inner.readers.capacity.saturating_sub(available),
        }
    }

    /// Inspect canonical database health and required settings.
    ///
    /// # Errors
    ///
    /// Returns an error when health queries cannot complete.
    pub fn integrity_report(&self) -> Result<IntegrityReport> {
        // Integrity checks are deliberately isolated on a short-lived
        // connection. FTS5 integrity-check statements can retain extension
        // state on a persistent connection across later writes from another
        // connection, yielding a false checksum mismatch on the next check.
        // Operational queries continue to use the bounded persistent pool.
        let connection = open_read_only_connection(self.path())?;
        integrity_report_connection(&connection)
    }

    /// Verify an existing backup without creating, migrating, or changing its
    /// journal mode.
    ///
    /// # Errors
    ///
    /// Returns an error for a missing/non-regular file, incompatible migration
    /// history, checksum mismatch, or failed integrity check.
    pub fn verify_existing(path: &Path) -> Result<IntegrityReport> {
        let metadata = fs::symlink_metadata(path).map_err(|source| io_error(path, source))?;
        if !metadata.file_type().is_file() {
            return Err(Error::Config(format!(
                "backup path must be an existing regular file: {}",
                path.display()
            )));
        }
        let connection = open_read_only_connection(path)?;
        validate_applied_migrations_read_only(&connection)?;
        integrity_report_connection(&connection)
    }

    /// Create a transactionally consistent backup using `SQLite`'s `VACUUM INTO`.
    ///
    /// # Errors
    ///
    /// Returns an error when the destination exists or the backup cannot be written safely.
    pub fn backup(&self, destination: &Path) -> Result<()> {
        if destination.exists() {
            return Err(Error::Config(format!(
                "backup destination already exists: {}",
                destination.display()
            )));
        }
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(|source| io_error(parent, source))?;
        }
        let writer = self
            .inner
            .writer
            .lock()
            .map_err(|_| Error::StorageLockPoisoned)?;
        writer.execute("VACUUM INTO ?1", [destination.to_string_lossy().as_ref()])?;
        restrict_file(destination)?;
        Ok(())
    }

    /// Remove obsolete WAL frames after an explicit content purge.
    ///
    /// # Errors
    /// Returns an error if a reader keeps the WAL pinned; callers may retry.
    pub fn truncate_wal(&self) -> Result<()> {
        let writer = self
            .inner
            .writer
            .lock()
            .map_err(|_| Error::StorageLockPoisoned)?;
        let busy: i64 =
            writer.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))?;
        if busy != 0 {
            return Err(Error::Config("content removed, but a reader holds the WAL; retry the purge after readers finish to remove old WAL frames".to_owned()));
        }
        Ok(())
    }

    pub(crate) fn write_immediate<T>(
        &self,
        operation: impl FnOnce(&Transaction<'_>) -> Result<T>,
    ) -> Result<T> {
        let mut writer = self
            .inner
            .writer
            .lock()
            .map_err(|_| Error::StorageLockPoisoned)?;
        immediate_transaction(&mut writer, operation)
    }
}

fn integrity_report_connection(connection: &Connection) -> Result<IntegrityReport> {
    (|| {
        // Exhaust the pragma cursor. Reading only its first row leaves the
        // connection's read snapshot pinned when multiple diagnostics are
        // possible, which can make a later FTS5 check compare old content
        // against a new index and report a false checksum mismatch.
        let integrity = {
            let mut statement = connection.prepare("PRAGMA integrity_check")?;
            let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
            let messages = rows.collect::<std::result::Result<Vec<_>, _>>()?;
            if messages.len() == 1 && messages[0] == "ok" {
                "ok".to_owned()
            } else {
                messages.join("; ")
            }
        };
        let journal_mode = connection.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
        let foreign_keys = connection.query_row("PRAGMA foreign_keys", [], |row| {
            row.get::<_, i64>(0).map(|enabled| enabled == 1)
        })?;
        let migration_version = connection.query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
            [],
            |row| row.get(0),
        )?;
        Ok(IntegrityReport {
            integrity,
            journal_mode,
            foreign_keys,
            migration_version,
        })
    })()
}

impl fmt::Debug for Database {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Database")
            .field("path", &self.path())
            .field("read_connections", &self.inner.readers.capacity)
            .finish_non_exhaustive()
    }
}

impl ReadPool {
    fn acquire(&self) -> Result<ReadLease<'_>> {
        let deadline = Instant::now() + self.wait_timeout;
        let mut available = self
            .available
            .lock()
            .map_err(|_| Error::StorageLockPoisoned)?;
        while available.is_empty() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(Error::StorageReadTimeout);
            }
            let (next, timeout) = self
                .ready
                .wait_timeout(available, remaining)
                .map_err(|_| Error::StorageLockPoisoned)?;
            available = next;
            if timeout.timed_out() && available.is_empty() {
                return Err(Error::StorageReadTimeout);
            }
        }
        Ok(ReadLease {
            pool: self,
            connection: available.pop(),
        })
    }
}

impl Deref for ReadLease<'_> {
    type Target = Connection;

    fn deref(&self) -> &Self::Target {
        self.connection
            .as_ref()
            .expect("read lease always owns a connection")
    }
}

impl Drop for ReadLease<'_> {
    fn drop(&mut self) {
        let Some(connection) = self.connection.take() else {
            return;
        };
        let mut available = self
            .pool
            .available
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        available.push(connection);
        self.pool.ready.notify_one();
    }
}

fn open_configured_connection(path: &Path) -> Result<Connection> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    // Restrict the main file before WAL mode can create plaintext sidecars.
    restrict_file(path)?;
    configure_connection(&connection)?;
    restrict_database_files(path)?;
    Ok(connection)
}

fn open_read_only_connection(path: &Path) -> Result<Connection> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.busy_timeout(Duration::from_secs(5))?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    connection.pragma_update(None, "query_only", "ON")?;
    Ok(connection)
}

fn configure_connection(connection: &Connection) -> Result<()> {
    connection.busy_timeout(Duration::from_secs(5))?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "synchronous", "FULL")?;
    Ok(())
}

fn apply_migrations(connection: &mut Connection) -> Result<()> {
    // One writer transaction covers schema discovery, downgrade validation,
    // and every pending migration. No older/newer process can interleave a
    // schema change between validation and commit.
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            version       INTEGER PRIMARY KEY NOT NULL,
            name          TEXT NOT NULL UNIQUE,
            checksum      TEXT,
            applied_at_ms INTEGER NOT NULL
        ) STRICT;",
    )?;

    // Upgrade databases created by early Milestone 0 builds before checksums
    // were recorded. This is metadata hardening, not a domain migration.
    let has_checksum = {
        let mut statement = transaction.prepare("PRAGMA table_info(schema_migrations)")?;
        let columns = statement.query_map([], |row| row.get::<_, String>(1))?;
        let mut found = false;
        for column in columns {
            if column? == "checksum" {
                found = true;
            }
        }
        found
    };
    if !has_checksum {
        transaction.execute("ALTER TABLE schema_migrations ADD COLUMN checksum TEXT", [])?;
    }

    // Refuse to open schemas this binary does not understand. This prevents an
    // older binary from mutating a database after a newer binary upgraded it.
    {
        let mut statement = transaction
            .prepare("SELECT version, name, checksum FROM schema_migrations ORDER BY version")?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?;
        for row in rows {
            let (version, stored_name, stored_checksum) = row?;
            let Some((_, expected_name, sql)) = MIGRATIONS
                .iter()
                .find(|(known_version, _, _)| *known_version == version)
            else {
                return Err(Error::Migration(format!(
                    "schema version {version} is newer than supported version {LATEST_MIGRATION_VERSION}"
                )));
            };
            let expected_checksum = migration_checksum(sql);
            if stored_name != *expected_name {
                return Err(Error::Migration(format!(
                    "migration {version} name mismatch: expected `{expected_name}`, found `{stored_name}`"
                )));
            }
            if let Some(stored_checksum) = stored_checksum
                && stored_checksum != expected_checksum
            {
                return Err(Error::Migration(format!(
                    "migration {version} checksum mismatch"
                )));
            }
        }
    }

    for &(version, name, sql) in MIGRATIONS {
        let applied = transaction
            .query_row(
                "SELECT name, checksum FROM schema_migrations WHERE version = ?1",
                [version],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .optional()?;
        let checksum = migration_checksum(sql);
        if let Some((stored_name, stored_checksum)) = applied {
            if stored_name != name {
                return Err(Error::Migration(format!(
                    "migration {version} name mismatch: expected `{name}`, found `{stored_name}`"
                )));
            }
            if stored_checksum.is_none() {
                transaction.execute(
                    "UPDATE schema_migrations SET checksum = ?2 WHERE version = ?1",
                    params![version, checksum],
                )?;
            }
            continue;
        }
        transaction.execute_batch(sql)?;
        if version == 8 {
            crate::memory::rebuild_compact_injections_in_transaction(&transaction)?;
        }
        transaction.execute(
            "INSERT INTO schema_migrations(version, name, checksum, applied_at_ms)
             VALUES (?1, ?2, ?3, unixepoch('subsec') * 1000)",
            params![version, name, checksum],
        )?;
    }
    transaction.commit()?;
    Ok(())
}

fn validate_applied_migrations_read_only(connection: &Connection) -> Result<()> {
    let mut statement = connection
        .prepare("SELECT version, name, checksum FROM schema_migrations ORDER BY version")?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
        ))
    })?;
    let mut applied = 0_i64;
    for row in rows {
        let (version, stored_name, stored_checksum) = row?;
        let Some((_, expected_name, sql)) = MIGRATIONS
            .iter()
            .find(|(known_version, _, _)| *known_version == version)
        else {
            return Err(Error::Migration(format!(
                "schema version {version} is newer than supported version {LATEST_MIGRATION_VERSION}"
            )));
        };
        if stored_name != *expected_name {
            return Err(Error::Migration(format!(
                "migration {version} name mismatch: expected `{expected_name}`, found `{stored_name}`"
            )));
        }
        let expected_checksum = migration_checksum(sql);
        if stored_checksum.as_deref() != Some(expected_checksum.as_str()) {
            return Err(Error::Migration(format!(
                "migration {version} checksum is missing or mismatched"
            )));
        }
        applied = version;
    }
    if applied == 0 {
        return Err(Error::Migration(
            "backup contains no applied Engram schema migrations".to_owned(),
        ));
    }
    Ok(())
}

fn migration_checksum(sql: &str) -> String {
    let digest = Sha256::digest(sql.as_bytes());
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

/// Apply a domain mutation and its projection in one immediate transaction.
pub(crate) fn immediate_transaction<T>(
    connection: &mut Connection,
    operation: impl FnOnce(&Transaction<'_>) -> Result<T>,
) -> Result<T> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let value = operation(&transaction)?;
    transaction.commit()?;
    Ok(value)
}

#[cfg(unix)]
pub(crate) fn restrict_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = fs::metadata(path)
        .map_err(|source| io_error(path, source))?
        .permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(path, permissions).map_err(|source| io_error(path, source))
}

#[cfg(not(unix))]
pub(crate) fn restrict_directory(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
pub(crate) fn restrict_file(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = fs::metadata(path)
        .map_err(|source| io_error(path, source))?
        .permissions();
    permissions.set_mode(0o600);
    fs::set_permissions(path, permissions).map_err(|source| io_error(path, source))
}

fn restrict_database_files(path: &Path) -> Result<()> {
    restrict_file(path)?;
    for suffix in ["-wal", "-shm"] {
        let mut sidecar = path.as_os_str().to_os_string();
        sidecar.push(suffix);
        let sidecar = PathBuf::from(sidecar);
        if sidecar.exists() {
            restrict_file(&sidecar)?;
        }
    }
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn restrict_file(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::memory::{MemoryDraft, MemoryStore};
    use crate::projects::ProjectRegistry;

    fn database() -> (TempDir, Database) {
        let temp = TempDir::new().expect("temp dir");
        let db = Database::open(temp.path().join("brain.sqlite")).expect("database");
        (temp, db)
    }

    #[test]
    fn test_wal_mode_enabled() {
        let (_temp, db) = database();
        assert_eq!(db.integrity_report().expect("report").journal_mode, "wal");
    }

    #[test]
    fn test_foreign_keys_enabled() {
        let (_temp, db) = database();
        assert!(db.integrity_report().expect("report").foreign_keys);
    }

    #[test]
    fn migration_16_preserves_old_contexts_and_synchronizes_owner_aliases() {
        let connection = Connection::open_in_memory().unwrap();
        for (_, _, sql) in &MIGRATIONS[..15] {
            connection.execute_batch(sql).unwrap();
        }
        connection.execute_batch(
            "INSERT INTO projects(id,name,created_at_ms) VALUES ('prj_one','one',0),('prj_two','two',0);
             INSERT INTO nodes(id,project_id,kind,stable_key,label,file_path,current_digest,source_kind,source_ref,extractor_version,observed_at_ms)
             VALUES ('owner','prj_one','Type','owner','Options','src/options.rs','d','native','head','v1',0);
             INSERT INTO context_runs(id,request_id,project_id,request_fingerprint,mode,task_text_digest,token_budget,selected_tokens,candidate_count,stable_prefix_hash,stable_prefix,dynamic_context,compile_ms,created_at_ms)
             VALUES ('old','old','prj_one','legacy','brain','d',100,1,1,'h','unchanged','saved output',0,0);"
        ).unwrap();
        connection.execute_batch(MIGRATIONS[15].2).unwrap();
        let old: (String, String, Option<String>) = connection.query_row(
            "SELECT stable_prefix,dynamic_context,delivery_digest FROM context_runs WHERE id='old'", [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).unwrap();
        assert_eq!(old, ("unchanged".into(), "saved output".into(), None));
        let count = |query: &str| -> i64 {
            connection
                .query_row(
                    "SELECT count(*) FROM source_node_fts WHERE source_node_fts MATCH ?1",
                    [query],
                    |row| row.get(0),
                )
                .unwrap()
        };
        assert_eq!(count("label:Options"), 1);
        connection.execute("UPDATE nodes SET search_aliases='enable_cache\nOptions::enable_cache' WHERE id='owner'", []).unwrap();
        assert_eq!(
            count("project_id:prjone AND search_aliases:\"enable_cache\""),
            1
        );
        connection.execute("UPDATE nodes SET search_aliases='disable_cache',project_id='prj_two' WHERE id='owner'", []).unwrap();
        assert_eq!(count("search_aliases:\"enable_cache\""), 0);
        assert_eq!(
            count("project_id:prjone AND search_aliases:\"disable_cache\""),
            0
        );
        assert_eq!(
            count("project_id:prjtwo AND search_aliases:\"disable_cache\""),
            1
        );
        for update in [
            "kind='ExternalReference'",
            "kind='Type',tombstoned_at_ms=1",
            "tombstoned_at_ms=NULL,file_path=NULL",
        ] {
            connection
                .execute(&format!("UPDATE nodes SET {update} WHERE id='owner'"), [])
                .unwrap();
            assert_eq!(count("search_aliases:\"disable_cache\""), 0);
        }
        connection
            .execute(
                "UPDATE nodes SET file_path='src/new.rs' WHERE id='owner'",
                [],
            )
            .unwrap();
        assert_eq!(count("search_aliases:\"disable_cache\""), 1);
        connection
            .execute("UPDATE nodes SET search_aliases='' WHERE id='owner'", [])
            .unwrap();
        assert_eq!(count("search_aliases:\"disable_cache\""), 0);
        connection
            .execute("DELETE FROM nodes WHERE id='owner'", [])
            .unwrap();
        assert_eq!(count("label:Options"), 0);
    }

    #[test]
    fn test_transaction_atomicity() {
        let (_temp, db) = database();
        let mut connection = db.connect().expect("connection");
        let result: Result<()> = immediate_transaction(&mut connection, |transaction| {
            transaction.execute(
                "INSERT INTO projects(id, name, created_at_ms) VALUES ('prj_test', 'test', 0)",
                [],
            )?;
            Err(Error::Config("force rollback".to_owned()))
        });
        assert!(result.is_err());
        let count: i64 = connection
            .query_row("SELECT COUNT(*) FROM projects", [], |row| row.get(0))
            .expect("count");
        assert_eq!(count, 0);
    }

    #[test]
    fn test_backup_snapshot_consistent() {
        let (temp, db) = database();
        let backup_path = temp.path().join("backup.sqlite");
        db.backup(&backup_path).expect("backup");
        assert_eq!(
            Database::verify_existing(&backup_path)
                .expect("verify backup")
                .integrity,
            "ok"
        );
        let missing = temp.path().join("missing.sqlite");
        assert!(Database::verify_existing(&missing).is_err());
        assert!(!missing.exists());
    }

    #[cfg(unix)]
    #[test]
    fn database_and_live_sidecars_are_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = TempDir::new().expect("temp");
        let shared = temp.path().join("shared");
        fs::create_dir(&shared).expect("shared");
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o755)).expect("shared mode");
        let path = shared.join("brain.sqlite");
        let database = Database::open(&path).expect("database");
        database
            .write_immediate(|transaction| {
                transaction.execute(
                    "INSERT INTO projects(id, name, created_at_ms)
                     VALUES ('prj_permissions', 'private text', 0)",
                    [],
                )?;
                Ok(())
            })
            .expect("write");
        for suffix in ["", "-wal", "-shm"] {
            let mut file = path.as_os_str().to_os_string();
            file.push(suffix);
            let file = PathBuf::from(file);
            assert!(
                file.exists(),
                "expected live SQLite file {}",
                file.display()
            );
            let mode = fs::metadata(&file).expect("metadata").permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "unsafe permissions on {}", file.display());
        }
    }

    #[cfg(unix)]
    #[test]
    fn opening_database_does_not_chmod_existing_parent() {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new().expect("temp dir");
        let shared = temp.path().join("shared");
        fs::create_dir(&shared).expect("shared dir");
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o755)).expect("permissions");
        Database::open(shared.join("brain.sqlite")).expect("database");
        let mode = fs::metadata(&shared)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o755);
    }

    #[test]
    fn future_schema_version_is_rejected() {
        let (temp, db) = database();
        db.connect()
            .expect("connection")
            .execute(
                "INSERT INTO schema_migrations(version, name, checksum, applied_at_ms)
                 VALUES (?1, 'future', 'unknown', 0)",
                [LATEST_MIGRATION_VERSION + 1],
            )
            .expect("future marker");
        drop(db);
        let error =
            Database::open(temp.path().join("brain.sqlite")).expect_err("future schema must fail");
        assert!(error.to_string().contains("newer than supported"));
    }

    #[test]
    fn migration_checksum_mismatch_is_rejected() {
        let (temp, db) = database();
        db.connect()
            .expect("connection")
            .execute(
                "UPDATE schema_migrations SET checksum = 'tampered' WHERE version = 1",
                [],
            )
            .expect("tamper checksum");
        drop(db);
        let error = Database::open(temp.path().join("brain.sqlite"))
            .expect_err("checksum mismatch must fail");
        assert!(error.to_string().contains("checksum mismatch"));
    }

    #[test]
    fn migration_8_backfills_existing_memories_transactionally() {
        let (temp, db) = database();
        let root = temp.path().join("migration-project");
        fs::create_dir(&root).expect("project directory");
        let project = ProjectRegistry::new(db.clone())
            .resolve_or_register(&root)
            .expect("project")
            .project
            .id;
        let memory = MemoryStore::new(db.clone())
            .create_candidate(
                &project,
                &MemoryDraft {
                    scope_type: "project".to_owned(),
                    scope_id: project.to_string(),
                    kind: "preference".to_owned(),
                    claim: "Keep CaseSensitive API names.\nNever flatten them.".to_owned(),
                    rationale: "Canonical rationale survives outside compact text.".to_owned(),
                    confidence: 0.8,
                    importance: 0.9,
                    tags: Vec::new(),
                    valid_from_commit: None,
                    valid_to_commit: None,
                    trust_source: "human_instruction".to_owned(),
                    evidence_ids: Vec::new(),
                    source_task_ids: Vec::new(),
                    node_ids: Vec::new(),
                },
            )
            .expect("pre-migration memory");
        let expected = db
            .with_reader(|connection| {
                connection
                    .query_row(
                        "SELECT renderer_version, source_digest, compact_text, token_estimate
                         FROM memory_compact_injections WHERE memory_id = ?1",
                        [memory.id.as_str()],
                        |row| {
                            Ok((
                                row.get::<_, i64>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, String>(2)?,
                                row.get::<_, i64>(3)?,
                            ))
                        },
                    )
                    .map_err(Error::from)
            })
            .expect("original projection");

        db.write_immediate(|transaction| {
            transaction.execute("DROP TABLE memory_compact_injections", [])?;
            transaction.execute("DELETE FROM schema_migrations WHERE version = 8", [])?;
            Ok(())
        })
        .expect("simulate a version-7 database");
        let path = db.path().to_path_buf();
        drop(db);

        let migrated = Database::open(&path).expect("apply migration 8");
        assert_eq!(
            migrated
                .integrity_report()
                .expect("integrity")
                .migration_version,
            LATEST_MIGRATION_VERSION
        );
        let actual = migrated
            .with_reader(|connection| {
                connection
                    .query_row(
                        "SELECT renderer_version, source_digest, compact_text, token_estimate
                         FROM memory_compact_injections WHERE memory_id = ?1",
                        [memory.id.as_str()],
                        |row| {
                            Ok((
                                row.get::<_, i64>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, String>(2)?,
                                row.get::<_, i64>(3)?,
                            ))
                        },
                    )
                    .map_err(Error::from)
            })
            .expect("backfilled projection");
        assert_eq!(actual, expected);
        assert_eq!(
            actual.2,
            "- PREF: Keep CaseSensitive API names. Never flatten them.\n"
        );
    }

    #[test]
    fn in_memory_database_is_rejected_explicitly() {
        let error = Database::open(":memory:").expect_err("in-memory is not canonical");
        assert!(error.to_string().contains("unsupported"));
    }

    #[test]
    fn zero_sized_read_pool_is_rejected() {
        let temp = TempDir::new().expect("temp dir");
        let error = Database::open_with_read_pool(temp.path().join("brain.sqlite"), 0)
            .expect_err("zero pool");
        assert!(error.to_string().contains("greater than zero"));
    }

    #[test]
    fn pooled_read_connection_is_reused() {
        let temp = TempDir::new().expect("temp dir");
        let db =
            Database::open_with_read_pool(temp.path().join("brain.sqlite"), 1).expect("database");
        db.with_reader(|connection| {
            connection.pragma_update(None, "cache_size", -1_234)?;
            Ok(())
        })
        .expect("set connection-local state");
        let value = db
            .with_reader(|connection| {
                connection
                    .query_row("PRAGMA cache_size", [], |row| row.get::<_, i64>(0))
                    .map_err(Error::from)
            })
            .expect("same pooled connection");
        assert_eq!(value, -1_234);
    }

    #[test]
    fn pooled_readers_cannot_bypass_the_canonical_writer() {
        let (_temp, db) = database();
        let result = db.with_reader(|connection| {
            connection.execute(
                "INSERT INTO projects(id, name, created_at_ms) VALUES ('prj_bad', 'bad', 0)",
                [],
            )?;
            Ok(())
        });
        assert!(result.is_err(), "read-pool connection accepted a mutation");
    }

    #[test]
    fn read_pool_applies_bounded_backpressure() {
        use std::sync::{Arc, Barrier, mpsc};
        use std::time::Duration;

        let temp = TempDir::new().expect("temp dir");
        let db = Arc::new(
            Database::open_with_read_pool(temp.path().join("brain.sqlite"), 1).expect("database"),
        );
        let barrier = Arc::new(Barrier::new(2));
        let first_db = Arc::clone(&db);
        let first_barrier = Arc::clone(&barrier);
        let (started_sender, started_receiver) = mpsc::sync_channel(1);
        let first = std::thread::spawn(move || {
            first_db
                .with_reader(|_| {
                    started_sender.send(()).expect("started");
                    first_barrier.wait();
                    Ok(())
                })
                .expect("first read");
        });
        started_receiver.recv().expect("first acquired");

        let second_db = Arc::clone(&db);
        let (acquired_sender, acquired_receiver) = mpsc::sync_channel(1);
        let second = std::thread::spawn(move || {
            second_db
                .with_reader(|_| {
                    acquired_sender.send(()).expect("acquired");
                    Ok(())
                })
                .expect("second read");
        });
        assert!(
            acquired_receiver
                .recv_timeout(Duration::from_millis(50))
                .is_err(),
            "second reader must wait for bounded capacity"
        );
        barrier.wait();
        acquired_receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("second reader eventually acquired");
        first.join().expect("first thread");
        second.join().expect("second thread");
    }

    #[test]
    fn read_pool_wait_has_a_bounded_timeout() {
        use std::sync::{Arc, Barrier, mpsc};

        let temp = TempDir::new().expect("temp dir");
        let db = Arc::new(
            Database::open_with_options(
                temp.path().join("brain.sqlite"),
                1,
                Duration::from_millis(50),
            )
            .expect("database"),
        );
        assert_eq!(
            db.read_pool_stats(),
            ReadPoolStats {
                capacity: 1,
                available: 1,
                in_use: 0,
            }
        );

        let release = Arc::new(Barrier::new(2));
        let held_db = Arc::clone(&db);
        let held_release = Arc::clone(&release);
        let (started_sender, started_receiver) = mpsc::sync_channel(1);
        let holder = std::thread::spawn(move || {
            held_db
                .with_reader(|_| {
                    started_sender.send(()).expect("started");
                    held_release.wait();
                    Ok(())
                })
                .expect("held read");
        });
        started_receiver.recv().expect("reader acquired");
        assert_eq!(db.read_pool_stats().in_use, 1);

        let started = Instant::now();
        let error = db.with_reader(|_| Ok(())).expect_err("must time out");
        assert!(matches!(error, Error::StorageReadTimeout));
        assert!(started.elapsed() >= Duration::from_millis(40));

        release.wait();
        holder.join().expect("holder thread");
        assert_eq!(db.read_pool_stats().available, 1);
    }

    #[test]
    fn zero_read_wait_timeout_is_rejected() {
        let temp = TempDir::new().expect("temp dir");
        let error =
            Database::open_with_options(temp.path().join("brain.sqlite"), 1, Duration::ZERO)
                .expect_err("zero timeout");
        assert!(error.to_string().contains("timeout"));
    }
}
