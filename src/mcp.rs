//! Stateless MCP edge over stdio with legacy handshake compatibility.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::context::{ContextCompileRequest, ContextMode};
use crate::daemon::{
    AttemptCreateRequest, ContextExplainRequest, DaemonClient, EvidenceCreateRequest,
    MemoryCreateRequest, MemoryListRequest, PastFailuresRequest, TaskFinishRequest,
    TaskStartRequest, WorldNeighborsRequest, WorldSearchRequest,
};
use crate::error::{Error, Result};
use crate::experience::{AttemptOutcome, EvidenceStrength, TaskOutcome};
use crate::ids::{AttemptId, ContextRunId, MemoryId, NodeId, ProjectId, TaskId};
use crate::learning::DistillationRequest;
use crate::memory::MemoryDraft;

const MODERN_PROTOCOL: &str = "2026-07-28";
const LEGACY_PROTOCOL: &str = "2025-11-25";

#[derive(Clone, Debug)]
pub struct McpServer {
    client: DaemonClient,
    max_frame_bytes: usize,
}

impl McpServer {
    #[must_use]
    pub fn new(socket_path: impl Into<PathBuf>, timeout: Duration, max_frame_bytes: usize) -> Self {
        Self {
            client: DaemonClient::new(socket_path, timeout),
            max_frame_bytes,
        }
    }

    /// Run newline-delimited JSON-RPC over stdin/stdout.
    ///
    /// Both the stateless 2026-07-28 methods and the legacy initialize probe
    /// used by existing Claude Code releases are accepted. No application
    /// state lives in this adapter.
    ///
    /// # Errors
    ///
    /// Returns an error for stdio failures. Individual tool failures are MCP
    /// tool results with `isError: true`, not protocol transport failures.
    pub async fn serve_stdio(&self) -> Result<()> {
        let mut input = BufReader::new(tokio::io::stdin());
        let mut stdout = tokio::io::stdout();
        let mut frame = Vec::new();
        while read_bounded_line(&mut input, &mut frame, self.max_frame_bytes).await? {
            let line = std::str::from_utf8(&frame)
                .map_err(|error| Error::Daemon(format!("MCP frame is not UTF-8: {error}")))?;
            if line.trim().is_empty() {
                continue;
            }
            let incoming = match serde_json::from_str::<Value>(line) {
                Ok(value) => value,
                Err(error) => {
                    write_message(
                        &mut stdout,
                        &protocol_error(&Value::Null, -32700, &error.to_string()),
                    )
                    .await?;
                    continue;
                }
            };
            if let Some(batch) = incoming.as_array() {
                let mut responses = Vec::new();
                for request in batch {
                    if let Some(response) = self.handle(request).await {
                        responses.push(response);
                    }
                }
                if !responses.is_empty() {
                    write_message(&mut stdout, &Value::Array(responses)).await?;
                }
            } else if let Some(response) = self.handle(&incoming).await {
                write_message(&mut stdout, &response).await?;
            }
        }
        Ok(())
    }

