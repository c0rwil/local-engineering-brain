//! Natively indexed temporary repositories for hermetic tests.

use std::path::{Path, PathBuf};

use tempfile::TempDir;

use crate::ids::ProjectId;
use crate::projects::{ProjectRegistry, ResolvedProject};
use crate::storage::Database;
use crate::world::{TreeSitterProvider, WorldModel};

/// A synthetic contract suite: a settings helper and two tests acquire the
/// same lock directly; one more test reaches the lock through the helper.
pub const CONTRACT_SUITE: &str = "use std::sync::Mutex; use crate::settings::{load_settings, mode};

static TEST_ENV_LOCK: Mutex<()> = Mutex::new(());

fn settings_for_test() -> u32 {
    let _guard = TEST_ENV_LOCK.lock().unwrap();
    load_settings()
}

fn cluster_mode_keeps_its_own_url() {
    let _guard = TEST_ENV_LOCK.lock().unwrap();
    assert_eq!(mode(), 2);
}

fn hosted_mode_keeps_its_own_url() {
    let _guard = TEST_ENV_LOCK.lock().unwrap();
    assert_eq!(mode(), 3);
}

fn defaults_load_through_the_helper() {
    let loaded = settings_for_test();
    assert_eq!(loaded, 1);
}

fn unrelated_arithmetic() {
    assert_eq!(mode(), 1);
}
";

pub const SETTINGS: &str =
    "pub fn load_settings() -> u32 {\n    1\n}\n\npub fn mode() -> u32 {\n    1\n}\n";

/// A `src/` layout Python service: `app.services.sessions` defines the
/// anchors; another module defines a same-named `get_task`.
pub const PYTHON_SERVICE: &[(&str, &str)] = &[
    (
        "src/app/services/sessions.py",
        "class Store:\n    def load(self):\n        return 1\n\n    def refresh(self):\n        return self.load()\n\n    @classmethod\n    def build(cls):\n        return cls.load(cls())\n\n\ndef get_task(task_id):\n    return Store().load()\n",
    ),
    (
        "src/app/handlers.py",
        "from app.services import sessions as svc\nfrom app.services.sessions import get_task, Store\n\n\ndef status(task_id):\n    return svc.get_task(task_id)\n\n\ndef direct(task_id):\n    return get_task(task_id)\n\n\ndef build_store():\n    return Store.load(Store())\n",
    ),
    (
        "src/app/services/sibling.py",
        "from . import sessions\n\n\ndef nearby():\n    return sessions.get_task(1)\n",
    ),
    (
        "src/other/sessions.py",
        "def get_task(task_id):\n    return None\n",
    ),
    (
        "src/app/unrelated.py",
        "from other import sessions as svc\n\n\ndef wrong(task_id):\n    return svc.get_task(task_id)\n",
    ),
    (
        "src/app/guessing.py",
        "def unimported():\n    return sessions.get_task(1)\n\n\ndef also_unimported():\n    return get_task(2)\n",
    ),
];

pub struct IndexedRepo {
    _temp: TempDir,
    pub root: PathBuf,
    pub database: Database,
    pub project: ResolvedProject,
}

impl IndexedRepo {
    /// Write synthetic files into a new checkout and index it natively.
    pub fn new(files: &[(&str, &str)]) -> Self {
        let temp = TempDir::new().expect("temp dir");
        let root = temp.path().join("repo");
        std::fs::create_dir(&root).expect("repo");
        for (path, content) in files {
            write(&root, path, content);
        }
        let database = Database::open(temp.path().join("brain.sqlite")).expect("database");
        let project = ProjectRegistry::new(database.clone())
            .resolve_or_register(&root)
            .expect("project");
        let repo = Self {
            _temp: temp,
            root,
            database,
            project,
        };
        repo.reindex();
        repo
    }

    pub fn id(&self) -> &ProjectId {
        &self.project.project.id
    }

    pub fn write(&self, path: &str, content: &str) {
        write(&self.root, path, content);
    }

    /// Index every file again, as a refresh after a commit would.
    pub fn reindex(&self) {
        WorldModel::new(self.database.clone())
            .index(
                &TreeSitterProvider::new(1_000, 1_000_000, 100_000, 100_000, 64_000_000),
                &self.project,
                None,
            )
            .expect("index");
    }

    /// The single active definition, section or file node with this label.
    pub fn node(&self, label: &str) -> String {
        self.database
            .with_reader(|connection| {
                Ok(connection.query_row(
                    "SELECT id FROM nodes WHERE project_id=?1 AND label=?2
                       AND kind<>'ExternalReference' AND tombstoned_at_ms IS NULL",
                    [self.id().as_str(), label],
                    |row| row.get::<_, String>(0),
                )?)
            })
            .unwrap_or_else(|error| panic!("node {label}: {error}"))
    }

    /// The single active non-reference node with this label in one file.
    pub fn node_in(&self, label: &str, path: &str) -> String {
        self.database
            .with_reader(|connection| {
                Ok(connection.query_row(
                    "SELECT id FROM nodes WHERE project_id=?1 AND label=?2 AND file_path=?3
                       AND kind<>'ExternalReference' AND tombstoned_at_ms IS NULL",
                    [self.id().as_str(), label, path],
                    |row| row.get::<_, String>(0),
                )?)
            })
            .unwrap_or_else(|error| panic!("node {label} in {path}: {error}"))
    }
}

fn write(root: &Path, path: &str, content: &str) {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("directories");
    std::fs::write(path, content).expect("file");
}
