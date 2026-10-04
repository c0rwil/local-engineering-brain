//! Optional semantic retrieval: local embeddings and a rebuildable `SQLite`
//! vector sidecar.
//!
//! Two embedders sit behind [`EmbeddingProvider`]:
//!
//! - [`HashedNGramEmbedder`], the default: a dependency-free hashed
//!   character-n-gram / token term-frequency projection into a fixed
//!   [`EMBEDDING_DIMENSION`]-dimensional, L2-normalized vector. It is a
//!   *lexical-semantic approximation*, not a learned model: two texts score
//!   close when they share tokens and character n-grams. It needs no model
//!   download and never fails.
//! - [`LocalEmbedder`]: an `OpenAI`-compatible `POST /v1/embeddings` endpoint
//!   on a loopback address (for example Ollama), with bounded request and
//!   response sizes, bounded time, bounded batches, no proxy and no
//!   redirects. Its retrieval benefit is unmeasured.
//!
//! Per ADR-008 and spec Appendix E/G the whole feature stays disabled by
//! default until a measured benchmark win justifies it.
//!
//! The vector sidecar (`memory_vectors`) is **rebuildable derived state**,
//! never canonical truth. It holds one row per (memory, embedder identity).
//! Hashed rows are maintained inside canonical transactions, exactly like
//! `memories_fts`, and remain the fallback. Model rows are written by
//! [`rebuild_project`], which embeds outside the canonical writer and commits
//! in short scheduled transactions; they are searched only under the
//! configured model identity and dimension, so vectors from different models
//! never mix. Losing any of it degrades retrieval to lexical + structural and
//! requests a rebuild, exactly as spec §27.2 demands; an unreachable
//! embedding endpoint falls back to the hashed rows for that request.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use rusqlite::types::ValueRef;
use rusqlite::{OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};

use crate::config::SemanticConfig;
use crate::error::{Error, Result};
use crate::ids::{MemoryId, ProjectId};
use crate::scheduler::{MutationPriority, MutationScheduler};
use crate::storage::Database;

/// Fixed dimensionality of every hashed embedding.
pub const EMBEDDING_DIMENSION: usize = 256;
const EMBEDDING_DIMENSION_U64: u64 = 256;

/// Stable identity of the built-in embedder; stored per row so a model swap
/// can detect and rebuild incompatible vectors.
pub const EMBEDDER_ID: &str = "engram-hashed-ngram-v1-d256";

/// Largest vector a local embedding model may return.
pub const MAX_MODEL_DIMENSION: usize = 8_192;
/// Largest number of texts sent in one embeddings request.
pub const MAX_BATCH_SIZE: usize = 64;
/// Longest text sent to a local model, prefix included. Longer memory or task
/// text is cut at a character boundary, deterministically.
pub const MAX_INPUT_BYTES: usize = 8_192;
/// Hard ceiling for one serialized embeddings request body.
const MAX_REQUEST_BYTES: usize = 4 * 1_024 * 1_024;
/// Response budget: fixed overhead plus this many JSON bytes per returned
/// vector component (generous for `-1.2345678e-05, `).
const RESPONSE_OVERHEAD_BYTES: usize = 64 * 1_024;
const RESPONSE_BYTES_PER_VALUE: usize = 32;
/// Bytes of an error response kept for the error message.
const ERROR_BODY_BYTES: usize = 512;
/// After a failed query embedding, the hot path skips the endpoint this long
/// so an unresponsive server cannot stall every context compile.
const QUERY_FAILURE_COOLDOWN: Duration = Duration::from_secs(30);
/// Memories read, embedded and committed per rebuild page.
const REBUILD_PAGE: usize = 256;
/// Fixed text embedded first by every model rebuild.
const PROBE_TEXT: &str = "engram embedding probe";
/// Longest provider detail kept in retrieval notes.
const NOTE_DETAIL_CHARS: usize = 160;

/// Stable, wall-clock-free seed for feature hashing.
const FEATURE_SEED: u64 = 0x1eb5_ed00_c0de_0001;

const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Which embedder produces the semantic stage's vectors.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EmbeddingProviderKind {
    /// Built-in hashed n-gram projection.
    #[default]
    Hashed,
    /// `OpenAI`-compatible embeddings endpoint on a loopback address.
    Local,
}

/// Maps batches of texts to fixed-dimension `f32` vectors (spec Appendix B).
pub trait EmbeddingProvider {
    /// Identity stored with every vector this provider writes. Vectors with
    /// different identities are never compared.
    fn identity(&self) -> &str;

    /// Dimensionality of every vector this provider returns, or 0 when the
    /// provider learns it from the model's first response.
    fn dimension(&self) -> usize;

    /// Embed a batch of texts into one vector per text.
    ///
    /// # Errors
    ///
    /// Returns an error when the provider cannot produce a vector for every
    /// input text.
    fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>>;
}

/// One vector to store for a memory (spec Appendix B `VectorItem`).
#[derive(Clone, Debug, PartialEq)]
pub struct VectorItem {
    pub memory_id: MemoryId,
    pub project_id: ProjectId,
    pub vector: Vec<f32>,
}

/// One scored search result (spec Appendix B `Hit`). `score` is raw cosine
/// similarity in `[-1, 1]`; retrieval maps it to `[0, 1]`.
#[derive(Clone, Debug, PartialEq)]
pub struct VectorHit {
    pub memory_id: MemoryId,
    pub score: f64,
}

/// Project-filtered vector store over derived, rebuildable state
/// (spec Appendix B `VectorIndex`).
pub trait VectorIndex {
    /// Insert or replace vectors for the given memories.
    ///
    /// # Errors
    ///
    /// Returns an error for a dimension mismatch or a failed canonical
    /// transaction.
    fn upsert(&self, items: &[VectorItem]) -> Result<()>;

    /// Return up to `k` hits for `query`, restricted to one project and to
    /// active validated memories, with a deterministic tie-break.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid query vector or failed storage reads.
    fn search(&self, project_id: &ProjectId, query: &[f32], k: usize) -> Result<Vec<VectorHit>>;

    /// Remove vectors for the given memories; missing rows are ignored.
    ///
    /// # Errors
    ///
    /// Returns an error for a failed canonical transaction.
    fn delete(&self, memory_ids: &[MemoryId]) -> Result<()>;

    /// Drop and regenerate derived vectors from canonical memories, for one
    /// project or for every project, returning the number of vectors written.
    ///
    /// # Errors
    ///
    /// Returns an error for a failed canonical transaction.
    fn rebuild(&self, project_id: Option<&ProjectId>) -> Result<u64>;
}

/// Bounds and provider threaded from `[semantic]` configuration into
/// retrieval.
#[derive(Clone, Debug, PartialEq)]
pub struct SemanticOptions {
    /// Maximum semantic candidates merged into hybrid retrieval.
    pub max_candidates: usize,
    /// Hard bound on memories scanned per brute-force search.
    pub max_scanned: usize,
    /// Cosine similarity at or below this floor contributes no semantic
    /// score; higher similarity is rescaled to `(0, 1]`. Applies to the
    /// configured provider; the hashed fallback always uses 0.0.
    pub min_similarity: f64,
    /// Local embedding endpoint; `None` selects the hashed embedder.
    pub local: Option<LocalEmbedderSettings>,
}

impl SemanticOptions {
    /// Hashed-embedder options with the given bounds and no similarity floor.
    #[must_use]
    pub const fn hashed(max_candidates: usize, max_scanned: usize) -> Self {
        Self {
            max_candidates,
            max_scanned,
            min_similarity: 0.0,
            local: None,
        }
    }

    /// Options whose bounds come from `bounds` (a project may scope them) and
    /// whose provider comes from `machine`, the user or workspace
    /// configuration: a repository cannot redirect where text is embedded.
    #[must_use]
    pub fn from_config(bounds: &SemanticConfig, machine: &SemanticConfig) -> Self {
        Self {
            max_candidates: usize::try_from(bounds.max_candidates).unwrap_or(usize::MAX),
            max_scanned: usize::try_from(bounds.max_scanned).unwrap_or(usize::MAX),
            min_similarity: machine.min_similarity,
            local: LocalEmbedderSettings::from_config(machine),
        }
    }
}

/// Settings for the `OpenAI`-compatible local embeddings provider.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalEmbedderSettings {
    /// Loopback `POST /v1/embeddings` URL.
    pub endpoint: String,
    /// Embedding model name, as the endpoint knows it.
    pub model: String,
    /// Required vector dimension; `None` accepts the model's dimension.
    pub dimension: Option<usize>,
    /// Texts per request during a rebuild.
    pub batch_size: usize,
    /// Bound on each rebuild request.
    pub timeout: Duration,
    /// Bound on the single query embedding during context compilation.
    pub query_timeout: Duration,
    /// Prepended to the task text before embedding a query.
    pub query_prefix: String,
    /// Prepended to memory text before embedding it for storage.
    pub document_prefix: String,
}

impl LocalEmbedderSettings {
    /// Settings for a configured local provider; `None` for the hashed
    /// default.
    #[must_use]
    pub fn from_config(config: &SemanticConfig) -> Option<Self> {
        (config.provider == EmbeddingProviderKind::Local).then(|| Self {
            endpoint: config.endpoint.clone(),
            model: config.model.clone(),
            dimension: (config.dimension > 0)
                .then(|| usize::try_from(config.dimension).unwrap_or(usize::MAX)),
            batch_size: usize::try_from(config.batch_size)
                .unwrap_or(MAX_BATCH_SIZE)
                .clamp(1, MAX_BATCH_SIZE),
            timeout: Duration::from_millis(config.timeout_ms),
            query_timeout: Duration::from_millis(config.query_timeout_ms),
            query_prefix: config.query_prefix.clone(),
            document_prefix: config.document_prefix.clone(),
        })
    }

    /// Identity stored with this model's vectors: the model name plus a
    /// digest of the document prefix, which changes every stored vector.
    #[must_use]
    pub fn identity(&self) -> String {
        if self.document_prefix.is_empty() {
            format!("local:{}", self.model)
        } else {
            let digest = crate::experience::sha256(self.document_prefix.as_bytes());
            format!("local:{}#{}", self.model, &digest[..12])
        }
    }
}

/// Validate `[semantic]` provider settings. The endpoint must be loopback
/// whatever `network.local_only` says: remote embedding providers are not
/// supported, so local-only is always respected.
///
/// # Errors
///
/// Returns a configuration error for a non-loopback endpoint, a missing or
/// malformed model name, or limits outside their supported bounds.
pub fn validate_config(config: &SemanticConfig, local_only: bool) -> Result<()> {
    loopback_endpoint(&config.endpoint, local_only)?;
    if config.model.len() > 256
        || config.model.trim() != config.model
        || config.model.chars().any(char::is_control)
    {
        return Err(Error::Config(
            "semantic.model must be at most 256 bytes without surrounding whitespace or control \
             characters"
                .to_owned(),
        ));
    }
    if config.provider == EmbeddingProviderKind::Local && config.model.is_empty() {
        return Err(Error::Config(
            "semantic.provider = \"local\" requires semantic.model, an embedding model the \
             endpoint serves (for example nomic-embed-text)"
                .to_owned(),
        ));
    }
    let dimension = usize::try_from(config.dimension).unwrap_or(usize::MAX);
    let batch_size = usize::try_from(config.batch_size).unwrap_or(usize::MAX);
    if dimension > MAX_MODEL_DIMENSION
        || !(1..=MAX_BATCH_SIZE).contains(&batch_size)
        || !(100..=300_000).contains(&config.timeout_ms)
        || !(50..=30_000).contains(&config.query_timeout_ms)
    {
        return Err(Error::Config(format!(
            "semantic.dimension must be 0..={MAX_MODEL_DIMENSION}, batch_size 1..={MAX_BATCH_SIZE}, \
             timeout_ms 100..=300000 and query_timeout_ms 50..=30000"
        )));
    }
    if !config.min_similarity.is_finite() || !(0.0..=0.99).contains(&config.min_similarity) {
        return Err(Error::Config(
            "semantic.min_similarity must be between 0.0 and 0.99".to_owned(),
        ));
    }
    for (name, prefix) in [
        ("query_prefix", &config.query_prefix),
        ("document_prefix", &config.document_prefix),
    ] {
        if prefix.len() > 256 || prefix.chars().any(char::is_control) {
            return Err(Error::Config(format!(
                "semantic.{name} must be at most 256 bytes without control characters"
            )));
        }
    }
    Ok(())
}

