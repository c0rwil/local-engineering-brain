//! Bounded importer for normalized Graphify JSON exports.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::BufReader;
use std::path::{Component, Path, PathBuf};

use serde::de::{DeserializeSeed, Error as DeError, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use sha2::{Digest, Sha256};

use crate::error::{Error, Result, io_error};

use super::structural::{
    FileScan, ScanResult, ScannedEdge, ScannedNode, StructuralProvider, file_scan_heap_bytes,
};

const PROVIDER_VERSION: &str = "graphify-json-v1";
const GLOBAL_PATH: &str = ".graphify/global";

/// Imports an explicit, offline Graphify JSON export into LEB's canonical graph.
///
/// Graphify remains an optional producer. The imported representation is
/// validated and bounded before it reaches the canonical write transaction.
#[derive(Clone, Debug)]
pub struct GraphifyProvider {
    export_path: PathBuf,
    max_nodes: usize,
    max_edges: usize,
    max_export_bytes: u64,
    max_prepared_bytes: u64,
}

impl GraphifyProvider {
    #[must_use]
    pub const fn new(
        export_path: PathBuf,
        max_nodes: usize,
        max_edges: usize,
        max_export_bytes: u64,
        max_prepared_bytes: u64,
    ) -> Self {
        Self {
            export_path,
            max_nodes,
            max_edges,
            max_export_bytes,
            max_prepared_bytes,
        }
    }
}

impl StructuralProvider for GraphifyProvider {
    fn name(&self) -> &'static str {
        "graphify"
    }

    fn version(&self) -> &'static str {
        PROVIDER_VERSION
    }

    // Import validation intentionally stays linear so all externally supplied
    // fields are checked before any canonical mutation is prepared.
    #[allow(clippy::too_many_lines)]
    fn scan(
        &self,
        root: &Path,
        changed_paths: Option<&[PathBuf]>,
        known_digests: &BTreeMap<String, String>,
    ) -> Result<ScanResult> {
        if changed_paths.is_some() {
            return Err(Error::Structural(
                "Graphify imports are whole-export snapshots, not changed-file scans".to_owned(),
            ));
        }
        let root = root
            .canonicalize()
            .map_err(|source| io_error(root, source))?;
        let metadata = fs::metadata(&self.export_path)
            .map_err(|source| io_error(&self.export_path, source))?;
        if metadata.len() > self.max_export_bytes {
            return Err(Error::Structural(format!(
                "Graphify export exceeds {} byte limit: {}",
                self.max_export_bytes,
                self.export_path.display()
            )));
        }
        let file =
            File::open(&self.export_path).map_err(|source| io_error(&self.export_path, source))?;
        let mut deserializer = serde_json::Deserializer::from_reader(BufReader::new(file));
        let export = GraphifyExportSeed {
            nodes: self.max_nodes,
            edges: self.max_edges,
            raw_bytes: self.max_prepared_bytes / 2,
        }
        .deserialize(&mut deserializer)?;
        deserializer.end()?;
        let raw_heap_bytes = export.estimated_heap_bytes();

        let mut nodes_by_id = BTreeMap::new();
        let mut file_nodes = BTreeMap::<String, Vec<ScannedNode>>::new();
        for node in export.nodes {
            validate_text("node id", &node.id, 512)?;
            validate_text("node kind", &node.kind, 128)?;
            validate_text("node label", &node.label, 4_096)?;
            if nodes_by_id.contains_key(&node.id) {
                return Err(Error::Structural(format!(
                    "duplicate Graphify node id `{}`",
                    node.id
                )));
            }
            let path = node.file_path.as_deref().map_or_else(
                || Ok(GLOBAL_PATH.to_owned()),
                |path| safe_relative(&root, path),
            )?;
            let stable_key = format!("graphify:{}", node.id);
            let digest = node.digest.unwrap_or_else(|| {
                sha256(
                    format!(
                        "{}\0{}\0{}\0{}\0{:?}\0{:?}",
                        node.id, node.kind, node.label, path, node.start_byte, node.end_byte
                    )
                    .as_bytes(),
                )
            });
            let scanned = ScannedNode {
                stable_key: stable_key.clone(),
                kind: node.kind,
                label: node.label,
                file_path: path.clone(),
                language: node.language.unwrap_or_else(|| "unknown".to_owned()),
                start_byte: node.start_byte.unwrap_or(0),
                end_byte: node.end_byte.unwrap_or(0),
                digest,
            };
            nodes_by_id.insert(node.id, (stable_key, path.clone()));
            file_nodes.entry(path).or_default().push(scanned);
        }

        let mut file_edges = BTreeMap::<String, Vec<ScannedEdge>>::new();
        let mut unique_edges = BTreeSet::new();
        for edge in export.edges {
            validate_text("edge relation", &edge.relation, 128)?;
            if !edge.confidence.is_finite() || !(0.0..=1.0).contains(&edge.confidence) {
                return Err(Error::Structural(
                    "Graphify edge confidence must be finite and in 0..=1".to_owned(),
                ));
            }
            let (source_key, source_path) = nodes_by_id.get(&edge.source).ok_or_else(|| {
                Error::Structural(format!(
                    "Graphify edge has unknown source `{}`",
                    edge.source
                ))
            })?;
            let (target_key, _) = nodes_by_id.get(&edge.target).ok_or_else(|| {
                Error::Structural(format!(
                    "Graphify edge has unknown target `{}`",
                    edge.target
                ))
            })?;
            let relation = edge.relation.trim().to_ascii_uppercase();
            let edge_key = format!("{source_key}\0{relation}\0{target_key}");
            if !unique_edges.insert(edge_key.clone()) {
                continue;
            }
            file_edges
                .entry(source_path.clone())
                .or_default()
                .push(ScannedEdge {
                    source_key: source_key.clone(),
                    relation,
                    target_key: target_key.clone(),
                    confidence: edge.confidence,
                    source_ref: format!("graphify:{}", self.export_path.display()),
                    digest: sha256(edge_key.as_bytes()),
                });
        }

        let mut seen_paths = BTreeSet::new();
        let mut files = Vec::new();
        let mut unchanged_count = 0;
        for (path, mut nodes) in file_nodes {
            nodes.sort_by(|left, right| left.stable_key.cmp(&right.stable_key));
            let mut edges = file_edges.remove(&path).unwrap_or_default();
            edges.sort_by(|left, right| {
                (&left.source_key, &left.relation, &left.target_key).cmp(&(
                    &right.source_key,
                    &right.relation,
                    &right.target_key,
                ))
            });
            let digest = file_digest(&nodes, &edges);
            seen_paths.insert(path.clone());
            if known_digests.get(&path) == Some(&digest) {
                unchanged_count += 1;
                continue;
            }
            let language = common_language(&nodes);
            files.push(FileScan {
                path,
                language,
                digest,
                size_bytes: 0,
                nodes,
                edges,
            });
        }
        if !file_edges.is_empty() {
            return Err(Error::Structural(
                "Graphify edge source was not assigned to a file".to_owned(),
            ));
        }
        let prepared_bytes = files.iter().fold(0_u64, |total, file| {
            total.saturating_add(file_scan_heap_bytes(file))
        });
        if prepared_bytes.saturating_add(raw_heap_bytes) > self.max_prepared_bytes {
            return Err(Error::Structural(format!(
                "Graphify normalized scan exceeds {} byte prepared-data limit",
                self.max_prepared_bytes
            )));
        }
        Ok(ScanResult {
            provider: self.name().to_owned(),
            provider_version: export.exporter_version.map_or_else(
                || self.version().to_owned(),
                |value| format!("graphify:{value}"),
            ),
            incremental: false,
            seen_paths,
            deleted_paths: BTreeSet::new(),
            unchanged_count,
            files,
        })
    }
}