    async fn handle(&self, request: &Value) -> Option<Value> {
        let id = request.get("id").cloned()?;
        let method = request.get("method").and_then(Value::as_str);
        let Some(method) = method else {
            return Some(protocol_error(&id, -32600, "missing JSON-RPC method"));
        };
        let result = match method {
            "initialize" => Ok(initialize_result(request)),
            "server/discover" => Ok(discover_result()),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(tool_catalog()),
            "tools/call" => self.call_tool(request.get("params")).await,
            _ => return Some(protocol_error(&id, -32601, "method not found")),
        };
        Some(match result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(error) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "content": [{"type": "text", "text": error.to_string()}],
                    "isError": true
                }
            }),
        })
    }

    #[allow(clippy::too_many_lines)]
    async fn call_tool(&self, params: Option<&Value>) -> Result<Value> {
        let params =
            params.ok_or_else(|| Error::Config("tools/call requires params".to_owned()))?;
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Config("tools/call requires a name".to_owned()))?;
        let arguments = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let output = match name {
            "leb_project_status" => {
                let path = required_string(&arguments, "path")?;
                serde_json::to_value(
                    self.client
                        .resolve_project(PathBuf::from(path).as_path())
                        .await?,
                )?
            }
            "leb_query_world" => {
                let project_id = ProjectId::parse(required_string(&arguments, "project_id")?)?;
                let query = required_string(&arguments, "query")?.to_owned();
                let limit = optional_u64(&arguments, "limit").unwrap_or(20);
                let limit = usize::try_from(limit)
                    .map_err(|_| Error::Config("limit is too large".to_owned()))?;
                serde_json::to_value(
                    self.client
                        .search_world(&WorldSearchRequest {
                            project_id,
                            query,
                            limit,
                        })
                        .await?,
                )?
            }
            "leb_recall" => serde_json::to_value(
                self.client
                    .compile_context(&recall_request(&arguments)?)
                    .await?,
            )?,
            "leb_explain_context" => {
                let run_id = ContextRunId::parse(required_string(&arguments, "context_run_id")?)?;
                let ordinal = u32::try_from(optional_u64(&arguments, "ordinal").unwrap_or(1))
                    .map_err(|_| Error::Config("ordinal is too large".to_owned()))?;
                serde_json::to_value(
                    self.client
                        .explain_context(&ContextExplainRequest { run_id, ordinal })
                        .await?,
                )?
            }
            "leb_get_past_failures" => {
                let project_id = ProjectId::parse(required_string(&arguments, "project_id")?)?;
                let limit = usize::try_from(optional_u64(&arguments, "limit").unwrap_or(20))
                    .map_err(|_| Error::Config("limit is too large".to_owned()))?;
                serde_json::to_value(
                    self.client
                        .past_failures(&PastFailuresRequest {
                            project_id,
                            query: arguments
                                .get("query")
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                            limit,
                        })
                        .await?,
                )?
            }
            "leb_get_memory" => {
                let id = MemoryId::parse(required_string(&arguments, "memory_id")?)?;
                serde_json::to_value(self.client.get_memory(&id).await?)?
            }
            "leb_record_human_decision" => serde_json::to_value(
                self.client
                    .create_memory(&human_decision_request(&arguments)?)
                    .await?,
            )?,
            "leb_get_skill" => {
                let id = MemoryId::parse(required_string(&arguments, "memory_id")?)?;
                let memory = self.client.get_memory(&id).await?;
                if memory
                    .as_ref()
                    .is_some_and(|memory| !matches!(memory.kind.as_str(), "skill" | "procedure"))
                {
                    return Err(Error::Config(format!(
                        "memory `{id}` is not a skill/procedure"
                    )));
                }
                serde_json::to_value(memory)?
            }
            "leb_task_start" => {
                let project_id = ProjectId::parse(required_string(&arguments, "project_id")?)?;
                serde_json::to_value(
                    self.client
                        .start_task(&TaskStartRequest {
                            project_id,
                            prompt: required_string(&arguments, "prompt")?.to_owned(),
                            session_id: arguments
                                .get("session_id")
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                        })
                        .await?,
                )?
            }
            "leb_record_attempt" => {
                let task_id = TaskId::parse(required_string(&arguments, "task_id")?)?;
                serde_json::to_value(
                    self.client
                        .record_attempt(
                            &task_id,
                            &AttemptCreateRequest {
                                strategy_summary: required_string(&arguments, "summary")?
                                    .to_owned(),
                                outcome: parse_attempt_outcome(required_string(
                                    &arguments, "outcome",
                                )?)?,
                                files: string_array(&arguments, "files")?,
                            },
                        )
                        .await?,
                )?
            }
            "leb_record_evidence" => {
                let project_id = ProjectId::parse(required_string(&arguments, "project_id")?)?;
                serde_json::to_value(
                    self.client
                        .record_evidence(&EvidenceCreateRequest {
                            project_id,
                            task_id: arguments
                                .get("task_id")
                                .and_then(Value::as_str)
                                .map(TaskId::parse)
                                .transpose()?,
                            attempt_id: arguments
                                .get("attempt_id")
                                .and_then(Value::as_str)
                                .map(AttemptId::parse)
                                .transpose()?,
                            kind: required_string(&arguments, "kind")?.to_owned(),
                            strength: parse_strength(required_string(&arguments, "strength")?)?,
                            commit_hash: None,
                            command: arguments
                                .get("command")
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                            exit_code: arguments
                                .get("exit_code")
                                .and_then(Value::as_i64)
                                .and_then(|code| i32::try_from(code).ok()),
                            artifact_text: arguments
                                .get("artifact")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_owned(),
                            metadata: json!({}),
                        })
                        .await?,
                )?
            }
            "leb_learn" => {
                let project_id = ProjectId::parse(required_string(&arguments, "project_id")?)?;
                let task_ids = string_array(&arguments, "task_ids")?
                    .iter()
                    .map(|value| TaskId::parse(value))
                    .collect::<Result<Vec<_>>>()?;
                let evidence_ids = string_array(&arguments, "evidence_ids")?
                    .iter()
                    .map(|value| crate::ids::EvidenceId::parse(value))
                    .collect::<Result<Vec<_>>>()?;
                serde_json::to_value(
                    self.client
                        .distill(&DistillationRequest {
                            memory: MemoryDraft {
                                scope_type: "project".to_owned(),
                                scope_id: project_id.to_string(),
                                kind: arguments
                                    .get("kind")
                                    .and_then(Value::as_str)
                                    .unwrap_or("lesson")
                                    .to_owned(),
                                claim: required_string(&arguments, "claim")?.to_owned(),
                                rationale: arguments
                                    .get("rationale")
                                    .and_then(Value::as_str)
                                    .unwrap_or("Evidence-linked task distillation")
                                    .to_owned(),
                                confidence: 0.5,
                                importance: 0.7,
                                tags: string_array(&arguments, "tags")?,
                                valid_from_commit: None,
                                valid_to_commit: None,
                                trust_source: "agent_inference".to_owned(),
                                evidence_ids,
                                source_task_ids: task_ids,
                                node_ids: Vec::new(),
                            },
                            project_id,
                        })
                        .await?,
                )?
            }
            "leb_workspace" => {
                let snapshot = self.client.workspace().await?;
                let summary = snapshot
                    .projects
                    .iter()
                    .map(|project| {
                        json!({
                            "project_id": project.id,
                            "name": project.name,
                            "facts": project.facts,
                            "links": project.links,
                            "technologies": project.technologies
                                .iter()
                                .map(|tech| tech.name.clone())
                                .collect::<Vec<_>>(),
                            "validated_memories": project.validated_memories,
                        })
                    })
                    .collect::<Vec<_>>();
                json!(summary)
            }
            "leb_symbol_search" => {
                let query = required_string(&arguments, "query")?.to_owned();
                let snapshot = self.client.workspace().await?;
                let mut hits = Vec::new();
                for project in &snapshot.projects {
                    let Ok(nodes) = self
                        .client
                        .search_world(&WorldSearchRequest {
                            project_id: project.id.clone(),
                            query: query.clone(),
                            limit: 6,
                        })
                        .await
                    else {
                        continue;
                    };
                    for node in nodes {
                        if node.kind == "ExternalReference" {
                            continue;
                        }
                        hits.push(json!({
                            "project": project.name,
                            "kind": node.kind,
                            "label": node.label,
                            "file_path": node.file_path,
                        }));
                        if hits.len() >= 30 {
                            break;
                        }
                    }
                }
                json!(hits)
            }
            "leb_architecture_guide" => {
                let dossier = self.client.architect_dossier().await?;
                json!({ "dossier": dossier })
            }
            "leb_mark_outcome" => {
                let task_id = TaskId::parse(required_string(&arguments, "task_id")?)?;
                let outcome = parse_task_outcome(required_string(&arguments, "outcome")?)?;
                serde_json::to_value(
                    self.client
                        .finish_task(
                            &task_id,
                            &TaskFinishRequest {
                                outcome,
                                accepted_by_user: arguments
                                    .get("accepted_by_user")
                                    .and_then(Value::as_bool),
                                final_commit: arguments
                                    .get("final_commit")
                                    .and_then(Value::as_str)
                                    .map(str::to_owned),
                            },
                        )
                        .await?,
                )?
            }
            "leb_get_invariants" => {
                let project_id = ProjectId::parse(required_string(&arguments, "project_id")?)?;
                let limit = usize::try_from(optional_u64(&arguments, "limit").unwrap_or(50))
                    .map_err(|_| Error::Config("limit is too large".to_owned()))?;
                serde_json::to_value(
                    self.client
                        .list_memories(&MemoryListRequest {
                            project_id,
                            kind: Some("invariant".to_owned()),
                            limit,
                        })
                        .await?,
                )?
            }
            "leb_get_impact" => {
                let project_id = ProjectId::parse(required_string(&arguments, "project_id")?)?;
                let node_id = NodeId::parse(required_string(&arguments, "node_id")?)?;
                let depth = usize::try_from(optional_u64(&arguments, "max_depth").unwrap_or(3))
                    .map_err(|_| Error::Config("max_depth is too large".to_owned()))?;
                let max_nodes =
                    usize::try_from(optional_u64(&arguments, "max_nodes").unwrap_or(500))
                        .map_err(|_| Error::Config("max_nodes is too large".to_owned()))?;
                serde_json::to_value(
                    self.client
                        .world_neighbors(&WorldNeighborsRequest {
                            project_id,
                            node_id,
                            max_depth: depth,
                            max_nodes,
                        })
                        .await?,
                )?
            }
            "leb_forget" => {
                let project_id = ProjectId::parse(required_string(&arguments, "project_id")?)?;
                let id = MemoryId::parse(required_string(&arguments, "memory_id")?)?;
                let memory = self
                    .client
                    .get_memory(&id)
                    .await?
                    .ok_or_else(|| Error::Config(format!("unknown memory `{id}`")))?;
                if memory.project_id != project_id {
                    return Err(Error::Config(
                        "memory does not belong to the supplied project".to_owned(),
                    ));
                }
                serde_json::to_value(self.client.forget_memory(&id).await?)?
            }
            _ => return Err(Error::Config(format!("unknown LEB tool `{name}`"))),
        };
        Ok(json!({
            "content": [{"type": "text", "text": serde_json::to_string_pretty(&output)?}],
            "structuredContent": output,
            "isError": false
        }))
    }
}

