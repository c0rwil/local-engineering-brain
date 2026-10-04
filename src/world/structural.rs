use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use ignore::WalkBuilder;
use rusqlite::{OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tree_sitter::{Language, Node, Parser};
use uuid::Uuid;

use crate::error::{Error, Result, io_error};
use crate::events::{DomainEvent, append_generated_in_transaction};
use crate::ids::{EdgeId, NodeId, ProjectId};
use crate::projects::ResolvedProject;
use crate::storage::Database;

const EXTRACTOR_VERSION: &str = "tree-sitter-v2";

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct GraphNode {
    pub id: NodeId,
    pub project_id: ProjectId,
    pub kind: String,
    pub stable_key: String,
    pub label: String,
    pub file_path: Option<String>,
    pub language: Option<String>,
    pub start_byte: Option<u64>,
    pub end_byte: Option<u64>,
    pub current_digest: String,
    pub source_kind: String,
    pub source_ref: String,
    pub extractor_version: String,
    pub observed_at_ms: i64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct GraphEdge {
    pub id: EdgeId,
    pub project_id: ProjectId,
    pub source_id: NodeId,
    pub relation: String,
    pub target_id: NodeId,
    pub confidence: f64,
    pub source_kind: String,
    pub source_ref: String,
    pub content_digest: String,
    pub extractor_version: String,
    pub observed_at_ms: i64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct GraphSlice {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
    pub truncated: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct IndexReport {
    pub scan_id: String,
    pub provider: String,
    pub incremental: bool,
    pub discovered_files: u64,
    pub parsed_files: u64,
    pub unchanged_files: u64,
    pub removed_files: u64,
    pub nodes_written: u64,
    pub edges_written: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct StructuralIndexStatus {
    pub scan_id: String,
    pub provider: String,
    pub head: Option<String>,
    pub diff_fingerprint: String,
    pub completed_at_ms: i64,
    /// Working-tree paths that were dirty when this scan committed. `None`
    /// means the set is unknown and freshness planning must rescan fully.
    pub changed_paths: Option<Vec<PathBuf>>,
    /// Native files from an older extractor require one full candidate scan.
    #[serde(default)]
    pub needs_reindex: bool,
}

#[derive(Clone, Debug)]
pub struct PreparedIndex {
    project_id: ProjectId,
    scan: ScanResult,
}

#[derive(Clone, Debug)]
pub struct ScannedNode {
    pub stable_key: String,
    pub kind: String,
    pub label: String,
    /// Sorted identifier aliases, separated by newlines; never source bodies.
    pub search_aliases: String,
    pub file_path: String,
    pub language: String,
    pub start_byte: u64,
    pub end_byte: u64,
    pub digest: String,
}

#[derive(Clone, Debug)]
pub struct ScannedEdge {
    pub source_key: String,
    pub relation: String,
    pub target_key: String,
    pub confidence: f64,
    pub source_ref: String,
    pub digest: String,
}

#[derive(Clone, Debug)]
pub struct FileScan {
    pub path: String,
    pub language: String,
    pub digest: String,
    pub size_bytes: u64,
    pub nodes: Vec<ScannedNode>,
    pub edges: Vec<ScannedEdge>,
}

#[derive(Clone, Debug)]
pub struct ScanResult {
    pub provider: String,
    pub provider_version: String,
    pub incremental: bool,
    pub seen_paths: BTreeSet<String>,
    pub deleted_paths: BTreeSet<String>,
    pub unchanged_count: usize,
    pub files: Vec<FileScan>,
}

/// Contract implemented by native and external structural analyzers.
pub trait StructuralProvider: Send + Sync {
    fn name(&self) -> &'static str;
    fn version(&self) -> &'static str;

    /// Scan supported source files, skipping content whose digest is unchanged.
    ///
    /// # Errors
    ///
    /// Returns an error for unsafe paths, I/O failures, parser failures, or
    /// configured resource-limit violations.
    fn scan(
        &self,
        root: &Path,
        changed_paths: Option<&[PathBuf]>,
        known_digests: &BTreeMap<String, String>,
    ) -> Result<ScanResult>;
}

/// Native structural provider for Rust, Python, JavaScript, and TypeScript.
#[derive(Clone, Debug)]
pub struct TreeSitterProvider {
    file_capacity: usize,
    max_file_bytes: u64,
    node_capacity: usize,
    max_edges: usize,
    prepared_budget_bytes: u64,
}

impl TreeSitterProvider {
    #[must_use]
    pub const fn new(
        max_files: usize,
        max_file_bytes: u64,
        max_nodes: usize,
        max_edges: usize,
        max_prepared_bytes: u64,
    ) -> Self {
        Self {
            file_capacity: max_files,
            max_file_bytes,
            node_capacity: max_nodes,
            max_edges,
            prepared_budget_bytes: max_prepared_bytes,
        }
    }
}

impl StructuralProvider for TreeSitterProvider {
    fn name(&self) -> &'static str {
        "tree_sitter"
    }

    fn version(&self) -> &'static str {
        EXTRACTOR_VERSION
    }

    fn scan(
        &self,
        root: &Path,
        changed_paths: Option<&[PathBuf]>,
        known_digests: &BTreeMap<String, String>,
    ) -> Result<ScanResult> {
        let root = root
            .canonicalize()
            .map_err(|source| io_error(root, source))?;
        let incremental = changed_paths.is_some();
        let candidates = candidate_paths(&root, changed_paths, self.file_capacity)?;
        let mut result = ScanResult {
            provider: self.name().to_owned(),
            provider_version: self.version().to_owned(),
            incremental,
            seen_paths: BTreeSet::new(),
            deleted_paths: BTreeSet::new(),
            unchanged_count: 0,
            files: Vec::new(),
        };
        let mut prepared_bytes = 0_u64;
        let mut node_count = 0_usize;
        let mut edge_count = 0_usize;

        for candidate in candidates {
            let relative = relative_utf8(&root, &candidate)?;
            let Some(language) = SupportedLanguage::from_path(&candidate) else {
                continue;
            };
            if !candidate.exists() {
                result.deleted_paths.insert(relative);
                continue;
            }
            let metadata =
                fs::symlink_metadata(&candidate).map_err(|source| io_error(&candidate, source))?;
            if !metadata.file_type().is_file() {
                continue;
            }
            if metadata.len() > self.max_file_bytes {
                return Err(Error::Structural(format!(
                    "source file exceeds {} byte limit: {}",
                    self.max_file_bytes,
                    candidate.display()
                )));
            }
            result.seen_paths.insert(relative.clone());
            let source = fs::read(&candidate).map_err(|error| io_error(&candidate, error))?;
            let digest = sha256(&source);
            if known_digests.get(&relative) == Some(&digest) {
                result.unchanged_count += 1;
                continue;
            }
            let parsed = parse_file(&relative, language, &source, digest)?;
            node_count = node_count.saturating_add(parsed.nodes.len());
            edge_count = edge_count.saturating_add(parsed.edges.len());
            prepared_bytes = prepared_bytes.saturating_add(file_scan_heap_bytes(&parsed));
            if node_count > self.node_capacity
                || edge_count > self.max_edges
                || prepared_bytes > self.prepared_budget_bytes
            {
                return Err(Error::Structural(format!(
                    "native scan exceeds prepared limits ({node_count} nodes/{}, {edge_count} edges/{}, {prepared_bytes} bytes/{})",
                    self.node_capacity, self.max_edges, self.prepared_budget_bytes
                )));
            }
            result.files.push(parsed);
        }
        Ok(result)
    }
}

pub(super) fn file_scan_heap_bytes(file: &FileScan) -> u64 {
    let file_fields = file
        .path
        .len()
        .saturating_add(file.language.len())
        .saturating_add(file.digest.len());
    let node_bytes = file.nodes.iter().fold(0_usize, |total, node| {
        total
            .saturating_add(std::mem::size_of::<ScannedNode>())
            .saturating_add(node.stable_key.len())
            .saturating_add(node.kind.len())
            .saturating_add(node.label.len())
            .saturating_add(node.search_aliases.len())
            .saturating_add(node.file_path.len())
            .saturating_add(node.language.len())
            .saturating_add(node.digest.len())
    });
    let edge_bytes = file.edges.iter().fold(0_usize, |total, edge| {
        total
            .saturating_add(std::mem::size_of::<ScannedEdge>())
            .saturating_add(edge.source_key.len())
            .saturating_add(edge.relation.len())
            .saturating_add(edge.target_key.len())
            .saturating_add(edge.source_ref.len())
            .saturating_add(edge.digest.len())
    });
    u64::try_from(
        file_fields
            .saturating_add(node_bytes)
            .saturating_add(edge_bytes),
    )
    .unwrap_or(u64::MAX)
}

#[derive(Clone, Debug)]
pub struct WorldModel {
    database: Database,
}

impl WorldModel {
    #[must_use]
    pub const fn new(database: Database) -> Self {
        Self { database }
    }

    #[must_use]
    pub const fn database(&self) -> &Database {
        &self.database
    }

    /// Return the most recently committed structural snapshot for a project.
    ///
    /// # Errors
    ///
    /// Returns an error for storage failures.
    pub fn latest_index_status(
        &self,
        project_id: &ProjectId,
    ) -> Result<Option<StructuralIndexStatus>> {
        self.database.with_reader(|connection| {
            connection
                .query_row(
                    "SELECT id, provider, head, diff_fingerprint, completed_at_ms,
                            changed_paths_json,
                            provider = 'tree_sitter' AND EXISTS (
                                SELECT 1 FROM indexed_files f
                                JOIN structural_scans s ON s.id = f.scan_id
                                WHERE f.project_id = ?1
                                  AND (s.provider <> 'tree_sitter' OR s.provider_version <> ?2)
                            )
                     FROM structural_scans WHERE project_id = ?1
                     ORDER BY completed_at_ms DESC, rowid DESC LIMIT 1",
                    params![project_id.as_str(), EXTRACTOR_VERSION],
                    |row| {
                        let changed_paths_json: Option<String> = row.get(5)?;
                        Ok(StructuralIndexStatus {
                            scan_id: row.get(0)?,
                            provider: row.get(1)?,
                            head: row.get(2)?,
                            diff_fingerprint: row.get(3)?,
                            completed_at_ms: row.get(4)?,
                            changed_paths: changed_paths_json
                                .as_deref()
                                .and_then(parse_changed_paths_json),
                            needs_reindex: row.get(6)?,
                        })
                    },
                )
                .optional()
                .map_err(Error::from)
        })
    }

    /// Incrementally update the canonical graph from a structural provider.
    ///
    /// Callers route this method through the daemon mutation scheduler.
    ///
    /// # Errors
    ///
    /// Returns an error when scanning, graph validation, or the atomic storage
    /// transaction fails.
    pub fn index(
        &self,
        provider: &dyn StructuralProvider,
        project: &ResolvedProject,
        changed_paths: Option<&[PathBuf]>,
    ) -> Result<IndexReport> {
        let prepared = self.prepare_index(provider, project, changed_paths)?;
        self.apply_prepared(project, prepared)
    }

    /// Perform filesystem reads and parsing without entering a write path.
    ///
    /// # Errors
    ///
    /// Returns an error for provider, filesystem, or digest lookup failures.
    pub fn prepare_index(
        &self,
        provider: &dyn StructuralProvider,
        project: &ResolvedProject,
        changed_paths: Option<&[PathBuf]>,
    ) -> Result<PreparedIndex> {
        let (known, needs_reindex) = self.known_digests(&project.project.id, provider)?;
        let changed_paths = if needs_reindex { None } else { changed_paths };
        let scan = provider.scan(&project.root, changed_paths, &known)?;
        Ok(PreparedIndex {
            project_id: project.project.id.clone(),
            scan,
        })
    }

    /// Atomically install a prepared scan and its audit event.
    ///
    /// # Errors
    ///
    /// Returns an error for scope mismatch, graph validation, or storage failure.
    pub fn apply_prepared(
        &self,
        project: &ResolvedProject,
        prepared: PreparedIndex,
    ) -> Result<IndexReport> {
        if prepared.project_id != project.project.id {
            return Err(Error::Structural(
                "prepared scan belongs to another project".to_owned(),
            ));
        }
        self.apply_scan(project, prepared.scan)
    }

    /// Find active symbols by exact/prefix/substring label, deterministically.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid limits, IDs, or storage failures.
    pub fn search_symbols(
        &self,
        project_id: &ProjectId,
        query: &str,
        limit: usize,
    ) -> Result<Vec<GraphNode>> {
        if limit == 0 || limit > 1_000 {
            return Err(Error::Config(
                "graph search limit must be between 1 and 1000".to_owned(),
            ));
        }
        let pattern = format!("%{}%", escape_like(query));
        self.database.with_reader(|connection| {
            let mut statement = connection.prepare(
                "SELECT id, project_id, kind, stable_key, label, file_path, language,
                        start_byte, end_byte, current_digest, source_kind, source_ref,
                        extractor_version, observed_at_ms
                 FROM nodes
                 WHERE project_id = ?1 AND tombstoned_at_ms IS NULL
                   AND label LIKE ?2 ESCAPE '\\'
                 ORDER BY CASE WHEN label = ?3 THEN 0 WHEN label LIKE ?3 || '%' THEN 1 ELSE 2 END,
                          label, stable_key
                 LIMIT ?4",
            )?;
            let rows = statement.query_map(
                params![
                    project_id.as_str(),
                    pattern,
                    query,
                    i64::try_from(limit).unwrap_or(i64::MAX)
                ],
                graph_node_from_row,
            )?;
            rows.map(|row| row?.decode()).collect()
        })
    }

    /// Traverse a bounded structural neighborhood in either direction.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid bounds, IDs, or storage failures.
    pub fn neighbors(
        &self,
        project_id: &ProjectId,
        start: &NodeId,
        max_depth: usize,
        max_nodes: usize,
    ) -> Result<GraphSlice> {
        if max_depth == 0 || max_depth > 8 || max_nodes == 0 || max_nodes > 10_000 {
            return Err(Error::Config(
                "graph traversal requires depth 1..=8 and nodes 1..=10000".to_owned(),
            ));
        }
        self.database.with_reader(|connection| {
            let start_node = find_node(connection, project_id, start)?.ok_or_else(|| {
                Error::Structural(format!(
                    "unknown graph node `{start}` in project `{project_id}`"
                ))
            })?;
            let mut nodes = BTreeMap::from([(start.clone(), start_node)]);
            let mut edges = BTreeMap::<EdgeId, GraphEdge>::new();
            let mut queue = VecDeque::from([(start.clone(), 0_usize)]);
            let mut expanded = BTreeSet::new();
            let mut truncated = false;
            while let Some((node_id, depth)) = queue.pop_front() {
                if depth >= max_depth || !expanded.insert(node_id.clone()) {
                    continue;
                }
                for edge in adjacent_edges(connection, project_id, &node_id)? {
                    let next = if edge.source_id == node_id {
                        edge.target_id.clone()
                    } else {
                        edge.source_id.clone()
                    };
                    edges.insert(edge.id.clone(), edge);
                    if !nodes.contains_key(&next) {
                        if nodes.len() >= max_nodes {
                            truncated = true;
                            continue;
                        }
                        if let Some(node) = find_node(connection, project_id, &next)? {
                            nodes.insert(next.clone(), node);
                            queue.push_back((next, depth + 1));
                        }
                    }
                }
            }
            Ok(GraphSlice {
                nodes: nodes.into_values().collect(),
                edges: edges.into_values().collect(),
                truncated,
            })
        })
    }

    fn known_digests(
        &self,
        project_id: &ProjectId,
        provider: &dyn StructuralProvider,
    ) -> Result<(BTreeMap<String, String>, bool)> {
        self.database.with_reader(|connection| {
            let mut statement = connection.prepare(
                "SELECT f.path, f.content_digest, s.provider, s.provider_version
                 FROM indexed_files f JOIN structural_scans s ON s.id = f.scan_id
                 WHERE f.project_id = ?1",
            )?;
            let rows = statement.query_map([project_id.as_str()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?;
            let mut known = BTreeMap::new();
            let mut needs_reindex = false;
            for row in rows {
                let (path, digest, name, version) = row?;
                // Graphify versions may come from the external exporter. Its
                // own normalized digest remains the cache contract.
                if provider.name() == "tree_sitter"
                    && (name != provider.name() || version != provider.version())
                {
                    needs_reindex = true;
                } else {
                    known.insert(path, digest);
                }
            }
            Ok((known, needs_reindex))
        })
    }

    fn apply_scan(&self, project: &ResolvedProject, scan: ScanResult) -> Result<IndexReport> {
        let scan_id = format!("scn_{}", Uuid::new_v4());
        let now = unix_time_ms();
        let parsed_files = scan.files.len();
        let nodes_written = scan
            .files
            .iter()
            .map(|file| file.nodes.len())
            .sum::<usize>();
        let edges_written = scan
            .files
            .iter()
            .map(|file| file.edges.len())
            .sum::<usize>();
        let discovered_files = scan.seen_paths.len();
        let deleted_paths = if scan.incremental {
            scan.deleted_paths.clone()
        } else {
            self.removed_paths(&project.project.id, &scan.seen_paths)?
        };
        let event = DomainEvent::StructuralScanApplied {
            project_id: project.project.id.clone(),
            scan_id: scan_id.clone(),
            provider: scan.provider.clone(),
            head: project.git.as_ref().and_then(|git| git.head.clone()),
            diff_fingerprint: project
                .git
                .as_ref()
                .map_or_else(|| "no-git".to_owned(), |git| git.diff_fingerprint.clone()),
            file_count: usize_to_u64(discovered_files),
            node_count: usize_to_u64(nodes_written),
            edge_count: usize_to_u64(edges_written),
            incremental: scan.incremental,
        };

        self.database.write_immediate(|transaction| {
            insert_scan_row(
                transaction,
                &scan_id,
                project,
                &scan,
                now,
                discovered_files,
                nodes_written,
                edges_written,
            )?;
            for path in &deleted_paths {
                tombstone_file(transaction, &project.project.id, path, now)?;
            }
            for file in &scan.files {
                if scan.provider == "tree_sitter" && scan.provider_version == "tree-sitter-v2" {
                    preserve_native_upgrade_identities(transaction, &project.project.id, file)?;
                }
                tombstone_file(transaction, &project.project.id, &file.path, now)?;
                upsert_indexed_file(transaction, &project.project.id, &scan_id, file, now)?;
            }
            let mut ids = BTreeMap::new();
            for file in &scan.files {
                ids.extend(upsert_nodes(
                    transaction,
                    &project.project.id,
                    file,
                    &scan.provider,
                    &scan.provider_version,
                    now,
                )?);
            }
            for file in &scan.files {
                upsert_edges(
                    transaction,
                    &project.project.id,
                    file,
                    &ids,
                    &scan.provider,
                    &scan.provider_version,
                    now,
                )?;
            }
            // Incoming edges owned by unchanged source files survive a target
            // refresh. Once every changed node has been reactivated, retire
            // only edges whose endpoint is still tombstoned or absent.
            retire_edges_with_inactive_endpoints(transaction, &project.project.id, now)?;
            append_generated_in_transaction(transaction, &event, now)
        })?;

        Ok(IndexReport {
            scan_id,
            provider: scan.provider,
            incremental: scan.incremental,
            discovered_files: usize_to_u64(discovered_files),
            parsed_files: usize_to_u64(parsed_files),
            unchanged_files: usize_to_u64(scan.unchanged_count),
            removed_files: usize_to_u64(deleted_paths.len()),
            nodes_written: usize_to_u64(nodes_written),
            edges_written: usize_to_u64(edges_written),
        })
    }

    fn removed_paths(
        &self,
        project_id: &ProjectId,
        seen_paths: &BTreeSet<String>,
    ) -> Result<BTreeSet<String>> {
        let existing = self.database.with_reader(|connection| {
            let mut statement =
                connection.prepare("SELECT path FROM indexed_files WHERE project_id = ?1")?;
            let rows = statement.query_map([project_id.as_str()], |row| row.get::<_, String>(0))?;
            rows.collect::<std::result::Result<BTreeSet<_>, _>>()
                .map_err(Error::from)
        })?;
        Ok(existing.difference(seen_paths).cloned().collect())
    }
}

#[derive(Clone, Copy, Debug)]
enum SupportedLanguage {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    Tsx,
    Go,
    Java,
    Kotlin,
    C,
    Cpp,
    CSharp,
    Ruby,
    Php,
    Bash,
    Scala,
    /// Documentation is part of the world model: headings become Section
    /// nodes so docs are searchable and scope-matchable like code. No
    /// tree-sitter grammar — a bounded line scanner.
    Markdown,
}

impl SupportedLanguage {
    fn from_path(path: &Path) -> Option<Self> {
        match path.extension()?.to_str()? {
            "rs" => Some(Self::Rust),
            "py" | "pyi" => Some(Self::Python),
            "js" | "jsx" | "mjs" | "cjs" => Some(Self::JavaScript),
            "ts" | "mts" | "cts" => Some(Self::TypeScript),
            "tsx" => Some(Self::Tsx),
            "go" => Some(Self::Go),
            "java" => Some(Self::Java),
            "kt" | "kts" => Some(Self::Kotlin),
            "c" | "h" => Some(Self::C),
            "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" => Some(Self::Cpp),
            "cs" => Some(Self::CSharp),
            "rb" | "rake" => Some(Self::Ruby),
            "php" => Some(Self::Php),
            "sh" | "bash" | "zsh" => Some(Self::Bash),
            "scala" | "sc" => Some(Self::Scala),
            "md" | "markdown" => Some(Self::Markdown),
            _ => None,
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Python => "python",
            Self::JavaScript => "javascript",
            Self::TypeScript => "typescript",
            Self::Tsx => "tsx",
            Self::Go => "go",
            Self::Java => "java",
            Self::Kotlin => "kotlin",
            Self::C => "c",
            Self::Cpp => "cpp",
            Self::CSharp => "csharp",
            Self::Ruby => "ruby",
            Self::Php => "php",
            Self::Bash => "bash",
            Self::Scala => "scala",
            Self::Markdown => "markdown",
        }
    }

    fn grammar(self) -> Language {
        match self {
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            Self::Python => tree_sitter_python::LANGUAGE.into(),
            Self::JavaScript | Self::TypeScript => {
                tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
            }
            Self::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Self::Go => tree_sitter_go::LANGUAGE.into(),
            Self::Java => tree_sitter_java::LANGUAGE.into(),
            Self::Kotlin => tree_sitter_kotlin_ng::LANGUAGE.into(),
            Self::C => tree_sitter_c::LANGUAGE.into(),
            Self::Cpp => tree_sitter_cpp::LANGUAGE.into(),
            Self::CSharp => tree_sitter_c_sharp::LANGUAGE.into(),
            Self::Ruby => tree_sitter_ruby::LANGUAGE.into(),
            Self::Php => tree_sitter_php::LANGUAGE_PHP.into(),
            Self::Bash => tree_sitter_bash::LANGUAGE.into(),
            Self::Scala => tree_sitter_scala::LANGUAGE.into(),
            // parse_file branches to the markdown scanner before reaching
            // the parser; this arm is unreachable by construction.
            Self::Markdown => unreachable!("markdown has no tree-sitter grammar"),
        }
    }

    fn definition_kind(self, kind: &str) -> Option<&'static str> {
        let function = matches!(
            (self, kind),
            (Self::Rust, "function_item") | (Self::Python, "function_definition")
        ) || matches!(
            (self, kind),
            (
                Self::JavaScript | Self::TypeScript | Self::Tsx,
                "function_declaration" | "generator_function_declaration"
            )
        ) || matches!(
            (self, kind),
            (Self::Go | Self::Kotlin, "function_declaration")
                | (
                    Self::C | Self::Cpp | Self::Php | Self::Bash | Self::Scala,
                    "function_definition"
                )
                | (Self::Ruby, "method")
        );
        if function {
            return Some("Function");
        }
        let data_type = matches!(
            (self, kind),
            (Self::Rust, "struct_item" | "enum_item") | (Self::Python, "class_definition")
        ) || matches!(
            (self, kind),
            (
                Self::JavaScript | Self::TypeScript | Self::Tsx,
                "class_declaration" | "interface_declaration" | "type_alias_declaration"
            )
        ) || matches!(
            (self, kind),
            (Self::Go, "type_spec")
                | (
                    Self::Java,
                    "class_declaration" | "enum_declaration" | "record_declaration"
                )
                | (Self::Kotlin, "class_declaration" | "object_declaration")
                | (
                    Self::C | Self::Cpp,
                    "struct_specifier" | "enum_specifier" | "union_specifier"
                )
                | (Self::Cpp, "class_specifier")
                | (
                    Self::CSharp,
                    "class_declaration"
                        | "struct_declaration"
                        | "enum_declaration"
                        | "record_declaration"
                )
                | (Self::Ruby, "class" | "module")
                | (Self::Php, "class_declaration" | "trait_declaration")
                | (Self::Scala, "class_definition" | "object_definition")
        );
        if data_type {
            return Some("Type");
        }
        match (self, kind) {
            (Self::Rust, "trait_item")
            | (Self::Java | Self::CSharp | Self::Php, "interface_declaration")
            | (Self::Scala, "trait_definition") => Some("Interface"),
            (Self::Rust, "mod_item") => Some("Module"),
            (
                Self::JavaScript | Self::TypeScript | Self::Tsx,
                "method_definition" | "method_signature",
            )
            | (Self::Go | Self::Php, "method_declaration")
            | (Self::Java | Self::CSharp, "method_declaration" | "constructor_declaration") => {
                Some("Method")
            }
            _ => None,
        }
    }

    fn is_import(self, kind: &str) -> bool {
        match self {
            Self::Rust => kind == "use_declaration" || kind == "extern_crate_declaration",
            Self::Python => kind == "import_statement" || kind == "import_from_statement",
            Self::JavaScript | Self::TypeScript | Self::Tsx => kind == "import_statement",
            Self::Go | Self::Java | Self::Scala => kind == "import_declaration",
            Self::Kotlin => kind == "import_header" || kind == "import",
            Self::C | Self::Cpp => kind == "preproc_include",
            Self::CSharp => kind == "using_directive",
            Self::Php => kind == "namespace_use_declaration",
            Self::Ruby | Self::Bash | Self::Markdown => false,
        }
    }

    fn is_call(self, kind: &str) -> bool {
        match self {
            Self::Java => matches!(kind, "method_invocation" | "object_creation_expression"),
            Self::CSharp => matches!(kind, "invocation_expression" | "object_creation_expression"),
            Self::Ruby => kind == "call",
            Self::Php => matches!(
                kind,
                "function_call_expression"
                    | "member_call_expression"
                    | "object_creation_expression"
            ),
            Self::Bash | Self::Markdown => false,
            _ => matches!(kind, "call_expression" | "macro_invocation"),
        }
    }
}

fn candidate_paths(
    root: &Path,
    changed_paths: Option<&[PathBuf]>,
    max_files: usize,
) -> Result<Vec<PathBuf>> {
    let mut paths = BTreeSet::new();
    if let Some(changed_paths) = changed_paths {
        for path in changed_paths {
            let candidate = if path.is_absolute() {
                path.clone()
            } else {
                root.join(path)
            };
            let normalized = normalize_without_fs(&candidate);
            if !normalized.starts_with(root) {
                return Err(Error::Structural(format!(
                    "changed path escapes project root: {}",
                    path.display()
                )));
            }
            paths.insert(normalized);
        }
    } else {
        let walker = WalkBuilder::new(root)
            .hidden(false)
            .follow_links(false)
            .filter_entry(|entry| {
                !matches!(
                    entry.file_name().to_str(),
                    Some(
                        ".git" | ".engram" | ".leb" | "target" | "node_modules" | ".venv" | "venv"
                    )
                )
            })
            .build();
        for entry in walker {
            let entry = entry.map_err(|error| Error::Structural(error.to_string()))?;
            if entry.file_type().is_some_and(|kind| kind.is_file())
                && SupportedLanguage::from_path(entry.path()).is_some()
            {
                paths.insert(entry.into_path());
                if paths.len() > max_files {
                    return Err(Error::Structural(format!(
                        "repository exceeds configured {max_files} source-file limit"
                    )));
                }
            }
        }
    }
    if paths.len() > max_files {
        return Err(Error::Structural(format!(
            "scan exceeds configured {max_files} source-file limit"
        )));
    }
    Ok(paths.into_iter().collect())
}

/// Markdown scanner: the file node plus one Section node per heading, so
/// documentation participates in symbol search and scope matching.
fn parse_markdown(relative: &str, source: &[u8], digest: String) -> FileScan {
    const MAX_SECTIONS: usize = 80;
    let text = String::from_utf8_lossy(source);
    let file_key = format!("file:{relative}");
    let mut nodes = BTreeMap::new();
    nodes.insert(
        file_key.clone(),
        ScannedNode {
            stable_key: file_key.clone(),
            kind: "File".to_owned(),
            label: relative.to_owned(),
            search_aliases: String::new(),
            file_path: relative.to_owned(),
            language: "markdown".to_owned(),
            start_byte: 0,
            end_byte: u64::try_from(source.len()).unwrap_or(u64::MAX),
            digest: digest.clone(),
        },
    );
    let mut edges = BTreeMap::new();
    let mut offset = 0_usize;
    let mut in_fence = false;
    let mut sections = 0_usize;
    let mut links = 0_usize;
    for line in text.lines() {
        let line_len = line.len() + 1;
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
        } else if !in_fence {
            // Doc links become graph edges: [[wikilinks]] and markdown link
            // targets each get an ExternalReference resolved later by label
            // (the same mechanism call references use), so documents that
            // cite each other connect the way code that calls each other does.
            for label in markdown_link_labels(line) {
                if links >= 60 {
                    break;
                }
                links += 1;
                let target_key = format!("reference:{relative}:doclink:{label}");
                let start = usize_to_u64(offset);
                let end = usize_to_u64(offset + line.len());
                let link_digest = sha256(label.as_bytes());
                nodes
                    .entry(target_key.clone())
                    .or_insert_with(|| ScannedNode {
                        stable_key: target_key.clone(),
                        kind: "ExternalReference".to_owned(),
                        label,
                        search_aliases: String::new(),
                        file_path: relative.to_owned(),
                        language: "markdown".to_owned(),
                        start_byte: start,
                        end_byte: end,
                        digest: link_digest,
                    });
                insert_edge(
                    &mut edges,
                    &file_key,
                    "LINKS_TO",
                    &target_key,
                    0.9,
                    relative,
                );
            }
        }
        if !in_fence && let Some(rest) = line.strip_prefix('#') {
            let level = 1 + rest.chars().take_while(|c| *c == '#').count();
            let title = rest.trim_start_matches('#').trim();
            if level <= 6 && !title.is_empty() && sections < MAX_SECTIONS {
                sections += 1;
                let label: String = title.chars().take(120).collect();
                let base_key = format!("symbol:{relative}:Section:{label}");
                let stable_key = if nodes.contains_key(&base_key) {
                    format!("{base_key}:{offset}")
                } else {
                    base_key
                };
                nodes.insert(
                    stable_key.clone(),
                    ScannedNode {
                        stable_key: stable_key.clone(),
                        kind: "Section".to_owned(),
                        label,
                        search_aliases: String::new(),
                        file_path: relative.to_owned(),
                        language: "markdown".to_owned(),
                        start_byte: usize_to_u64(offset),
                        end_byte: usize_to_u64(offset + line.len()),
                        digest: sha256(line.as_bytes()),
                    },
                );
                insert_edge(&mut edges, &file_key, "DEFINES", &stable_key, 1.0, relative);
            }
        }
        offset += line_len;
    }
    FileScan {
        path: relative.to_owned(),
        language: "markdown".to_owned(),
        digest,
        size_bytes: u64::try_from(source.len()).unwrap_or(u64::MAX),
        nodes: nodes.into_values().collect(),
        edges: edges.into_values().collect(),
    }
}

fn parse_file(
    relative: &str,
    language: SupportedLanguage,
    source: &[u8],
    digest: String,
) -> Result<FileScan> {
    if matches!(language, SupportedLanguage::Markdown) {
        return Ok(parse_markdown(relative, source, digest));
    }
    let mut parser = Parser::new();
    parser
        .set_language(&language.grammar())
        .map_err(|error| Error::Structural(format!("parser language setup failed: {error}")))?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| Error::Structural(format!("parser returned no tree for {relative}")))?;
    let file_key = format!("file:{relative}");
    let mut nodes = BTreeMap::new();
    nodes.insert(
        file_key.clone(),
        ScannedNode {
            stable_key: file_key.clone(),
            kind: "File".to_owned(),
            label: relative.to_owned(),
            search_aliases: String::new(),
            file_path: relative.to_owned(),
            language: language.name().to_owned(),
            start_byte: 0,
            end_byte: u64::try_from(source.len()).unwrap_or(u64::MAX),
            digest: digest.clone(),
        },
    );
    let mut edges = BTreeMap::new();
    visit_tree(
        tree.root_node(),
        source,
        language,
        relative,
        &file_key,
        "",
        &mut nodes,
        &mut edges,
    );
    if matches!(language, SupportedLanguage::Rust) {
        repair_rust_ownership(relative, &nodes, &mut edges);
    }
    Ok(FileScan {
        path: relative.to_owned(),
        language: language.name().to_owned(),
        digest,
        size_bytes: u64::try_from(source.len()).unwrap_or(u64::MAX),
        nodes: nodes.into_values().collect(),
        edges: edges.into_values().collect(),
    })
}

/// Link labels on one markdown line: `[[wikilink]]` inner text and
/// markdown link targets `](target)` (fragments/queries stripped, external
/// URLs skipped). Bounded by the caller.
fn markdown_link_labels(line: &str) -> Vec<String> {
    let mut labels = Vec::new();
    let mut rest = line;
    while let Some(start) = rest.find("[[") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("]]") else { break };
        let inner = after[..end].split('|').next().unwrap_or("").trim();
        if !inner.is_empty() && inner.len() <= 120 {
            labels.push(inner.to_owned());
        }
        rest = &after[end + 2..];
    }
    let mut rest = line;
    while let Some(start) = rest.find("](") {
        let after = &rest[start + 2..];
        let Some(end) = after.find(')') else { break };
        let target = after[..end].trim();
        rest = &after[end + 1..];
        if target.is_empty()
            || target.len() > 200
            || target.starts_with("http://")
            || target.starts_with("https://")
            || target.starts_with('#')
        {
            continue;
        }
        let cleaned = target
            .split(['#', '?'])
            .next()
            .unwrap_or(target)
            .trim_start_matches("./");
        if !cleaned.is_empty() {
            labels.push(cleaned.to_owned());
        }
    }
    labels
}

/// C-family declarations bury the name under nested `declarator` fields
/// (`function_definition` -> `function_declarator` -> identifier): descend
/// bounded until an identifier appears.
fn declarator_identifier(node: Node<'_>) -> Option<Node<'_>> {
    let mut current = node.child_by_field_name("declarator")?;
    for _ in 0..6 {
        if matches!(
            current.kind(),
            "identifier" | "field_identifier" | "type_identifier" | "qualified_identifier"
        ) {
            return Some(current);
        }
        match current.child_by_field_name("declarator") {
            Some(next) => current = next,
            None => return named_identifier_child(current),
        }
    }
    None
}

/// Fallback for grammars without a `name` field on declarations (Kotlin
/// notably): the first named child that is an identifier.
fn named_identifier_child(node: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).find(|child| {
        matches!(
            child.kind(),
            "identifier" | "simple_identifier" | "type_identifier"
        )
    })
}

/// Only direct named Rust struct fields become owner aliases. Initializers,
/// method locals, tuple positions, and field-use expressions are excluded.
fn rust_field_aliases(node: Node<'_>, source: &[u8], owner: &str) -> String {
    const MAX_FIELDS: usize = 128;
    const MAX_BYTES: usize = 16 * 1024;
    if node.kind() != "struct_item" {
        return String::new();
    }
    let Some(body) = node.child_by_field_name("body") else {
        return String::new();
    };
    let mut aliases = BTreeSet::new();
    let mut bytes = 0_usize;
    let mut cursor = body.walk();
    for field in body
        .named_children(&mut cursor)
        .filter(|child| child.kind() == "field_declaration")
        .take(MAX_FIELDS)
    {
        let Some(name) = field
            .child_by_field_name("name")
            .and_then(|name| name.utf8_text(source).ok())
        else {
            continue;
        };
        let name = name.strip_prefix("r#").unwrap_or(name);
        if name.len() > 256 || owner.len() > 512 {
            continue;
        }
        let qualified = format!("{owner}::{name}");
        bytes = bytes.saturating_add(name.len() + qualified.len() + 2);
        if bytes > MAX_BYTES {
            break;
        }
        aliases.insert(name.to_owned());
        aliases.insert(qualified);
    }
    aliases.into_iter().collect::<Vec<_>>().join("\n")
}

/// Keep type paths and discard generic arguments, never an arbitrary type body.
fn rust_type_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    match node.kind() {
        "generic_type" => rust_type_name(node.child_by_field_name("type")?, source),
        "scoped_type_identifier" | "scoped_identifier" => {
            let name = rust_type_name(node.child_by_field_name("name")?, source)?;
            node.child_by_field_name("path").map_or_else(
                || Some(name.clone()),
                |path| Some(format!("{}::{name}", rust_type_name(path, source)?)),
            )
        }
        "identifier" | "type_identifier" | "crate" | "self" | "super" => {
            let name = node.utf8_text(source).ok()?;
            (name.len() <= 256).then(|| name.to_owned())
        }
        _ => None,
    }
}