#[derive(Debug)]
struct GraphifyExport {
    exporter_version: Option<String>,
    nodes: Vec<GraphifyNode>,
    edges: Vec<GraphifyEdge>,
}

impl GraphifyExport {
    fn estimated_heap_bytes(&self) -> u64 {
        self.nodes
            .iter()
            .fold(0_u64, |total, node| {
                total.saturating_add(node.estimated_heap_bytes())
            })
            .saturating_add(self.edges.iter().fold(0_u64, |total, edge| {
                total.saturating_add(edge.estimated_heap_bytes())
            }))
            .saturating_add(
                self.exporter_version
                    .as_ref()
                    .map_or(0, |value| usize_to_u64(value.capacity())),
            )
    }
}

struct GraphifyExportSeed {
    nodes: usize,
    edges: usize,
    raw_bytes: u64,
}

impl<'de> DeserializeSeed<'de> for GraphifyExportSeed {
    type Value = GraphifyExport;

    fn deserialize<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(GraphifyExportVisitor { limits: self })
    }
}

struct GraphifyExportVisitor {
    limits: GraphifyExportSeed,
}

impl<'de> Visitor<'de> for GraphifyExportVisitor {
    type Value = GraphifyExport;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a bounded Graphify export object")
    }

    fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut exporter_version = None;
        let mut nodes = None;
        let mut edges = None;
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "exporter_version" => {
                    if exporter_version.is_some() {
                        return Err(A::Error::duplicate_field("exporter_version"));
                    }
                    exporter_version = Some(map.next_value::<String>()?);
                }
                "nodes" => {
                    if nodes.is_some() {
                        return Err(A::Error::duplicate_field("nodes"));
                    }
                    nodes = Some(map.next_value_seed(BoundedVecSeed::<GraphifyNode>::new(
                        self.limits.nodes,
                        self.limits.raw_bytes,
                        "nodes",
                    ))?);
                }
                "edges" => {
                    if edges.is_some() {
                        return Err(A::Error::duplicate_field("edges"));
                    }
                    edges = Some(map.next_value_seed(BoundedVecSeed::<GraphifyEdge>::new(
                        self.limits.edges,
                        self.limits.raw_bytes,
                        "edges",
                    ))?);
                }
                _ => {
                    return Err(A::Error::unknown_field(
                        &key,
                        &["exporter_version", "nodes", "edges"],
                    ));
                }
            }
        }
        let export = GraphifyExport {
            exporter_version,
            nodes: nodes.ok_or_else(|| A::Error::missing_field("nodes"))?,
            edges: edges.ok_or_else(|| A::Error::missing_field("edges"))?,
        };
        if export.estimated_heap_bytes() > self.limits.raw_bytes {
            return Err(A::Error::custom(
                "Graphify raw data exceeds its prepared-memory share",
            ));
        }
        Ok(export)
    }
}