fn loopback_endpoint(raw: &str, local_only: bool) -> Result<url::Url> {
    let url = url::Url::parse(raw)
        .map_err(|error| Error::Config(format!("invalid semantic.endpoint: {error}")))?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(Error::Config(
            "semantic.endpoint must be an http(s) URL without credentials, query or fragment"
                .to_owned(),
        ));
    }
    let loopback = match url.host() {
        Some(url::Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    };
    if !loopback {
        return Err(Error::Config(format!(
            "semantic.endpoint must be a loopback address (localhost, 127.0.0.0/8 or [::1]); \
             remote embedding providers are unsupported (network.local_only = {local_only})"
        )));
    }
    Ok(url)
}

/// Deterministic hashed token + character-trigram embedder.
///
/// Same text always produces the same vector: hashing is seeded by a stable
/// constant, never wall-clock time or process randomness.
#[derive(Clone, Copy, Debug, Default)]
pub struct HashedNGramEmbedder;

impl HashedNGramEmbedder {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    fn embed_one(text: &str) -> Vec<f32> {
        let mut vector = vec![0.0_f32; EMBEDDING_DIMENSION];
        for token in tokens(text) {
            accumulate_feature(&mut vector, "t:", &token);
            let characters = token.chars().collect::<Vec<_>>();
            for window in characters.windows(3) {
                let trigram = window.iter().collect::<String>();
                accumulate_feature(&mut vector, "g:", &trigram);
            }
        }
        l2_normalize(&mut vector);
        vector
    }
}

impl EmbeddingProvider for HashedNGramEmbedder {
    fn identity(&self) -> &str {
        EMBEDDER_ID
    }

    fn dimension(&self) -> usize {
        EMBEDDING_DIMENSION
    }

    fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|text| Self::embed_one(text)).collect())
    }
}

/// `OpenAI`-compatible embeddings client for a loopback endpoint.
///
/// Every request is bounded: at most `batch_size` texts of at most
/// [`MAX_INPUT_BYTES`] each, one deadline covering connect through the last
/// response byte, a response budget proportional to the vectors requested,
/// no proxy, no redirects, and `localhost` pinned to 127.0.0.1.
#[derive(Clone, Debug)]
pub struct LocalEmbedder {
    endpoint: url::Url,
    model: String,
    identity: String,
    dimension: Option<usize>,
    batch_size: usize,
    timeout: Duration,
    prefix: String,
}

impl LocalEmbedder {
    /// Embedder for stored memory text: document prefix and rebuild timeout.
    ///
    /// # Errors
    ///
    /// Returns a configuration error for a non-loopback endpoint or an
    /// empty model name.
    pub fn for_documents(settings: &LocalEmbedderSettings) -> Result<Self> {
        Self::new(settings, &settings.document_prefix, settings.timeout)
    }

    /// Embedder for task text at retrieval time: query prefix and the short
    /// query timeout.
    ///
    /// # Errors
    ///
    /// Returns a configuration error for a non-loopback endpoint or an
    /// empty model name.
    pub fn for_queries(settings: &LocalEmbedderSettings) -> Result<Self> {
        Self::new(settings, &settings.query_prefix, settings.query_timeout)
    }

    fn new(settings: &LocalEmbedderSettings, prefix: &str, timeout: Duration) -> Result<Self> {
        let endpoint = loopback_endpoint(&settings.endpoint, true)?;
        if settings.model.trim().is_empty() {
            return Err(Error::Config(
                "a local embedding provider requires a model name".to_owned(),
            ));
        }
        Ok(Self {
            endpoint,
            model: settings.model.clone(),
            identity: settings.identity(),
            dimension: settings.dimension,
            batch_size: settings.batch_size.clamp(1, MAX_BATCH_SIZE),
            timeout,
            prefix: prefix.to_owned(),
        })
    }

    /// The exact input sent for `text`: prefix plus text, cut at
    /// [`MAX_INPUT_BYTES`] on a character boundary.
    #[must_use]
    pub fn prepared(&self, text: &str) -> String {
        prepare_input(&self.prefix, text)
    }

    fn cooldown_key(&self) -> String {
        format!("{} {}", self.endpoint, self.model)
    }

    /// Embed already-prepared inputs in bounded batches. Every vector must
    /// have the same dimension: `expected`, else the configured dimension,
    /// else whatever the first batch returned.
    fn embed_prepared(&self, inputs: &[String], expected: Option<usize>) -> Result<Vec<Vec<f32>>> {
        let mut expected = expected.or(self.dimension);
        let mut vectors = Vec::with_capacity(inputs.len());
        for batch in inputs.chunks(self.batch_size) {
            let embedded = self.request(batch, expected)?;
            expected = expected.or_else(|| embedded.first().map(Vec::len));
            vectors.extend(embedded);
        }
        Ok(vectors)
    }

    fn request(&self, inputs: &[String], expected: Option<usize>) -> Result<Vec<Vec<f32>>> {
        let body = serde_json::to_vec(&serde_json::json!({
            "model": self.model,
            "input": inputs,
        }))?;
        if body.len() > MAX_REQUEST_BYTES {
            return Err(Error::Semantic(format!(
                "embedding request of {} bytes exceeds the {MAX_REQUEST_BYTES}-byte bound",
                body.len()
            )));
        }
        let limit = response_limit(inputs.len(), expected);
        let bytes = post_bounded(&self.endpoint, body, self.timeout, limit)?;
        parse_embeddings(&bytes, inputs.len(), expected, &self.model)
    }
}

impl EmbeddingProvider for LocalEmbedder {
    fn identity(&self) -> &str {
        &self.identity
    }

    fn dimension(&self) -> usize {
        self.dimension.unwrap_or(0)
    }

    fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        let inputs = texts
            .iter()
            .map(|text| self.prepared(text))
            .collect::<Vec<_>>();
        self.embed_prepared(&inputs, None)
    }
}

fn prepare_input(prefix: &str, text: &str) -> String {
    let mut input = format!("{prefix}{text}");
    let mut end = input.len().min(MAX_INPUT_BYTES);
    while !input.is_char_boundary(end) {
        end -= 1;
    }
    input.truncate(end);
    input
}

/// Response budget for `count` vectors: proportional to the vectors asked
/// for, using the largest supported dimension when the dimension is unknown.
fn response_limit(count: usize, dimension: Option<usize>) -> usize {
    let dimension = dimension.unwrap_or(MAX_MODEL_DIMENSION).max(1);
    count
        .saturating_mul(dimension)
        .saturating_mul(RESPONSE_BYTES_PER_VALUE)
        .saturating_add(RESPONSE_OVERHEAD_BYTES)
}

/// POST `body` and return the complete response body, refusing redirects,
/// proxies, error statuses and bodies over `limit` bytes. The request runs on
/// a dedicated thread with its own runtime, so callers may be synchronous code
/// on any thread, including one already driving a tokio runtime.
fn post_bounded(
    endpoint: &url::Url,
    body: Vec<u8>,
    timeout: Duration,
    limit: usize,
) -> Result<Vec<u8>> {
    let endpoint = endpoint.clone();
    run_isolated(timeout, async move {
        let display = endpoint.to_string();
        let mut builder = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(timeout)
            .timeout(timeout)
            .pool_max_idle_per_host(0);
        // Pin localhost rather than trusting a changed hosts/DNS resolution.
        if endpoint.host_str() == Some("localhost") {
            builder = builder.resolve(
                "localhost",
                std::net::SocketAddr::from((
                    [127, 0, 0, 1],
                    endpoint.port_or_known_default().unwrap_or(80),
                )),
            );
        }
        let client = builder
            .build()
            .map_err(|error| Error::Semantic(format!("embedding client: {error}")))?;
        let mut response = client
            .post(endpoint)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await
            .map_err(|error| transport_error(&display, timeout, &error))?;
        let status = response.status();
        if !status.is_success() {
            let detail = read_body(&mut response, ERROR_BODY_BYTES, true, &display, timeout)
                .await
                .map(|bytes| error_detail(&bytes))
                .unwrap_or_default();
            return Err(Error::Semantic(format!(
                "local embedding endpoint returned HTTP {status}{} ({display})",
                if detail.is_empty() {
                    String::new()
                } else {
                    format!(": {detail}")
                }
            )));
        }
        if response
            .content_length()
            .is_some_and(|length| length > u64::try_from(limit).unwrap_or(u64::MAX))
        {
            return Err(oversized(limit));
        }
        read_body(&mut response, limit, false, &display, timeout).await
    })
}

/// Read at most `limit` body bytes. Over the limit, either truncate (error
/// bodies kept for a message) or fail (embedding payloads).
async fn read_body(
    response: &mut reqwest::Response,
    limit: usize,
    truncate: bool,
    display: &str,
    timeout: Duration,
) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| transport_error(display, timeout, &error))?
    {
        if bytes.len() + chunk.len() > limit {
            if truncate {
                bytes.extend_from_slice(&chunk[..limit - bytes.len()]);
                return Ok(bytes);
            }
            return Err(oversized(limit));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// Run `future` to completion on a fresh current-thread runtime in a scoped
/// thread, bounded by `timeout`.
fn run_isolated<T: Send>(
    timeout: Duration,
    future: impl Future<Output = Result<T>> + Send,
) -> Result<T> {
    std::thread::scope(|scope| {
        let worker = std::thread::Builder::new()
            .name("engram-embeddings".to_owned())
            .spawn_scoped(scope, move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|error| Error::Semantic(format!("embedding runtime: {error}")))?;
                runtime.block_on(async move {
                    tokio::time::timeout(timeout, future)
                        .await
                        .map_err(|_| timed_out(timeout))?
                })
            })
            .map_err(|error| Error::Semantic(format!("embedding worker: {error}")))?;
        worker
            .join()
            .map_err(|_| Error::Semantic("embedding worker panicked".to_owned()))?
    })
}

fn timed_out(timeout: Duration) -> Error {
    Error::Semantic(format!(
        "local embedding request timed out after {} ms",
        timeout.as_millis()
    ))
}

fn oversized(limit: usize) -> Error {
    Error::Semantic(format!(
        "local embedding response exceeds the {limit}-byte bound for this request"
    ))
}

fn transport_error(endpoint: &str, timeout: Duration, error: &reqwest::Error) -> Error {
    if error.is_timeout() {
        return timed_out(timeout);
    }
    let mut detail = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        detail.push_str(": ");
        detail.push_str(&cause.to_string());
        source = cause.source();
    }
    let detail = clip(&sanitize(&detail), 300);
    if error.is_connect() {
        Error::Semantic(format!(
            "local embedding endpoint {endpoint} is unreachable: {detail}"
        ))
    } else {
        Error::Semantic(format!(
            "local embedding request to {endpoint} failed: {detail}"
        ))
    }
}

#[derive(Deserialize)]
struct EmbeddingsResponse {
    data: Vec<EmbeddingDatum>,
}

#[derive(Deserialize)]
struct EmbeddingDatum {
    #[serde(default)]
    index: Option<usize>,
    embedding: Vec<f64>,
}