fn rust_receiver_scope(scope: &str, receiver: &str) -> String {
    if let Some(rooted) = receiver.strip_prefix("crate::") {
        return rooted.to_owned();
    }
    let mut scope = scope.to_owned();
    let mut receiver = receiver.strip_prefix("self::").unwrap_or(receiver);
    while let Some(parent) = receiver.strip_prefix("super::") {
        scope = scope
            .rsplit_once("::")
            .map_or("", |(parent, _)| parent)
            .to_owned();
        receiver = parent;
    }
    if scope.is_empty() || receiver.starts_with(&format!("{scope}::")) {
        receiver.to_owned()
    } else {
        format!("{scope}::{receiver}")
    }
}

/// Definitions may follow their impl block. Resolve ownership after visiting
/// the whole file; absent or ambiguous receiver types keep the lexical owner.
fn repair_rust_ownership(
    path: &str,
    nodes: &BTreeMap<String, ScannedNode>,
    edges: &mut BTreeMap<String, ScannedEdge>,
) {
    let mut owners = BTreeMap::<&str, Vec<&str>>::new();
    for node in nodes.values().filter(|node| node.kind == "Type") {
        owners
            .entry(&node.label)
            .or_default()
            .push(&node.stable_key);
    }
    let replacements: Vec<_> = edges
        .iter()
        .filter_map(|(key, edge)| {
            if edge.relation != "DEFINES" {
                return None;
            }
            let target = nodes.get(&edge.target_key)?;
            if target.kind != "Function" {
                return None;
            }
            let (owner, _) = target.label.rsplit_once("::")?;
            let [owner_key] = owners.get(owner)?.as_slice() else {
                return None;
            };
            Some((
                key.clone(),
                (*owner_key).to_owned(),
                edge.target_key.clone(),
            ))
        })
        .collect();
    for (key, owner, target) in replacements {
        edges.remove(&key);
        insert_edge(edges, &owner, "DEFINES", &target, 1.0, path);
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn visit_tree(
    node: Node<'_>,
    source: &[u8],
    language: SupportedLanguage,
    path: &str,
    owner_key: &str,
    scope: &str,
    nodes: &mut BTreeMap<String, ScannedNode>,
    edges: &mut BTreeMap<String, ScannedEdge>,
) {
    let mut child_owner = owner_key.to_owned();
    let mut child_scope = scope.to_owned();
    if let Some(kind) = language.definition_kind(node.kind())
        && let Some(name_node) = node
            .child_by_field_name("name")
            .or_else(|| declarator_identifier(node))
            .or_else(|| named_identifier_child(node))
        && let Ok(name) = name_node.utf8_text(source)
    {
        let qualified = if scope.is_empty() {
            name.to_owned()
        } else {
            format!("{scope}::{name}")
        };
        let base_key = format!("symbol:{path}:{kind}:{qualified}");
        let stable_key = if nodes.contains_key(&base_key) {
            format!("{base_key}:{}", node.start_byte())
        } else {
            base_key
        };
        let text = node.utf8_text(source).unwrap_or(name);
        nodes.insert(
            stable_key.clone(),
            ScannedNode {
                stable_key: stable_key.clone(),
                kind: kind.to_owned(),
                label: qualified.clone(),
                search_aliases: if matches!(language, SupportedLanguage::Rust) {
                    rust_field_aliases(node, source, &qualified)
                } else {
                    String::new()
                },
                file_path: path.to_owned(),
                language: language.name().to_owned(),
                start_byte: usize_to_u64(node.start_byte()),
                end_byte: usize_to_u64(node.end_byte()),
                digest: sha256(text.as_bytes()),
            },
        );
        insert_edge(edges, owner_key, "DEFINES", &stable_key, 1.0, path);
        child_owner.clone_from(&stable_key);
        child_scope = qualified;
    } else if matches!(language, SupportedLanguage::Rust) && node.kind() == "impl_item" {
        if let Some(receiver) = node
            .child_by_field_name("type")
            .and_then(|receiver| rust_type_name(receiver, source))
        {
            child_scope = rust_receiver_scope(scope, &receiver);
        }
    } else if language.is_import(node.kind()) {
        if let Ok(text) = node.utf8_text(source) {
            let label = normalize_reference(text);
            let target_key = format!("reference:{path}:import:{label}");
            nodes
                .entry(target_key.clone())
                .or_insert_with(|| ScannedNode {
                    stable_key: target_key.clone(),
                    kind: "ExternalReference".to_owned(),
                    label,
                    search_aliases: String::new(),
                    file_path: path.to_owned(),
                    language: language.name().to_owned(),
                    start_byte: usize_to_u64(node.start_byte()),
                    end_byte: usize_to_u64(node.end_byte()),
                    digest: sha256(text.as_bytes()),
                });
            insert_edge(edges, owner_key, "IMPORTS", &target_key, 1.0, path);
        }
    } else if language.is_call(node.kind())
        && let Some(function) = node
            .child_by_field_name("function")
            // Java/C# invocations name their callee "name"/"function"; `new`
            // expressions name the constructed type "type"; Ruby calls use
            // "method".
            .or_else(|| node.child_by_field_name("name"))
            .or_else(|| node.child_by_field_name("type"))
            .or_else(|| node.child_by_field_name("method"))
            .or_else(|| node.child(0))
        && let Ok(text) = function.utf8_text(source)
        && text.len() <= 200
    {
        let label = normalize_reference(text);
        let target_key = format!("reference:{path}:call:{label}");
        nodes
            .entry(target_key.clone())
            .or_insert_with(|| ScannedNode {
                stable_key: target_key.clone(),
                kind: "ExternalReference".to_owned(),
                label,
                search_aliases: String::new(),
                file_path: path.to_owned(),
                language: language.name().to_owned(),
                start_byte: usize_to_u64(function.start_byte()),
                end_byte: usize_to_u64(function.end_byte()),
                digest: sha256(text.as_bytes()),
            });
        insert_edge(edges, owner_key, "CALLS", &target_key, 0.9, path);
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit_tree(
            child,
            source,
            language,
            path,
            &child_owner,
            &child_scope,
            nodes,
            edges,
        );
    }
}

fn insert_edge(
    edges: &mut BTreeMap<String, ScannedEdge>,
    source_key: &str,
    relation: &str,
    target_key: &str,
    confidence: f64,
    source_ref: &str,
) {
    let key = format!("{source_key}\0{relation}\0{target_key}");
    let digest = sha256(key.as_bytes());
    edges.entry(key).or_insert_with(|| ScannedEdge {
        source_key: source_key.to_owned(),
        relation: relation.to_owned(),
        target_key: target_key.to_owned(),
        confidence,
        source_ref: source_ref.to_owned(),
        digest,
    });
}

fn upsert_indexed_file(
    transaction: &Transaction<'_>,
    project_id: &ProjectId,
    scan_id: &str,
    file: &FileScan,
    now: i64,
) -> Result<()> {
    transaction.execute(
        "INSERT INTO indexed_files
         (project_id, path, language, content_digest, size_bytes, scan_id, indexed_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(project_id, path) DO UPDATE SET
            language = excluded.language,
            content_digest = excluded.content_digest,
            size_bytes = excluded.size_bytes,
            scan_id = excluded.scan_id,
            indexed_at_ms = excluded.indexed_at_ms",
        params![
            project_id.as_str(),
            file.path,
            file.language,
            file.digest,
            u64_to_i64(file.size_bytes),
            scan_id,
            now
        ],
    )?;
    Ok(())
}

/// v2 qualifies Rust impl methods and their descendants. Rebind only unique
/// unchanged native-v1 definitions before tombstoning so existing graph and
/// memory references keep their IDs. A source edit is never an upgrade match.
fn preserve_native_upgrade_identities(
    transaction: &Transaction<'_>,
    project_id: &ProjectId,
    file: &FileScan,
) -> Result<()> {
    type Signature = (String, String, i64, i64, String);
    if file.language != "rust" {
        return Ok(());
    }
    let previous_digest: Option<String> = transaction
        .query_row(
            "SELECT f.content_digest FROM indexed_files f JOIN structural_scans s ON s.id=f.scan_id
         WHERE f.project_id=?1 AND f.path=?2
           AND s.provider='tree_sitter' AND s.provider_version='tree-sitter-v1'",
            params![project_id.as_str(), file.path],
            |row| row.get(0),
        )
        .optional()?;
    if previous_digest.as_deref() != Some(file.digest.as_str()) {
        return retire_native_v1_symbol_keys(transaction, project_id, file, true);
    }
    // Deleted historical owners are not evidence of the current definition's
    // identity, even when their old key is now occupied by a qualified method.
    retire_native_v1_symbol_keys(transaction, project_id, file, false)?;
    let mut new_by_signature = BTreeMap::<Signature, Vec<&ScannedNode>>::new();
    for node in &file.nodes {
        let signature = (
            node.kind.clone(),
            node.language.clone(),
            u64_to_i64(node.start_byte),
            u64_to_i64(node.end_byte),
            node.digest.clone(),
        );
        new_by_signature.entry(signature).or_default().push(node);
    }
    let mut old_by_signature = BTreeMap::<Signature, Vec<(String, String)>>::new();
    let mut statement = transaction.prepare(
        "SELECT kind,language,start_byte,end_byte,current_digest,id,stable_key FROM nodes
         WHERE project_id=?1 AND file_path=?2 AND tombstoned_at_ms IS NULL
           AND source_kind='static_analysis' AND extractor_version='tree-sitter-v1'
           AND language='rust' AND start_byte>=0 AND end_byte>=start_byte",
    )?;
    let rows = statement.query_map(params![project_id.as_str(), file.path], |row| {
        Ok((
            (
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ),
            (row.get(5)?, row.get(6)?),
        ))
    })?;
    for row in rows {
        let (signature, identity) = row?;
        old_by_signature
            .entry(signature)
            .or_default()
            .push(identity);
    }
    let mut matched_ids = BTreeMap::new();
    let mut relocations = BTreeMap::new();
    for (signature, old) in old_by_signature {
        let [(id, old_key)] = old.as_slice() else {
            continue;
        };
        let Some(new) = new_by_signature.get(&signature) else {
            continue;
        };
        let [new] = new.as_slice() else { continue };
        matched_ids.insert(new.stable_key.clone(), id.clone());
        if old_key == &new.stable_key {
            continue;
        }
        relocations.insert(id.clone(), new.stable_key.clone());
    }
    for node in &file.nodes {
        let target_key = &node.stable_key;
        let occupant: Option<String> = transaction
            .query_row(
                "SELECT id FROM nodes WHERE project_id=?1 AND stable_key=?2",
                params![project_id.as_str(), target_key],
                |row| row.get(0),
            )
            .optional()?;
        if occupant.is_some_and(|id| {
            matched_ids.get(target_key) != Some(&id) && !relocations.contains_key(&id)
        }) {
            return Err(Error::Structural(
                "native extractor upgrade has an ambiguous stable-key collision".to_owned(),
            ));
        }
    }
    // A v1 impl method may occupy the key now needed by a free function with
    // the same name. Move every matched identity out of the way atomically,
    // then assign final keys, rather than allowing exact-key upserts to swap
    // their source identities. Temporary keys cannot escape this transaction.
    for id in relocations.keys() {
        transaction.execute(
            "UPDATE nodes SET stable_key=?1 WHERE id=?2",
            params![format!("native-upgrade:{}", Uuid::new_v4()), id],
        )?;
    }
    for (id, target_key) in relocations {
        transaction.execute(
            "UPDATE nodes SET stable_key=?1 WHERE id=?2",
            params![target_key, id],
        )?;
    }
    Ok(())
}

/// A dirty file cannot prove ownership continuity from spans/digests alone.
/// Conservatively retire all native-v1 Rust symbol keys before v2 upserts;
/// the old IDs will be tombstoned normally and anchored memories quarantined.
fn retire_native_v1_symbol_keys(
    transaction: &Transaction<'_>,
    project_id: &ProjectId,
    file: &FileScan,
    include_active: bool,
) -> Result<()> {
    transaction.execute(
        "UPDATE nodes SET stable_key='retired-native-v1:' || id
         WHERE project_id=?1 AND file_path=?2 AND (?3 OR tombstoned_at_ms IS NOT NULL)
           AND source_kind='static_analysis' AND extractor_version='tree-sitter-v1'
           AND language='rust' AND substr(stable_key,1,7)='symbol:'",
        params![project_id.as_str(), file.path, include_active],
    )?;
    Ok(())
}

fn upsert_nodes(
    transaction: &Transaction<'_>,
    project_id: &ProjectId,
    file: &FileScan,
    provider: &str,
    provider_version: &str,
    now: i64,
) -> Result<BTreeMap<String, NodeId>> {
    let source_kind = if provider == "graphify" {
        "import"
    } else {
        "static_analysis"
    };
    let mut ids = BTreeMap::new();
    for node in &file.nodes {
        let existing = transaction
            .query_row(
                "SELECT id FROM nodes WHERE project_id = ?1 AND stable_key = ?2",
                params![project_id.as_str(), node.stable_key],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let id = existing.map_or_else(|| Ok(NodeId::new()), |value| NodeId::parse(&value))?;
        transaction.execute(
            "INSERT INTO nodes
             (id, project_id, kind, stable_key, label, file_path, language, start_byte,
              end_byte, current_digest, source_kind, source_ref, extractor_version,
              observed_at_ms, tombstoned_at_ms, search_aliases)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11,
                     ?6, ?12, ?13, NULL, ?14)
             ON CONFLICT(project_id, stable_key) DO UPDATE SET
                kind = excluded.kind, label = excluded.label, file_path = excluded.file_path,
                language = excluded.language, start_byte = excluded.start_byte,
                end_byte = excluded.end_byte, current_digest = excluded.current_digest,
                source_kind = excluded.source_kind, source_ref = excluded.source_ref,
                extractor_version = excluded.extractor_version,
                search_aliases = excluded.search_aliases,
                observed_at_ms = excluded.observed_at_ms, tombstoned_at_ms = NULL",
            params![
                id.as_str(),
                project_id.as_str(),
                node.kind,
                node.stable_key,
                node.label,
                node.file_path,
                node.language,
                u64_to_i64(node.start_byte),
                u64_to_i64(node.end_byte),
                node.digest,
                source_kind,
                provider_version,
                now,
                node.search_aliases,
            ],
        )?;
        ids.insert(node.stable_key.clone(), id);
    }
    Ok(ids)
}

fn upsert_edges(
    transaction: &Transaction<'_>,
    project_id: &ProjectId,
    file: &FileScan,
    ids: &BTreeMap<String, NodeId>,
    provider: &str,
    provider_version: &str,
    now: i64,
) -> Result<()> {
    let source_kind = if provider == "graphify" {
        "import"
    } else {
        "static_analysis"
    };
    for edge in &file.edges {
        let source_id = resolve_node_id(transaction, project_id, ids, &edge.source_key)?;
        let target_id = resolve_node_id(transaction, project_id, ids, &edge.target_key)?;
        let existing = transaction
            .query_row(
                "SELECT id FROM edges
                 WHERE project_id = ?1 AND source_id = ?2 AND relation = ?3 AND target_id = ?4",
                params![
                    project_id.as_str(),
                    source_id.as_str(),
                    edge.relation,
                    target_id.as_str()
                ],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let id = existing.map_or_else(|| Ok(EdgeId::new()), |value| EdgeId::parse(&value))?;
        transaction.execute(
            "INSERT INTO edges
             (id, project_id, source_id, relation, target_id, confidence, source_kind,
              source_ref, content_digest, extractor_version, observed_at_ms, tombstoned_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, NULL)
             ON CONFLICT(project_id, source_id, relation, target_id) DO UPDATE SET
                confidence = excluded.confidence, source_kind = excluded.source_kind,
                source_ref = excluded.source_ref, content_digest = excluded.content_digest,
                extractor_version = excluded.extractor_version,
                observed_at_ms = excluded.observed_at_ms, tombstoned_at_ms = NULL",
            params![
                id.as_str(),
                project_id.as_str(),
                source_id.as_str(),
                edge.relation,
                target_id.as_str(),
                edge.confidence,
                source_kind,
                edge.source_ref,
                edge.digest,
                provider_version,
                now,
            ],
        )?;
    }
    Ok(())
}

fn resolve_node_id(
    transaction: &Transaction<'_>,
    project_id: &ProjectId,
    ids: &BTreeMap<String, NodeId>,
    stable_key: &str,
) -> Result<NodeId> {
    if let Some(id) = ids.get(stable_key) {
        return Ok(id.clone());
    }
    let raw = transaction
        .query_row(
            "SELECT id FROM nodes
             WHERE project_id = ?1 AND stable_key = ?2 AND tombstoned_at_ms IS NULL",
            params![project_id.as_str(), stable_key],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    raw.map_or_else(
        || {
            Err(Error::Structural(format!(
                "edge endpoint missing: {stable_key}"
            )))
        },
        |value| NodeId::parse(&value),
    )
}

fn tombstone_file(
    transaction: &Transaction<'_>,
    project_id: &ProjectId,
    path: &str,
    now: i64,
) -> Result<()> {
    transaction.execute(
        "UPDATE edges SET tombstoned_at_ms = ?3
         WHERE project_id = ?1 AND tombstoned_at_ms IS NULL
           AND source_id IN (
               SELECT id FROM nodes WHERE project_id = ?1 AND file_path = ?2
           )",
        params![project_id.as_str(), path, now],
    )?;
    transaction.execute(
        "UPDATE nodes SET tombstoned_at_ms = ?3
         WHERE project_id = ?1 AND file_path = ?2 AND tombstoned_at_ms IS NULL",
        params![project_id.as_str(), path, now],
    )?;
    transaction.execute(
        "DELETE FROM indexed_files WHERE project_id = ?1 AND path = ?2",
        params![project_id.as_str(), path],
    )?;
    Ok(())
}

fn retire_edges_with_inactive_endpoints(
    transaction: &Transaction<'_>,
    project_id: &ProjectId,
    now: i64,
) -> Result<()> {
    transaction.execute(
        "UPDATE edges SET tombstoned_at_ms = ?2
         WHERE project_id = ?1 AND tombstoned_at_ms IS NULL AND (
             source_id IN (
                 SELECT id FROM nodes
                 WHERE project_id = ?1 AND tombstoned_at_ms IS NOT NULL
             )
             OR target_id IN (
                 SELECT id FROM nodes
                 WHERE project_id = ?1 AND tombstoned_at_ms IS NOT NULL
             )
         )",
        params![project_id.as_str(), now],
    )?;
    Ok(())
}

fn find_node(
    connection: &rusqlite::Connection,
    project_id: &ProjectId,
    node_id: &NodeId,
) -> Result<Option<GraphNode>> {
    connection
        .query_row(
            "SELECT id, project_id, kind, stable_key, label, file_path, language,
                    start_byte, end_byte, current_digest, source_kind, source_ref,
                    extractor_version, observed_at_ms
             FROM nodes WHERE id = ?1 AND project_id = ?2 AND tombstoned_at_ms IS NULL",
            params![node_id.as_str(), project_id.as_str()],
            graph_node_from_row,
        )
        .optional()?
        .map(RawGraphNode::decode)
        .transpose()
}

fn adjacent_edges(
    connection: &rusqlite::Connection,
    project_id: &ProjectId,
    node_id: &NodeId,
) -> Result<Vec<GraphEdge>> {
    let mut statement = connection.prepare(
        "SELECT id, project_id, source_id, relation, target_id, confidence,
                source_kind, source_ref, content_digest, extractor_version, observed_at_ms
         FROM edges
         WHERE project_id = ?1 AND tombstoned_at_ms IS NULL
           AND (source_id = ?2 OR target_id = ?2)
         ORDER BY relation, source_id, target_id",
    )?;
    let rows = statement.query_map(
        params![project_id.as_str(), node_id.as_str()],
        graph_edge_from_row,
    )?;
    rows.map(|row| row?.decode()).collect()
}

struct RawGraphNode {
    id: String,
    project_id: String,
    kind: String,
    stable_key: String,
    label: String,
    file_path: Option<String>,
    language: Option<String>,
    start_byte: Option<i64>,
    end_byte: Option<i64>,
    current_digest: String,
    source_kind: String,
    source_ref: String,
    extractor_version: String,
    observed_at_ms: i64,
}

impl RawGraphNode {
    fn decode(self) -> Result<GraphNode> {
        Ok(GraphNode {
            id: NodeId::parse(&self.id)?,
            project_id: ProjectId::parse(&self.project_id)?,
            kind: self.kind,
            stable_key: self.stable_key,
            label: self.label,
            file_path: self.file_path,
            language: self.language,
            start_byte: self.start_byte.map(i64_to_u64).transpose()?,
            end_byte: self.end_byte.map(i64_to_u64).transpose()?,
            current_digest: self.current_digest,
            source_kind: self.source_kind,
            source_ref: self.source_ref,
            extractor_version: self.extractor_version,
            observed_at_ms: self.observed_at_ms,
        })
    }
}

fn graph_node_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawGraphNode> {
    Ok(RawGraphNode {
        id: row.get(0)?,
        project_id: row.get(1)?,
        kind: row.get(2)?,
        stable_key: row.get(3)?,
        label: row.get(4)?,
        file_path: row.get(5)?,
        language: row.get(6)?,
        start_byte: row.get(7)?,
        end_byte: row.get(8)?,
        current_digest: row.get(9)?,
        source_kind: row.get(10)?,
        source_ref: row.get(11)?,
        extractor_version: row.get(12)?,
        observed_at_ms: row.get(13)?,
    })
}

struct RawGraphEdge {
    id: String,
    project_id: String,
    source_id: String,
    relation: String,
    target_id: String,
    confidence: f64,
    source_kind: String,
    source_ref: String,
    content_digest: String,
    extractor_version: String,
    observed_at_ms: i64,
}

impl RawGraphEdge {
    fn decode(self) -> Result<GraphEdge> {
        Ok(GraphEdge {
            id: EdgeId::parse(&self.id)?,
            project_id: ProjectId::parse(&self.project_id)?,
            source_id: NodeId::parse(&self.source_id)?,
            relation: self.relation,
            target_id: NodeId::parse(&self.target_id)?,
            confidence: self.confidence,
            source_kind: self.source_kind,
            source_ref: self.source_ref,
            content_digest: self.content_digest,
            extractor_version: self.extractor_version,
            observed_at_ms: self.observed_at_ms,
        })
    }
}

fn graph_edge_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawGraphEdge> {
    Ok(RawGraphEdge {
        id: row.get(0)?,
        project_id: row.get(1)?,
        source_id: row.get(2)?,
        relation: row.get(3)?,
        target_id: row.get(4)?,
        confidence: row.get(5)?,
        source_kind: row.get(6)?,
        source_ref: row.get(7)?,
        content_digest: row.get(8)?,
        extractor_version: row.get(9)?,
        observed_at_ms: row.get(10)?,
    })
}

fn normalize_reference(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(200)
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn insert_scan_row(
    transaction: &rusqlite::Transaction<'_>,
    scan_id: &str,
    project: &ResolvedProject,
    scan: &ScanResult,
    now: i64,
    discovered_files: usize,
    nodes_written: usize,
    edges_written: usize,
) -> Result<()> {
    let changed_paths_json = project.git.as_ref().map_or_else(
        || Some("[]".to_owned()),
        |git| encode_changed_paths_json(&git.changed_paths),
    );
    transaction.execute(
        "INSERT INTO structural_scans
         (id, project_id, provider, provider_version, head, diff_fingerprint,
          started_at_ms, completed_at_ms, file_count, node_count, edge_count, incremental,
          changed_paths_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            scan_id,
            project.project.id.as_str(),
            scan.provider,
            scan.provider_version,
            project.git.as_ref().and_then(|git| git.head.as_deref()),
            project
                .git
                .as_ref()
                .map_or("no-git", |git| git.diff_fingerprint.as_str()),
            now,
            usize_to_i64(discovered_files),
            usize_to_i64(nodes_written),
            usize_to_i64(edges_written),
            i64::from(scan.incremental),
            changed_paths_json,
        ],
    )?;
    Ok(())
}

/// Encode a scan's dirty working-tree paths for persistence. Returns `None`
/// when any path is not UTF-8, so freshness planning falls back to a full
/// rescan instead of matching against a lossy path.
fn encode_changed_paths_json(paths: &[PathBuf]) -> Option<String> {
    let utf8 = paths
        .iter()
        .map(|path| path.to_str())
        .collect::<Option<Vec<_>>>()?;
    serde_json::to_string(&utf8).ok()
}

fn parse_changed_paths_json(json: &str) -> Option<Vec<PathBuf>> {
    serde_json::from_str::<Vec<String>>(json)
        .ok()
        .map(|paths| paths.into_iter().map(PathBuf::from).collect())
}

fn relative_utf8(root: &Path, path: &Path) -> Result<String> {
    path.strip_prefix(root)
        .map_err(|_| Error::Structural(format!("path escapes project root: {}", path.display())))?
        .to_str()
        .map(str::to_owned)
        .ok_or_else(|| Error::Structural(format!("non-UTF-8 source path: {}", path.display())))
}

fn normalize_without_fs(path: &Path) -> PathBuf {
    use std::path::Component;

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn escape_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn unix_time_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
        })
}

const fn usize_to_u64(value: usize) -> u64 {
    value as u64
}

fn usize_to_i64(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn u64_to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn i64_to_u64(value: i64) -> Result<u64> {
    u64::try_from(value)
        .map_err(|_| Error::Structural(format!("negative source span stored: {value}")))
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use tempfile::TempDir;

    use super::*;
    use crate::projects::ProjectRegistry;

    fn fixture() -> (TempDir, Database, ResolvedProject) {
        let temp = TempDir::new().expect("temp dir");
        let root = temp.path().join("repo");
        fs::create_dir(&root).expect("repo");
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(&root)
            .status()
            .expect("git init");
        fs::write(
            root.join("lib.rs"),
            "use std::path::Path;\npub struct Engine;\nimpl Engine { pub fn run() { helper(); } }\nfn helper() {}\n",
        )
        .expect("source");
        fs::write(
            root.join("worker.py"),
            "import asyncio\nclass Worker:\n    def run(self):\n        print('ok')\n",
        )
        .expect("python");
        let database = Database::open(temp.path().join("brain.sqlite")).expect("database");
        let project = ProjectRegistry::new(database.clone())
            .resolve_or_register(&root)
            .expect("project");
        (temp, database, project)
    }

    #[test]
    fn native_provider_indexes_and_queries_multiple_languages() {
        let (_temp, database, project) = fixture();
        let world = WorldModel::new(database);
        let provider = TreeSitterProvider::new(100, 1_000_000, 10_000, 20_000, 16_000_000);
        let report = world.index(&provider, &project, None).expect("index");
        assert_eq!(report.parsed_files, 2);
        assert!(report.nodes_written >= 7);
        assert!(report.edges_written >= 5);

        let engines = world
            .search_symbols(&project.project.id, "Engine", 10)
            .expect("search");
        assert_eq!(engines[0].label, "Engine");
        let slice = world
            .neighbors(&project.project.id, &engines[0].id, 2, 100)
            .expect("neighbors");
        assert!(slice.nodes.len() >= 2);
    }

    #[test]
    fn rust_fields_and_impls_preserve_their_definition_owners() {
        let source = br"
            impl<T> Settings<T> { fn from_env() { let local_only = false; } }
            struct Settings<T> { use_app_guides: bool, r#type: T }
            struct SessionActionRequest { use_app_guides: Option<bool> }
            struct Tuple(bool);
            trait Load { fn from_env(); }
            impl Load for SessionActionRequest { fn from_env() {} }
            mod nested {
                struct Settings { enabled: bool }
                impl self::Settings { fn from_env() {} }
                impl Settings { fn other() {} }
            }
            impl nested::Settings { fn outside() {} }
        ";
        let scan = parse_file("lib.rs", SupportedLanguage::Rust, source, sha256(source))
            .expect("parse Rust");
        let by_label = |label: &str| {
            scan.nodes
                .iter()
                .find(|node| node.label == label)
                .expect("node")
        };
        assert_eq!(
            by_label("Settings").search_aliases,
            "Settings::type\nSettings::use_app_guides\ntype\nuse_app_guides"
        );
        assert_eq!(
            by_label("SessionActionRequest").search_aliases,
            "SessionActionRequest::use_app_guides\nuse_app_guides"
        );
        assert!(by_label("Tuple").search_aliases.is_empty());
        assert!(
            scan.nodes
                .iter()
                .all(|node| !node.search_aliases.contains("local_only"))
        );
        for (owner, method) in [
            ("Settings", "Settings::from_env"),
            ("SessionActionRequest", "SessionActionRequest::from_env"),
            ("nested::Settings", "nested::Settings::from_env"),
            ("nested::Settings", "nested::Settings::other"),
            ("nested::Settings", "nested::Settings::outside"),
        ] {
            let method = by_label(method);
            let definitions: Vec<_> = scan
                .edges
                .iter()
                .filter(|edge| edge.relation == "DEFINES" && edge.target_key == method.stable_key)
                .collect();
            assert_eq!(definitions.len(), 1);
            assert_eq!(definitions[0].source_key, by_label(owner).stable_key);
        }
    }

    #[test]
    fn rust_field_aliases_are_bounded_and_leave_other_languages_unchanged() {
        let fields = (0..1_000).fold(String::new(), |mut fields, n| {
            use std::fmt::Write as _;
            write!(fields, "field_{n}: bool,").unwrap();
            fields
        });
        let source = format!("struct Many {{ {fields} }}");
        let scan = parse_file(
            "lib.rs",
            SupportedLanguage::Rust,
            source.as_bytes(),
            sha256(source.as_bytes()),
        )
        .expect("parse");
        let owner = scan
            .nodes
            .iter()
            .find(|node| node.label == "Many")
            .expect("owner");
        assert_eq!(owner.search_aliases.lines().count(), 256);
        assert!(owner.search_aliases.len() <= 16 * 1024);
        let aliases_bytes = owner.search_aliases.len();
        let mut without_aliases = scan.clone();
        without_aliases
            .nodes
            .iter_mut()
            .for_each(|node| node.search_aliases.clear());
        assert_eq!(
            file_scan_heap_bytes(&scan) - file_scan_heap_bytes(&without_aliases),
            usize_to_u64(aliases_bytes)
        );
        let source = b"class Settings:\n    use_app_guides: bool\n";
        let python = parse_file(
            "config.py",
            SupportedLanguage::Python,
            source,
            sha256(source),
        )
        .expect("Python");
        assert!(
            python
                .nodes
                .iter()
                .all(|node| node.search_aliases.is_empty())
        );
    }

    #[test]
    fn owner_aliases_follow_field_changes_and_file_removal() {
        let (_temp, database, project) = fixture();
        let path = project.root.join("lib.rs");
        fs::write(&path, "struct ActionRequest { use_app_guides: bool }").expect("source");
        let world = WorldModel::new(database.clone());
        let provider = TreeSitterProvider::new(100, 1_000_000, 10_000, 20_000, 16_000_000);
        world
            .index(&provider, &project, None)
            .expect("initial index");
        let matches = |field: &str| {
            database
                .with_reader(|connection| {
                    Ok(connection.query_row(
                        "SELECT count(*) FROM source_node_fts f JOIN nodes n ON n.rowid=f.rowid
                     WHERE source_node_fts MATCH ?1 AND n.project_id=?2 AND n.kind='Type'",
                        params![format!("\"{field}\""), project.project.id.as_str()],
                        |row| row.get::<_, i64>(0),
                    )?)
                })
                .expect("FTS count")
        };
        assert_eq!(matches("use_app_guides"), 1);
        fs::write(&path, "struct ActionRequest { use_task_guides: bool }").expect("edit");
        let changed = [PathBuf::from("lib.rs")];
        world
            .index(&provider, &project, Some(&changed))
            .expect("refresh");
        assert_eq!(matches("use_app_guides"), 0);
        assert_eq!(matches("use_task_guides"), 1);
        fs::remove_file(&path).expect("remove");
        world
            .index(&provider, &project, Some(&changed))
            .expect("remove index");
        assert_eq!(matches("use_task_guides"), 0);
    }

    #[test]
    fn native_extractor_upgrade_revisits_unchanged_files_after_a_partial_scan() {
        let (_temp, database, project) = fixture();
        fs::write(
            project.root.join("lib.rs"),
            "struct Request { use_app_guides: bool }",
        )
        .expect("source");
        let world = WorldModel::new(database.clone());
        let provider = TreeSitterProvider::new(100, 1_000_000, 10_000, 20_000, 16_000_000);
        world
            .index(&provider, &project, None)
            .expect("initial index");
        database.write_immediate(|tx| {
            tx.execute("UPDATE structural_scans SET provider_version='tree-sitter-v1'", [])?;
            tx.execute("UPDATE nodes SET search_aliases=''", [])?;
            tx.execute("UPDATE nodes SET extractor_version='tree-sitter-v1' WHERE file_path='lib.rs'", [])?;
            // A newer partial scan must not hide older extraction in lib.rs.
            tx.execute(
                "INSERT INTO structural_scans
                 (id,project_id,provider,provider_version,head,diff_fingerprint,
                  started_at_ms,completed_at_ms,file_count,node_count,edge_count,incremental,changed_paths_json)
                 SELECT 'partial-upgrade',project_id,provider,?1,head,diff_fingerprint,
                        started_at_ms,completed_at_ms,file_count,node_count,edge_count,1,changed_paths_json
                 FROM structural_scans LIMIT 1", [EXTRACTOR_VERSION],
            )?;
            tx.execute("UPDATE indexed_files SET scan_id='partial-upgrade' WHERE path='worker.py'", [])?;
            Ok(())
        }).expect("simulate old extraction");
        let status = world
            .latest_index_status(&project.project.id)
            .expect("status")
            .expect("scan");
        assert_eq!(status.scan_id, "partial-upgrade");
        assert!(status.needs_reindex);
        let upgraded = world
            .index(&provider, &project, Some(&[]))
            .expect("upgrade");
        assert!(!upgraded.incremental);
        assert_eq!(upgraded.parsed_files, 1);
        assert_eq!(upgraded.unchanged_files, 1);
        assert!(
            !world
                .latest_index_status(&project.project.id)
                .expect("status")
                .expect("scan")
                .needs_reindex
        );
        let aliases: String = database.with_reader(|connection| {
            Ok(connection.query_row("SELECT search_aliases FROM nodes WHERE label='Request' AND tombstoned_at_ms IS NULL", [], |row| row.get(0))?)
        }).expect("aliases");
        assert!(aliases.lines().any(|alias| alias == "use_app_guides"));
        let unchanged = world.index(&provider, &project, None).expect("unchanged");
        assert_eq!(unchanged.parsed_files, 0);
        assert_eq!(unchanged.unchanged_files, 2);
    }

    #[test]
    fn native_upgrade_preserves_method_anchors_but_source_changes_still_invalidate() {
        assert_native_upgrade_identity(false, false);
    }

    #[test]
    fn dirty_native_upgrade_conservatively_retires_v1_symbol_anchors() {
        assert_native_upgrade_identity(true, false);
    }

    #[test]
    fn recreated_v1_file_cannot_reuse_retired_symbol_anchors() {
        assert_native_upgrade_identity(true, true);
    }

    #[allow(clippy::too_many_lines)]
    fn assert_native_upgrade_identity(dirty_before_upgrade: bool, recreated_file: bool) {
        use crate::memory::{MemoryDraft, MemoryStatus, MemoryStore};

        let (_temp, database, project) = fixture();
        // Identical method/free-function bodies exercise the v1 stable-key
        // collision as well as descendants; digests alone cannot tell them apart.
        let source = "struct Settings;\nimpl Settings { fn from_env() -> u8 { fn read_flag() -> u8 { 1 } read_flag() } }\nfn from_env() -> u8 { fn read_flag() -> u8 { 1 } read_flag() }\n";
        fs::write(project.root.join("lib.rs"), source).expect("source");
        let mut legacy = parse_file(
            "lib.rs",
            SupportedLanguage::Rust,
            source.as_bytes(),
            sha256(source.as_bytes()),
        )
        .expect("parse");
        // Recreate native-v1 labels and lexical ownership, using identical
        // source spans/digests rather than fabricated matching IDs.
        let mut lexical_nodes: Vec<_> = legacy.nodes.iter().collect();
        lexical_nodes.sort_by_key(|node| node.start_byte);
        let mut taken = BTreeSet::new();
        let mut keys = BTreeMap::new();
        for node in lexical_nodes {
            let base = node.stable_key.replace("Settings::", "");
            let key = if taken.insert(base.clone()) {
                base
            } else {
                format!("{base}:{}", node.start_byte)
            };
            keys.insert(node.stable_key.clone(), key);
        }
        for node in &mut legacy.nodes {
            node.stable_key = keys[&node.stable_key].clone();
            node.label = node.label.replace("Settings::", "");
        }
        for edge in &mut legacy.edges {
            edge.source_key = keys[&edge.source_key].clone();
            edge.target_key = keys[&edge.target_key].clone();
            if edge.relation == "DEFINES" && edge.target_key == "symbol:lib.rs:Function:from_env" {
                edge.source_key = "file:lib.rs".to_owned();
            }
            edge.digest = sha256(
                format!(
                    "{}\0{}\0{}",
                    edge.source_key, edge.relation, edge.target_key
                )
                .as_bytes(),
            );
        }
        let world = WorldModel::new(database.clone());
        world
            .apply_scan(
                &project,
                ScanResult {
                    provider: "tree_sitter".to_owned(),
                    provider_version: "tree-sitter-v1".to_owned(),
                    incremental: false,
                    seen_paths: BTreeSet::from(["lib.rs".to_owned()]),
                    deleted_paths: BTreeSet::new(),
                    unchanged_count: 0,
                    files: vec![legacy],
                },
            )
            .expect("legacy index");
        let find = |label: &str| {
            world
                .search_symbols(&project.project.id, label, 20)
                .expect("symbols")
                .into_iter()
                .find(|node| node.label == label)
                .expect("symbol")
        };
        let method = find("from_env");
        let descendant = find("from_env::read_flag");
        let reference = find("read_flag");
        let global = world
            .search_symbols(&project.project.id, "from_env", 20)
            .expect("global")
            .into_iter()
            .find(|node| {
                node.start_byte
                    == Some(usize_to_u64(
                        source.rfind("fn from_env").expect("global offset"),
                    ))
            })
            .expect("global function");
        let call_edge: String = database
            .with_reader(|connection| {
                Ok(connection.query_row(
                    "SELECT id FROM edges WHERE source_id=?1 AND target_id=?2 AND relation='CALLS'",
                    params![method.id.as_str(), reference.id.as_str()],
                    |row| row.get(0),
                )?)
            })
            .expect("call edge");
        let memories = MemoryStore::new(database.clone());
        let memory = memories
            .create_candidate(
                &project.project.id,
                &MemoryDraft {
                    scope_type: "project".to_owned(),
                    scope_id: project.project.id.to_string(),
                    kind: "lesson".to_owned(),
                    claim: "Settings reads a flag through its local helper.".to_owned(),
                    rationale: "Source inspection.".to_owned(),
                    confidence: 0.5,
                    importance: 0.5,
                    tags: Vec::new(),
                    valid_from_commit: None,
                    valid_to_commit: None,
                    trust_source: "agent_inference".to_owned(),
                    evidence_ids: Vec::new(),
                    source_task_ids: Vec::new(),
                    node_ids: vec![
                        method.id.clone(),
                        descendant.id.clone(),
                        reference.id.clone(),
                    ],
                },
            )
            .expect("anchored memory");
        let provider = TreeSitterProvider::new(100, 1_000_000, 10_000, 20_000, 16_000_000);
        let historical = NodeId::new();
        database.write_immediate(|tx| {
            // A deleted v1 module function can occupy the new impl method key
            // with the same body digest. It must never be reactivated as it.
            tx.execute(
                "INSERT INTO nodes(id,project_id,kind,stable_key,label,file_path,language,start_byte,end_byte,current_digest,source_kind,source_ref,extractor_version,observed_at_ms,tombstoned_at_ms)
                 SELECT ?1,project_id,kind,'symbol:lib.rs:Function:Settings::from_env','Settings::from_env',file_path,language,start_byte,end_byte,current_digest,source_kind,source_ref,extractor_version,observed_at_ms,1 FROM nodes WHERE id=?2",
                params![historical.as_str(), method.id.as_str()],
            )?;
            if recreated_file {
                tx.execute("DELETE FROM indexed_files WHERE project_id=?1 AND path='lib.rs'", [project.project.id.as_str()])?;
                tx.execute("UPDATE nodes SET tombstoned_at_ms=1 WHERE project_id=?1 AND file_path='lib.rs'", [project.project.id.as_str()])?;
            }
            Ok(())
        }).expect("historical tombstone");
        if dirty_before_upgrade {
            // Even unchanged spans/digests cannot establish ownership after a
            // concurrent source edit. The upgrade deliberately invalidates.
            fs::write(
                project.root.join("lib.rs"),
                format!("{source}\n// concurrent edit\n"),
            )
            .expect("dirty source");
        }
        let requested_paths = if recreated_file {
            vec![PathBuf::from("lib.rs")]
        } else {
            Vec::new()
        };
        world
            .index(&provider, &project, Some(&requested_paths))
            .expect("upgrade");
        assert_ne!(find("Settings::from_env").id, historical);
        assert!(
            database
                .with_reader(|connection| {
                    Ok(connection.query_row(
                        "SELECT tombstoned_at_ms IS NOT NULL FROM nodes WHERE id=?1",
                        [historical.as_str()],
                        |row| row.get::<_, bool>(0),
                    )?)
                })
                .unwrap()
        );
        if dirty_before_upgrade {
            assert_ne!(find("Settings::from_env").id, method.id);
            assert_ne!(find("Settings::from_env::read_flag").id, descendant.id);
            assert_ne!(find("from_env").id, global.id);
            assert_eq!(
                memories
                    .mark_symbol_drift_suspect(&project.project.id)
                    .expect("dirty upgrade drift")
                    .invalidated,
                1
            );
            assert_eq!(
                memories
                    .get(&memory.id)
                    .expect("memory")
                    .expect("record")
                    .status,
                MemoryStatus::Suspect
            );
            return;
        }
        assert_eq!(find("Settings::from_env").id, method.id);
        assert_eq!(find("from_env").id, global.id);
        assert_ne!(method.id, global.id);
        assert_eq!(find("Settings::from_env::read_flag").id, descendant.id);
        assert_eq!(find("read_flag").id, reference.id);
        let active_call: bool = database
            .with_reader(|connection| {
                Ok(connection.query_row(
                    "SELECT tombstoned_at_ms IS NULL FROM edges WHERE id=?1",
                    [&call_edge],
                    |row| row.get(0),
                )?)
            })
            .expect("preserved call");
        assert!(active_call);
        assert_eq!(
            memories
                .mark_symbol_drift_suspect(&project.project.id)
                .expect("drift")
                .invalidated,
            0
        );
        assert_eq!(
            memories
                .get(&memory.id)
                .expect("memory")
                .expect("record")
                .status,
            MemoryStatus::Active
        );
        fs::write(
            project.root.join("lib.rs"),
            source.replace("{ 1 }", "{ 2 }"),
        )
        .expect("edit");
        world
            .index(&provider, &project, Some(&[PathBuf::from("lib.rs")]))
            .expect("changed source");
        assert_eq!(
            memories
                .mark_symbol_drift_suspect(&project.project.id)
                .expect("changed drift")
                .invalidated,
            1
        );
        assert_eq!(
            memories
                .get(&memory.id)
                .expect("memory")
                .expect("record")
                .status,
            MemoryStatus::Suspect
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn new_languages_and_markdown_extract_symbols() {
        let temp = TempDir::new().expect("temp");
        let root = temp.path().join("poly");
        fs::create_dir_all(&root).expect("root");
        fs::write(
            root.join("svc.go"),
            "package svc\n\nimport \"fmt\"\n\ntype Handler struct{}\n\nfunc Serve() { fmt.Println(\"ok\") }\n\nfunc (h Handler) Route() { Serve() }\n",
        )
        .expect("go");
        fs::write(
            root.join("App.java"),
            "import java.util.List;\npublic class App {\n  public void boot() { helper(); }\n  private void helper() {}\n}\ninterface Api {}\n",
        )
        .expect("java");
        fs::write(
            root.join("Main.kt"),
            "import kotlin.io.println\n\nclass Session {}\n\nfun launch() { println(\"go\") }\n",
        )
        .expect("kotlin");
        fs::write(
            root.join("GUIDE.md"),
            "# Operations guide\n\nIntro.\n\n## Deploy steps\n\n```sh\n# not a heading\n```\n\n## Rollback\n",
        )
        .expect("markdown");
        fs::write(
            root.join("core.c"),
            "#include <stdio.h>\nstruct Packet { int id; };\nint checksum(struct Packet p) { return p.id; }\n",
        )
        .expect("c");
        fs::write(
            root.join("engine.cpp"),
            "class Reactor { public: void ignite(); };\nvoid Reactor::ignite() { }\nint boot() { return 0; }\n",
        )
        .expect("cpp");
        fs::write(
            root.join("Billing.cs"),
            "using System;\npublic interface IInvoice {}\npublic class Billing {\n  public void Charge() { Console.WriteLine(\"ok\"); }\n}\n",
        )
        .expect("csharp");
        fs::write(
            root.join("worker.rb"),
            "class Worker\n  def perform\n    puts 'ok'\n  end\nend\n",
        )
        .expect("ruby");
        fs::write(
            root.join("index.php"),
            "<?php\nclass Cart {\n  public function total() { return 0; }\n}\nfunction render() { }\n",
        )
        .expect("php");
        fs::write(
            root.join("deploy.sh"),
            "#!/bin/bash\nrollout() {\n  echo done\n}\n",
        )
        .expect("bash");
        fs::write(
            root.join("Api.scala"),
            "trait Api {}\nobject Server {\n  def start(): Unit = {}\n}\nclass Router {}\n",
        )
        .expect("scala");
        let database = Database::open(temp.path().join("brain.sqlite")).expect("database");
        let project = ProjectRegistry::new(database.clone())
            .resolve_or_register(&root)
            .expect("project");
        let world = WorldModel::new(database);
        let provider = TreeSitterProvider::new(100, 1_000_000, 10_000, 20_000, 16_000_000);
        let report = world.index(&provider, &project, None).expect("index");
        assert_eq!(report.parsed_files, 11);

        let find = |query: &str| {
            world
                .search_symbols(&project.project.id, query, 10)
                .expect("search")
        };
        assert!(find("Serve").iter().any(|n| n.kind == "Function"), "go fn");
        assert!(find("Handler").iter().any(|n| n.kind == "Type"), "go type");
        assert!(
            find("Route").iter().any(|n| n.kind == "Method"),
            "go method"
        );
        assert!(find("App").iter().any(|n| n.kind == "Type"), "java class");
        assert!(
            find("Api").iter().any(|n| n.kind == "Interface"),
            "java interface"
        );
        assert!(
            find("boot").iter().any(|n| n.kind == "Method"),
            "java method"
        );
        assert!(
            find("Session").iter().any(|n| n.kind == "Type"),
            "kotlin class"
        );
        assert!(
            find("launch").iter().any(|n| n.kind == "Function"),
            "kotlin fn"
        );
        assert!(
            find("Deploy steps").iter().any(|n| n.kind == "Section"),
            "markdown heading"
        );
        assert!(
            find("not a heading").is_empty(),
            "fenced code lines are not sections"
        );
        assert!(
            find("checksum").iter().any(|n| n.kind == "Function"),
            "c fn via declarator"
        );
        assert!(find("Packet").iter().any(|n| n.kind == "Type"), "c struct");
        assert!(
            find("Reactor").iter().any(|n| n.kind == "Type"),
            "cpp class"
        );
        assert!(find("boot").iter().any(|n| n.kind == "Function"), "cpp fn");
        assert!(find("Billing").iter().any(|n| n.kind == "Type"), "cs class");
        assert!(
            find("IInvoice").iter().any(|n| n.kind == "Interface"),
            "cs interface"
        );
        assert!(
            find("Charge").iter().any(|n| n.kind == "Method"),
            "cs method"
        );
        assert!(
            find("Worker").iter().any(|n| n.kind == "Type"),
            "ruby class"
        );
        assert!(
            find("perform").iter().any(|n| n.kind == "Function"),
            "ruby method"
        );
        assert!(find("Cart").iter().any(|n| n.kind == "Type"), "php class");
        assert!(
            find("render").iter().any(|n| n.kind == "Function"),
            "php fn"
        );
        assert!(
            find("rollout").iter().any(|n| n.kind == "Function"),
            "bash fn"
        );
        assert!(
            find("Api").iter().any(|n| n.kind == "Interface"),
            "scala trait"
        );
        assert!(
            find("Server").iter().any(|n| n.kind == "Type"),
            "scala object"
        );
        assert!(
            find("start").iter().any(|n| n.kind == "Function"),
            "scala def"
        );
    }

    #[test]
    fn markdown_link_labels_extract_wikilinks_and_targets() {
        let labels = markdown_link_labels(
            "See [[Deploy steps]] and [runbook](docs/runbook.md#top), not [x](https://e.com).",
        );
        assert_eq!(
            labels,
            vec!["Deploy steps".to_owned(), "docs/runbook.md".to_owned()]
        );
        assert!(markdown_link_labels("no links here").is_empty());
    }

    #[test]
    fn unchanged_files_are_not_reparsed_and_deleted_files_are_tombstoned() {
        let (_temp, database, project) = fixture();
        let world = WorldModel::new(database);
        let provider = TreeSitterProvider::new(100, 1_000_000, 10_000, 20_000, 16_000_000);
        world.index(&provider, &project, None).expect("first index");
        let unchanged = world
            .index(&provider, &project, None)
            .expect("second index");
        assert_eq!(unchanged.parsed_files, 0);
        assert_eq!(unchanged.unchanged_files, 2);

        fs::remove_file(project.root.join("worker.py")).expect("remove");
        let removed = world
            .index(&provider, &project, None)
            .expect("remove index");
        assert_eq!(removed.removed_files, 1);
        assert!(
            world
                .search_symbols(&project.project.id, "Worker", 10)
                .expect("search")
                .is_empty()
        );
    }

    #[test]
    fn incremental_scan_rejects_paths_outside_project() {
        let (temp, database, project) = fixture();
        let world = WorldModel::new(database);
        let provider = TreeSitterProvider::new(100, 1_000_000, 10_000, 20_000, 16_000_000);
        let outside = temp.path().join("outside.rs");
        fs::write(&outside, "fn outside() {}").expect("outside");
        let error = world
            .index(&provider, &project, Some(&[outside]))
            .expect_err("outside path");
        assert!(error.to_string().contains("escapes"));
    }

    #[test]
    fn native_scan_rejects_prepared_graph_over_budget() {
        let (_temp, database, project) = fixture();
        let world = WorldModel::new(database);
        let provider = TreeSitterProvider::new(100, 1_000_000, 1, 1, 1_024);
        let error = world
            .index(&provider, &project, None)
            .expect_err("prepared limits");
        assert!(error.to_string().contains("prepared limits"));
    }
}