trait EstimatedHeap {
    fn estimated_heap_bytes(&self) -> u64;
}

struct BoundedVecSeed<T> {
    max_items: usize,
    max_bytes: u64,
    label: &'static str,
    marker: std::marker::PhantomData<T>,
}

impl<T> BoundedVecSeed<T> {
    const fn new(max_items: usize, max_bytes: u64, label: &'static str) -> Self {
        Self {
            max_items,
            max_bytes,
            label,
            marker: std::marker::PhantomData,
        }
    }
}

impl<'de, T> DeserializeSeed<'de> for BoundedVecSeed<T>
where
    T: Deserialize<'de> + EstimatedHeap,
{
    type Value = Vec<T>;

    fn deserialize<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_seq(BoundedVecVisitor { seed: self })
    }
}

struct BoundedVecVisitor<T> {
    seed: BoundedVecSeed<T>,
}

impl<'de, T> Visitor<'de> for BoundedVecVisitor<T>
where
    T: Deserialize<'de> + EstimatedHeap,
{
    type Value = Vec<T>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "a bounded Graphify {} array", self.seed.label)
    }

    fn visit_seq<A>(self, mut sequence: A) -> std::result::Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        let mut bytes = 0_u64;
        while let Some(value) = sequence.next_element::<T>()? {
            if values.len() >= self.seed.max_items {
                return Err(A::Error::custom(format_args!(
                    "Graphify {} exceed the {} item limit",
                    self.seed.label, self.seed.max_items
                )));
            }
            bytes = bytes.saturating_add(value.estimated_heap_bytes());
            if bytes > self.seed.max_bytes {
                return Err(A::Error::custom(format_args!(
                    "Graphify {} exceed the {} byte raw-memory limit",
                    self.seed.label, self.seed.max_bytes
                )));
            }
            values.push(value);
        }
        Ok(values)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GraphifyNode {
    id: String,
    kind: String,
    #[serde(alias = "name")]
    label: String,
    #[serde(default, alias = "path")]
    file_path: Option<String>,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    start_byte: Option<u64>,
    #[serde(default)]
    end_byte: Option<u64>,
    #[serde(default)]
    digest: Option<String>,
}