/// Parse an `OpenAI` embeddings list into exactly `count` finite vectors of
/// one dimension, ordered by their `index` fields when present.
fn parse_embeddings(
    bytes: &[u8],
    count: usize,
    expected: Option<usize>,
    model: &str,
) -> Result<Vec<Vec<f32>>> {
    let response: EmbeddingsResponse = serde_json::from_slice(bytes).map_err(|error| {
        Error::Semantic(format!(
            "local embedding response is not an OpenAI-compatible list of float vectors: {error}"
        ))
    })?;
    if response.data.len() != count {
        return Err(Error::Semantic(format!(
            "local embedding response has {} vectors for {count} inputs",
            response.data.len()
        )));
    }
    let indexed = response
        .data
        .iter()
        .filter(|datum| datum.index.is_some())
        .count();
    if indexed != 0 && indexed != count {
        return Err(Error::Semantic(
            "local embedding response mixes indexed and unindexed vectors".to_owned(),
        ));
    }
    let mut dimension = expected;
    let mut slots: Vec<Option<Vec<f32>>> = vec![None; count];
    for (position, datum) in response.data.into_iter().enumerate() {
        let slot = datum.index.unwrap_or(position);
        if slot >= count || slots[slot].is_some() {
            return Err(Error::Semantic(format!(
                "local embedding response has an invalid or duplicate index {slot}"
            )));
        }
        let length = datum.embedding.len();
        if length == 0 || length > MAX_MODEL_DIMENSION {
            return Err(Error::Semantic(format!(
                "local embedding model `{model}` returned a {length}-dimensional vector; \
                 supported dimensions are 1..={MAX_MODEL_DIMENSION}"
            )));
        }
        if let Some(required) = dimension
            && length != required
        {
            return Err(Error::Semantic(format!(
                "local embedding model `{model}` returned {length}-dimensional vectors where \
                 {required} were expected (semantic.dimension or the model's earlier vectors); \
                 vector spaces are never mixed, so rebuild after changing models"
            )));
        }
        dimension = Some(length);
        let mut vector = Vec::with_capacity(length);
        for value in datum.embedding {
            #[allow(clippy::cast_possible_truncation)]
            let value = value as f32;
            if !value.is_finite() {
                return Err(Error::Semantic(format!(
                    "local embedding model `{model}` returned a non-finite vector component"
                )));
            }
            vector.push(value);
        }
        slots[slot] = Some(vector);
    }
    Ok(slots.into_iter().flatten().collect())
}

/// Brute-force cosine search over one embedder's rows in the `SQLite`
/// sidecar of vector BLOBs.
///
/// All writes reuse the canonical single-writer connection; searches use the
/// bounded read pool and scan at most `max_scanned` memories per query.
#[derive(Clone, Debug)]
pub struct SqliteVectorIndex {
    database: Database,
    max_scanned: usize,
    embedder: String,
    dimension: Option<usize>,
}

/// Search hits plus how much of the project the searched embedder covers.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CoverageSearch {
    pub hits: Vec<VectorHit>,
    /// Eligible (active, validated, structurally current) memories scanned.
    pub eligible: usize,
    /// Eligible memories with a vector of the query's dimension.
    pub covered: usize,
    /// Eligible memories whose vector has another dimension (stale).
    pub mismatched: usize,
}

impl SqliteVectorIndex {
    /// Index over the built-in hashed embedder's vectors.
    #[must_use]
    pub fn new(database: Database, max_scanned: usize) -> Self {
        Self {
            database,
            max_scanned,
            embedder: EMBEDDER_ID.to_owned(),
            dimension: Some(EMBEDDING_DIMENSION),
        }
    }

    /// Index over one model identity's vectors, of any single dimension.
    #[must_use]
    pub fn for_embedder(database: Database, max_scanned: usize, embedder: &str) -> Self {
        Self {
            database,
            max_scanned,
            embedder: embedder.to_owned(),
            dimension: None,
        }
    }

    fn check_dimension(&self, length: usize, what: &str) -> Result<()> {
        let valid = self
            .dimension
            .map_or((1..=MAX_MODEL_DIMENSION).contains(&length), |dimension| {
                dimension == length
            });
        if valid {
            Ok(())
        } else {
            Err(Error::Semantic(format!(
                "{what} has dimension {length}, which `{}` vectors cannot have",
                self.embedder
            )))
        }
    }

    /// Search, and count how many eligible memories this embedder covers.
    /// Vectors of another dimension than `query` are counted, never compared.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid query vector, a corrupt stored vector
    /// or failed storage reads.
    pub fn search_with_coverage(
        &self,
        project_id: &ProjectId,
        query: &[f32],
        k: usize,
    ) -> Result<CoverageSearch> {
        self.check_dimension(query.len(), "query vector")?;
        let max_scanned = i64::try_from(self.max_scanned).unwrap_or(i64::MAX);
        let mut found = CoverageSearch::default();
        self.database.with_reader(|connection| {
            let mut statement = connection.prepare_cached(
                "SELECT m.id, v.vector FROM memories m
                 LEFT JOIN memory_vectors v ON v.memory_id = m.id AND v.embedder = ?2
                 WHERE m.project_id = ?1 AND m.status = 'active'
                   AND m.epistemic_state = 'validated'
                   AND NOT EXISTS (
                     SELECT 1 FROM memory_nodes mn
                     LEFT JOIN nodes n ON n.id = mn.node_id AND n.tombstoned_at_ms IS NULL
                     WHERE mn.memory_id = m.id
                       AND (n.id IS NULL OR n.current_digest <> mn.observed_digest)
                   )
                 ORDER BY m.id LIMIT ?3",
            )?;
            let mut rows =
                statement.query(params![project_id.as_str(), self.embedder, max_scanned])?;
            while let Some(row) = rows.next()? {
                found.eligible += 1;
                let blob = match row.get_ref(1)? {
                    ValueRef::Blob(blob) => blob,
                    ValueRef::Null => continue,
                    _ => {
                        return Err(Error::Semantic("stored vector is not a BLOB".to_owned()));
                    }
                };
                match blob_cosine(query, blob)? {
                    Some(score) => {
                        found.covered += 1;
                        found.hits.push(VectorHit {
                            memory_id: MemoryId::parse(&row.get::<_, String>(0)?)?,
                            score,
                        });
                    }
                    None => found.mismatched += 1,
                }
            }
            Ok(())
        })?;
        found.hits.sort_by(|left, right| {
            right
                .score
                .total_cmp(&left.score)
                .then_with(|| left.memory_id.cmp(&right.memory_id))
        });
        found.hits.truncate(k);
        Ok(found)
    }
}

impl VectorIndex for SqliteVectorIndex {
    fn upsert(&self, items: &[VectorItem]) -> Result<()> {
        for item in items {
            self.check_dimension(
                item.vector.len(),
                &format!("vector for `{}`", item.memory_id),
            )?;
        }
        if items.is_empty() {
            return Ok(());
        }
        let now = unix_time_ms();
        self.database.write_immediate(|transaction| {
            for item in items {
                store_vector(
                    transaction,
                    &item.memory_id,
                    item.project_id.as_str(),
                    &self.embedder,
                    &item.vector,
                    None,
                    now,
                )?;
            }
            Ok(())
        })
    }

    fn search(&self, project_id: &ProjectId, query: &[f32], k: usize) -> Result<Vec<VectorHit>> {
        if k == 0 {
            self.check_dimension(query.len(), "query vector")?;
            return Ok(Vec::new());
        }
        Ok(self.search_with_coverage(project_id, query, k)?.hits)
    }

    fn delete(&self, memory_ids: &[MemoryId]) -> Result<()> {
        if memory_ids.is_empty() {
            return Ok(());
        }
        self.database.write_immediate(|transaction| {
            for memory_id in memory_ids {
                transaction.execute(
                    "DELETE FROM memory_vectors WHERE memory_id = ?1 AND embedder = ?2",
                    params![memory_id.as_str(), self.embedder],
                )?;
            }
            Ok(())
        })
    }

    /// Regenerate hashed vectors inside one canonical transaction. Model
    /// vectors need an embedding endpoint, which must never be called while
    /// holding the canonical writer: use [`rebuild_project`] for those.
    fn rebuild(&self, project_id: Option<&ProjectId>) -> Result<u64> {
        if self.embedder != EMBEDDER_ID {
            return Err(Error::Semantic(format!(
                "`{}` vectors come from an embedding endpoint and are rebuilt outside the \
                 canonical writer (`engram semantic rebuild`)",
                self.embedder
            )));
        }
        let now = unix_time_ms();
        self.database.write_immediate(|transaction| {
            transaction.execute(
                "DELETE FROM memory_vectors
                 WHERE embedder = ?2 AND (?1 IS NULL OR project_id = ?1)",
                params![project_id.map(ProjectId::as_str), EMBEDDER_ID],
            )?;
            let sources = {
                let mut statement = transaction.prepare(
                    "SELECT id, project_id, claim, rationale, tags_json FROM memories
                     WHERE status = 'active' AND (?1 IS NULL OR project_id = ?1)
                     ORDER BY id",
                )?;
                let rows = statement.query_map([project_id.map(ProjectId::as_str)], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                })?;
                rows.collect::<std::result::Result<Vec<_>, _>>()?
            };
            let mut written = 0_u64;
            for (raw_id, raw_project, claim, rationale, tags_json) in sources {
                let memory_id = MemoryId::parse(&raw_id)?;
                let tags: Vec<String> = serde_json::from_str(&tags_json)?;
                let vector = memory_vector(&claim, &rationale, &tags);
                store_vector(
                    transaction,
                    &memory_id,
                    &raw_project,
                    EMBEDDER_ID,
                    &vector,
                    None,
                    now,
                )?;
                written = written.saturating_add(1);
            }
            Ok(written)
        })
    }
}

/// Query-time semantic stage. It embeds the task text with the configured
/// provider and searches that provider's vectors. It never fails retrieval:
/// an unreachable endpoint falls back to the hashed sidecar, and a missing,
/// stale or unreadable index contributes nothing and requests a rebuild
/// (spec §27.2). Every outcome is explained in the returned note.
#[derive(Clone, Debug)]
pub struct SemanticSearch {
    database: Database,
    options: SemanticOptions,
    local: std::result::Result<Option<LocalEmbedder>, String>,
    /// Last successful query embedding, so linked-project passes of one
    /// compile embed the task text once.
    memo: QueryMemo,
}

type QueryMemo = Arc<Mutex<Option<(String, Vec<f32>)>>>;

/// Semantic hits and their provenance for `why.note`.
#[derive(Clone, Debug, PartialEq)]
pub struct SemanticOutcome {
    /// Hits whose score is already mapped into `(0, 1]`.
    pub hits: Vec<VectorHit>,
    /// Which embedder served, any fallback, and coverage gaps.
    pub note: String,
}

impl SemanticSearch {
    #[must_use]
    pub fn new(database: Database, options: SemanticOptions) -> Self {
        let local = options
            .local
            .as_ref()
            .map(LocalEmbedder::for_queries)
            .transpose()
            .map_err(|error| detail(&error));
        Self {
            database,
            options,
            local,
            memo: Arc::new(Mutex::new(None)),
        }
    }

    /// Embed `text` and return the top semantic hits for the project.
    #[must_use]
    pub fn search(&self, project_id: &ProjectId, text: &str) -> SemanticOutcome {
        let local = match &self.local {
            Ok(None) => {
                let query = HashedNGramEmbedder::embed_one(text);
                return self.outcome(
                    project_id,
                    EMBEDDER_ID,
                    &query,
                    self.options.min_similarity,
                    None,
                );
            }
            Ok(Some(local)) => local,
            Err(error) => {
                let reason = format!("local embedder misconfigured: {error}");
                return self.hashed_fallback(project_id, text, &reason);
            }
        };
        if let Some(reason) = cooling_down(&local.cooldown_key()) {
            return self.hashed_fallback(
                project_id,
                text,
                &format!(
                    "{} unavailable, retrying after a cooldown: {reason}",
                    local.identity
                ),
            );
        }
        match self.query_vector(local, text) {
            Ok(query) => {
                forget_cooldown(&local.cooldown_key());
                self.outcome(
                    project_id,
                    &local.identity,
                    &query,
                    self.options.min_similarity,
                    None,
                )
            }
            Err(error) => {
                let reason = clip(&detail(&error), NOTE_DETAIL_CHARS);
                start_cooldown(&local.cooldown_key(), &reason);
                self.hashed_fallback(
                    project_id,
                    text,
                    &format!("{} unavailable: {reason}", local.identity),
                )
            }
        }
    }