async fn read_bounded_line<R>(
    reader: &mut R,
    frame: &mut Vec<u8>,
    max_frame_bytes: usize,
) -> Result<bool>
where
    R: AsyncBufRead + Unpin,
{
    if max_frame_bytes == 0 {
        return Err(Error::Config(
            "MCP frame limit must be greater than zero".to_owned(),
        ));
    }
    frame.clear();
    loop {
        let available = reader
            .fill_buf()
            .await
            .map_err(|error| Error::Daemon(format!("MCP stdin failed: {error}")))?;
        if available.is_empty() {
            return Ok(!frame.is_empty());
        }
        let take = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |position| position + 1);
        if frame.len().saturating_add(take) > max_frame_bytes.saturating_add(1) {
            return Err(Error::Config(format!(
                "MCP input frame exceeds {max_frame_bytes} byte limit"
            )));
        }
        frame.extend_from_slice(&available[..take]);
        reader.consume(take);
        if frame.last() == Some(&b'\n') {
            frame.pop();
            if frame.last() == Some(&b'\r') {
                frame.pop();
            }
            return Ok(true);
        }
    }
}

fn recall_request(arguments: &Value) -> Result<ContextCompileRequest> {
    let project_id = ProjectId::parse(required_string(arguments, "project_id")?)?;
    let budget = optional_u64(arguments, "token_budget").unwrap_or(2_000);
    let token_budget =
        u32::try_from(budget).map_err(|_| Error::Config("token_budget is too large".to_owned()))?;
    Ok(ContextCompileRequest {
        framing: None,
        request_id: format!("mcp_{}", uuid::Uuid::new_v4()),
        project_id,
        task_text: required_string(arguments, "task")?.to_owned(),
        current_paths: string_array(arguments, "current_paths")?,
        token_budget,
        mode: ContextMode::Brain,
        repository_root: None,
        head: None,
        task_id: arguments
            .get("task_id")
            .and_then(Value::as_str)
            .map(TaskId::parse)
            .transpose()?,
    })
}

