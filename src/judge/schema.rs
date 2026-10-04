//! Verdict JSON schema for runtimes that enforce structured output.
//!
//! The template is shared with `scripts/eval_local_judge.py`. Identifier
//! enums are filled per packet, so constrained decoding cannot produce an
//! unknown field, an empty claim or an evidence or memory ID the packet did not
//! present. Strict parsing and verdict validation still run on every response.

use serde_json::{Value, json};

use super::JudgePacket;
use crate::error::Result;

const TEMPLATE: &str = include_str!("verdict_schema.json");
/// `contradicts_memory_ids` bound shared with verdict validation.
pub(super) const MAX_CONTRADICTIONS: usize = 8;

/// Build the strict verdict schema for one packet.
/// # Errors
/// Returns an error only if the embedded template is not valid JSON.
pub(super) fn verdict_schema(packet: &JudgePacket) -> Result<Value> {
    let mut schema: Value = serde_json::from_str(TEMPLATE)?;
    let evidence: Vec<&str> = packet.evidence.iter().map(|e| e.id.as_str()).collect();
    let memories: Vec<&str> = packet
        .review
        .related_memories
        .iter()
        .filter(|memory| !memory.exact)
        .map(|memory| memory.id.as_str())
        .collect();
    if evidence.is_empty() {
        schema["properties"]["citations"] = json!({"type": "array", "maxItems": 0});
    } else {
        schema["properties"]["citations"]["items"]["properties"]["evidence_id"]["enum"] =
            json!(evidence);
    }
    if memories.is_empty() {
        schema["properties"]["contradicts_memory_ids"] = json!({"type": "array", "maxItems": 0});
    } else {
        schema["properties"]["contradicts_memory_ids"]["maxItems"] =
            json!(memories.len().min(MAX_CONTRADICTIONS));
        schema["properties"]["contradicts_memory_ids"]["items"]["enum"] = json!(memories);
    }
    Ok(schema)
}

/// OpenAI-compatible `response_format` carrying the verdict schema.
/// # Errors
/// Returns an error only if the embedded template is not valid JSON.
pub(super) fn response_format(packet: &JudgePacket) -> Result<Value> {
    Ok(json!({"type": "json_schema", "json_schema": {
        "name": "engram_judge_verdict", "strict": true, "schema": verdict_schema(packet)?
    }}))
}