    fn query_vector(&self, local: &LocalEmbedder, text: &str) -> Result<Vec<f32>> {
        if let Some((cached, vector)) = self
            .memo
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            && cached == text
        {
            return Ok(vector.clone());
        }
        let vector = local
            .embed(&[text])?
            .into_iter()
            .next()
            .ok_or_else(|| Error::Semantic("embedder returned no query vector".to_owned()))?;
        *self.memo.lock().unwrap_or_else(PoisonError::into_inner) =
            Some((text.to_owned(), vector.clone()));
        Ok(vector)
    }

    fn hashed_fallback(&self, project_id: &ProjectId, text: &str, reason: &str) -> SemanticOutcome {
        let query = HashedNGramEmbedder::embed_one(text);
        self.outcome(project_id, EMBEDDER_ID, &query, 0.0, Some(reason))
    }

    fn outcome(
        &self,
        project_id: &ProjectId,
        embedder: &str,
        query: &[f32],
        floor: f64,
        fallback: Option<&str>,
    ) -> SemanticOutcome {
        let served = fallback.map_or_else(
            || embedder.to_owned(),
            |reason| format!("{embedder} fallback ({reason})"),
        );
        let as_fallback = fallback.map_or_else(String::new, |reason| {
            format!(", used as fallback ({reason})")
        });
        let index = if embedder == EMBEDDER_ID {
            SqliteVectorIndex::new(self.database.clone(), self.options.max_scanned)
        } else {
            SqliteVectorIndex::for_embedder(
                self.database.clone(),
                self.options.max_scanned,
                embedder,
            )
        };
        let found = match index.search_with_coverage(project_id, query, self.options.max_candidates)
        {
            Ok(found) => found,
            Err(error) => {
                request_rebuild(&self.database, project_id);
                return SemanticOutcome {
                    hits: Vec::new(),
                    note: format!(
                        "semantic skipped: {embedder} index unavailable ({}){as_fallback}; \
                         lexical and structural only; rebuild requested",
                        clip(&detail(&error), NOTE_DETAIL_CHARS)
                    ),
                };
            }
        };
        if found.covered == found.eligible {
            return SemanticOutcome {
                hits: floor_scores(found.hits, floor),
                note: format!("semantic: {served}"),
            };
        }
        request_rebuild(&self.database, project_id);
        if found.covered == 0 {
            return SemanticOutcome {
                hits: Vec::new(),
                note: format!(
                    "semantic skipped: no current {embedder} vectors for {} memories{as_fallback}; \
                     lexical and structural only; rebuild requested",
                    found.eligible
                ),
            };
        }
        SemanticOutcome {
            hits: floor_scores(found.hits, floor),
            note: format!(
                "semantic: {served} ({} of {} memories have current vectors; rebuild requested)",
                found.covered, found.eligible
            ),
        }
    }
}

/// Map raw cosine similarity to `(0, 1]` above `floor`; drop the rest.
fn floor_scores(hits: Vec<VectorHit>, floor: f64) -> Vec<VectorHit> {
    hits.into_iter()
        .filter_map(|hit| {
            let score = ((hit.score - floor) / (1.0 - floor)).clamp(0.0, 1.0);
            (score > 0.0).then_some(VectorHit {
                memory_id: hit.memory_id,
                score,
            })
        })
        .collect()
}

static COOLDOWNS: Mutex<BTreeMap<String, (Instant, String)>> = Mutex::new(BTreeMap::new());

fn cooling_down(key: &str) -> Option<String> {
    let mut cooldowns = COOLDOWNS.lock().unwrap_or_else(PoisonError::into_inner);
    match cooldowns.get(key) {
        Some((until, reason)) if Instant::now() < *until => Some(reason.clone()),
        Some(_) => {
            cooldowns.remove(key);
            None
        }
        None => None,
    }
}

fn start_cooldown(key: &str, reason: &str) {
    COOLDOWNS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(
            key.to_owned(),
            (Instant::now() + QUERY_FAILURE_COOLDOWN, reason.to_owned()),
        );
}

fn forget_cooldown(key: &str) {
    COOLDOWNS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(key);
}

/// Projects whose semantic index retrieval found missing, stale or
/// unreadable, keyed by database so one process never schedules work against
/// another database.
static REBUILD_REQUESTS: Mutex<BTreeMap<PathBuf, BTreeSet<ProjectId>>> =
    Mutex::new(BTreeMap::new());

fn request_rebuild(database: &Database, project_id: &ProjectId) {
    REBUILD_REQUESTS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .entry(database.path().to_path_buf())
        .or_default()
        .insert(project_id.clone());
}

/// Drain the projects for which retrieval requested a semantic rebuild. The
/// daemon's job loop turns them into `semantic_rebuild` jobs.
#[must_use]
pub fn take_rebuild_requests(database: &Database) -> Vec<ProjectId> {
    REBUILD_REQUESTS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(database.path())
        .map(|projects| projects.into_iter().collect())
        .unwrap_or_default()
}

/// What one model-vector rebuild did.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ModelRebuildReport {
    /// Identity the vectors are stored and searched under.
    pub embedder: String,
    /// Dimension the model returned for the probe and every vector.
    pub dimension: usize,
    /// Active memories considered.
    pub active_memories: u64,
    /// Vectors embedded and written by this run.
    pub embedded: u64,
    /// Vectors whose embedded text was unchanged and were kept.
    pub unchanged: u64,
    /// Memories that changed or deactivated while being embedded; a later
    /// rebuild picks them up.
    pub skipped_changed: u64,
    /// Rows removed: other models' vectors, other dimensions, and vectors of
    /// inactive memories in this project.
    pub removed: u64,
    /// Embedding requests made, probe included.
    pub requests: u64,
    pub duration_ms: u64,
    /// False when the deadline stopped the run; progress is kept and a rerun
    /// continues from the remaining memories.
    pub complete: bool,
}

/// What one project rebuild did.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct RebuildReport {
    /// Hashed vectors rewritten inside the canonical writer.
    pub vectors: u64,
    /// Model vectors, when a local embedding provider is configured.
    pub model: Option<ModelRebuildReport>,
}

/// Rebuild one project's derived vectors. Hashed vectors are regenerated in
/// one canonical transaction. With a local model configured, model vectors
/// are then embedded outside the writer and committed page by page through
/// the scheduler, skipping memories whose embedded text is unchanged and
/// removing vectors from other models, so a model change rebuilds rather
/// than mixing vector spaces. Each committed page is kept even when a later
/// step fails or `deadline` passes.
///
/// # Errors
///
/// Returns storage, scheduler, or embedding endpoint errors.
pub fn rebuild_project(
    database: &Database,
    scheduler: &MutationScheduler,
    local: Option<&LocalEmbedderSettings>,
    project_id: &ProjectId,
    deadline: Option<Instant>,
) -> Result<RebuildReport> {
    let index = SqliteVectorIndex::new(database.clone(), usize::MAX);
    let project = project_id.clone();
    let vectors = scheduler
        .submit(MutationPriority::BackgroundLearning, move || {
            index.rebuild(Some(&project))
        })?
        .wait()?;
    let model = local
        .map(|settings| rebuild_model_vectors(database, scheduler, settings, project_id, deadline))
        .transpose()?;
    Ok(RebuildReport { vectors, model })
}

struct ModelSource {
    memory_id: MemoryId,
    text: String,
    stored_digest: Option<String>,
    stored_dimension: Option<usize>,
}

fn rebuild_model_vectors(
    database: &Database,
    scheduler: &MutationScheduler,
    settings: &LocalEmbedderSettings,
    project_id: &ProjectId,
    deadline: Option<Instant>,
) -> Result<ModelRebuildReport> {
    let started = Instant::now();
    let embedder = LocalEmbedder::for_documents(settings)?;
    // The probe learns the model's current dimension and proves the endpoint
    // answers before any stored vector is touched.
    let dimension = embedder
        .embed_prepared(&[embedder.prepared(PROBE_TEXT)], None)?
        .first()
        .map_or(0, Vec::len);
    let mut report = ModelRebuildReport {
        embedder: embedder.identity.clone(),
        dimension,
        requests: 1,
        complete: true,
        ..ModelRebuildReport::default()
    };
    report.removed = {
        let (database, project, identity) = (
            database.clone(),
            project_id.clone(),
            report.embedder.clone(),
        );
        scheduler
            .submit(MutationPriority::BackgroundLearning, move || {
                database.write_immediate(|transaction| {
                    remove_foreign_model_rows(transaction, &project, &identity, dimension)
                })
            })?
            .wait()?
    };
    let mut after: Option<String> = None;
    loop {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            report.complete = false;
            break;
        }
        let page = model_sources(database, project_id, &report.embedder, after.as_deref())?;
        let Some(last) = page.last() else {
            break;
        };
        after = Some(last.memory_id.to_string());
        report.active_memories += u64::try_from(page.len()).unwrap_or(u64::MAX);
        let mut pending = Vec::new();
        for source in page {
            let input = embedder.prepared(&source.text);
            let digest = crate::experience::sha256(input.as_bytes());
            if source.stored_digest.as_deref() == Some(digest.as_str())
                && source.stored_dimension == Some(dimension)
            {
                report.unchanged += 1;
            } else {
                pending.push((source.memory_id, input, digest));
            }
        }
        if pending.is_empty() {
            continue;
        }
        let inputs = pending
            .iter()
            .map(|(_, input, _)| input.clone())
            .collect::<Vec<_>>();
        let vectors = embedder.embed_prepared(&inputs, Some(dimension))?;
        report.requests +=
            u64::try_from(inputs.len().div_ceil(embedder.batch_size)).unwrap_or(u64::MAX);
        let writes = pending
            .into_iter()
            .zip(vectors)
            .map(|((memory_id, _, digest), vector)| (memory_id, digest, vector))
            .collect::<Vec<_>>();
        let (written, changed) = {
            let (database, project, identity, prefix) = (
                database.clone(),
                project_id.clone(),
                report.embedder.clone(),
                embedder.prefix.clone(),
            );
            scheduler
                .submit(MutationPriority::BackgroundLearning, move || {
                    database.write_immediate(|transaction| {
                        write_model_rows(transaction, &project, &identity, &prefix, &writes)
                    })
                })?
                .wait()?
        };
        report.embedded += written;
        report.skipped_changed += changed;
    }
    report.duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    Ok(report)
}

/// Remove this project's vectors that the rebuilt model space must not see:
/// other model identities, the same identity at another dimension, and
/// vectors of memories that are no longer active. Hashed rows are kept.
fn remove_foreign_model_rows(
    transaction: &Transaction<'_>,
    project_id: &ProjectId,
    identity: &str,
    dimension: usize,
) -> Result<u64> {
    let dimension = i64::try_from(dimension).unwrap_or(i64::MAX);
    let removed = transaction.execute(
        "DELETE FROM memory_vectors
         WHERE project_id = ?1 AND embedder <> ?2
           AND (embedder <> ?3 OR dimension <> ?4 OR memory_id NOT IN (
                SELECT id FROM memories WHERE project_id = ?1 AND status = 'active'))",
        params![project_id.as_str(), EMBEDDER_ID, identity, dimension],
    )?;
    Ok(u64::try_from(removed).unwrap_or(u64::MAX))
}