impl EstimatedHeap for GraphifyNode {
    fn estimated_heap_bytes(&self) -> u64 {
        usize_to_u64(std::mem::size_of::<Self>())
            .saturating_add(usize_to_u64(self.id.capacity()))
            .saturating_add(usize_to_u64(self.kind.capacity()))
            .saturating_add(usize_to_u64(self.label.capacity()))
            .saturating_add(
                self.file_path
                    .as_ref()
                    .map_or(0, |value| usize_to_u64(value.capacity())),
            )
            .saturating_add(
                self.language
                    .as_ref()
                    .map_or(0, |value| usize_to_u64(value.capacity())),
            )
            .saturating_add(
                self.digest
                    .as_ref()
                    .map_or(0, |value| usize_to_u64(value.capacity())),
            )
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GraphifyEdge {
    source: String,
    target: String,
    relation: String,
    #[serde(default = "default_confidence")]
    confidence: f64,
}

impl EstimatedHeap for GraphifyEdge {
    fn estimated_heap_bytes(&self) -> u64 {
        usize_to_u64(std::mem::size_of::<Self>())
            .saturating_add(usize_to_u64(self.source.capacity()))
            .saturating_add(usize_to_u64(self.target.capacity()))
            .saturating_add(usize_to_u64(self.relation.capacity()))
    }
}

fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

const fn default_confidence() -> f64 {
    1.0
}

fn validate_text(field: &str, value: &str, max_bytes: usize) -> Result<()> {
    if value.trim().is_empty() || value.len() > max_bytes || value.contains('\0') {
        return Err(Error::Structural(format!(
            "Graphify {field} must be non-empty, NUL-free, and at most {max_bytes} bytes"
        )));
    }
    Ok(())
}

fn safe_relative(root: &Path, value: &str) -> Result<String> {
    validate_text("node path", value, 8_192)?;
    let path = Path::new(value);
    if path.is_absolute()
        || path.components().any(|part| {
            matches!(
                part,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(Error::Structural(format!(
            "Graphify node path escapes project root: {value}"
        )));
    }
    let joined = root.join(path);
    if !joined.starts_with(root) {
        return Err(Error::Structural(format!(
            "Graphify node path escapes project root: {value}"
        )));
    }
    Ok(path.to_string_lossy().replace('\\', "/"))
}

fn common_language(nodes: &[ScannedNode]) -> String {
    nodes
        .first()
        .map_or_else(|| "unknown".to_owned(), |node| node.language.clone())
}

fn file_digest(nodes: &[ScannedNode], edges: &[ScannedEdge]) -> String {
    let mut hasher = Sha256::new();
    for node in nodes {
        hasher.update(node.stable_key.as_bytes());
        hasher.update(node.digest.as_bytes());
    }
    for edge in edges {
        hasher.update(edge.digest.as_bytes());
    }
    hex_digest(hasher.finalize())
}

fn sha256(bytes: &[u8]) -> String {
    hex_digest(Sha256::digest(bytes))
}

fn hex_digest(bytes: impl AsRef<[u8]>) -> String {
    let bytes = bytes.as_ref();
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::projects::ProjectRegistry;
    use crate::storage::Database;
    use crate::world::WorldModel;

    #[test]
    fn imports_cross_file_nodes_and_edges_incrementally() {
        let temp = TempDir::new().expect("temp");
        let root = temp.path().join("repo");
        fs::create_dir(&root).expect("root");
        let export = temp.path().join("graphify.json");
        fs::write(
            &export,
            r#"{
              "exporter_version":"1.2.3",
              "nodes":[
                {"id":"a","kind":"Function","name":"caller","path":"src/a.rs","language":"rust"},
                {"id":"b","kind":"Function","name":"callee","path":"src/b.rs","language":"rust"}
              ],
              "edges":[{"source":"a","target":"b","relation":"calls","confidence":0.99}]
            }"#,
        )
        .expect("export");
        let database = Database::open(temp.path().join("brain.sqlite")).expect("database");
        let project = ProjectRegistry::new(database.clone())
            .resolve_or_register(&root)
            .expect("project");
        let world = WorldModel::new(database.clone());
        let provider = GraphifyProvider::new(export, 100, 100, 1_000_000, 1_000_000);
        let first = world
            .index(&provider, &project, None)
            .expect("first import");
        assert_eq!(first.nodes_written, 2);
        assert_eq!(first.edges_written, 1);
        let caller = world
            .search_symbols(&project.project.id, "caller", 10)
            .expect("caller");
        let slice = world
            .neighbors(&project.project.id, &caller[0].id, 1, 10)
            .expect("neighbors");
        assert_eq!(slice.nodes.len(), 2);
        assert_eq!(slice.edges[0].relation, "CALLS");
        assert_eq!(slice.nodes[0].source_kind, "import");

        let second = world.index(&provider, &project, None).expect("unchanged");
        assert_eq!(second.parsed_files, 0);
        assert_eq!(second.unchanged_files, 2);

        fs::write(
            &provider.export_path,
            r#"{
              "exporter_version":"1.2.3",
              "nodes":[
                {"id":"a","kind":"Function","name":"caller","path":"src/a.rs","language":"rust"},
                {"id":"b","kind":"Function","name":"renamed_callee","path":"src/b.rs","language":"rust"}
              ],
              "edges":[{"source":"a","target":"b","relation":"calls","confidence":0.99}]
            }"#,
        )
        .expect("change only target node");
        let target_refresh = world
            .index(&provider, &project, None)
            .expect("target refresh");
        assert_eq!(target_refresh.parsed_files, 1);
        let slice = world
            .neighbors(&project.project.id, &caller[0].id, 1, 10)
            .expect("neighbors after target refresh");
        assert_eq!(slice.edges.len(), 1);
        assert!(
            slice
                .nodes
                .iter()
                .any(|node| node.label == "renamed_callee")
        );
        let event_count = database
            .with_reader(|connection| {
                connection
                    .query_row(
                        "SELECT COUNT(*) FROM events
                         WHERE project_id = ?1 AND event_type = 'STRUCTURAL_SCAN_APPLIED'",
                        [project.project.id.as_str()],
                        |row| row.get::<_, i64>(0),
                    )
                    .map_err(Error::from)
            })
            .expect("events");
        assert_eq!(event_count, 3);
    }

    #[test]
    fn rejects_escaping_paths_and_dangling_edges() {
        let temp = TempDir::new().expect("temp");
        let root = temp.path().join("repo");
        fs::create_dir(&root).expect("root");
        let export = temp.path().join("bad.json");
        fs::write(
            &export,
            r#"{"nodes":[{"id":"a","kind":"File","label":"x","path":"../secret"}],"edges":[]}"#,
        )
        .expect("export");
        let provider = GraphifyProvider::new(export, 10, 10, 1_000_000, 1_000_000);
        let error = provider
            .scan(&root, None, &BTreeMap::new())
            .expect_err("escape rejected");
        assert!(error.to_string().contains("escapes"));
    }
}
