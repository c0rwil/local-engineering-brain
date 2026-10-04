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
            "engram_project_status" => {
                let path = required_string(&arguments, "path")?;
                self.client
                    .ensure_project_fresh(PathBuf::from(path).as_path(), Vec::new())
                    .await?;
                serde_json::to_value(
                    self.client
                        .resolve_project(PathBuf::from(path).as_path())
                        .await?,
                )?
            }
            "engram_query_world" => {
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
            "engram_complete_task" => {
                let request: crate::product::lessons::Completion =
                    serde_json::from_value(arguments.clone())?;
                self.client
                    .lesson_mutate(&crate::product::lessons::LessonMutation::Complete(request))
                    .await?
            }
            "engram_review_lesson" => {
                let request: crate::product::lessons::ReviewRequest =
                    serde_json::from_value(arguments.clone())?;
                if request.acknowledge_security_flags {
                    return Err(Error::Config(
                        "acknowledge_security_flags is reserved for a human reviewer; flagged lessons are accepted in the web Knowledge inbox or with the CLI".to_owned(),
                    ));
                }
                self.client
                    .lesson_mutate(&crate::product::lessons::LessonMutation::Review(request))
                    .await?
            }
            "engram_product" => {
                self.client
                    .product_query(&serde_json::from_value(arguments.clone())?)
                    .await?
            }
            "engram_product_edit" => serde_json::to_value(
                self.client
                    .product_mutate(&serde_json::from_value(arguments.clone())?)
                    .await?,
            )?,
            "engram_recall" => {
                let context = self
                    .client
                    .compile_context(&recall_request(&arguments)?)
                    .await?;
                return Ok(recall_result_envelope(&context));
            }
            "engram_effectiveness" => serde_json::to_value(
                self.client
                    .effectiveness(&serde_json::from_value(arguments.clone())?)
                    .await?,
            )?,
            "engram_record_usage" => serde_json::to_value(
                self.client
                    .record_usage(&serde_json::from_value(arguments.clone())?)
                    .await?,
            )?,
            "engram_explain_context" => {
                let run_id = ContextRunId::parse(required_string(&arguments, "context_run_id")?)?;
                let ordinal = u32::try_from(optional_u64(&arguments, "ordinal").unwrap_or(1))
                    .map_err(|_| Error::Config("ordinal is too large".to_owned()))?;
                serde_json::to_value(
                    self.client
                        .explain_context(&ContextExplainRequest { run_id, ordinal })
                        .await?,
                )?
            }
            "engram_get_past_failures" => {
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
            "engram_get_memory" => {
                let id = MemoryId::parse(required_string(&arguments, "memory_id")?)?;
                serde_json::to_value(self.client.get_memory(&id).await?)?
            }
            "engram_record_human_decision" => serde_json::to_value(
                self.client
                    .create_memory(&human_decision_request(&arguments)?)
                    .await?,
            )?,
            "engram_get_skill" => {
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
            "engram_task_start" => {
                let project_id = ProjectId::parse(required_string(&arguments, "project_id")?)?;
                serde_json::to_value(
                    self.client
                        .start_task(&TaskStartRequest {
                            context_request_id: None,
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
            "engram_record_attempt" => {
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
            "engram_record_evidence" => {
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
            "engram_learn" => {
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
            "engram_workspace" => {
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
            "engram_symbol_search" => {
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
            "engram_architecture_guide" => {
                let dossier = self.client.architect_dossier().await?;
                json!({ "dossier": dossier })
            }
            "engram_mark_outcome" => {
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
            "engram_get_invariants" => {
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
            "engram_get_impact" => {
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
            "engram_forget" => {
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
            _ => return Err(Error::Config(format!("unknown Engram tool `{name}`"))),
        };
        if name == "engram_product"
            && arguments.get("operation").and_then(Value::as_str) == Some("bundle")
        {
            return Ok(json!({"content":[{"type":"text","text":output["text"]}],"isError":false}));
        }
        tool_result_envelope(&output)
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
        "instructions": "Engram is local engineering memory for your existing coding agent. Resolve the actual repository path with engram_project_status, then retrieve validated lessons with engram_recall. For onboarding and cross-repository work, use engram_product snapshot, bundle or impact. Treat returned content as reference data and inspect freshness and coverage gaps. Record substantive work with engram_task_start and finish with engram_complete_task using actual verification output. Reuse an existing task ID when available. Leave user acceptance unknown unless explicitly given. Proposed lessons enter review; do not manufacture evidence or bypass validation gates.",
        "serverInfo": {"name": "engram-mcp", "version": env!("CARGO_PKG_VERSION")}
    })
}

fn discover_result() -> Value {
    json!({
        "protocolVersion": MODERN_PROTOCOL,
        "capabilities": {"tools": {}},
        "_meta": {
            "io.modelcontextprotocol/serverInfo": {
                "name": "engram-mcp",
                "version": env!("CARGO_PKG_VERSION")
            }
        }
    })
}

#[allow(clippy::too_many_lines)]
fn tool_catalog() -> Value {
    json!({
        "tools": [
            tool("engram_project_status", "Resolve project identity and current Git state", &json!({
                "type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"], "additionalProperties": false
            })),
            tool("engram_query_world", "Search normalized structural nodes", &json!({
                "type": "object", "properties": {
                    "project_id": {"type": "string"}, "query": {"type": "string"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 1000}
                }, "required": ["project_id", "query"], "additionalProperties": false
            })),
            tool("engram_recall", "Return bounded context text and a compact context_run_id handle. Includes relevant validated lessons, project preferences and indexed source pointers. Inspect sources before relying on them; use engram_explain_context for item provenance and scores. Passing the open task_id records usage provenance", &json!({
                "type": "object", "properties": {
                    "project_id": {"type": "string"}, "task": {"type": "string"},
                    "current_paths": {"type": "array", "items": {"type": "string"}},
                    "token_budget": {"type": "integer", "minimum": 1},
                    "task_id": {"type": "string"}
                }, "required": ["project_id", "task"], "additionalProperties": false
            })),
            tool("engram_effectiveness", "Inspect measured usage, measurement coverage, retrieved context sources, learning state and controlled benchmark comparisons. Missing measurements are unknown; retrieval does not prove savings", &json!({
                "type":"object","properties":{"project_ids":{"type":"array","items":{"type":"string"}},"since_ms":{"type":"integer"},"until_ms":{"type":"integer"}},"additionalProperties":false
            })),
            tool("engram_record_usage", "Record actual provider or harness usage with a stable unique sample_id. Omit unknown metrics. Never estimate token counts, cost or avoided tool calls. Repeating an identical sample is safe", &json!({
                "type":"object","properties":{
                    "sample_id":{"type":"string"},"project_id":{"type":"string"},"source":{"type":"string"},
                    "task_id":{"type":"string"},"context_run_id":{"type":"string"},"session_id":{"type":"string"},
                    "provider":{"type":"string"},"model":{"type":"string"},"mode":{"enum":["off","repo","brain"]},"observed_at_ms":{"type":"integer"},
                    "metrics":{"type":"object","properties":{
                        "input_tokens":{"type":"integer","minimum":0},"output_tokens":{"type":"integer","minimum":0},
                        "cache_read_tokens":{"type":"integer","minimum":0},"cache_write_tokens":{"type":"integer","minimum":0},
                        "tool_calls":{"type":"integer","minimum":0},"context_bytes":{"type":"integer","minimum":0},"context_tokens":{"type":"integer","minimum":0},"cost_usd":{"type":"number","minimum":0}
                    },"additionalProperties":false}
                },"required":["sample_id","project_id","source","metrics"],"additionalProperties":false
            })),
            tool("engram_explain_context", "Explain one selected item from a durable context run", &json!({
                "type": "object", "properties": {
                    "context_run_id": {"type": "string"},
                    "ordinal": {"type": "integer", "minimum": 1}
                }, "required": ["context_run_id", "ordinal"], "additionalProperties": false
            })),
            tool("engram_get_past_failures", "Query demonstrated failed approaches for a project", &json!({
                "type": "object", "properties": {
                    "project_id": {"type": "string"}, "query": {"type": "string"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 1000}
                }, "required": ["project_id"], "additionalProperties": false
            })),
            tool("engram_get_memory", "Inspect one durable memory and its state", &json!({
                "type": "object", "properties": {"memory_id": {"type": "string"}},
                "required": ["memory_id"], "additionalProperties": false
            })),
            tool("engram_record_human_decision", "Record an explicit project-scoped human decision as a candidate", &json!({
                "type": "object", "properties": {
                    "project_id": {"type": "string"}, "claim": {"type": "string"},
                    "rationale": {"type": "string"}, "tags": {"type": "array", "items": {"type": "string"}},
                    "valid_from_commit": {"type": "string"}
                }, "required": ["project_id", "claim"], "additionalProperties": false
            })),
            tool("engram_get_skill", "Inspect a typed skill or procedure memory. Procedures are synthesized from repeated evidence-backed episodes; their rationale carries a procedure_json payload (steps, preconditions, source episodes). Skills are procedures promoted after validation plus repeated successful use", &json!({
                "type": "object", "properties": {"memory_id": {"type": "string"}},
                "required": ["memory_id"], "additionalProperties": false
            })),
            tool("engram_task_start", "Open an explicit task episode BEFORE starting substantive work, so attempts and evidence attach to it and finished work can be learned from", &json!({
                "type": "object", "properties": {
                    "project_id": {"type": "string"},
                    "prompt": {"type": "string", "description": "The user's task, verbatim or summarized"},
                    "session_id": {"type": "string"}
                }, "required": ["project_id", "prompt"], "additionalProperties": false
            })),
            tool("engram_record_attempt", "Record one strategy attempt on an open task (outcome: success|failure|partial|unknown)", &json!({
                "type": "object", "properties": {
                    "task_id": {"type": "string"},
                    "summary": {"type": "string", "description": "What was tried, phrased as the reusable takeaway"},
                    "outcome": {"type": "string"},
                    "files": {"type": "array", "items": {"type": "string"}}
                }, "required": ["task_id", "summary", "outcome"], "additionalProperties": false
            })),
            tool("engram_record_evidence", "Attach actual verification output to a task in the same project. Deterministic: test_result|build_result|lint_result|typecheck_result|benchmark_result with command and exit_code=0. Ops: runtime_observation|trace|metric with strength=observational, artifact output, and exit_code=0 or omitted (gcloud describe, logs, curl probes). Record evidence before marking the task success, then pass its id to engram_learn. User acceptance is separate and may be unknown", &json!({
                "type": "object", "properties": {
                    "project_id": {"type": "string"},
                    "task_id": {"type": "string"},
                    "attempt_id": {"type": "string"},
                    "kind": {"type": "string", "description": "test_result, build_result, lint_result, typecheck_result, benchmark_result, runtime_observation, trace, metric, or user_acceptance"},
                    "strength": {"type": "string", "enum": ["deterministic", "observational", "inferred"]},
                    "command": {"type": "string"},
                    "exit_code": {"type": "integer"},
                    "artifact": {"type": "string", "description": "Bounded output excerpt"}
                }, "required": ["project_id", "kind", "strength"], "additionalProperties": false
            })),
            tool("engram_learn", "Distill a successful task into a candidate (lesson|preference, confidence capped at 0.50). First record deterministic or observational evidence on that same task, then engram_mark_outcome(success), then learn with its task_ids and evidence_ids. No attempt is required. Candidates do not inject; review and Promote in the web UI, or use stepwise evidence-gated CLI promotion.", &json!({
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
            tool("engram_complete_task", "Complete an existing task in one call: capture actual verification/observation output, close the outcome, and propose up to five lessons for explicit review. Retries reuse evidence. Do not fabricate output or infer user acceptance from a commit, merge or silence. Omit accepted_by_user unless explicitly known. No candidate auto-injects.", &json!({
                "type":"object","properties":{
                    "task_id":{"type":"string"},"outcome":{"type":"string","enum":["success","partial","failure","abandoned"]},
                    "accepted_by_user":{"type":"boolean"},"final_commit":{"type":"string"},"summary":{"type":"string"},
                    "files":{"type":"array","items":{"type":"string"}},
                    "verifications":{"type":"array","maxItems":16,"items":{"type":"object","properties":{
                        "kind":{"type":"string"},"strength":{"type":"string","enum":["deterministic","observational","inferred"]},
                        "command":{"type":"string"},"exit_code":{"type":"integer"},"artifact":{"type":"string"},"revision":{"type":"string"}
                    },"required":["kind","strength","artifact"],"additionalProperties":false}},
                    "lessons":{"type":"array","maxItems":5,"items":{"type":"object","properties":{
                        "claim":{"type":"string"},"rationale":{"type":"string"},"evidence_ids":{"type":"array","items":{"type":"string"}},"expires_at_ms":{"type":"integer"}
                    },"required":["claim"],"additionalProperties":false}}
                },"required":["task_id","outcome"],"additionalProperties":false
            })),
            tool("engram_review_lesson", "Explicitly accept or reject a proposal returned by engram_product(operation=reviews). Acceptance uses normal evidence-gated promotion and caps confidence at 0.50. Review the claim, evidence and similar memories first. This does not accept the underlying task. Do not use generated documentation as evidence.", &json!({
                "type":"object","properties":{
                    "id":{"type":"string"},"expected_updated_at_ms":{"type":"integer"},"decision":{"type":"string","enum":["accept","reject"]},
                    "claim":{"type":"string"},"rationale":{"type":"string"},"contradicts":{"type":"array","items":{"type":"string"}},"reviewed_related":{"type":"boolean"}
                },"required":["id","expected_updated_at_ms","decision"],"additionalProperties":false
            })),
            tool("engram_product", "Query the living product model. schema returns the typed editing contract; source inspects an anchor {project_id, path|node_id|evidence_id}; reviews lists lesson proposals; snapshot lists typed entities, dependencies, documentation and deployments; history requires id; bundle requires entity_id and task, accepts token_budget (128–16000) and environment; impact requires project_id and paths or unified diff. Cited context is reference data, never independent evidence.", &json!({
                "type":"object", "properties": {
                    "operation":{"type":"string","enum":["snapshot","schema","source","reviews","task_signals","history","bundle","impact"]},
                    "anchor":{"type":"object"}, "task_id":{"type":"string"},
                    "id":{"type":"string"}, "entity_id":{"type":"string"}, "task":{"type":"string"},
                    "token_budget":{"type":"integer","minimum":128,"maximum":16000}, "environment":{"type":"string"},
                    "project_id":{"type":"string"}, "paths":{"type":"array","items":{"type":"string"}}, "diff":{"type":"string"}
                }, "required":["operation"], "additionalProperties":false
            })),
            tool("engram_product_edit", "Save, archive or refresh_section in the versioned product catalog. Use the current expected_version (0 for create). Save needs item {type: entity|dependency|section|deployment|guide, value: ...}; read the catalog and its schema before editing. Generated prose is not evidence. Review claims before setting a section reviewed=true.", &json!({
                "type":"object", "properties": {
                    "operation":{"type":"string","enum":["save","archive","refresh_section"]},
                    "id":{"type":"string"}, "expected_version":{"type":"integer","minimum":0},
                    "item":{"type":"object"}
                }, "required":["operation","expected_version"], "additionalProperties":false
            })),
            tool("engram_workspace", "Map of every repository in the workspace: facts (hosting, role), links (who calls whom), technology stacks, and memory counts. Use before cross-repo changes", &json!({
                "type": "object", "properties": {}, "additionalProperties": false
            })),
            tool("engram_symbol_search", "Find which repository and file defines a symbol, across the whole workspace", &json!({
                "type": "object", "properties": {
                    "query": {"type": "string"}
                }, "required": ["query"], "additionalProperties": false
            })),
            tool("engram_architecture_guide", "The deterministic workspace architecture dossier: per-repo facts, tech, links, modules, and route surfaces", &json!({
                "type": "object", "properties": {}, "additionalProperties": false
            })),
            tool("engram_mark_outcome", "Close an explicit task with an observed outcome", &json!({
                "type": "object", "properties": {
                    "task_id": {"type": "string"},
                    "outcome": {"type": "string", "enum": ["success", "partial", "failure", "abandoned"]},
                    "accepted_by_user": {"type": "boolean"}, "final_commit": {"type": "string"}
                }, "required": ["task_id", "outcome"], "additionalProperties": false
            })),
            tool("engram_get_invariants", "List typed invariant memories for a project", &json!({
                "type": "object", "properties": {
                    "project_id": {"type": "string"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 10000}
                }, "required": ["project_id"], "additionalProperties": false
            })),
            tool("engram_get_impact", "Traverse a bounded structural impact neighborhood", &json!({
                "type": "object", "properties": {
                    "project_id": {"type": "string"}, "node_id": {"type": "string"},
                    "max_depth": {"type": "integer", "minimum": 1, "maximum": 8},
                    "max_nodes": {"type": "integer", "minimum": 1, "maximum": 10000}
                }, "required": ["project_id", "node_id"], "additionalProperties": false
            })),
            tool("engram_forget", "Logically archive a project-scoped memory so retrieval excludes it while preserving audit history", &json!({
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

/// MCP requires `structuredContent` to be a JSON object; list-shaped tool
/// outputs (symbol search) are wrapped so validating clients keep the result.
fn tool_result_envelope(output: &serde_json::Value) -> Result<serde_json::Value> {
    let structured = if output.is_object() {
        output.clone()
    } else {
        json!({ "result": output })
    };
    Ok(json!({
        "content": [{"type": "text", "text": serde_json::to_string_pretty(output)?}],
        "structuredContent": structured,
        "isError": false
    }))
}

// The compiled text is the model-facing payload. Full scoring/audit records
// stay in the daemon and are available through engram_explain_context instead
// of being repeated in pretty JSON and structuredContent on every recall.
fn recall_result_envelope(context: &crate::context::CompiledContext) -> Value {
    json!({
        "content": [{"type": "text", "text": format!("{}{}", context.stable_prefix, context.dynamic_context)}],
        "structuredContent": {
            "context_run_id": context.id,
            "item_count": context.items.len(),
            "selected_tokens": context.metrics.selected_tokens,
            "token_budget": context.metrics.token_budget
        },
        "isError": false
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn recall_sends_bounded_text_once_and_keeps_audit_handle() {
        let id = crate::ids::ContextRunId::new();
        let context: crate::context::CompiledContext = serde_json::from_value(serde_json::json!({
            "id": id, "request_id": "test", "mode": "brain",
            "stable_prefix": "Project: fixture\n", "dynamic_context": "- src/auth.rs\n",
            "items": [], "metrics": {"compile_ms": 1,"candidate_count":0,"selected_tokens":8,
                "token_budget":100,"stable_prefix_hash":"fixture"}
        }))
        .unwrap();
        let result = super::recall_result_envelope(&context);
        assert_eq!(
            result["content"][0]["text"],
            "Project: fixture\n- src/auth.rs\n"
        );
        assert_eq!(
            result["structuredContent"]["context_run_id"],
            id.to_string()
        );
        assert!(result["structuredContent"].get("dynamic_context").is_none());
        assert!(result["structuredContent"].get("items").is_none());
        assert!(
            result["content"][0]["text"]
                .as_str()
                .unwrap()
                .len()
                .div_ceil(4)
                <= 100
        );
    }

    #[tokio::test]
    async fn agents_cannot_acknowledge_security_flags() {
        let server = super::McpServer::new(
            "/nonexistent/engram-test.sock",
            std::time::Duration::from_secs(1),
            1 << 20,
        );
        let error = server
            .call_tool(Some(&serde_json::json!({
                "name": "engram_review_lesson",
                "arguments": {"id": "review_fixture", "expected_updated_at_ms": 1,
                    "decision": "accept", "reviewed_related": true,
                    "acknowledge_security_flags": true}
            })))
            .await
            .expect_err("agent acknowledgment");
        assert!(error.to_string().contains("reserved for a human reviewer"));
    }

    #[test]
    fn tool_result_envelope_keeps_structured_content_an_object() {
        let list = super::tool_result_envelope(&serde_json::json!([{"label": "x"}])).unwrap();
        assert!(list["structuredContent"].is_object());
        assert_eq!(list["structuredContent"]["result"][0]["label"], "x");
        let object = super::tool_result_envelope(&serde_json::json!({"a": 1})).unwrap();
        assert_eq!(object["structuredContent"]["a"], 1);
    }

    use super::*;

    #[test]
    fn catalog_is_deterministic_and_uses_explicit_handles() {
        let first = tool_catalog();
        assert_eq!(first, tool_catalog());
        let tools = first["tools"].as_array().expect("tools");
        assert_eq!(tools[0]["name"], "engram_project_status");
        let recall = tools
            .iter()
            .find(|tool| tool["name"] == "engram_recall")
            .expect("recall");
        assert!(
            recall["inputSchema"]["required"]
                .as_array()
                .expect("required")
                .contains(&Value::String("project_id".to_owned()))
        );
        let forget = tools
            .iter()
            .find(|tool| tool["name"] == "engram_forget")
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