fn model_sources(
    database: &Database,
    project_id: &ProjectId,
    identity: &str,
    after: Option<&str>,
) -> Result<Vec<ModelSource>> {
    let page = i64::try_from(REBUILD_PAGE).unwrap_or(i64::MAX);
    database.with_reader(|connection| {
        let mut statement = connection.prepare_cached(
            "SELECT m.id, m.claim, m.rationale, m.tags_json, v.source_digest, v.dimension
             FROM memories m
             LEFT JOIN memory_vectors v ON v.memory_id = m.id AND v.embedder = ?2
             WHERE m.project_id = ?1 AND m.status = 'active' AND (?3 IS NULL OR m.id > ?3)
             ORDER BY m.id LIMIT ?4",
        )?;
        let mut rows = statement.query(params![project_id.as_str(), identity, after, page])?;
        let mut sources = Vec::new();
        while let Some(row) = rows.next()? {
            let tags: Vec<String> = serde_json::from_str(&row.get::<_, String>(3)?)?;
            sources.push(ModelSource {
                memory_id: MemoryId::parse(&row.get::<_, String>(0)?)?,
                text: memory_text(&row.get::<_, String>(1)?, &row.get::<_, String>(2)?, &tags),
                stored_digest: row.get(4)?,
                stored_dimension: row
                    .get::<_, Option<i64>>(5)?
                    .and_then(|dimension| usize::try_from(dimension).ok()),
            });
        }
        Ok(sources)
    })
}

/// Commit embedded vectors whose memory is still active with the text that
/// was embedded. Returns `(written, skipped_changed)`.
fn write_model_rows(
    transaction: &Transaction<'_>,
    project_id: &ProjectId,
    identity: &str,
    prefix: &str,
    writes: &[(MemoryId, String, Vec<f32>)],
) -> Result<(u64, u64)> {
    let now = unix_time_ms();
    let (mut written, mut changed) = (0_u64, 0_u64);
    for (memory_id, digest, vector) in writes {
        let current = transaction
            .query_row(
                "SELECT claim, rationale, tags_json FROM memories
                 WHERE id = ?1 AND project_id = ?2 AND status = 'active'",
                params![memory_id.as_str(), project_id.as_str()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?;
        let unchanged = match current {
            Some((claim, rationale, tags_json)) => {
                let tags: Vec<String> = serde_json::from_str(&tags_json)?;
                let input = prepare_input(prefix, &memory_text(&claim, &rationale, &tags));
                crate::experience::sha256(input.as_bytes()) == *digest
            }
            None => false,
        };
        if unchanged {
            store_vector(
                transaction,
                memory_id,
                project_id.as_str(),
                identity,
                vector,
                Some(digest),
                now,
            )?;
            written += 1;
        } else {
            changed += 1;
        }
    }
    Ok((written, changed))
}

/// The retrievable text of one memory, as every embedder sees it.
fn memory_text(claim: &str, rationale: &str, tags: &[String]) -> String {
    format!("{claim}\n{rationale}\n{}", tags.join(" "))
}

/// Embed the retrievable text of one memory with the default embedder.
#[must_use]
pub fn memory_vector(claim: &str, rationale: &str, tags: &[String]) -> Vec<f32> {
    HashedNGramEmbedder::embed_one(&memory_text(claim, rationale, tags))
}

/// Cosine similarity between two vectors; zero-norm inputs score 0.0.
#[must_use]
pub fn cosine_similarity(left: &[f32], right: &[f32]) -> f64 {
    let mut dot = 0.0_f64;
    let mut left_norm = 0.0_f64;
    let mut right_norm = 0.0_f64;
    for (a, b) in left.iter().zip(right.iter()) {
        dot += f64::from(*a) * f64::from(*b);
        left_norm += f64::from(*a) * f64::from(*a);
        right_norm += f64::from(*b) * f64::from(*b);
    }
    let denominator = left_norm.sqrt() * right_norm.sqrt();
    if denominator <= f64::EPSILON {
        0.0
    } else {
        dot / denominator
    }
}

/// Cosine similarity against a stored little-endian `f32` BLOB, or `None`
/// when the stored vector has another dimension than `query`.
fn blob_cosine(query: &[f32], blob: &[u8]) -> Result<Option<f64>> {
    if blob.len() % 4 != 0 {
        return Err(Error::Semantic(
            "stored vector BLOB is not a whole number of f32 values".to_owned(),
        ));
    }
    if blob.len() / 4 != query.len() {
        return Ok(None);
    }
    let stored = blob
        .chunks_exact(4)
        .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect::<Vec<_>>();
    Ok(Some(cosine_similarity(query, &stored)))
}

/// Upsert a memory's derived hashed vector inside an existing canonical
/// transaction, mirroring how `memories_fts` is maintained.
pub(crate) fn upsert_memory_vector(
    transaction: &Transaction<'_>,
    memory_id: &MemoryId,
    project_id: &ProjectId,
    claim: &str,
    rationale: &str,
    tags: &[String],
    now: i64,
) -> Result<()> {
    let vector = memory_vector(claim, rationale, tags);
    store_vector(
        transaction,
        memory_id,
        project_id.as_str(),
        EMBEDDER_ID,
        &vector,
        None,
        now,
    )
}

/// Replace every derived vector of a memory whose text was written or
/// rewritten: model vectors of the old text are dropped (the next rebuild
/// embeds the new text) and the hashed vector is re-derived.
pub(crate) fn replace_memory_vectors(
    transaction: &Transaction<'_>,
    memory_id: &MemoryId,
    project_id: &ProjectId,
    claim: &str,
    rationale: &str,
    tags: &[String],
    now: i64,
) -> Result<()> {
    delete_memory_vector(transaction, memory_id)?;
    upsert_memory_vector(
        transaction,
        memory_id,
        project_id,
        claim,
        rationale,
        tags,
        now,
    )
}

/// Remove every derived vector of a memory, from every embedder, inside an
/// existing canonical transaction.
pub(crate) fn delete_memory_vector(
    transaction: &Transaction<'_>,
    memory_id: &MemoryId,
) -> Result<()> {
    transaction.execute(
        "DELETE FROM memory_vectors WHERE memory_id = ?1",
        [memory_id.as_str()],
    )?;
    Ok(())
}

/// Re-derive the hashed vector row for a memory from its canonical text:
/// active memories keep an up-to-date vector, every other status drops all of
/// its vectors. Model vectors of an unchanged active memory stay valid.
pub(crate) fn sync_memory_vector(
    transaction: &Transaction<'_>,
    memory_id: &MemoryId,
    active: bool,
    now: i64,
) -> Result<()> {
    if !active {
        return delete_memory_vector(transaction, memory_id);
    }
    let source = transaction
        .query_row(
            "SELECT project_id, claim, rationale, tags_json FROM memories WHERE id = ?1",
            [memory_id.as_str()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .optional()?;
    let Some((project_id, claim, rationale, tags_json)) = source else {
        return Ok(());
    };
    let tags: Vec<String> = serde_json::from_str(&tags_json)?;
    let vector = memory_vector(&claim, &rationale, &tags);
    store_vector(
        transaction,
        memory_id,
        &project_id,
        EMBEDDER_ID,
        &vector,
        None,
        now,
    )
}

fn store_vector(
    transaction: &Transaction<'_>,
    memory_id: &MemoryId,
    project_id: &str,
    embedder: &str,
    vector: &[f32],
    source_digest: Option<&str>,
    now: i64,
) -> Result<()> {
    let dimension = i64::try_from(vector.len())
        .map_err(|_| Error::Semantic("vector dimension exceeds i64".to_owned()))?;
    transaction.execute(
        "INSERT INTO memory_vectors(memory_id, project_id, embedder, dimension, vector,
                                    updated_at_ms, source_digest)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(memory_id, embedder) DO UPDATE SET
             project_id = excluded.project_id,
             dimension = excluded.dimension,
             vector = excluded.vector,
             updated_at_ms = excluded.updated_at_ms,
             source_digest = excluded.source_digest",
        params![
            memory_id.as_str(),
            project_id,
            embedder,
            dimension,
            encode_vector(vector),
            now,
            source_digest
        ],
    )?;
    Ok(())
}

fn encode_vector(vector: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(vector.len() * 4);
    for value in vector {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

fn tokens(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|character: char| !character.is_alphanumeric() && character != '_')
        .filter(|token| !token.is_empty())
        .map(str::to_lowercase)
}

fn accumulate_feature(vector: &mut [f32], namespace: &str, feature: &str) {
    let mut hash = fnv1a(FEATURE_SEED, namespace.as_bytes());
    hash = fnv1a(hash, feature.as_bytes());
    let index = usize::try_from(hash % EMBEDDING_DIMENSION_U64).unwrap_or(0);
    let sign = if (hash >> 63) & 1 == 1 { -1.0 } else { 1.0 };
    vector[index] += sign;
}

fn fnv1a(seed: u64, bytes: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET_BASIS ^ seed;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

fn l2_normalize(vector: &mut [f32]) {
    let norm = vector
        .iter()
        .map(|value| f64::from(*value) * f64::from(*value))
        .sum::<f64>()
        .sqrt();
    if norm > f64::EPSILON {
        for value in vector.iter_mut() {
            #[allow(clippy::cast_possible_truncation)]
            {
                *value = (f64::from(*value) / norm) as f32;
            }
        }
    }
}

/// The useful part of an error body: an `OpenAI`/Ollama-style
/// `error.message` (or string `error`) when present, else the sanitized text.
fn error_detail(bytes: &[u8]) -> String {
    let parsed = serde_json::from_slice::<serde_json::Value>(bytes).ok();
    let message = parsed.as_ref().and_then(|body| {
        body.pointer("/error/message")
            .or_else(|| body.get("error"))
            .or_else(|| body.get("message"))
            .and_then(serde_json::Value::as_str)
    });
    sanitize(&message.map_or_else(
        || String::from_utf8_lossy(bytes).into_owned(),
        str::to_owned,
    ))
}

/// An error's message without the error kind's own prefix.
fn detail(error: &Error) -> String {
    match error {
        Error::Semantic(message) | Error::Config(message) => message.clone(),
        other => other.to_string(),
    }
}

/// Replace control characters so server-supplied text stays on one line.
fn sanitize(text: &str) -> String {
    text.chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>()
        .trim()
        .to_owned()
}

fn clip(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_owned(),
    }
}

fn unix_time_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};

    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
        })
}

/// A scripted `OpenAI`-compatible embeddings server on 127.0.0.1 for
/// hermetic tests: no network, no model.
#[cfg(test)]
pub(crate) mod fake_endpoint {
    use std::io::{Read as _, Write as _};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use serde_json::{Value, json};

    /// What the server does with one request.
    pub(crate) enum Reply {
        /// 200 with a `content-length`.
        Json(String),
        /// Another status with a body.
        Status(u16, String),
        /// 200 with `transfer-encoding: chunked` and no length.
        Chunked(Vec<Vec<u8>>),
        /// 307 to the given location.
        Redirect(String),
        /// Read the request, then stay silent this long.
        Hang(Duration),
    }

    pub(crate) struct FakeEndpoint {
        pub(crate) url: String,
        pub(crate) requests: Arc<Mutex<Vec<Value>>>,
    }

    impl FakeEndpoint {
        pub(crate) fn requests(&self) -> Vec<Value> {
            self.requests.lock().expect("requests").clone()
        }
    }