fn human_decision_request(arguments: &Value) -> Result<MemoryCreateRequest> {
    let project_id = ProjectId::parse(required_string(arguments, "project_id")?)?;
    Ok(MemoryCreateRequest {
        project_id: project_id.clone(),
        memory: MemoryDraft {
            scope_type: "project".to_owned(),
            scope_id: project_id.to_string(),
            kind: "decision".to_owned(),
            claim: required_string(arguments, "claim")?.to_owned(),
            rationale: arguments
                .get("rationale")
                .and_then(Value::as_str)
                .unwrap_or("Explicit human decision")
                .to_owned(),
            confidence: 0.5,
            importance: 0.8,
            tags: string_array(arguments, "tags")?,
            valid_from_commit: arguments
                .get("valid_from_commit")
                .and_then(Value::as_str)
                .map(str::to_owned),
            valid_to_commit: None,
            trust_source: "human_instruction".to_owned(),
            evidence_ids: Vec::new(),
            source_task_ids: Vec::new(),
            node_ids: Vec::new(),
        },
    })
}

fn initialize_result(request: &Value) -> Value {
    let requested = request
        .pointer("/params/protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or(LEGACY_PROTOCOL);
    let protocol = if requested == MODERN_PROTOCOL {
        MODERN_PROTOCOL
    } else {
        LEGACY_PROTOCOL
    };
    json!({
        "protocolVersion": protocol,
        "capabilities": {"tools": {"listChanged": false}},
        "serverInfo": {"name": "leb-mcp", "version": env!("CARGO_PKG_VERSION")}
    })
}

fn discover_result() -> Value {
    json!({
        "protocolVersion": MODERN_PROTOCOL,
        "capabilities": {"tools": {}},
        "_meta": {
            "io.modelcontextprotocol/serverInfo": {
                "name": "leb-mcp",
                "version": env!("CARGO_PKG_VERSION")
            }
        }
    })
}

#[allow(clippy::too_many_lines)]
fn tool_catalog() -> Value {
    json!({
        "tools": [
            tool("leb_project_status", "Resolve project identity and current Git state", &json!({
                "type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"], "additionalProperties": false
            })),
            tool("leb_query_world", "Search normalized structural nodes", &json!({
                "type": "object", "properties": {
                    "project_id": {"type": "string"}, "query": {"type": "string"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 1000}
                }, "required": ["project_id", "query"], "additionalProperties": false
            })),
            tool("leb_recall", "Compile relevant validated context with explanations. Passing the open task_id records truthful usage provenance for procedure-to-skill promotion", &json!({
                "type": "object", "properties": {
                    "project_id": {"type": "string"}, "task": {"type": "string"},
                    "current_paths": {"type": "array", "items": {"type": "string"}},
                    "token_budget": {"type": "integer", "minimum": 1},
                    "task_id": {"type": "string"}
                }, "required": ["project_id", "task"], "additionalProperties": false
            })),
            tool("leb_explain_context", "Explain one selected item from a durable context run", &json!({
                "type": "object", "properties": {
                    "context_run_id": {"type": "string"},
                    "ordinal": {"type": "integer", "minimum": 1}
                }, "required": ["context_run_id", "ordinal"], "additionalProperties": false
            })),
            tool("leb_get_past_failures", "Query demonstrated failed approaches for a project", &json!({
                "type": "object", "properties": {
                    "project_id": {"type": "string"}, "query": {"type": "string"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 1000}
                }, "required": ["project_id"], "additionalProperties": false
            })),
            tool("leb_get_memory", "Inspect one durable memory and its state", &json!({
                "type": "object", "properties": {"memory_id": {"type": "string"}},
                "required": ["memory_id"], "additionalProperties": false
            })),
            tool("leb_record_human_decision", "Record an explicit project-scoped human decision as a candidate", &json!({
                "type": "object", "properties": {
                    "project_id": {"type": "string"}, "claim": {"type": "string"},
                    "rationale": {"type": "string"}, "tags": {"type": "array", "items": {"type": "string"}},
                    "valid_from_commit": {"type": "string"}
                }, "required": ["project_id", "claim"], "additionalProperties": false
            })),
            tool("leb_get_skill", "Inspect a typed skill or procedure memory. Procedures are synthesized from repeated evidence-backed episodes; their rationale carries a procedure_json payload (steps, preconditions, source episodes). Skills are procedures promoted after validation plus repeated successful use", &json!({
                "type": "object", "properties": {"memory_id": {"type": "string"}},
                "required": ["memory_id"], "additionalProperties": false
            })),
            tool("leb_task_start", "Open an explicit task episode BEFORE starting substantive work, so attempts and evidence attach to it and finished work can be learned from", &json!({
                "type": "object", "properties": {
                    "project_id": {"type": "string"},
                    "prompt": {"type": "string", "description": "The user's task, verbatim or summarized"},
                    "session_id": {"type": "string"}
                }, "required": ["project_id", "prompt"], "additionalProperties": false
            })),
            tool("leb_record_attempt", "Record one strategy attempt on an open task (outcome: success|failure|partial|unknown)", &json!({
                "type": "object", "properties": {
                    "task_id": {"type": "string"},
                    "summary": {"type": "string", "description": "What was tried, phrased as the reusable takeaway"},
                    "outcome": {"type": "string"},
                    "files": {"type": "array", "items": {"type": "string"}}
                }, "required": ["task_id", "summary", "outcome"], "additionalProperties": false
            })),
            tool("leb_record_evidence", "Attach verification evidence (a test/build/lint run) to a task. kind: test_result|build_result|lint_result|typecheck_result; strength: deterministic|observational|inferred. Only deterministic exit-0 evidence on successful accepted tasks can later promote memories", &json!({
                "type": "object", "properties": {
                    "project_id": {"type": "string"},
                    "task_id": {"type": "string"},
                    "attempt_id": {"type": "string"},
                    "kind": {"type": "string"},
                    "strength": {"type": "string"},
                    "command": {"type": "string"},
                    "exit_code": {"type": "integer"},
                    "artifact": {"type": "string", "description": "Bounded output excerpt"}
                }, "required": ["project_id", "kind", "strength"], "additionalProperties": false
            })),
            tool("leb_learn", "Distill a finished, evidence-backed task into a candidate memory (kind: lesson|preference). Candidates never inject until promoted through evidence gates", &json!({
                "type": "object", "properties": {
                    "project_id": {"type": "string"},
                    "claim": {"type": "string", "description": "The reusable rule, phrased for a future engineer"},
                    "kind": {"type": "string"},
                    "rationale": {"type": "string"},
                    "task_ids": {"type": "array", "items": {"type": "string"}},
                    "evidence_ids": {"type": "array", "items": {"type": "string"}},
                    "tags": {"type": "array", "items": {"type": "string"}}
                }, "required": ["project_id", "claim", "task_ids", "evidence_ids"], "additionalProperties": false
            })),
            tool("leb_workspace", "Map of every repository in the workspace: facts (hosting, role), links (who calls whom), technology stacks, and memory counts. Use before cross-repo changes", &json!({
                "type": "object", "properties": {}, "additionalProperties": false
            })),
            tool("leb_symbol_search", "Find which repository and file defines a symbol, across the whole workspace", &json!({
                "type": "object", "properties": {
                    "query": {"type": "string"}
                }, "required": ["query"], "additionalProperties": false
            })),
            tool("leb_architecture_guide", "The deterministic workspace architecture dossier: per-repo facts, tech, links, modules, and route surfaces", &json!({
                "type": "object", "properties": {}, "additionalProperties": false
            })),
            tool("leb_mark_outcome", "Close an explicit task with an observed outcome", &json!({
                "type": "object", "properties": {
                    "task_id": {"type": "string"},
                    "outcome": {"type": "string", "enum": ["success", "partial", "failure", "abandoned"]},
                    "accepted_by_user": {"type": "boolean"}, "final_commit": {"type": "string"}
                }, "required": ["task_id", "outcome"], "additionalProperties": false
            })),
            tool("leb_get_invariants", "List typed invariant memories for a project", &json!({
                "type": "object", "properties": {
                    "project_id": {"type": "string"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 10000}
                }, "required": ["project_id"], "additionalProperties": false
            })),
            tool("leb_get_impact", "Traverse a bounded structural impact neighborhood", &json!({
                "type": "object", "properties": {
                    "project_id": {"type": "string"}, "node_id": {"type": "string"},
                    "max_depth": {"type": "integer", "minimum": 1, "maximum": 8},
                    "max_nodes": {"type": "integer", "minimum": 1, "maximum": 10000}
                }, "required": ["project_id", "node_id"], "additionalProperties": false
            })),
            tool("leb_forget", "Logically archive a project-scoped memory so retrieval excludes it while preserving audit history", &json!({
                "type": "object", "properties": {
                    "project_id": {"type": "string"}, "memory_id": {"type": "string"}
                },
                "required": ["project_id", "memory_id"], "additionalProperties": false
            }))
        ],
        "ttlMs": 300_000,
        "cacheScope": "public"
    })
}

