//! Bounded, read-only usage extraction from stock coding-agent transcripts.
//!
//! Only usage counters and explicitly reported model/provider metadata leave
//! this module. Message content, tool arguments, and transcript text are never
//! retained. Claude message snapshots should be ingested at session stop, once
//! their provider usage is final. Codex counts are cumulative; only increments
//! become additive ledger samples, and resets establish a fresh baseline.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read as _;
use std::path::Path;

use serde::Serialize;
use serde_json::Value;

use crate::effectiveness::UsageMetrics;
use crate::error::{Error, Result, io_error};

const MAX_TRANSCRIPT_BYTES: u64 = 32 * 1024 * 1024;
const MAX_LINE_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CapturedUsage {
    pub sample_id: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub metrics: UsageMetrics,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct CaptureReport {
    pub samples: Vec<CapturedUsage>,
    pub lines_seen: usize,
    pub usage_rows: usize,
    /// Malformed, oversized, or unrecognized rows; never treated as zero usage.
    pub skipped_rows: usize,
    /// Reset rows establish a baseline without inventing usage for that gap.
    pub counter_resets: usize,
}

/// Read additive usage samples from a Claude or Codex transcript.
///
/// # Errors
///
/// Returns an error for unreadable/non-regular files or files above 32 MiB.
/// Lines above 2 MiB and unknown formats are skipped, without guessing usage.
pub fn read_transcript(path: &Path) -> Result<Vec<CapturedUsage>> {
    Ok(read_transcript_report(path)?.samples)
}

/// Read samples and coverage diagnostics, without retaining transcript content.
///
/// # Errors
///
/// Returns an error for unreadable/non-regular files or files above 32 MiB.
pub fn read_transcript_report(path: &Path) -> Result<CaptureReport> {
    let metadata = std::fs::metadata(path).map_err(|source| io_error(path, source))?;
    if !metadata.is_file() || metadata.len() > MAX_TRANSCRIPT_BYTES {
        return Err(Error::Config(
            "usage transcript must be a regular file no larger than 32 MiB".to_owned(),
        ));
    }
    let file = File::open(path).map_err(|source| io_error(path, source))?;
    let metadata = file.metadata().map_err(|source| io_error(path, source))?;
    if !metadata.is_file() || metadata.len() > MAX_TRANSCRIPT_BYTES {
        return Err(Error::Config(
            "usage transcript must be a regular file no larger than 32 MiB".to_owned(),
        ));
    }
    let mut bytes = Vec::new();
    file.take(MAX_TRANSCRIPT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| io_error(path, source))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_TRANSCRIPT_BYTES {
        return Err(Error::Config(
            "usage transcript exceeded 32 MiB while reading".to_owned(),
        ));
    }
    Ok(parse_transcript(&bytes, &path.to_string_lossy()))
}

#[derive(Default)]
struct CodexState {
    session_id: String,
    provider: Option<String>,
    model: Option<String>,
    previous: UsageMetrics,
}

fn parse_transcript(bytes: &[u8], fallback_identity: &str) -> CaptureReport {
    let mut report = CaptureReport::default();
    let mut claude = BTreeMap::new();
    let mut codex = CodexState {
        session_id: fallback_identity.to_owned(),
        ..CodexState::default()
    };
    let mut offset = 0;
    for line in bytes.split(|byte| *byte == b'\n') {
        let line_offset = offset;
        offset += line.len() + 1;
        if line.is_empty() {
            continue;
        }
        report.lines_seen += 1;
        let row = if line.len() <= MAX_LINE_BYTES {
            serde_json::from_slice::<Value>(line).ok()
        } else {
            None
        };
        let Some(row) = row else {
            report.skipped_rows += 1;
            continue;
        };
        if let Some(sample) = claude_sample(&row) {
            report.usage_rows += 1;
            // Streaming blocks repeat one provider message. Keep its LAST
            // usage report, not a sum of the snapshots of the same response.
            claude.insert(sample.sample_id.clone(), sample);
        } else if !codex_row(&row, line_offset, &mut codex, &mut report) {
            report.skipped_rows += 1;
        }
    }
    report.samples.extend(claude.into_values());
    report
}

fn text_field(object: &Value, key: &str) -> Option<String> {
    object
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

fn count(object: &Value, key: &str) -> Option<i64> {
    object
        .get(key)
        .and_then(Value::as_i64)
        .filter(|value| *value >= 0)
}

fn has_tokens(metrics: &UsageMetrics) -> bool {
    [
        metrics.input_tokens,
        metrics.output_tokens,
        metrics.cache_read_tokens,
        metrics.cache_write_tokens,
    ]
    .iter()
    .any(Option::is_some)
}

fn claude_sample(row: &Value) -> Option<CapturedUsage> {
    if row.get("type")?.as_str()? != "assistant" {
        return None;
    }
    let message = row.get("message")?;
    let message_id = text_field(message, "id")?;
    let usage = message.get("usage")?;
    let metrics = UsageMetrics {
        input_tokens: count(usage, "input_tokens"),
        output_tokens: count(usage, "output_tokens"),
        cache_read_tokens: count(usage, "cache_read_input_tokens"),
        cache_write_tokens: count(usage, "cache_creation_input_tokens"),
        ..UsageMetrics::default()
    };
    has_tokens(&metrics).then(|| CapturedUsage {
        sample_id: format!(
            "claude:{}",
            crate::experience::sha256(message_id.as_bytes())
        ),
        provider: text_field(message, "provider").or_else(|| text_field(row, "model_provider")),
        model: text_field(message, "model"),
        metrics,
    })
}

fn codex_row(
    row: &Value,
    offset: usize,
    state: &mut CodexState,
    report: &mut CaptureReport,
) -> bool {
    let Some(payload) = row.get("payload") else {
        return false;
    };
    match row.get("type").and_then(Value::as_str) {
        Some("session_meta") => {
            if let Some(id) = text_field(payload, "id") {
                state.session_id = id;
            }
            state.provider = text_field(payload, "model_provider");
            true
        }
        Some("turn_context") => {
            if let Some(model) = text_field(payload, "model") {
                state.model = Some(model);
            }
            if let Some(provider) = text_field(payload, "model_provider") {
                state.provider = Some(provider);
            }
            true
        }
        Some("event_msg") if payload.get("type").and_then(Value::as_str) == Some("token_count") => {
            let Some(totals) = payload.pointer("/info/total_token_usage") else {
                return false;
            };
            let current = UsageMetrics {
                input_tokens: count(totals, "input_tokens"),
                output_tokens: count(totals, "output_tokens"),
                cache_read_tokens: count(totals, "cached_input_tokens"),
                ..UsageMetrics::default()
            };
            if !has_tokens(&current) {
                return false;
            }
            report.usage_rows += 1;
            match counter_delta(&mut state.previous, &current) {
                CounterDelta::Reset => report.counter_resets += 1,
                CounterDelta::Unchanged => {}
                CounterDelta::Increment(metrics) => report.samples.push(CapturedUsage {
                    sample_id: format!(
                        "codex:{}:{offset}",
                        &crate::experience::sha256(state.session_id.as_bytes())[..24]
                    ),
                    provider: state.provider.clone(),
                    model: state.model.clone(),
                    metrics,
                }),
            }
            true
        }
        _ => false,
    }
}

enum CounterDelta {
    Reset,
    Unchanged,
    Increment(UsageMetrics),
}

fn counter_delta(previous: &mut UsageMetrics, current: &UsageMetrics) -> CounterDelta {
    let fields = [
        (previous.input_tokens, current.input_tokens),
        (previous.output_tokens, current.output_tokens),
        (previous.cache_read_tokens, current.cache_read_tokens),
    ];
    if fields.iter().any(|(before, after)| {
        before
            .zip(*after)
            .is_some_and(|(before, after)| after < before)
    }) {
        previous.clone_from(current);
        return CounterDelta::Reset;
    }
    let delta =
        |before: Option<i64>, after: Option<i64>| after.map(|after| after - before.unwrap_or(0));
    let metrics = UsageMetrics {
        input_tokens: delta(previous.input_tokens, current.input_tokens),
        output_tokens: delta(previous.output_tokens, current.output_tokens),
        cache_read_tokens: delta(previous.cache_read_tokens, current.cache_read_tokens),
        ..UsageMetrics::default()
    };
    previous.input_tokens = current.input_tokens.or(previous.input_tokens);
    previous.output_tokens = current.output_tokens.or(previous.output_tokens);
    previous.cache_read_tokens = current.cache_read_tokens.or(previous.cache_read_tokens);
    if [
        metrics.input_tokens,
        metrics.output_tokens,
        metrics.cache_read_tokens,
    ]
    .iter()
    .any(|count| count.is_some_and(|count| count > 0))
    {
        CounterDelta::Increment(metrics)
    } else {
        CounterDelta::Unchanged
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn lines(rows: &[Value]) -> Vec<u8> {
        rows.iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            .into_bytes()
    }

    fn codex_counts(input: i64, output: i64, cached: i64) -> Value {
        json!({"type":"event_msg", "payload":{"type":"token_count", "info":{"total_token_usage":{
            "input_tokens":input, "output_tokens":output, "cached_input_tokens":cached
        }}}})
    }

    #[test]
    fn claude_repeated_blocks_choose_last_message_usage() {
        let rows = lines(&[
            json!({"type":"assistant","message":{"id":"m1","model":"claude-model","usage":{"input_tokens":30,"output_tokens":2,"cache_read_input_tokens":10}}}),
            json!({"type":"assistant","message":{"id":"m1","model":"claude-model","usage":{"input_tokens":30,"output_tokens":12,"cache_read_input_tokens":10}}}),
            json!({"type":"assistant","message":{"id":"m2","usage":{"input_tokens":20,"output_tokens":5}}}),
        ]);
        let report = parse_transcript(&rows, "test");
        assert_eq!(report.usage_rows, 3);
        assert_eq!(report.samples.len(), 2);
        assert_eq!(
            report
                .samples
                .iter()
                .map(|sample| sample.metrics.output_tokens.unwrap())
                .sum::<i64>(),
            17
        );
        assert!(
            report
                .samples
                .iter()
                .all(|sample| sample.metrics.cost_usd.is_none()
                    && sample.metrics.tool_calls.is_none()
                    && sample.provider.is_none())
        );
        assert_eq!(report.samples, parse_transcript(&rows, "test").samples);
    }

    #[test]
    fn codex_cumulative_rows_become_additive_deduplicated_increments() {
        let rows = lines(&[
            json!({"type":"session_meta","payload":{"id":"session-1","model_provider":"local"}}),
            json!({"type":"turn_context","payload":{"model":"qwen-model"}}),
            codex_counts(100, 20, 40),
            codex_counts(100, 20, 40),
            codex_counts(150, 30, 50),
        ]);
        let report = parse_transcript(&rows, "test");
        assert_eq!(report.samples.len(), 2);
        assert_eq!(report.samples[1].metrics.input_tokens, Some(50));
        assert_eq!(report.samples[1].metrics.output_tokens, Some(10));
        assert_eq!(report.samples[1].metrics.cache_read_tokens, Some(10));
        assert_eq!(report.samples[1].provider.as_deref(), Some("local"));
        assert_eq!(report.samples[1].model.as_deref(), Some("qwen-model"));
        let mut appended = rows.clone();
        appended.push(b'\n');
        appended.extend(lines(&[codex_counts(180, 40, 60)]));
        assert_eq!(
            report.samples,
            parse_transcript(&appended, "different-path").samples[..2]
        );
    }

    #[test]
    fn codex_reset_establishes_baseline_without_inventing_usage() {
        let report = parse_transcript(
            &lines(&[
                codex_counts(1000, 300, 500),
                codex_counts(10, 2, 5),
                codex_counts(35, 7, 8),
            ]),
            "test",
        );
        assert_eq!(report.counter_resets, 1);
        assert_eq!(report.samples.len(), 2);
        assert_eq!(report.samples[1].metrics.input_tokens, Some(25));
        assert_eq!(report.samples[1].metrics.output_tokens, Some(5));
    }

    #[test]
    fn missing_or_unrecognized_usage_is_not_reported_as_zero() {
        let report = parse_transcript(
            &lines(&[
                json!({"type":"assistant","message":{"id":"m","usage":{"output_tokens":2}}}),
                json!({"type":"assistant","message":{"id":"n","usage":{"input_tokens":-9,"output_tokens":"unknown"}}}),
                json!({"total_tokens":500}),
            ]),
            "test",
        );
        assert_eq!(report.samples.len(), 1);
        assert_eq!(report.samples[0].metrics.input_tokens, None);
        assert_eq!(report.skipped_rows, 2);
    }

    #[test]
    fn missing_cumulative_fields_do_not_reset_known_counters() {
        let rows = lines(&[
            codex_counts(100, 20, 40),
            json!({"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"output_tokens":25}}}}),
            codex_counts(150, 30, 50),
        ]);
        let report = parse_transcript(&rows, "test");
        assert_eq!(report.samples[2].metrics.input_tokens, Some(50));
        assert_eq!(report.samples[2].metrics.output_tokens, Some(5));
    }

    #[test]
    fn file_and_line_limits_are_enforced_without_retaining_text() {
        let mut rows = vec![b'x'; MAX_LINE_BYTES + 1];
        rows.push(b'\n');
        rows.extend(lines(&[codex_counts(10, 2, 0)]));
        let report = parse_transcript(&rows, "test");
        assert_eq!(report.skipped_rows, 1);
        assert_eq!(report.samples.len(), 1);
        let file = tempfile::NamedTempFile::new().expect("temp");
        file.as_file()
            .set_len(MAX_TRANSCRIPT_BYTES + 1)
            .expect("file length");
        assert!(read_transcript(file.path()).is_err());
    }
}