    /// Serve every request with `handler` until the test process exits.
    pub(crate) fn serve(handler: impl Fn(&Value) -> Reply + Send + 'static) -> FakeEndpoint {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake endpoint");
        let url = format!(
            "http://{}/v1/embeddings",
            listener.local_addr().expect("address")
        );
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&requests);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else {
                    break;
                };
                if let Some(body) = read_request(&mut stream) {
                    let reply = handler(&body);
                    seen.lock().expect("requests").push(body);
                    write_reply(&mut stream, reply);
                }
            }
        });
        FakeEndpoint { url, requests }
    }

    /// An `OpenAI` embeddings list, optionally in reverse index order.
    pub(crate) fn embeddings(vectors: &[Vec<f64>], reverse: bool) -> Reply {
        let mut data = vectors
            .iter()
            .enumerate()
            .map(|(index, vector)| json!({"object": "embedding", "index": index, "embedding": vector}))
            .collect::<Vec<_>>();
        if reverse {
            data.reverse();
        }
        Reply::Json(json!({"object": "list", "data": data, "model": "fake"}).to_string())
    }

    /// The request's `input` texts.
    pub(crate) fn inputs(request: &Value) -> Vec<String> {
        request["input"]
            .as_array()
            .expect("input array")
            .iter()
            .map(|input| input.as_str().expect("text input").to_owned())
            .collect()
    }

    const CONCEPTS: [&[&str]; 4] = [
        &["abort", "cancel", "stop", "halt"],
        &[
            "reservation",
            "reservations",
            "hold",
            "holds",
            "release",
            "free",
        ],
        &["log", "logs", "rotate", "archive", "archived"],
        &["retry", "retries", "attempt", "backoff"],
    ];

    /// A tiny deterministic "model": one dimension per concept, counting the
    /// concept's words, so paraphrases without shared words still match.
    pub(crate) fn concept_vector(text: &str, dimension: usize) -> Vec<f64> {
        let lower = text.to_lowercase();
        let words = lower
            .split(|character: char| !character.is_alphanumeric())
            .collect::<Vec<_>>();
        (0..dimension)
            .map(|axis| {
                CONCEPTS.get(axis).map_or(0.0, |keywords| {
                    let count = words.iter().filter(|word| keywords.contains(word)).count();
                    f64::from(u32::try_from(count).expect("small count"))
                })
            })
            .collect()
    }

    /// Serve concept vectors of `dimension` components for every input.
    pub(crate) fn concept_model(dimension: usize) -> FakeEndpoint {
        serve(move |request| {
            embeddings(
                &inputs(request)
                    .iter()
                    .map(|input| concept_vector(input, dimension))
                    .collect::<Vec<_>>(),
                false,
            )
        })
    }

    fn read_request(stream: &mut TcpStream) -> Option<Value> {
        stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
        let mut buffer = Vec::new();
        let mut chunk = [0_u8; 8_192];
        let header_end = loop {
            if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                break position + 4;
            }
            let read = stream.read(&mut chunk).ok()?;
            if read == 0 {
                return None;
            }
            buffer.extend_from_slice(&chunk[..read]);
        };
        let headers = String::from_utf8_lossy(&buffer[..header_end]).to_lowercase();
        let length = headers
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        while buffer.len() < header_end + length {
            let read = stream.read(&mut chunk).ok()?;
            if read == 0 {
                return None;
            }
            buffer.extend_from_slice(&chunk[..read]);
        }
        serde_json::from_slice(&buffer[header_end..header_end + length]).ok()
    }

    fn write_reply(stream: &mut TcpStream, reply: Reply) {
        let bytes = match reply {
            Reply::Json(body) => format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
                 connection: close\r\n\r\n{body}",
                body.len()
            )
            .into_bytes(),
            Reply::Status(code, body) => format!(
                "HTTP/1.1 {code} Fake\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
                 connection: close\r\n\r\n{body}",
                body.len()
            )
            .into_bytes(),
            Reply::Redirect(location) => format!(
                "HTTP/1.1 307 Temporary Redirect\r\nlocation: {location}\r\ncontent-length: 0\r\n\
                 connection: close\r\n\r\n"
            )
            .into_bytes(),
            Reply::Chunked(chunks) => {
                let mut bytes = b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                    transfer-encoding: chunked\r\nconnection: close\r\n\r\n"
                    .to_vec();
                for chunk in chunks {
                    bytes.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
                    bytes.extend_from_slice(&chunk);
                    bytes.extend_from_slice(b"\r\n");
                }
                bytes.extend_from_slice(b"0\r\n\r\n");
                bytes
            }
            Reply::Hang(duration) => {
                std::thread::sleep(duration);
                return;
            }
        };
        let _ = stream.write_all(&bytes);
        let _ = stream.flush();
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use serde_json::json;
    use tempfile::TempDir;

    use super::fake_endpoint::{self, Reply};
    use super::*;
    use crate::experience::{EvidenceStrength, ExperienceStore, TaskOutcome};
    use crate::memory::{EpistemicState, MemoryDraft, MemoryStore};
    use crate::projects::ProjectRegistry;

    const ABORT_CLAIM: &str = "Release reservations when abort cleanup runs";
    const LOG_CLAIM: &str = "Rotate archived log files every week";
    const RETRY_CLAIM: &str = "Retry backoff must count the failed attempt";

    fn fixture() -> (TempDir, Database, ProjectId) {
        let temp = TempDir::new().expect("temp");
        let root = temp.path().join("project");
        std::fs::create_dir(&root).expect("project");
        let database = Database::open(temp.path().join("brain.sqlite")).expect("database");
        let project = ProjectRegistry::new(database.clone())
            .resolve_or_register(&root)
            .expect("project")
            .project
            .id;
        (temp, database, project)
    }

    fn validated_memory(database: &Database, project: &ProjectId, claim: &str) -> MemoryId {
        let experience = ExperienceStore::new(database.clone());
        let task = experience.start_task(project, claim, None).expect("task");
        let evidence = experience
            .record_evidence(
                project,
                Some(&task.id),
                None,
                "test_result",
                EvidenceStrength::Deterministic,
                None,
                Some("cargo test"),
                Some(0),
                b"pass",
                &serde_json::json!({}),
            )
            .expect("evidence");
        experience
            .finish_task(&task.id, TaskOutcome::Success, Some(true), None)
            .expect("finish task");
        let store = MemoryStore::new(database.clone());
        let memory = store
            .create_candidate(
                project,
                &MemoryDraft {
                    scope_type: "project".to_owned(),
                    scope_id: project.to_string(),
                    kind: "lesson".to_owned(),
                    claim: claim.to_owned(),
                    rationale: "Validated by regression test".to_owned(),
                    confidence: 0.5,
                    importance: 0.5,
                    tags: Vec::new(),
                    valid_from_commit: None,
                    valid_to_commit: None,
                    trust_source: "agent_inference".to_owned(),
                    evidence_ids: vec![evidence.id],
                    source_task_ids: vec![task.id],
                    node_ids: Vec::new(),
                },
            )
            .expect("candidate");
        store
            .promote(&memory.id, EpistemicState::Validated, true)
            .expect("validated");
        memory.id
    }

    fn settings(endpoint: &str, model: &str) -> LocalEmbedderSettings {
        LocalEmbedderSettings {
            endpoint: endpoint.to_owned(),
            model: model.to_owned(),
            dimension: None,
            batch_size: 2,
            timeout: Duration::from_secs(5),
            query_timeout: Duration::from_secs(5),
            query_prefix: String::new(),
            document_prefix: String::new(),
        }
    }

    fn settings_identity(model: &str, document_prefix: &str) -> String {
        LocalEmbedderSettings {
            document_prefix: document_prefix.to_owned(),
            ..settings("http://127.0.0.1:1/v1/embeddings", model)
        }
        .identity()
    }

    fn local_options(settings: &LocalEmbedderSettings) -> SemanticOptions {
        SemanticOptions {
            local: Some(settings.clone()),
            ..SemanticOptions::hashed(50, 50_000)
        }
    }

    fn vector_rows(database: &Database, embedder: &str) -> i64 {
        database
            .with_reader(|connection| {
                connection
                    .query_row(
                        "SELECT COUNT(*) FROM memory_vectors WHERE embedder = ?1",
                        [embedder],
                        |row| row.get(0),
                    )
                    .map_err(Error::from)
            })
            .expect("vector rows")
    }

    /// A 127.0.0.1 port with nothing listening.
    fn closed_endpoint() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("address");
        drop(listener);
        format!("http://{address}/v1/embeddings")
    }

    fn scored(scores: &[f64]) -> Vec<VectorHit> {
        scores
            .iter()
            .map(|score| VectorHit {
                memory_id: MemoryId::new(),
                score: *score,
            })
            .collect()
    }

    #[test]
    fn embedder_is_deterministic_normalized_and_discriminative() {
        let embedder = HashedNGramEmbedder::new();
        let texts = [
            "release reservations during abort cleanup",
            "rotate log files weekly",
        ];
        let first = embedder.embed(&texts).expect("embed");
        let second = embedder.embed(&texts).expect("embed again");
        assert_eq!(first, second, "same text must map to the same vector");
        assert_ne!(first[0], first[1], "different texts must differ");
        for vector in &first {
            assert_eq!(vector.len(), EMBEDDING_DIMENSION);
            let norm = vector
                .iter()
                .map(|value| f64::from(*value) * f64::from(*value))
                .sum::<f64>()
                .sqrt();
            assert!((norm - 1.0).abs() < 1e-6, "vector must be L2-normalized");
        }
        assert!(cosine_similarity(&first[0], &first[0]) > 0.999_999);
        let related = embedder
            .embed(&["abort cleanup must release reservations"])
            .expect("related");
        assert!(
            cosine_similarity(&first[0], &related[0]) > cosine_similarity(&first[1], &related[0]),
            "shared tokens must score closer than unrelated text"
        );
        let empty = embedder.embed(&[""]).expect("empty");
        assert!(empty[0].iter().all(|value| *value == 0.0));
        assert_eq!(embedder.identity(), EMBEDDER_ID);
    }

    #[test]
    fn index_supports_upsert_search_delete_project_isolation_and_rebuild() {
        let (temp, database, project) = fixture();
        let other_root = temp.path().join("other");
        std::fs::create_dir(&other_root).expect("other root");
        let other_project = ProjectRegistry::new(database.clone())
            .resolve_or_register(&other_root)
            .expect("other project")
            .project
            .id;
        let abort_claim = "Release reservations during abort cleanup";
        let abort_id = validated_memory(&database, &project, abort_claim);
        let logging_id = validated_memory(&database, &project, "Rotate log files weekly");
        let foreign_id = validated_memory(&database, &other_project, abort_claim);

        let index = SqliteVectorIndex::new(database.clone(), 50_000);
        let query = memory_vector(abort_claim, "Validated by regression test", &[]);
        let hits = index.search(&project, &query, 10).expect("search");
        assert_eq!(hits[0].memory_id, abort_id);
        assert!(hits[0].score > 0.999, "self-similarity must be ~1.0");
        assert!(
            hits.iter().all(|hit| hit.memory_id != foreign_id),
            "search must never cross project boundaries"
        );
        let abort_score = hits[0].score;
        let logging_score = hits
            .iter()
            .find(|hit| hit.memory_id == logging_id)
            .map_or(0.0, |hit| hit.score);
        assert!(abort_score > logging_score);

        index
            .delete(std::slice::from_ref(&abort_id))
            .expect("delete");
        let after_delete = index.search(&project, &query, 10).expect("search");
        assert!(after_delete.iter().all(|hit| hit.memory_id != abort_id));

        // The sidecar is derived state: drop everything and rebuild from
        // canonical memories.
        database
            .write_immediate(|transaction| {
                transaction.execute("DELETE FROM memory_vectors", [])?;
                Ok(())
            })
            .expect("drop derived state");
        let rebuilt = index.rebuild(None).expect("rebuild");
        assert_eq!(rebuilt, 3);
        let after_rebuild = index.search(&project, &query, 10).expect("search");
        assert_eq!(after_rebuild[0].memory_id, abort_id);

        let scoped = index.rebuild(Some(&other_project)).expect("scoped rebuild");
        assert_eq!(scoped, 1);
        assert_eq!(
            index.search(&project, &query, 10).expect("search")[0].memory_id,
            abort_id,
            "scoped rebuild must not clear other projects"
        );
        let model_index = SqliteVectorIndex::for_embedder(database, 50_000, "local:any");
        assert!(
            model_index.rebuild(Some(&project)).is_err(),
            "model vectors must never be embedded inside the canonical writer"
        );
    }

    #[test]
    fn search_excludes_unvalidated_memories_and_rejects_bad_dimensions() {
        let (_temp, database, project) = fixture();
        let store = MemoryStore::new(database.clone());
        let candidate = store
            .create_candidate(
                &project,
                &MemoryDraft {
                    scope_type: "project".to_owned(),
                    scope_id: project.to_string(),
                    kind: "lesson".to_owned(),
                    claim: "Unvalidated abort cleanup lore".to_owned(),
                    rationale: "No supporting evidence yet".to_owned(),
                    confidence: 0.5,
                    importance: 0.5,
                    tags: Vec::new(),
                    valid_from_commit: None,
                    valid_to_commit: None,
                    trust_source: "agent_inference".to_owned(),
                    evidence_ids: Vec::new(),
                    source_task_ids: Vec::new(),
                    node_ids: Vec::new(),
                },
            )
            .expect("candidate");
        let index = SqliteVectorIndex::new(database, 50_000);
        let query = memory_vector("abort cleanup", "", &[]);
        let hits = index.search(&project, &query, 10).expect("search");
        assert!(
            hits.iter().all(|hit| hit.memory_id != candidate.id),
            "candidate memories must never be semantic candidates"
        );
        assert!(index.search(&project, &[0.0; 3], 10).is_err());
        assert!(
            index
                .upsert(&[VectorItem {
                    memory_id: candidate.id,
                    project_id: project,
                    vector: vec![0.0; 3],
                }])
                .is_err()
        );
    }

    #[test]
    fn local_embedder_batches_orders_by_index_and_applies_prefixes() {
        let endpoint = fake_endpoint::serve(|request| {
            let vectors = fake_endpoint::inputs(request)
                .iter()
                .map(|input| fake_endpoint::concept_vector(input, 4))
                .collect::<Vec<_>>();
            fake_endpoint::embeddings(&vectors, true)
        });
        let mut settings = settings(&endpoint.url, "fake-embed");
        settings.document_prefix = "search_document: ".to_owned();
        let embedder = LocalEmbedder::for_documents(&settings).expect("embedder");
        let texts = [
            "abort now",
            "rotate logs",
            "retry attempt",
            "free holds",
            "halt",
        ];
        let vectors = embedder.embed(&texts).expect("embed");

        let requests = endpoint.requests();
        let batch_sizes = requests
            .iter()
            .map(|request| fake_endpoint::inputs(request).len())
            .collect::<Vec<_>>();
        assert_eq!(
            batch_sizes,
            vec![2, 2, 1],
            "batches must respect batch_size"
        );
        assert!(
            requests
                .iter()
                .all(|request| request["model"] == "fake-embed")
        );
        assert!(
            requests
                .iter()
                .flat_map(fake_endpoint::inputs)
                .all(|input| input.starts_with("search_document: "))
        );
        for (text, vector) in texts.iter().zip(&vectors) {
            #[allow(clippy::cast_possible_truncation)]
            let expected = fake_endpoint::concept_vector(&format!("search_document: {text}"), 4)
                .iter()
                .map(|value| *value as f32)
                .collect::<Vec<_>>();
            assert_eq!(
                vector, &expected,
                "reverse-ordered data must map back by index"
            );
        }
        assert!(embedder.identity().starts_with("local:fake-embed#"));
        assert_eq!(embedder.dimension(), 0, "dimension is learned when unset");
        assert_eq!(settings_identity("fake-embed", ""), "local:fake-embed");
        let long = "x".repeat(MAX_INPUT_BYTES * 2);
        assert_eq!(embedder.prepared(&long).len(), MAX_INPUT_BYTES);
        assert!(prepare_input("", &"é".repeat(MAX_INPUT_BYTES)).len() <= MAX_INPUT_BYTES);
    }

    #[test]
    fn dimension_mismatch_and_malformed_vectors_are_refused() {
        let endpoint = fake_endpoint::concept_model(4);
        let mut configured = settings(&endpoint.url, "fake-embed");
        configured.dimension = Some(8);
        let error = LocalEmbedder::for_documents(&configured)
            .expect("embedder")
            .embed(&["abort"])
            .expect_err("dimension mismatch");
        let message = error.to_string();
        assert!(
            message.contains("4-dimensional") && message.contains("8 were expected"),
            "{message}"
        );

        // Later batches must keep the first batch's dimension.
        let drifting = fake_endpoint::serve(|request| {
            let inputs = fake_endpoint::inputs(request);
            let dimension = if inputs[0].contains("second") { 5 } else { 4 };
            fake_endpoint::embeddings(&vec![vec![1.0; dimension]; inputs.len()], false)
        });
        let mut drift_settings = settings(&drifting.url, "fake-embed");
        drift_settings.batch_size = 1;
        let error = LocalEmbedder::for_documents(&drift_settings)
            .expect("embedder")
            .embed(&["first", "second"])
            .expect_err("mixed dimensions");
        assert!(error.to_string().contains("5-dimensional"), "{error}");

        for (body, needle) in [
            (
                json!({"data": [{"index": 0, "embedding": [1.0]}]}),
                "1 vectors for 2 inputs",
            ),
            (
                json!({"data": [{"index": 0, "embedding": "AAAA"}, {"index": 1, "embedding": "AAAA"}]}),
                "OpenAI-compatible",
            ),
            (
                json!({"data": [{"index": 0, "embedding": [1.0]}, {"index": 0, "embedding": [1.0]}]}),
                "duplicate index",
            ),
            (
                json!({"data": [{"index": 0, "embedding": [1.0]}, {"embedding": [1.0]}]}),
                "mixes indexed",
            ),
            (
                json!({"data": [{"embedding": [1.0e300]}, {"embedding": [1.0]}]}),
                "non-finite",
            ),
            (
                json!({"data": [{"embedding": []}, {"embedding": [1.0]}]}),
                "0-dimensional",
            ),
        ] {
            let error = parse_embeddings(body.to_string().as_bytes(), 2, None, "fake-embed")
                .expect_err(needle);
            assert!(error.to_string().contains(needle), "{needle}: {error}");
        }
    }

    #[test]
    fn oversized_responses_are_refused_with_and_without_content_length() {
        let declared = fake_endpoint::serve(|_| {
            Reply::Json(
                json!({
                    "data": [{"index": 0, "embedding": [1.0, 0.0, 0.0, 0.0]}],
                    "padding": "x".repeat(200_000),
                })
                .to_string(),
            )
        });
        let mut bounded = settings(&declared.url, "fake-embed");
        bounded.dimension = Some(4);
        let error = LocalEmbedder::for_documents(&bounded)
            .expect("embedder")
            .embed(&["abort"])
            .expect_err("declared oversize");
        assert!(error.to_string().contains("exceeds"), "{error}");

        let streamed = fake_endpoint::serve(|_| Reply::Chunked(vec![vec![b' '; 16_384]; 16]));
        bounded.endpoint.clone_from(&streamed.url);
        let error = LocalEmbedder::for_documents(&bounded)
            .expect("embedder")
            .embed(&["abort"])
            .expect_err("chunked oversize");
        assert!(error.to_string().contains("exceeds"), "{error}");
        assert_eq!(response_limit(1, Some(4)), 65_536 + 4 * 32);
        assert_eq!(
            response_limit(2, None),
            65_536 + 2 * MAX_MODEL_DIMENSION * 32,
            "an unknown dimension is budgeted at the largest supported one"
        );
    }

    #[test]
    fn requests_time_out_within_their_bound() {
        let endpoint = fake_endpoint::serve(|_| Reply::Hang(Duration::from_secs(3)));
        let mut slow = settings(&endpoint.url, "fake-embed");
        slow.timeout = Duration::from_millis(200);
        let started = Instant::now();
        let error = LocalEmbedder::for_documents(&slow)
            .expect("embedder")
            .embed(&["abort"])
            .expect_err("timeout");
        assert!(
            error.to_string().contains("timed out after 200 ms"),
            "{error}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the timeout must bound the request"
        );
    }

    #[test]
    fn non_loopback_credentialed_and_redirecting_endpoints_are_refused() {
        let mut config = SemanticConfig::default();
        for endpoint in [
            "http://10.0.0.5:11434/v1/embeddings",
            "http://example.com/v1/embeddings",
            "https://api.openai.com/v1/embeddings",
            "http://user:secret@127.0.0.1:11434/v1/embeddings",
            "http://127.0.0.1:11434/v1/embeddings?key=secret",
            "http://127.0.0.1:11434/v1/embeddings#fragment",
            "ftp://127.0.0.1/v1/embeddings",
            "http://[::ffff:127.0.0.1]:11434/v1/embeddings",
            "http://localhost.:11434/v1/embeddings",
            "not a url",
        ] {
            config.endpoint = endpoint.to_owned();
            for local_only in [true, false] {
                assert!(
                    validate_config(&config, local_only).is_err(),
                    "{endpoint} (local_only = {local_only})"
                );
            }
            assert!(
                LocalEmbedder::for_queries(&settings(endpoint, "m")).is_err(),
                "{endpoint}"
            );
        }
        for endpoint in [
            "http://localhost:11434/v1/embeddings",
            "http://127.0.0.1:1/v1/embeddings",
            "http://127.3.2.1:8080/v1/embeddings",
            "http://[::1]:8080/v1/embeddings",
        ] {
            config.endpoint = endpoint.to_owned();
            assert!(validate_config(&config, true).is_ok(), "{endpoint}");
        }

        let redirecting = fake_endpoint::serve(|_| {
            Reply::Redirect("http://example.com/v1/embeddings".to_owned())
        });
        let error = LocalEmbedder::for_documents(&settings(&redirecting.url, "fake-embed"))
            .expect("embedder")
            .embed(&["abort"])
            .expect_err("redirect");
        assert!(error.to_string().contains("HTTP 307"), "{error}");
        assert_eq!(
            redirecting.requests().len(),
            1,
            "a redirect is never followed"
        );

        let missing = fake_endpoint::serve(|_| {
            Reply::Status(
                404,
                r#"{"error":"model \"fake\" not found, try pulling it first"}"#.to_owned(),
            )
        });
        let error = LocalEmbedder::for_documents(&settings(&missing.url, "fake"))
            .expect("embedder")
            .embed(&["abort"])
            .expect_err("missing model");
        let message = error.to_string();
        assert!(
            message.contains("HTTP 404") && message.contains("not found"),
            "{message}"
        );
    }

    #[test]
    fn provider_configuration_is_validated() {
        let mut config = SemanticConfig::default();
        assert!(validate_config(&config, true).is_ok(), "defaults are valid");
        config.provider = EmbeddingProviderKind::Local;
        assert!(
            validate_config(&config, true).is_err(),
            "local needs a model"
        );
        config.model = "nomic-embed-text".to_owned();
        assert!(validate_config(&config, true).is_ok());
        for broken in [
            SemanticConfig {
                model: " padded".to_owned(),
                ..config.clone()
            },
            SemanticConfig {
                dimension: 9_000,
                ..config.clone()
            },
            SemanticConfig {
                batch_size: 0,
                ..config.clone()
            },
            SemanticConfig {
                batch_size: 65,
                ..config.clone()
            },
            SemanticConfig {
                timeout_ms: 50,
                ..config.clone()
            },
            SemanticConfig {
                query_timeout_ms: 60_000,
                ..config.clone()
            },
            SemanticConfig {
                min_similarity: 1.0,
                ..config.clone()
            },
            SemanticConfig {
                min_similarity: f64::NAN,
                ..config.clone()
            },
            SemanticConfig {
                query_prefix: "a\nb".to_owned(),
                ..config.clone()
            },
            SemanticConfig {
                document_prefix: "x".repeat(300),
                ..config.clone()
            },
        ] {
            assert!(validate_config(&broken, true).is_err(), "{broken:?}");
        }
        let resolved = LocalEmbedderSettings::from_config(&config).expect("local settings");
        assert_eq!(resolved.identity(), "local:nomic-embed-text");
        assert_eq!(resolved.dimension, None);
        assert!(
            LocalEmbedderSettings::from_config(&SemanticConfig::default()).is_none(),
            "the hashed default needs no endpoint"
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn model_change_forces_a_rebuild_instead_of_mixing_vector_spaces() {
        let (_temp, database, project) = fixture();
        let abort_id = validated_memory(&database, &project, ABORT_CLAIM);
        validated_memory(&database, &project, LOG_CLAIM);
        validated_memory(&database, &project, RETRY_CLAIM);
        let scheduler = MutationScheduler::start(16);
        let model_a = fake_endpoint::concept_model(4);
        let settings_a = settings(&model_a.url, "embed-a");

        let report = rebuild_project(&database, &scheduler, Some(&settings_a), &project, None)
            .expect("rebuild a");
        assert_eq!(report.vectors, 3, "hashed vectors are rebuilt too");
        let model = report.model.expect("model report");
        assert_eq!(
            (
                model.embedder.as_str(),
                model.dimension,
                model.embedded,
                model.unchanged
            ),
            ("local:embed-a", 4, 3, 0)
        );
        assert_eq!(
            (model.removed, model.requests, model.active_memories),
            (0, 3, 3)
        );
        assert!(model.complete);

        // A paraphrase without shared words: only the model space finds it.
        let paraphrase = "stop the job and free its holds";
        let outcome = SemanticSearch::new(database.clone(), local_options(&settings_a))
            .search(&project, paraphrase);
        assert_eq!(outcome.note, "semantic: local:embed-a");
        assert_eq!(outcome.hits[0].memory_id, abort_id);

        let again = rebuild_project(&database, &scheduler, Some(&settings_a), &project, None)
            .expect("incremental rebuild")
            .model
            .expect("model report");
        assert_eq!((again.embedded, again.unchanged, again.requests), (0, 3, 1));

        // Model B is configured but not yet rebuilt: its space is empty, so
        // model A's vectors are never compared against model B's query.
        let model_b = fake_endpoint::concept_model(4);
        let settings_b = settings(&model_b.url, "embed-b");
        let _ = take_rebuild_requests(&database);
        let skipped = SemanticSearch::new(database.clone(), local_options(&settings_b))
            .search(&project, paraphrase);
        assert!(skipped.hits.is_empty(), "no mixing across models");
        assert!(
            skipped
                .note
                .starts_with("semantic skipped: no current local:embed-b vectors"),
            "{}",
            skipped.note
        );
        assert_eq!(take_rebuild_requests(&database), vec![project.clone()]);
        assert_eq!(vector_rows(&database, "local:embed-a"), 3);

        let switched = rebuild_project(&database, &scheduler, Some(&settings_b), &project, None)
            .expect("rebuild b")
            .model
            .expect("model report");
        assert_eq!((switched.removed, switched.embedded), (3, 3));
        assert_eq!(vector_rows(&database, "local:embed-a"), 0);
        assert_eq!(vector_rows(&database, "local:embed-b"), 3);
        assert_eq!(
            vector_rows(&database, EMBEDDER_ID),
            3,
            "hashed fallback kept"
        );
        let served = SemanticSearch::new(database.clone(), local_options(&settings_b))
            .search(&project, paraphrase);
        assert_eq!(served.note, "semantic: local:embed-b");
        assert_eq!(served.hits[0].memory_id, abort_id);

        // The same model name answering with another dimension is a
        // different vector space too.
        let resized = fake_endpoint::concept_model(3);
        let settings_resized = settings(&resized.url, "embed-b");
        let stale = SemanticSearch::new(database.clone(), local_options(&settings_resized))
            .search(&project, paraphrase);
        assert!(stale.hits.is_empty(), "{}", stale.note);
        assert!(stale.note.starts_with("semantic skipped"), "{}", stale.note);
        let rebuilt = rebuild_project(
            &database,
            &scheduler,
            Some(&settings_resized),
            &project,
            None,
        )
        .expect("rebuild resized")
        .model
        .expect("model report");
        assert_eq!(
            (rebuilt.dimension, rebuilt.removed, rebuilt.embedded),
            (3, 3, 3)
        );

        // A configured dimension that the model contradicts fails before any
        // stored vector is touched.
        let mut strict = settings_resized;
        strict.dimension = Some(4);
        assert!(rebuild_project(&database, &scheduler, Some(&strict), &project, None).is_err());
        assert_eq!(vector_rows(&database, "local:embed-b"), 3);
    }

    #[test]
    fn rebuild_reembeds_changed_text_and_skips_memories_changed_mid_run() {
        let (_temp, database, project) = fixture();
        let abort_id = validated_memory(&database, &project, ABORT_CLAIM);
        validated_memory(&database, &project, LOG_CLAIM);
        let scheduler = MutationScheduler::start(16);
        let endpoint = fake_endpoint::concept_model(4);
        let local = settings(&endpoint.url, "embed-a");
        rebuild_project(&database, &scheduler, Some(&local), &project, None).expect("rebuild");

        database
            .write_immediate(|transaction| {
                transaction.execute(
                    "UPDATE memories SET claim = 'Halt jobs before releasing holds' WHERE id = ?1",
                    [abort_id.as_str()],
                )?;
                Ok(())
            })
            .expect("edit canonical text");
        let report = rebuild_project(&database, &scheduler, Some(&local), &project, None)
            .expect("rebuild after edit")
            .model
            .expect("model report");
        assert_eq!((report.embedded, report.unchanged), (1, 1));

        let stale_digest = crate::experience::sha256(b"text that is no longer canonical");
        let (written, changed) = database
            .write_immediate(|transaction| {
                write_model_rows(
                    transaction,
                    &project,
                    "local:embed-a",
                    "",
                    &[(abort_id.clone(), stale_digest, vec![1.0, 0.0, 0.0, 0.0])],
                )
            })
            .expect("guarded write");
        assert_eq!(
            (written, changed),
            (0, 1),
            "changed memories are never overwritten"
        );

        let expired = rebuild_project(
            &database,
            &scheduler,
            Some(&local),
            &project,
            Some(Instant::now()),
        )
        .expect("deadline")
        .model
        .expect("model report");
        assert!(
            !expired.complete,
            "a passed deadline stops before the first page"
        );
    }

    #[test]
    fn unreachable_endpoint_falls_back_to_hashed_vectors_then_cools_down() {
        let (_temp, database, project) = fixture();
        let abort_id = validated_memory(&database, &project, ABORT_CLAIM);
        validated_memory(&database, &project, LOG_CLAIM);
        let unreachable = settings(&closed_endpoint(), "embed-a");
        let search = SemanticSearch::new(database.clone(), local_options(&unreachable));

        let first = search.search(&project, "abort cleanup must release reservations");
        assert!(
            first.note.starts_with(&format!(
                "semantic: {EMBEDDER_ID} fallback (local:embed-a unavailable"
            )),
            "{}",
            first.note
        );
        assert!(first.note.contains("unreachable"), "{}", first.note);
        assert_eq!(
            first.hits[0].memory_id, abort_id,
            "hashed vectors still serve"
        );

        let started = Instant::now();
        let second = search.search(&project, "abort cleanup must release reservations");
        assert!(second.note.contains("cooldown"), "{}", second.note);
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(second.hits, first.hits);

        // With the fallback sidecar gone too, retrieval is lexical and
        // structural only, and says so.
        database
            .write_immediate(|transaction| {
                transaction.execute("DELETE FROM memory_vectors", [])?;
                Ok(())
            })
            .expect("lose the derived sidecar");
        let degraded = search.search(&project, "abort cleanup must release reservations");
        assert!(degraded.hits.is_empty());
        assert!(
            degraded.note.starts_with(&format!(
                "semantic skipped: no current {EMBEDDER_ID} vectors for 2 memories, used as \
                 fallback (local:embed-a unavailable"
            )),
            "{}",
            degraded.note
        );
        assert_eq!(take_rebuild_requests(&database), vec![project]);
    }

    #[test]
    fn synced_replicas_keep_hashed_vectors_and_drop_stale_model_vectors() {
        let (_temp, database, project) = fixture();
        let store = MemoryStore::new(database.clone());
        let mut replica = crate::sync::SyncMemory {
            id: MemoryId::new().to_string(),
            kind: "lesson".to_owned(),
            claim: ABORT_CLAIM.to_owned(),
            rationale: "Validated on the central instance".to_owned(),
            confidence: 0.9,
            importance: 0.5,
            tags: Vec::new(),
            valid_from_commit: None,
            valid_to_commit: None,
            updated_at_ms: 1,
        };
        store.upsert_replica(&project, &replica).expect("added");
        assert_eq!(
            vector_rows(&database, EMBEDDER_ID),
            1,
            "synced memories are searchable"
        );

        let endpoint = fake_endpoint::concept_model(4);
        let local = settings(&endpoint.url, "embed-a");
        let scheduler = MutationScheduler::start(16);
        rebuild_project(&database, &scheduler, Some(&local), &project, None).expect("rebuild");
        assert_eq!(vector_rows(&database, "local:embed-a"), 1);

        replica.claim = LOG_CLAIM.to_owned();
        replica.updated_at_ms = unix_time_ms() + 60_000;
        store.upsert_replica(&project, &replica).expect("updated");
        assert_eq!(
            vector_rows(&database, "local:embed-a"),
            0,
            "a model vector of replaced text is dropped"
        );
        let stored_text = memory_vector(
            LOG_CLAIM,
            "Validated on the central instance",
            &["synced".to_owned()],
        );
        let hits = SqliteVectorIndex::new(database, 50_000)
            .search(&project, &stored_text, 1)
            .expect("search");
        assert!(
            hits[0].score > 0.999,
            "the hashed vector follows the new text"
        );
    }

    #[test]
    fn hashed_coverage_gaps_request_a_rebuild_and_degrade_to_lexical() {
        let (_temp, database, project) = fixture();
        validated_memory(&database, &project, ABORT_CLAIM);
        validated_memory(&database, &project, LOG_CLAIM);
        let search = SemanticSearch::new(database.clone(), SemanticOptions::hashed(50, 50_000));
        let _ = take_rebuild_requests(&database);
        let healthy = search.search(&project, "abort cleanup");
        assert_eq!(healthy.note, format!("semantic: {EMBEDDER_ID}"));
        assert!(take_rebuild_requests(&database).is_empty());

        database
            .write_immediate(|transaction| {
                transaction.execute("DELETE FROM memory_vectors", [])?;
                Ok(())
            })
            .expect("lose the derived sidecar");
        let degraded = search.search(&project, "abort cleanup");
        assert!(degraded.hits.is_empty());
        assert!(
            degraded.note.starts_with("semantic skipped: no current"),
            "{}",
            degraded.note
        );
        assert_eq!(take_rebuild_requests(&database), vec![project]);
    }

    #[test]
    fn query_embeddings_are_memoized_and_floored() {
        let (_temp, database, project) = fixture();
        validated_memory(&database, &project, ABORT_CLAIM);
        let endpoint = fake_endpoint::concept_model(4);
        let local = settings(&endpoint.url, "embed-a");
        let scheduler = MutationScheduler::start(16);
        rebuild_project(&database, &scheduler, Some(&local), &project, None).expect("rebuild");
        let before = endpoint.requests().len();
        let search = SemanticSearch::new(database, local_options(&local));
        let _ = search.search(&project, "stop now");
        let _ = search.search(&project, "stop now");
        assert_eq!(
            endpoint.requests().len(),
            before + 1,
            "one query embedding per text"
        );
        let _ = search.search(&project, "halt now");
        assert_eq!(endpoint.requests().len(), before + 2);

        let floored = floor_scores(scored(&[0.75, 0.5, 0.4]), 0.5);
        assert_eq!(floored.len(), 1);
        assert!((floored[0].score - 0.5).abs() < 1e-12);
        let raw = floor_scores(scored(&[0.3, -0.2, 0.0]), 0.0);
        assert_eq!(raw.len(), 1, "no floor keeps positive cosine unchanged");
        assert!((raw[0].score - 0.3).abs() < 1e-12);
    }
}