fn tool(name: &str, description: &str, input_schema: &Value) -> Value {
    json!({"name": name, "description": description, "inputSchema": input_schema})
}

fn required_string<'a>(arguments: &'a Value, key: &str) -> Result<&'a str> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| Error::Config(format!("tool argument `{key}` is required")))
}

fn optional_u64(arguments: &Value, key: &str) -> Option<u64> {
    arguments.get(key).and_then(Value::as_u64)
}

fn parse_attempt_outcome(value: &str) -> Result<AttemptOutcome> {
    match value {
        "success" => Ok(AttemptOutcome::Success),
        "failure" => Ok(AttemptOutcome::Failure),
        "partial" => Ok(AttemptOutcome::Partial),
        "unknown" => Ok(AttemptOutcome::Unknown),
        other => Err(Error::Config(format!("unknown attempt outcome `{other}`"))),
    }
}

fn parse_strength(value: &str) -> Result<EvidenceStrength> {
    match value {
        "deterministic" => Ok(EvidenceStrength::Deterministic),
        "observational" => Ok(EvidenceStrength::Observational),
        "inferred" => Ok(EvidenceStrength::Inferred),
        other => Err(Error::Config(format!(
            "unknown evidence strength `{other}`"
        ))),
    }
}

fn parse_task_outcome(value: &str) -> Result<TaskOutcome> {
    match value {
        "success" => Ok(TaskOutcome::Success),
        "partial" => Ok(TaskOutcome::Partial),
        "failure" => Ok(TaskOutcome::Failure),
        "abandoned" => Ok(TaskOutcome::Abandoned),
        _ => Err(Error::Config(
            "outcome must be success, partial, failure, or abandoned".to_owned(),
        )),
    }
}

fn string_array(arguments: &Value, key: &str) -> Result<Vec<String>> {
    let Some(value) = arguments.get(key) else {
        return Ok(Vec::new());
    };
    let values = value
        .as_array()
        .ok_or_else(|| Error::Config(format!("tool argument `{key}` must be an array")))?;
    values
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| Error::Config(format!("tool argument `{key}` must contain strings")))
        })
        .collect()
}

fn protocol_error(id: &Value, code: i32, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

async fn write_message(stdout: &mut tokio::io::Stdout, message: &Value) -> Result<()> {
    let mut encoded = serde_json::to_vec(message)?;
    encoded.push(b'\n');
    stdout
        .write_all(&encoded)
        .await
        .map_err(|error| Error::Daemon(format!("MCP stdout failed: {error}")))?;
    stdout
        .flush()
        .await
        .map_err(|error| Error::Daemon(format!("MCP stdout flush failed: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_is_deterministic_and_uses_explicit_handles() {
        let first = tool_catalog();
        assert_eq!(first, tool_catalog());
        let tools = first["tools"].as_array().expect("tools");
        assert_eq!(tools[0]["name"], "leb_project_status");
        let recall = tools
            .iter()
            .find(|tool| tool["name"] == "leb_recall")
            .expect("recall");
        assert!(
            recall["inputSchema"]["required"]
                .as_array()
                .expect("required")
                .contains(&Value::String("project_id".to_owned()))
        );
        let forget = tools
            .iter()
            .find(|tool| tool["name"] == "leb_forget")
            .expect("forget");
        assert!(
            forget["inputSchema"]["required"]
                .as_array()
                .expect("required")
                .contains(&Value::String("project_id".to_owned()))
        );
    }

    #[test]
    fn legacy_and_modern_discovery_are_supported() {
        let legacy = initialize_result(&json!({
            "params": {"protocolVersion": LEGACY_PROTOCOL}
        }));
        assert_eq!(legacy["protocolVersion"], LEGACY_PROTOCOL);
        assert_eq!(discover_result()["protocolVersion"], MODERN_PROTOCOL);
    }

    #[tokio::test]
    async fn input_framing_rejects_oversized_lines_without_unbounded_growth() {
        let (mut writer, reader) = tokio::io::duplex(128);
        let sender = tokio::spawn(async move {
            writer
                .write_all(b"0123456789abcdef\n")
                .await
                .expect("write");
        });
        let mut reader = BufReader::new(reader);
        let mut frame = Vec::new();
        let error = read_bounded_line(&mut reader, &mut frame, 8)
            .await
            .expect_err("oversized");
        assert!(error.to_string().contains("exceeds 8 byte"));
        assert!(frame.len() <= 8);
        sender.await.expect("sender");
    }
}
