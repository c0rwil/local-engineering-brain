# Local Engineering Brain (LEB)
## Product Requirements, Architecture, Verification, Performance, and Evaluation Specification

**Status:** Implementation blueprint / v0.1 design baseline  
**Audience:** Core maintainers, parallel implementation agents, benchmark/evaluation agents, integration agents, reviewers  
**Target:** Local-first software-engineering agents using MTPLX, Ollama, llama.cpp, vLLM, MLX, or any OpenAI/Anthropic-compatible local runtime  
**Date baseline:** 2026-08-28  
**Working name:** Local Engineering Brain (LEB); rename freely before public launch

---

## 0. Executive mandate

LEB is a local-first engineering intelligence layer that gives software-development agents a persistent, evidence-grounded, version-aware model of the software systems they work on. It is not primarily a vector database, chat-history store, MCP memory server, code knowledge graph, or prompt wrapper. Those are supporting mechanisms.

The product thesis is:

> **A local software agent should improve through verified experience without continuously retraining its base model, and the system should be able to prove when that accumulated experience improves engineering outcomes.**

LEB continuously combines six classes of information:

1. **Software structure:** symbols, files, imports, calls, dependencies, data stores, services, interfaces, tests, documentation.
2. **Software history:** commits, branches, diffs, ownership, churn, architectural changes, dependency/version transitions.
3. **Runtime evidence:** tests, compiler/type-checker results, traces, coverage, logs, process trees, metrics, benchmark outcomes.
4. **Engineering experience:** tasks, attempts, failed approaches, hypotheses, fixes, decisions, lessons, developer preferences, accepted/rejected changes.
5. **Learned competence:** procedures, skills, invariants, policies, heuristics, failure predictors, and task-specific strategies synthesized from repeated validated experience.
6. **Current intent:** the active repository, branch, files, task, constraints, acceptance criteria, current diff, and current agent trajectory.

These are represented as a persistent **engineering world model** with explicit provenance, confidence, scope, temporal validity, and empirical utility. A context compiler then selects a small, high-value projection of this world model for the active model invocation.

The system MUST be useful when completely offline after dependencies/models are installed. It MUST NOT require ProjectMem, Graphify, MCP, a vector database, or MTPLX internally; each is an adapter or optional accelerator. The canonical state and interfaces belong to LEB.

---

## 1. Product vision

### 1.1 User promise

A developer should be able to enter a repository and run a local coding agent. The agent should:

- already know stable project architecture and conventions;
- remember previously validated decisions and failed approaches;
- understand which old knowledge is stale on the current branch;
- retrieve only knowledge relevant to the current task;
- explain why any memory or graph fact was injected;
- learn from tests, diffs, commits, and explicit developer feedback;
- convert repeated successful experiences into reusable procedures;
- warn before violating known invariants or repeating demonstrated failures;
- avoid poisoning itself with unsupported model speculation;
- be able to disable, inspect, export, rollback, or purge any learned state;
- remain performant on a developer workstation;
- quantify whether the brain helped or hurt compared with the same model without it.

### 1.2 Long-term product identity

LEB should become the **local engineering world model and experience compiler** sitting between developer tooling and arbitrary local models.

```text
IDE / TUI / CLI / coding agent / CI
                  |
      OpenAI/Anthropic proxy + MCP edge
                  |
          LOCAL ENGINEERING BRAIN
  ------------------------------------------------
  World Model | Evidence | Experience | Validity
  Skills      | Policy   | Context    | Evaluation
  ------------------------------------------------
       |       |       |       |       |
      Git    Code    Tests   Runtime  Human input
       |
  Local inference runtime
  MTPLX / Ollama / llama.cpp / vLLM / MLX
```

### 1.3 What makes LEB distinguishable

LEB MUST differentiate on the following five capabilities, not merely on storage/retrieval performance:

1. **Evidence:** every high-confidence learned claim is traceable to software evidence or explicit human instruction.
2. **Validity:** knowledge carries the repository versions, scopes, branches, symbols, dependencies, and environmental conditions under which it is believed true.
3. **Compilation:** repeated episodes can graduate into lessons, procedures, skills, invariants, and enforceable policies.
4. **Utility:** the system records whether retrieved knowledge was used and whether its use correlated with better or worse task outcomes.
5. **Evaluation:** the same system ships an A/B harness that compares identical models with LEB disabled versus enabled.

---

## 2. Current ecosystem boundary and prior art

LEB should deliberately integrate with, learn from, or compete against existing systems without reproducing them unnecessarily.

### 2.1 ProjectMem

As of the design baseline, ProjectMem provides local-first append-only typed engineering memory, deterministic projections, MCP access, git-driven capture, token-budgeted context, and pre-action failure warnings. It is strong prior art for event-sourced coding memory.

LEB MAY initially use a ProjectMem adapter for importing/exporting events or bootstrap compatibility. LEB MUST NOT make ProjectMem's event schema its canonical internal schema because LEB requires richer evidence, validity, bitemporal state, skill compilation, runtime linkage, and memory-utility tracking.

### 2.2 Graphify

Graphify provides a local typed code knowledge graph derived from syntax/static structure, including nodes/edges and multi-hop traversal exposed via CLI/MCP. LEB SHOULD support Graphify as a structural provider.

LEB MUST NOT assume Graphify is installed. StructuralProvider MUST allow Graphify, tree-sitter-native, SCIP, LSP, language-specific analyzers, or future providers.

### 2.3 MCP

The 2026-07-28 MCP specification uses a stateless protocol core. LEB MUST therefore treat MCP as an interoperability boundary, not as its internal state-management substrate. Persistent application state belongs in the LEB daemon and is addressed by explicit identifiers.

### 2.4 MTPLX

MTPLX provides local OpenAI/Anthropic-compatible serving, agent tool-call support, and RAM/SSD session/prefix caching on Apple Silicon. LEB SHOULD exploit stable prompt-prefix construction and cache-aware context compilation when connected to MTPLX, but MUST remain runtime-agnostic.

### 2.5 Letta and related memory runtimes

Context repositories, progressive disclosure, memory agents, skills, reflection, and memory evaluation are established directions. LEB's differentiation is the explicit connection of learned engineering knowledge to code/runtime evidence, version validity, executable policies, and controlled with/without-brain evaluation.

### 2.6 Reference URLs

- ProjectMem: https://projectmem.dev/ ; paper: https://arxiv.org/abs/2606.12329
- Graphify: https://graphify.com/ ; concepts: https://graphify.com/concepts
- MCP 2026-07-28 release: https://blog.modelcontextprotocol.io/posts/2026-07-28/
- MTPLX: https://github.com/youssofal/MTPLX
- Letta context repositories: https://www.letta.com/blog/context-repositories/
- Letta memory evaluation: https://www.letta.com/blog/evaluating-memory-in-production-agents/

---

## 3. Design principles

### P1. Canonical truth is external to the model
Model weights and model context are not durable storage. Durable state lives in LEB.

### P2. Evidence outranks narration
Observed code, tests, traces, compiler results, commits, and explicit human decisions outrank agent-generated prose.

### P3. Memory is typed
A hypothesis is not a fact. A human policy is not an inferred heuristic. A failed attempt is not a validated lesson.

### P4. Knowledge is scoped
Every learned object carries scope: global developer, organization, project, branch family, subsystem, problem family, task, or session.

### P5. Knowledge is temporal
Software evolves. A memory must be capable of being correct in one commit range and wrong in another.

### P6. Retrieval is not learning
Finding old text is only one mechanism. Learning requires extracting structure, verifying, consolidating, measuring utility, and changing future behavior.

### P7. Context is compiled
The LLM should receive the best task-specific context under explicit token/latency/cache budgets, not a raw top-k RAG dump.

### P8. Local-first means inspectable and controllable
The developer can inspect state, see provenance, disable learning, purge scopes, export data, and reproduce decisions.

### P9. No invisible magic
Every injected item and every blocking policy is explainable through `why`/provenance surfaces.

### P10. The same model is the control group
Claims of improvement are only meaningful when tested against the same base model/runtime/configuration without LEB augmentation.

---

## 4. Goals and non-goals

### 4.1 V1 goals

- Single-user local daemon.
- Multi-project persistent state.
- Automatic project identity from repository metadata.
- Structural graph ingestion through at least one native provider and one external adapter.
- Git history/diff ingestion.
- Test command/result ingestion.
- Typed episode and memory model.
- Evidence-based memory promotion.
- Version/symbol validity tracking.
- Hybrid retrieval: exact, lexical, structural, semantic, and historical utility.
- Context compiler with token budget and stable-prefix support.
- OpenAI-compatible proxy integration.
- MCP compatibility server.
- CLI for inspect/recall/learn/purge/export.
- A/B benchmark harness.
- Thorough automated test suite.

### 4.2 V1.5 goals

- Runtime trace/coverage ingestion.
- Skill synthesis and skill graph.
- Invariant extraction and enforcement.
- Cross-project transfer as hypotheses.
- Automatic contradiction detection.
- Background consolidation and garbage collection.
- IDE/TUI dashboard.

### 4.3 Longer-term goals

- Multi-developer/team world model.
- Signed knowledge provenance.
- Distributed/remote encrypted sync.
- Fine-tuning/LoRA dataset generation from validated trajectories.
- Learned retrieval policy.
- Agent-policy compiler.
- Automated architecture drift detection.
- Benchmark corpus of repeated/evolving repository tasks.

### 4.4 Non-goals for V1

- Training foundation models.
- Replacing Git.
- Replacing static analyzers or language servers.
- Becoming a general-purpose personal-memory product.
- Storing every model token forever by default.
- Maintaining correctness solely through LLM judgment.
- Requiring a graph database server.
- Requiring cloud embeddings or hosted inference.
- Automatically executing destructive policy actions without explicit configuration.

---

## 5. Personas and primary workflows

### 5.1 Local-first individual developer
Runs a 9B-70B local model, wants privacy and accumulated project competence.

### 5.2 Power user with multiple agents
Uses Pi/OpenCode/Cline/custom harnesses and wants one shared brain.

### 5.3 Maintainer of a long-lived codebase
Wants architectural history, invariant protection, and less repeated debugging.

### 5.4 Researcher
Wants controlled measurements of memory/experience augmentation.

### 5.5 Team/enterprise future persona
Wants approved shared knowledge while retaining provenance and access controls.

### 5.6 Canonical workflows

**A. Start work**
1. Client opens repository.
2. LEB resolves project identity and branch state.
3. LEB refreshes changed structural/git facts incrementally.
4. User submits task.
5. Context compiler identifies entities and retrieves relevant validated knowledge.
6. Stable + dynamic context is assembled.
7. Request is forwarded to local model.

**B. Agent works**
1. Tool calls and file operations are observed.
2. Attempts are recorded as episode events.
3. Compiler/test/runtime outcomes become evidence.
4. Candidate hypotheses remain low-confidence.
5. Validated fixes can promote lessons.

**C. Task completes**
1. Completion criterion fires: explicit user accept, passing target tests, commit, or configured signal.
2. Distillation creates candidate lessons/procedures.
3. Evidence engine verifies deterministic claims where possible.
4. Existing knowledge is deduplicated/superseded/contradicted.
5. Utility counters are updated.

**D. Future related task**
1. Structural/task similarity finds relevant past episode.
2. Compiler validates its applicability to current commit/symbols/dependencies.
3. A compact lesson or skill is injected.
4. Agent avoids past dead end or follows proven procedure.
5. Outcome feeds back into utility.

---

## 6. System context and process topology

### 6.1 Required processes

**`lebd` - local brain daemon**
- Owns canonical state.
- Owns project registry, world model, memory/experience, evidence, validity, context compiler, job queue, evaluation logs.
- Long-running per-user process.
- Default bind: Unix domain socket; optional loopback TCP.

**`leb-proxy` - OpenAI/Anthropic-compatible proxy**
- May run in-process with `lebd` for V1.
- Accepts client requests.
- Associates request with project/session/task.
- Calls context compiler.
- Forwards augmented request to runtime.
- Observes responses/tool traces.

**`leb-mcp` - MCP compatibility adapter**
- Stateless at protocol layer.
- Each call includes explicit project/session/task handles.
- Exposes query/recall/explain/store/inspect tools.

**`leb` - CLI**
- Project setup, status, query, inspect, memory operations, benchmark controls.

**workers**
- Low-priority background tasks: index refresh, embeddings, consolidation, contradiction checks, utility analysis, compaction.

### 6.2 Internal communications

Preferred V1:
- Internal typed Python APIs when in one process.
- Unix socket + HTTP/JSON or MsgPack if split.
- No internal MCP.
- No localhost TCP by default unless client compatibility requires it.

### 6.3 Portability

Core code MUST run on macOS and Linux. MTPLX-specific optimizations SHOULD be isolated behind RuntimeCapabilities.

---

## 7. Repository/project identity

### 7.1 Project identity requirements

The same project should remain the same brain if moved on disk. A fork should be distinguishable but optionally linked.

Project identity inputs, in priority order:
1. explicit `.leb/project.toml` UUID;
2. configured canonical Git remote identity;
3. repository root metadata;
4. generated UUID persisted under `.leb` or user registry.

### 7.2 Identity model

```yaml
project_id: prj_01J...
name: scheduler-service
repo_fingerprint:
  canonical_remote: git@github.com:org/scheduler-service.git
  initial_root_commit: abc123
aliases:
  - ~/src/scheduler
  - /Volumes/work/scheduler
parent_project_id: null
```

### 7.3 Branch identity

Branches are not permanent knowledge scopes by name alone. Use:
- current HEAD;
- merge-base against configured trunk;
- branch family identifier;
- reachable commit graph.

Knowledge validity can be tested against commit ancestry rather than string branch names.

### 7.4 Worktree and monorepo support

- Multiple Git worktrees MUST map to one project with different active branch contexts.
- Monorepo SHOULD support nested logical components with namespace scopes.
- Nested repositories MUST be separately registered unless explicitly federated.

---

## 8. Engineering world model

### 8.1 Logical graph

LEB uses a logical property graph, regardless of physical persistence implementation.

**Node families:**
- Project, Repository, BranchFamily, Commit, Diff, File, Symbol, Type, Module, Package.
- Service, APIEndpoint, DBTable, QueueTopic, ConfigKey, Dependency.
- Test, BuildStep, Trace, LogEvent, Metric, Process, RuntimeArtifact.
- Task, Problem, Attempt, Hypothesis, Experiment, Outcome, Fix.
- Memory, Lesson, Procedure, Skill, Invariant, Policy, Decision, Preference.
- HumanInstruction, Agent, Model, Runtime, Session.

**Edge families:**
- structural: DEFINES, CALLS, IMPORTS, REFERENCES, IMPLEMENTS, OVERRIDES, READS, WRITES, PUBLISHES, SUBSCRIBES.
- testing/runtime: TESTS, COVERS, OBSERVED_CALL, FAILS_IN, PASSES_IN, PRODUCES, CONSUMES.
- history: INTRODUCED_BY, MODIFIED_BY, REMOVED_BY, RENAMED_TO, SUPERSEDES.
- experience: ATTEMPTED_FOR, BASED_ON, RESULTED_IN, FAILED_BECAUSE, SOLVED_BY.
- epistemic: SUPPORTS, CONTRADICTS, INVALIDATES, VERIFIED_BY, INFERRED_FROM.
- learning: DERIVED_FROM, GENERALIZES, SPECIALIZES, APPLIES_TO, TRIGGERS.
- governance: PROTECTS, REQUIRES_VALIDATION, BLOCKS, WARNS_ON.

### 8.2 Provenance requirements

Every non-trivial node/edge MUST include provenance:

```yaml
provenance:
  source_kind: static_analysis | git | test | runtime | human | agent_inference | import
  source_ref: ...
  extractor_version: ...
  observed_at: ...
  content_hash: ...
```

### 8.3 Confidence classes

Use both categorical provenance and numeric confidence.

Default confidence floors:
- explicit current file/static parse fact: 1.00
- compiler/test deterministic result: 1.00
- explicit human instruction/decision: 1.00 for instruction existence, not necessarily technical truth
- runtime observed relation: 0.98+
- deterministic Git derivation: 1.00
- external imported fact: configured trust
- LLM inference: <=0.70 until verified
- LLM hypothesis: <=0.50 default

Confidence MUST NOT automatically increase merely because the same model repeats a claim.

---

## 9. Persistence architecture

### 9.1 Canonical source of truth

V1 canonical storage: **SQLite** with WAL mode.

Reasons:
- zero external daemon;
- transactional;
- fast local reads;
- FTS5 available;
- good backup/export story;
- sufficient for millions of graph rows on a workstation;
- easier correctness testing than a distributed store.

### 9.2 Recommended physical stores

**SQLite (`brain.sqlite`)**
- canonical events and entities;
- graph node/edge tables;
- provenance;
- memory state;
- task/session metadata;
- FTS5 lexical index;
- utility/evaluation metrics;
- background jobs.

**Vector index**
- optional pluggable backend: sqlite-vector, usearch, hnswlib, Qdrant local, LanceDB.
- rebuildable accelerator only.
- canonical text remains SQLite.

**Blob store**
- content-addressed files under `~/.leb/blobs/sha256/...` for large traces/logs/diffs when needed.
- DB stores digest + metadata.
- retention policies MUST be configurable.

**Structural graph cache**
- provider-native graph files MAY remain external.
- normalized graph relationships needed by LEB are persisted in canonical form.

### 9.3 Event sourcing strategy

All learning-relevant mutations SHOULD append immutable events and update materialized projections transactionally.

Example events:
- PROJECT_REGISTERED
- TASK_STARTED
- ATTEMPT_RECORDED
- TEST_OBSERVED
- HYPOTHESIS_CREATED
- MEMORY_PROMOTED
- MEMORY_MARKED_SUSPECT
- MEMORY_SUPERSEDED
- SKILL_SYNTHESIZED
- POLICY_APPLIED
- MEMORY_PURGED

Event payloads MUST be versioned.

### 9.4 Data durability

- SQLite WAL checkpoint on safe intervals.
- atomic backups using SQLite backup API.
- optional encrypted backup.
- integrity check command.
- vector index rebuild command.
- corrupted derived indexes MUST NOT corrupt canonical data.

### 9.5 Encryption

V1 SHOULD offer optional SQLCipher or filesystem encryption integration. Sensitive project data MUST never be sent to remote embeddings unless user explicitly configures a remote provider.

---

## 10. Core data model

### 10.1 Memory record

```json
{
  "id": "mem_01J...",
  "project_id": "prj_01J...",
  "scope_type": "project",
  "scope_id": "prj_01J...",
  "kind": "lesson",
  "epistemic_state": "validated",
  "claim": "A reservation acquired by Reactor must be released in post_process even after abort.",
  "rationale": "Regression observed when cleanup was skipped.",
  "confidence": 0.98,
  "importance": 0.91,
  "status": "active",
  "validity": {
    "from_commit": "abc...",
    "to_commit": null,
    "symbol_refs": ["sym_reactor_post_process"],
    "dependency_constraints": [],
    "environment_constraints": []
  },
  "evidence_ids": ["evd_1", "evd_2"],
  "source_episode_ids": ["ep_1"],
  "tags": ["abort", "cleanup", "reservation"],
  "token_estimate": 44,
  "created_at": "...",
  "updated_at": "...",
  "usage": {
    "retrieved": 0,
    "injected": 0,
    "acknowledged": 0,
    "successful_associations": 0,
    "harmful_associations": 0
  }
}
```

### 10.2 Evidence record

```json
{
  "id": "evd_01J...",
  "kind": "test_result",
  "strength": "deterministic",
  "project_id": "...",
  "commit": "...",
  "command": "pytest tests/test_abort_cleanup.py -q",
  "exit_code": 0,
  "artifact_digest": "sha256:...",
  "observed_at": "..."
}
```

### 10.3 Episode

An episode is a bounded engineering experience, usually task-scoped.

```yaml
episode:
  task_id: task_...
  initial_problem: ...
  attempts: [...]
  evidence: [...]
  final_outcome: success | partial | failure | abandoned
  accepted_by_user: true/false/unknown
  final_commit: ...
```

### 10.4 Skill

```yaml
skill:
  id: skill_debug_worker_lifecycle
  title: Diagnose worker-lifecycle timeout
  trigger_features:
    terms: [timeout, worker, abort]
    graph_patterns: [...]
    stack_constraints: [...]
  procedure:
    - inspect process ownership
    - inspect signal recipient
    - inspect cleanup path
    - run lifecycle regression test
  evidence_ids: [...]
  derived_from_episodes: [...]
  confidence: 0.95
  scope: project
  applicability_model: ...
  utility: ...
```

### 10.5 Invariant

```yaml
invariant:
  statement: Every acquired reservation is released exactly once.
  applies_to_nodes: [Reactor, ReservationManager]
  validating_tests: [test_abort_cleanup, test_timeout_cleanup]
  enforcement: warn | require_test | block
  evidence_ids: [...]
```

---

## 11. Bitemporal validity model

LEB MUST distinguish:

1. **Software-valid time:** commit ranges in which a claim is believed true.
2. **Knowledge-valid time:** when LEB believed the claim and its state changes.

Minimum fields:
- `valid_from_commit`
- `valid_to_commit`
- `known_from_timestamp`
- `known_to_timestamp`
- `superseded_by`
- `contradicted_by`

### 11.1 Validity evaluation

For a memory to be auto-injected, the compiler evaluates:
- Is current HEAD descendant of `valid_from_commit`?
- Is current HEAD before/excluded by `valid_to_commit`?
- Have referenced symbols changed beyond configured threshold?
- Do dependency constraints match?
- Are relevant tests still present/passing when last observed?
- Has a contradictory higher-confidence fact appeared?

### 11.2 Staleness states

- ACTIVE
- SUSPECT
- STALE
- SUPERSEDED
- CONTRADICTED
- ARCHIVED
- PURGED

Only ACTIVE is injected by default. SUSPECT may be surfaced with a warning. Stale/superseded knowledge remains queryable for historical branches unless purged.

---

## 12. Structural intelligence providers

### 12.1 Provider interface

```python
class StructuralProvider(Protocol):
    def scan(self, project: ProjectRef, changed_paths: list[str] | None) -> ScanResult: ...
    def resolve_symbol(self, query: str) -> list[SymbolRef]: ...
    def neighbors(self, node_id: str, edge_types: set[str] | None = None) -> list[Edge]: ...
    def shortest_path(self, source: str, target: str) -> list[Edge]: ...
    def impact(self, nodes: list[str], max_depth: int = 3) -> ImpactResult: ...
```

### 12.2 Providers

V1:
- `TreeSitterProvider`: native minimal provider.
- `GraphifyProvider`: adapter to normalized graph.

Future:
- SCIP
- Pyright/Pylance
- rust-analyzer
- clangd
- tsserver
- CodeQL

### 12.3 Incremental indexing

Full repository rescans MUST NOT run per prompt.

On each request:
- compute Git diff/hash of changed files;
- update only changed files and dependent structural slices where provider supports it;
- schedule deeper refresh asynchronously.

### 12.4 Source-of-truth rule

The live filesystem and current Git state outrank stored structural facts. If a retrieved fact references a changed file, compiler SHOULD refresh/verify that slice before injection when latency budget permits.

---

## 13. Experience engine

### 13.1 Capture

Capture, subject to privacy configuration:
- task prompt;
- model/runtime identity;
- context items injected;
- tool calls;
- files read/changed;
- shell/test/compiler commands and exit codes;
- diffs;
- explicit user corrections;
- result/acceptance signal.

Raw full transcripts SHOULD use configurable retention. Durable learning should use compact structured episodes, not unlimited transcript retention.

### 13.2 Attempt detection

An attempt is a coherent strategy, not every tool call. Heuristics:
- model declares approach;
- code-edit group followed by validation;
- rollback/revert;
- significant change in targeted symbols;
- test outcome boundary.

V1 may use conservative segmentation and allow LLM-assisted labeling after task completion.

### 13.3 Outcome classification

Strong success:
- configured tests pass AND user accepts/commits;
- CI pass linked to change.

Provisional success:
- tests pass but no user acceptance.

Failure:
- tests fail after attempt;
- revert;
- explicit user rejection;
- agent abandons due to demonstrated issue.

Unknown:
- insufficient signal.

Unknown MUST NOT be treated as success.

---

## 14. Evidence engine

### 14.1 Purpose

Prevent self-reinforcing hallucinated memory and raise confidence only through independent evidence.

### 14.2 Evidence hierarchy

**Tier A deterministic**
- file content at digest;
- AST/static analyzer output;
- compiler/type checker result;
- test exit/result;
- Git ancestry/diff;
- runtime trace event;
- explicit developer instruction/decision.

**Tier B strong observational**
- repeated logs/metrics;
- coverage correlation;
- reproducible shell experiment.

**Tier C inferred**
- model explanation;
- architecture hypothesis;
- causal guess.

Promotion rules MUST require Tier A/B evidence for technical claims classified as validated unless manually approved.

### 14.3 Verification jobs

Examples:
- verify symbol existence/hash;
- verify call edge via parser;
- re-run narrow regression test;
- verify dependency version;
- compare old/new behavior snapshot;
- link fix diff to changed symbols;
- detect contradictory current implementation.

### 14.4 Promotion state machine

```text
candidate -> observed -> supported -> validated
    |           |           |
    +-------> rejected <-----+
                   |
                archived
```

Manual pinning MAY bypass promotion but must record `trust_source=human_override`.

---

## 15. Contradiction and invalidation engine

### 15.1 Triggers

- referenced symbol changes or disappears;
- static edge changes;
- dependency version transition;
- relevant config value changes;
- validating test removed/fails;
- new higher-confidence fact conflicts;
- user states previous decision is obsolete.

### 15.2 Behavior

1. mark affected memory SUSPECT;
2. queue cheap verification;
3. if disproven, mark SUPERSEDED or CONTRADICTED;
4. create relationship to replacement fact;
5. prevent automatic injection until resolved.

### 15.3 Historical reuse

When user checks out an older commit that lies in the old validity interval, a superseded memory may become applicable again for that historical context.

---

## 16. Learning/consolidation engine

### 16.1 Distillation

At task completion, create candidate:
- facts;
- decisions;
- failed approaches;
- gotchas;
- procedures;
- invariant candidates;
- supersession candidates.

Distillation prompt MUST include evidence references and must forbid promotion based solely on narrative plausibility.

### 16.2 Deduplication

Candidate matching uses:
- exact normalized claim;
- entity overlap;
- semantic similarity;
- graph relation similarity;
- existing supersession chain.

Do not blindly merge contradictory claims.

### 16.3 Episode-to-procedure synthesis

After N similar validated episodes (configurable, default 3), background consolidation MAY propose a procedure.

Requirements:
- source episodes listed;
- common trigger features extracted;
- steps supported by at least two successful episodes or one manually approved episode;
- failed variants retained as negative examples;
- scope initially narrow.

### 16.4 Procedure-to-skill promotion

Skill promotion requires:
- sufficient successful reuse count;
- low harmful-use rate;
- stable applicability conditions;
- no unresolved contradictions;
- optional manual approval in conservative mode.

### 16.5 Cross-project transfer

Cross-project lessons MUST enter destination project as hypotheses, not validated facts.

Applicability scoring uses:
- language/framework versions;
- structural graph motif;
- dependency match;
- problem class;
- runtime environment;
- prior transfer success.

---

## 17. Invariants and policy engine

### 17.1 Invariant sources

- explicit developer declaration;
- tests that encode architecture/safety guarantees;
- repeated regression patterns;
- static constraints;
- ADR/decision records.

### 17.2 Policy actions

Configurable levels:
- `inform`: add context only;
- `warn`: explicit warning in agent context/tool response;
- `require_validation`: require named checks before task can be marked complete;
- `block`: reject a proxy/tool action unless human override token is provided.

V1 SHOULD default to inform/warn; blocking is opt-in.

### 17.3 Policy example

If current diff touches symbols participating in invariant `reservation_release_exactly_once`, compiler injects the invariant and task completion requires configured tests.

---

## 18. Retrieval architecture

### 18.1 Multi-stage retrieval

Retrieval MUST be hybrid.

Stage 0: hard scope filters
- current project;
- current commit validity;
- allowed memory scopes;
- status;
- privacy/access policy.

Stage 1: candidate generation
- exact keyword/tag;
- FTS/BM25;
- entity/symbol match;
- graph neighborhood;
- semantic embedding;
- explicit trigger rule;
- similar historical problem/episode;
- active invariant/policy match.

Stage 2: feature calculation
- semantic similarity;
- lexical score;
- structural distance;
- same-file/symbol overlap;
- problem-family match;
- confidence;
- validity score;
- empirical utility;
- recency/churn;
- negative-transfer risk;
- token cost.

Stage 3: reranking
Start deterministic/linear. Later learn ranking weights from benchmark outcomes.

Stage 4: diversity/coverage
Avoid ten near-duplicate memories. Select across:
- architecture;
- prior failure;
- procedure;
- invariant;
- acceptance criteria.

### 18.2 Baseline score

Example initial ranker:

```text
score =
  0.24 semantic
+ 0.18 lexical
+ 0.18 structural/entity overlap
+ 0.12 task/problem match
+ 0.10 confidence
+ 0.10 historical utility
+ 0.05 importance
+ 0.03 recency
- 0.12 stale-risk
- 0.08 redundancy
```

Weights MUST be configurable and benchmarked.

### 18.3 Explicit recall

Users/agents can force:
- memory ID;
- tag;
- problem;
- project;
- historical session;
- old commit range.

Forced recalls are labeled separately from automatic injection.

---

## 19. Context compiler

### 19.1 Purpose

Transform a potentially huge world model into a small task-specific context projection optimized for usefulness, correctness, cache reuse, and latency.

### 19.2 Inputs

- user task;
- system/developer instructions;
- project/branch/HEAD;
- current files/diff;
- active task graph;
- candidate memories/skills/invariants;
- structural neighborhood;
- model/runtime capabilities;
- model context limit;
- target token budget;
- stable-prefix cache metadata;
- latency budget.

### 19.3 Output sections

1. Stable developer/project policy.
2. Stable project architecture summary.
3. Task-specific entities and graph slice.
4. Validated relevant lessons.
5. Relevant failed approaches/gotchas.
6. Applicable skills/procedures.
7. Invariants/required validation.
8. Current working state/diff summary.
9. Provenance IDs sufficient for `why` queries.

### 19.4 Budgeting

Compiler MUST use explicit section budgets rather than unbounded insertion.

Default relative budget for an agent prompt augmentation:
- 15% stable project facts;
- 20% structural/task graph;
- 25% lessons/failures;
- 20% skills/procedures;
- 10% invariants/validation;
- 10% working-state summary.

These are starting defaults, not hard rules.

### 19.5 Knapsack-style selection

Each candidate has:
- expected utility;
- confidence;
- validity;
- novelty;
- token cost;
- cache placement cost.

Compiler maximizes expected utility subject to token and latency limits.

### 19.6 Stable-prefix strategy

For runtimes with prefix/KV reuse such as MTPLX:
- order stable content first;
- keep stable project-policy block deterministic;
- avoid timestamps/random IDs inside stable block;
- canonicalize ordering;
- place volatile task material later;
- expose stable-prefix hash in metrics.

### 19.7 Model-aware context

Small/local weaker model:
- more explicit procedure steps;
- more direct architecture summary;
- fewer ambiguous raw artifacts.

Larger/stronger model:
- less prescriptive text;
- more raw evidence/graph detail when useful.

Runtime adapter provides a `ModelProfile` to compiler.

---

## 20. Inference/runtime integration

### 20.1 Runtime abstraction

```python
class RuntimeAdapter(Protocol):
    def capabilities(self) -> RuntimeCapabilities: ...
    async def chat(self, request: ChatRequest) -> ChatResponse: ...
    async def health(self) -> Health: ...
    def metrics(self) -> RuntimeMetrics: ...
```

### 20.2 Supported V1 surfaces

- OpenAI-compatible `/v1/chat/completions`.
- Anthropic-compatible `/v1/messages` optional but recommended.
- streaming preserved end-to-end.
- tool calls passed through without schema corruption.

### 20.3 MTPLX optimizations

When MTPLX detected:
- preserve stable prefix ordering;
- record warm-prefix/session-cache hit metadata if exposed;
- avoid injecting unchanged dynamic boilerplate that breaks prefix equality;
- monitor prompt tokens, TTFT, decode TPS, cache restore source, cache footprint when available;
- allow `LEB_MODE=off` control path without changing model/runtime parameters.

### 20.4 Runtime neutrality

No memory logic may depend directly on MLX/Metal or MTPLX internals. Optimizations are capability-gated.

---

## 21. MCP architecture

### 21.1 Role

MCP is an edge API for explicit agent calls. It is not the automatic context path and is not the canonical state layer.

### 21.2 Tools

Minimum:
- `leb_project_status`
- `leb_query_world`
- `leb_recall`
- `leb_get_memory`
- `leb_explain_context`
- `leb_get_past_failures`
- `leb_get_skill`
- `leb_record_human_decision`
- `leb_mark_outcome`
- `leb_forget`
- `leb_get_invariants`
- `leb_get_impact`

Every stateful operation accepts explicit `project_id`, `task_id`, or `session_id` when required.

### 21.3 MCP correctness rule

Automatic correctness-critical context MUST NOT rely on the model remembering to call an MCP tool. The proxy/compiler path performs baseline injection deterministically.

---

## 22. CLI requirements

Examples:

```bash
leb init
leb status
leb project info
leb project reindex

leb chat --runtime http://127.0.0.1:8000
leb proxy --runtime mtplx

leb memory list
leb memory show mem_...
leb memory pin mem_...
leb memory mark-stale mem_...
leb memory forget mem_...
leb memory purge --project current

leb recall "abort resource cleanup"
leb context preview "fix abort race"
leb context why <context-item-id>

leb task start "fix scheduler abort race"
leb task finish --success

leb skill list
leb skill show skill_...

leb graph query "AuthService"
leb graph impact src/auth/service.py

leb eval run config/bench.yaml
leb eval compare run_A run_B
leb doctor
leb backup create
leb backup verify
leb index rebuild
```

CLI output MUST have `--json` for automation.

---

## 23. Configuration

### 23.1 Config layers

Priority:
1. CLI/request override
2. project `.leb/config.toml`
3. user `~/.config/leb/config.toml`
4. defaults

### 23.2 Example

```toml
[brain]
auto_learn = true
conservative_promotion = true
raw_transcript_retention_days = 14

[context]
max_injected_tokens = 6000
stable_prefix_tokens = 1800
semantic_retrieval = true
compact_injection = false

[storage]
db_path = "~/.leb/brain.sqlite"
blob_budget_gb = 5
vector_backend = "hnswlib"

[embeddings]
provider = "local"
model = "embeddinggemma"

[structural]
provider = "graphify"
fallback = "tree_sitter"

[policy]
default_enforcement = "warn"

[evaluation]
record_trajectory_metrics = true
```

---

## 24. Performance and memory-efficiency requirements

The product is local-first; overhead that makes a local coding agent feel sluggish is a product failure.

### 24.1 Latency budgets

On a warmed medium repository (<250k LOC) on modern developer hardware:

- project resolution: p95 < 10 ms
- DB metadata lookup: p95 < 10 ms
- FTS candidate retrieval: p95 < 25 ms
- vector candidate retrieval: p95 < 40 ms for <=500k vectors
- local graph neighborhood query: p95 < 30 ms for 2-hop bounded traversal
- rerank + context selection excluding embeddings inference: p95 < 40 ms
- context assembly: p95 < 25 ms
- total synchronous LEB pre-model overhead target: p95 < 150 ms; hard V1 ceiling < 300 ms
- explicit deep graph/history queries may exceed this and are treated as tools, not automatic hot path.

### 24.2 Incremental indexing budgets

- no full scan on normal prompt path;
- changed-file hash check < 30 ms typical;
- lightweight parse of one medium file < 50 ms target;
- full repo index is background/manual;
- embeddings generation never blocks hot path when cached lexical/structural fallback is available.

### 24.3 RAM budgets

Default daemon target excluding inference model:
- idle: < 200 MB RSS
- active medium repo: < 600 MB RSS
- large repo mode: < 1.5 GB RSS unless user raises budget.

Graph/vector caches MUST be bounded by configurable LRU budgets.

### 24.4 Disk budgets

Defaults:
- canonical DB: unbounded by hard deletion, but warn at thresholds;
- raw transcripts: time/size bounded;
- blobs/logs: default 5 GB quota;
- vector index: rebuildable and quota-reportable;
- runtime KV cache belongs to runtime, not duplicated by LEB.

### 24.5 Context token efficiency

Track:
- injected tokens per request;
- percentage used/acknowledged by agent;
- duplicated/redundant token fraction;
- stable-prefix reuse fraction;
- task success per injected 1k tokens.

A memory feature that increases prompt size without measurable value should be downranked or disabled.

### 24.6 Cache hierarchy

```text
L0: request-local objects
L1: in-process LRU for project metadata / graph neighborhoods / tokenized memory
L2: SQLite page cache + FTS
L3: vector index mmap/on-disk
L4: content-addressed blob store
External: inference runtime KV/session cache
```

LEB MUST NOT duplicate full model KV state.

### 24.7 Memory object compaction

- maintain compact canonical claim separate from verbose source episode;
- store token estimate;
- inject summaries, not raw episode transcripts, by default;
- consolidate duplicate memories;
- preserve evidence references without embedding full evidence text.

### 24.8 Background scheduling

Jobs are prioritized:
1. correctness-critical invalidation
2. changed-file structural refresh
3. candidate evidence verification
4. embedding generation
5. consolidation/skill synthesis
6. analytics/compaction

Background workers MUST yield CPU/GPU under active model generation where detectable/configured.

---

## 25. Observability and explainability

### 25.1 Required metrics

Per request:
- request/session/task/project IDs;
- mode: off/baseline/brain;
- compile latency by stage;
- candidate counts by retriever;
- selected context IDs and tokens;
- stable-prefix hash;
- runtime prompt/completion tokens;
- TTFT and wall time if available;
- tool-call count;
- files read/edited;
- test invocations/outcomes;
- memory IDs acknowledged/referenced;
- task outcome.

### 25.2 `context why`

Every injected item must explain:
- why it was eligible;
- scoring components;
- evidence/confidence;
- current validity;
- token cost;
- historical utility;
- source episode/commit where applicable.

### 25.3 Audit log

Learning mutations MUST be reconstructible from event log.

---

## 26. Security, privacy, and trust

### 26.1 Local defaults

- bind Unix socket or loopback only;
- no telemetry by default;
- no remote embeddings by default;
- no background network calls without explicit provider configuration.

### 26.2 Secrets

Never persist obvious secrets from environment/tool output by default. Implement redaction for:
- API keys/tokens;
- private keys;
- common credential formats;
- user-configured regexes.

### 26.3 Path/data isolation

Project scopes must not leak private project memories into unrelated projects unless explicit cross-project transfer is enabled.

### 26.4 Prompt injection from repository content

Repository docs/comments are untrusted data, not system policy. Structural/content ingestion MUST label source type. Only explicit developer-configured instructions may become policy automatically.

### 26.5 Purge semantics

`forget` = logical tombstone, excluded from retrieval.  
`purge` = delete canonical content + embeddings + blobs + cached derived summaries.  
Encrypted hard-delete mode MAY implement crypto-shredding of per-scope keys.

Backups must be reported when a purge cannot retroactively delete historical backup copies.

---

## 27. Failure and recovery behavior

### 27.1 LEB unavailable

Proxy MUST have configurable fail behavior:
- default `fail_open`: forward request without augmentation and clearly record degradation;
- strict mode `fail_closed` for policy-sensitive environments.

### 27.2 Vector index unavailable

Fallback to lexical + structural retrieval. Schedule rebuild.

### 27.3 Structural provider unavailable

Fallback to native tree-sitter/grep-style structural metadata. Do not block normal chat.

### 27.4 Corrupted DB

- `leb doctor` runs integrity checks;
- backup restore documented;
- derived stores can rebuild;
- no silent reset.

### 27.5 Partial task crash

Episodes remain open/recoverable. Next startup marks orphaned sessions and allows resume/close.

---

## 28. Functional requirements catalog

Priority: P0 required V1, P1 near-term, P2 later.

| ID | Priority | Requirement | Acceptance |
|---|---|---|---|
| FR-001 | P0 | Resolve stable project identity | Moving repo path retains project ID |
| FR-002 | P0 | Track current Git HEAD/diff | Status matches Git CLI |
| FR-003 | P0 | Store typed immutable events | Replay reconstructs projection |
| FR-004 | P0 | Normalize structural graph | At least tree-sitter provider passes contract suite |
| FR-005 | P0 | Import Graphify graph | Graphify sample matches normalized nodes/edges |
| FR-006 | P0 | Record task/session/attempts | End-to-end trajectory stored |
| FR-007 | P0 | Record deterministic evidence | Test/Git/file evidence queryable |
| FR-008 | P0 | Candidate memory lifecycle | Candidate cannot auto-inject as validated |
| FR-009 | P0 | Validate supported technical claims | Evidence linkage required |
| FR-010 | P0 | Version-aware memory applicability | Invalid commit range excluded |
| FR-011 | P0 | Symbol-change staleness | Changed dependency marks linked memory suspect |
| FR-012 | P0 | Hybrid retrieval | Lexical + structural + semantic optional |
| FR-013 | P0 | Context token budget | Never exceeds configured budget |
| FR-014 | P0 | Explain selected context | Every item has reason/provenance |
| FR-015 | P0 | OpenAI-compatible proxy | Standard client completes streaming/tool calls |
| FR-016 | P0 | Brain-disabled control path | Same runtime request with no augmentation |
| FR-017 | P0 | MCP query interface | Stateless explicit-handle tools work |
| FR-018 | P0 | CLI inspect/forget/purge | Changes immediately affect retrieval |
| FR-019 | P0 | A/B benchmark runner | Produces paired JSONL + comparison report |
| FR-020 | P0 | Local-only default | Network disabled test passes |
| FR-021 | P0 | Incremental refresh | Unchanged repo does not full-scan |
| FR-022 | P0 | Background jobs bounded | Hot path remains under performance ceiling |
| FR-023 | P1 | Runtime trace ingestion | Trace edges attach to symbols/services |
| FR-024 | P1 | Skill synthesis | Repeated episodes propose a skill |
| FR-025 | P1 | Invariant enforcement | Changed symbols trigger required validation |
| FR-026 | P1 | Utility-aware ranking | Ranking feature uses measured outcomes |
| FR-027 | P1 | Cross-project transfer | Imported lessons begin as hypotheses |
| FR-028 | P1 | Contradiction detector | Higher-confidence contradiction deactivates old memory |
| FR-029 | P1 | Context cache awareness | MTPLX stable prefix remains deterministic |
| FR-030 | P1 | IDE/dashboard | User can inspect active brain visually |
| FR-031 | P2 | Team sync | Permissions + signed shared knowledge |
| FR-032 | P2 | Fine-tune export | Validated trajectories export dataset |

---

## 29. Non-functional requirements

### Reliability
- Event mutations transactional.
- No learned object considered durable until commit succeeds.
- Recovery after process kill must not corrupt projections.

### Determinism
- Same DB state + same task + same config should produce same deterministic candidate ordering before LLM-assisted rerank.
- Benchmark control modes pin random seeds/model settings where supported.

### Testability
- Each engine has pure logic layer separated from I/O.
- Clock, Git, runtime, embedding, and model calls injectable/mocked.

### Extensibility
- provider interfaces for runtime, structural graph, embeddings, vector index, memory import/export.

### Compatibility
- Python 3.11+ recommended for V1.
- macOS arm64 and Linux x86_64/arm64.

---

## 30. API design

### 30.1 Internal service API groups

`/api/v1/projects`  
`/api/v1/tasks`  
`/api/v1/world`  
`/api/v1/memories`  
`/api/v1/evidence`  
`/api/v1/skills`  
`/api/v1/context/compile`  
`/api/v1/evaluation`  
`/api/v1/admin`

### 30.2 Context compile request

```json
{
  "project_id": "...",
  "session_id": "...",
  "task_text": "Fix abort race",
  "current_paths": ["src/reactor.py"],
  "token_budget": 6000,
  "model_profile": "qwen-27b-local",
  "mode": "brain"
}
```

### 30.3 Context compile response

```json
{
  "stable_prefix": "...",
  "dynamic_context": "...",
  "items": [
    {
      "id": "ctx_...",
      "source_id": "mem_...",
      "type": "lesson",
      "tokens": 81,
      "score": 0.91,
      "why": {"structural": 0.95, "utility": 0.80, "validity": 1.0}
    }
  ],
  "metrics": {
    "compile_ms": 72,
    "candidate_count": 118,
    "selected_tokens": 3211
  }
}
```

---

## 31. Suggested repository structure

```text
leb/
  pyproject.toml
  README.md
  src/leb/
    daemon/
    proxy/
    mcp/
    cli/
    config/
    projects/
    events/
    storage/
    world/
      graph/
      structural/
      git/
      runtime/
    evidence/
    experience/
    memory/
    validity/
    learning/
    policy/
    retrieval/
    context/
    runtime/
    embeddings/
    eval/
    observability/
    security/
  tests/
    unit/
    integration/
    e2e/
    perf/
    fixtures/
  scripts/
  docs/
```

---

# 32. Complete automated testing strategy

The test suite is part of the product. A memory/learning system that cannot prove isolation, determinism, applicability, and no-regression is dangerous.

## 32.1 Test layers

1. **Unit:** pure algorithms, state machines, schemas, ranking, validity, budgets.
2. **Contract:** provider interfaces against fake/reference providers.
3. **Integration:** SQLite, Git repos, parser/index, proxy, MCP, background jobs.
4. **End-to-end:** real temporary repos + stub/runtime model.
5. **Golden:** deterministic context snapshots.
6. **Property/fuzz:** event replay, scope isolation, token budgets, graph traversal.
7. **Fault injection:** kill process, corrupt derived index, disk full, provider timeout.
8. **Performance:** latency/RSS/disk/token overhead.
9. **Evaluation benchmarks:** model with brain OFF vs ON.

## 32.2 Unit test catalog

The following test names are expected minimum coverage. Sub-agents MAY add tests but SHOULD NOT delete these without architectural review.

### Project identity: `tests/unit/test_projects.py`
- `test_explicit_project_uuid_wins_over_path`
- `test_repo_move_preserves_identity`
- `test_same_remote_different_worktree_same_project`
- `test_distinct_fork_can_be_linked_but_not_collapsed`
- `test_nested_repo_resolves_nearest_root`
- `test_monorepo_component_scope_resolves`
- `test_no_git_repo_gets_persisted_generated_uuid`
- `test_project_alias_path_normalization`
- `test_project_identity_does_not_depend_on_branch_name`

### Event store: `tests/unit/test_events.py`
- `test_event_schema_version_required`
- `test_event_ids_unique`
- `test_append_order_monotonic`
- `test_event_payload_immutable`
- `test_replay_reconstructs_projection`
- `test_duplicate_event_id_is_idempotent_or_rejected`
- `test_unknown_event_version_fails_explicitly`
- `test_transaction_rolls_back_event_and_projection_together`
- `test_tombstone_event_preserves_audit_history`

### Graph model: `tests/unit/test_graph.py`
- `test_node_identity_is_stable_for_unchanged_symbol`
- `test_edge_requires_provenance`
- `test_edge_type_validation`
- `test_graph_neighbor_directionality`
- `test_shortest_path_respects_edge_filter`
- `test_bounded_traversal_never_exceeds_depth`
- `test_duplicate_edges_coalesce_without_losing_provenance`
- `test_removed_symbol_closes_validity_interval`
- `test_renamed_symbol_can_link_predecessor`
- `test_graph_scope_isolation_between_projects`

### Structural provider contract: `tests/unit/test_structural_contract.py`
- `test_provider_scan_returns_versioned_result`
- `test_provider_changed_paths_incremental`
- `test_provider_symbol_resolution`
- `test_provider_neighbors_schema`
- `test_provider_impact_bounded`
- `test_provider_timeout_is_recoverable`
- `test_provider_bad_edge_does_not_poison_canonical_store`

### Git model: `tests/unit/test_git_model.py`
- `test_head_commit_recorded`
- `test_merge_base_branch_family`
- `test_commit_ancestry_check`
- `test_dirty_diff_hash_changes_on_edit`
- `test_untracked_file_included_when_configured`
- `test_commit_range_validity_on_merged_branch`
- `test_old_branch_reactivates_historical_memory`

### Memory state machine: `tests/unit/test_memory_state.py`
- `test_new_model_claim_defaults_candidate`
- `test_candidate_not_injected_as_validated`
- `test_supported_requires_evidence`
- `test_validated_requires_policy_threshold`
- `test_rejected_memory_not_injected`
- `test_suspect_memory_not_injected_by_default`
- `test_superseded_memory_links_replacement`
- `test_contradicted_memory_retains_history`
- `test_archived_memory_queryable_but_not_auto_injected`
- `test_purged_memory_unavailable`
- `test_human_pin_records_override_provenance`

### Evidence: `tests/unit/test_evidence.py`
- `test_test_result_tier_a`
- `test_git_diff_tier_a`
- `test_file_digest_tier_a`
- `test_llm_claim_tier_c`
- `test_repeated_llm_claim_does_not_raise_confidence`
- `test_independent_deterministic_evidence_can_promote`
- `test_failed_test_can_contradict_claim`
- `test_evidence_content_hash_stable`
- `test_evidence_missing_artifact_detected`
- `test_manual_approval_is_distinguishable_from_technical_verification`

### Validity: `tests/unit/test_validity.py`
- `test_before_valid_from_commit_is_inapplicable`
- `test_after_valid_from_commit_is_applicable`
- `test_after_valid_to_commit_is_inapplicable`
- `test_symbol_hash_match_preserves_validity`
- `test_symbol_hash_change_marks_suspect`
- `test_removed_symbol_invalidates_memory`
- `test_dependency_constraint_match`
- `test_dependency_constraint_mismatch`
- `test_environment_constraint_match`
- `test_higher_confidence_contradiction_marks_suspect`
- `test_historical_checkout_selects_historical_memory`
- `test_bitemporal_known_time_query`

### Experience: `tests/unit/test_experience.py`
- `test_task_start_creates_episode`
- `test_tool_calls_link_to_episode`
- `test_edit_validate_cycle_forms_attempt`
- `test_revert_marks_attempt_failure`
- `test_test_pass_without_acceptance_is_provisional`
- `test_user_acceptance_promotes_success_signal`
- `test_unknown_outcome_not_success`
- `test_session_crash_leaves_resumable_episode`
- `test_multiple_attempts_preserve_order`
- `test_failed_approach_summary_links_changed_symbols`

### Consolidation/learning: `tests/unit/test_learning.py`
- `test_distiller_output_must_reference_source_evidence`
- `test_duplicate_candidate_merges_only_when_compatible`
- `test_contradictory_candidates_not_merged`
- `test_skill_requires_minimum_episode_support`
- `test_skill_trigger_scope_defaults_narrow`
- `test_skill_promotion_blocked_by_harmful_use_rate`
- `test_skill_preserves_negative_examples`
- `test_cross_project_transfer_creates_hypothesis`
- `test_cross_project_transfer_never_directly_validates`
- `test_procedure_steps_trace_to_source_episodes`

### Invariants/policy: `tests/unit/test_policy.py`
- `test_invariant_matches_changed_symbol`
- `test_invariant_nonmatching_diff_no_trigger`
- `test_warn_policy_does_not_block`
- `test_require_validation_adds_required_test`
- `test_block_requires_opt_in`
- `test_human_override_audited`
- `test_removed_validation_test_marks_invariant_suspect`
- `test_repository_comment_cannot_auto_create_policy`

### FTS/lexical retrieval: `tests/unit/test_lexical_retrieval.py`
- `test_exact_tag_match_boosted`
- `test_bm25_returns_expected_memory`
- `test_project_scope_filter_applied_before_ranking`
- `test_invalid_status_excluded`
- `test_keyword_trigger_forces_candidate`
- `test_lexical_query_handles_symbols_and_punctuation`

### Semantic retrieval: `tests/unit/test_semantic_retrieval.py`
- `test_embedding_backend_optional`
- `test_semantic_candidate_mapping`
- `test_missing_embedding_falls_back_cleanly`
- `test_rebuild_does_not_change_canonical_ids`
- `test_remote_embedding_disabled_by_default`

### Structural retrieval: `tests/unit/test_structural_retrieval.py`
- `test_same_symbol_memory_boosted`
- `test_same_file_memory_boosted`
- `test_graph_distance_penalizes_far_nodes`
- `test_unrelated_project_graph_never_leaks`
- `test_recent_changed_symbol_requires_validity_check`

### Ranker: `tests/unit/test_ranker.py`
- `test_ranker_is_deterministic`
- `test_validated_beats_equal_candidate_hypothesis`
- `test_high_utility_boost`
- `test_stale_risk_penalty`
- `test_token_cost_penalty`
- `test_redundancy_penalty`
- `test_forced_recall_bypasses_score_cutoff`
- `test_weight_configuration_validation`

### Context compiler: `tests/unit/test_context_compiler.py`
- `test_never_exceeds_token_budget`
- `test_stable_prefix_order_deterministic`
- `test_stable_prefix_omits_timestamps`
- `test_dynamic_items_sorted_deterministically_on_tie`
- `test_diversity_prevents_duplicate_memory_flood`
- `test_required_invariant_survives_budget_pressure`
- `test_invalid_memory_never_compiled`
- `test_small_model_profile_prefers_procedure`
- `test_large_model_profile_can_prefer_raw_evidence`
- `test_context_item_has_why_payload`
- `test_no_brain_mode_returns_no_augmentation`
- `test_project_only_mode_excludes_global_preferences`
- `test_fresh_mode_excludes_all_durable_memory`

### Token accounting: `tests/unit/test_tokens.py`
- `test_token_estimate_nonnegative`
- `test_token_cache_invalidated_on_text_change`
- `test_budget_accounts_for_section_headers`
- `test_model_tokenizer_adapter_fallback`

### Utility model: `tests/unit/test_utility.py`
- `test_retrieval_count_increment`
- `test_injection_count_increment`
- `test_acknowledged_memory_recorded`
- `test_successful_association_updates_utility`
- `test_harmful_association_reduces_utility`
- `test_no_causal_claim_from_single_correlation`
- `test_utility_decay_or_prior_prevents_one_sample_dominance`

### Storage: `tests/unit/test_storage.py`
- `test_wal_mode_enabled`
- `test_foreign_keys_enabled`
- `test_transaction_atomicity`
- `test_scope_delete_cascades_derived_not_audit_unless_purge`
- `test_backup_snapshot_consistent`
- `test_blob_content_addressed_dedup`
- `test_blob_quota_enforced`
- `test_vector_index_is_rebuildable`

### Cache: `tests/unit/test_cache.py`
- `test_lru_bound_enforced`
- `test_graph_cache_key_includes_commit`
- `test_memory_cache_invalidated_on_state_change`
- `test_no_full_episode_blob_cached_in_hot_lru_by_default`
- `test_cache_metrics_report_hits_and_bytes`

### Security/redaction: `tests/unit/test_security.py`
- `test_api_key_pattern_redacted`
- `test_private_key_redacted`
- `test_custom_redaction_regex`
- `test_repository_instruction_not_promoted_to_system_policy`
- `test_cross_project_private_memory_disabled_by_default`
- `test_mcp_requires_explicit_scope_handle_for_mutations`
- `test_network_provider_rejected_when_local_only_mode`

### Purge: `tests/unit/test_purge.py`
- `test_forget_tombstones_memory`
- `test_purge_removes_memory_text`
- `test_purge_removes_embedding`
- `test_purge_removes_unshared_blob`
- `test_purge_preserves_shared_blob_until_unreferenced`
- `test_purge_invalidates_context_cache`
- `test_purge_reports_backup_limitation`

### Proxy: `tests/unit/test_proxy.py`
- `test_proxy_preserves_user_messages`
- `test_proxy_inserts_context_at_configured_boundary`
- `test_proxy_streaming_passthrough`
- `test_proxy_tool_call_schema_passthrough`
- `test_proxy_fail_open_on_brain_error`
- `test_proxy_fail_closed_when_configured`
- `test_off_mode_has_identical_runtime_parameters`
- `test_request_project_resolution_from_cwd_header_or_session`

### MCP: `tests/unit/test_mcp.py`
- `test_tools_are_stateless_at_transport_layer`
- `test_project_handle_required_where_needed`
- `test_recall_tool_scope_isolation`
- `test_forget_tool_requires_valid_id`
- `test_explain_context_returns_provenance`
- `test_mcp_server_can_restart_without_losing_state`

### Configuration: `tests/unit/test_config.py`
- `test_cli_overrides_project_overrides_user`
- `test_invalid_token_budget_rejected`
- `test_remote_embedding_requires_explicit_enable`
- `test_policy_block_not_default`
- `test_paths_expand_user_safely`

### Observability: `tests/unit/test_observability.py`
- `test_request_metrics_have_mode`
- `test_context_ids_logged_without_secret_text_when_redacted_mode`
- `test_latency_stage_timers_complete_on_exception`
- `test_stable_prefix_hash_deterministic`

### Evaluation: `tests/unit/test_eval.py`
- `test_pairing_key_matches_same_task_model_config`
- `test_off_and_on_modes_differ_only_by_brain_controls`
- `test_failed_run_retained_not_silently_dropped`
- `test_success_metric_plugin`
- `test_token_delta_calculation`
- `test_tool_call_delta_calculation`
- `test_bootstrap_ci_reproducible_with_seed`
- `test_report_flags_underpowered_sample`

## 32.3 Property-based tests

Use Hypothesis or equivalent.

Properties:
- event replay == current projection after arbitrary valid event sequence;
- no automatic context includes PURGED/SUPERSEDED/invalid memory;
- compiled token count <= budget for arbitrary candidate sizes;
- project A candidates never appear in project B when transfer disabled;
- graph traversal never emits cross-project edges without federation;
- ranker deterministic for identical input;
- forget/purge operations idempotent;
- bitemporal interval queries obey ancestry boundaries.

## 32.4 Integration test catalog

### SQLite/event replay
- create DB, mutate state, close, reopen, replay, compare projections.
- forced process kill during transaction.
- WAL checkpoint under concurrent reads.

### Temporary Git repos
Create fixture repos with commits A-B-C-D and divergent branch. Assert validity across checkouts and merges.

### Structural parsing
Small Python/TS/Rust fixture repos. Verify calls/imports/definitions and incremental update after file edit.

### Graphify adapter
If Graphify installed in CI optional job, build fixture graph and compare normalized output.

### Proxy/runtime
Use deterministic fake OpenAI server plus optional real local-model nightly job. Validate streaming, tools, timeouts, fail-open.

### MCP
Start server, call tools over supported transport, restart, verify persistent state.

### Test evidence
Execute real pytest fixture where attempt 1 fails and attempt 2 passes. Verify failure/lesson chain.

### Background jobs
Queue embeddings/consolidation, saturate worker, ensure interactive context request stays responsive.

## 32.5 End-to-end scenarios

**E2E-01 Repeated bug**
- Task 1: agent discovers fix after failed approach.
- Store verified lesson.
- Reset repo to new related bug.
- Task 2 with brain ON receives relevant lesson; OFF does not.

**E2E-02 Stale memory**
- Validate Redis-based implementation memory.
- Commit migration to Postgres.
- Structural invalidation marks old memory suspect/superseded.
- New task cannot receive stale Redis fact automatically.

**E2E-03 Historical branch**
- Checkout pre-migration commit.
- Historical Redis memory becomes applicable again.

**E2E-04 Cross-project negative transfer**
- Learn project A framework-specific lesson.
- Project B differs in dependency version.
- Transfer, if enabled, enters as hypothesis and is not used as validated policy.

**E2E-05 Invariant protection**
- Establish invariant through explicit declaration + tests.
- Agent edits linked symbol.
- Compiler injects invariant and requires validation test.

**E2E-06 Purge**
- Store memory, embedding, blob, cached context.
- Purge.
- Verify no retrieval and no derived artifact remains.

**E2E-07 Offline operation**
- Block network.
- Run full local workflow.
- No failure except explicitly configured remote integrations.

**E2E-08 MTPLX cache-aware context**
- Same project stable prefix across two tasks is byte-identical.
- Dynamic suffix changes.
- Record runtime cache metrics if available.

---

# 33. Performance test plan

## 33.1 Synthetic repository sizes

- Small: 20k LOC / 500 symbols / 500 memories.
- Medium: 250k LOC / 10k symbols / 20k memories.
- Large: 2M LOC / 100k+ symbols / 500k memories.

## 33.2 Benchmarks

- cold daemon startup;
- warm daemon startup;
- project resolution;
- changed-file detection;
- lexical retrieval p50/p95/p99;
- vector retrieval p50/p95/p99;
- 1/2/3-hop graph traversal;
- validity filter latency;
- rank latency vs 100/1k/10k candidates;
- compile latency vs token budget;
- RSS idle/active/large;
- DB growth per 1k episodes;
- vector index growth per 100k items;
- full rebuild throughput;
- purge throughput;
- concurrent proxy requests while background indexing;
- impact on model TTFT with stable vs unstable prefix.

## 33.3 Regression gates

CI perf smoke:
- compile p95 < 300 ms on standardized fixture;
- no >15% latency regression vs committed baseline without waiver;
- idle RSS < configured test ceiling;
- token overhead for golden task within expected range.

Nightly hardware benchmark SHOULD maintain historical trend data.

---

# 34. Model A/B evaluation framework

This is a first-class deliverable, not an afterthought.

## 34.1 Experimental principle

Compare:

**CONTROL**
```text
same model
same runtime
same temperature/top_p
same toolset
same repository state
LEB capture allowed only if it cannot affect model
NO context injection
NO learned warnings/policies
```

**TREATMENT**
```text
same everything
+ LEB context compiler
+ applicable skills/invariants/policies according to test condition
```

The benchmark runner MUST record configuration hashes proving equivalence.

## 34.2 Evaluation conditions

At minimum:
- `off`: no brain influence.
- `memory_only`: validated memories, no graph expansion.
- `graph_only`: structural world model, no experience memory.
- `memory_graph`: both.
- `full`: evidence validity + memory + graph + skills/policies.

This supports ablation studies, not just on/off.

## 34.3 Task categories

1. repeated/family bug;
2. architecture navigation;
3. regression prevention;
4. dependency migration;
5. project convention adherence;
6. stale-memory trap;
7. misleading old approach;
8. cross-project transfer;
9. long-horizon multi-session feature;
10. unseen unrelated task control.

The last category checks that LEB does not hurt unrelated work.

## 34.4 Primary metrics

- task success rate;
- tests passed / acceptance criteria satisfied;
- regression rate;
- wall-clock time;
- model input tokens;
- model output tokens;
- total token-equivalent compute if available;
- tool calls;
- files read;
- files edited;
- failed attempts;
- repeated known-bad attempts;
- context compile overhead;
- peak daemon RSS;
- user intervention count.

## 34.5 Memory-specific metrics

- recall precision@k;
- recall coverage of known relevant memories;
- stale-memory injection rate;
- harmful-memory rate;
- memory adherence rate;
- injected tokens / successful task;
- utility by memory kind;
- context redundancy.

## 34.6 Statistical requirements

For publishable claims:
- paired task design;
- multiple seeds/runs where model stochasticity exists;
- report raw N;
- bootstrap confidence intervals for rate/delta metrics;
- McNemar or paired proportion analysis for success where appropriate;
- Wilcoxon/paired bootstrap for skewed tool/token/time metrics;
- do not claim significance from one anecdotal run;
- preserve all failed runs.

## 34.7 Benchmark lifecycle

A benchmark episode used to create memories MUST NOT leak treatment knowledge into the control run. Use isolated brain snapshots or run control before treatment learning as defined by the experiment.

---

## 35. Automated benchmark scripts

This handoff package includes executable/reference scripts:

- `scripts/bench_ab.py`: paired runner for OFF/ON/ablation modes.
- `scripts/compare_runs.py`: aggregate paired metrics and bootstrap intervals.
- `scripts/check_perf_budget.py`: fail CI when overhead budgets regress.
- `scripts/run_benchmark.sh`: shell wrapper.
- `config/bench.example.yaml`: benchmark configuration.

The runner uses an adapter command rather than hard-coding one agent. Each benchmark task defines:
- repo fixture path/setup command;
- task prompt;
- agent command template;
- success checker command;
- max wall time;
- run count/seeds.

Agents implementing production integration SHOULD preserve this abstraction.

---

# 36. Benchmark task fixture format

```yaml
id: scheduler_abort_002
description: related abort cleanup regression
repo: fixtures/scheduler
setup:
  command: ./tasks/scheduler_abort_002/setup.sh
prompt_file: ./tasks/scheduler_abort_002/prompt.txt
agent:
  command: >-
    opencode run --model local/qwen --prompt-file {prompt_file}
success:
  command: pytest -q tests/test_abort_cleanup.py
limits:
  wall_seconds: 900
modes: [off, memory_only, graph_only, full]
seeds: [1, 2, 3]
```

All benchmark runs MUST occur in disposable Git worktrees or copies.

---

# 37. Acceptance gates by milestone

## Milestone 0 - skeleton

Deliver:
- package/repo structure;
- config;
- SQLite migrations;
- event store;
- project identity;
- CLI doctor/status.

Exit:
- unit suite for these modules green;
- process-kill transaction test green.

## Milestone 1 - world model

Deliver:
- Git ingestion;
- native structural provider;
- graph normalization/query;
- Graphify adapter optional.

Exit:
- fixture repos indexed;
- incremental updates proven;
- graph queries meet latency ceiling.

## Milestone 2 - evidence + memory

Deliver:
- episode model;
- evidence records;
- memory lifecycle;
- validity engine;
- contradiction triggers.

Exit:
- repeated bug and stale-memory E2E scenarios green.

## Milestone 3 - retrieval + context compiler

Deliver:
- FTS;
- optional embeddings;
- structural retrieval;
- ranker;
- token optimizer;
- context why.

Exit:
- golden contexts deterministic;
- hot-path p95 under ceiling.

## Milestone 4 - runtime integration

Deliver:
- OpenAI proxy;
- streaming/tool support;
- MCP adapter;
- MTPLX runtime profile.

Exit:
- real local agent can operate on fixture repo with brain on/off.

## Milestone 5 - learning

Deliver:
- task distillation;
- memory promotion;
- dedupe/supersession;
- procedure synthesis.

Exit:
- learned memory must be evidence-linked;
- no unsupported LLM hypothesis auto-validates.

## Milestone 6 - evaluation

Deliver:
- benchmark scripts;
- ablations;
- paired reporting;
- perf CI.

Exit:
- reproducible benchmark report from clean machine.

## Milestone 7 - skills/invariants beta

Deliver:
- skill synthesis;
- empirical utility;
- invariant/policy engine.

Exit:
- demonstrates reduced repeat failures without unacceptable stale/harmful injection.

---

# 38. Parallel sub-agent work packages

The implementation can be divided among agents with strict interfaces.

### WP-A Core/storage
Owns SQLite schema, migrations, event store, blob store, backup, replay.
Depends on: data contracts only.

### WP-B Project/Git
Owns project identity, worktrees, branches, commit ancestry, diff fingerprints.

### WP-C Structural world model
Owns normalized graph schema, tree-sitter provider, Graphify adapter, incremental indexing.

### WP-D Evidence/validity
Owns evidence classes, promotion rules, bitemporal applicability, invalidation/contradiction.

### WP-E Experience/learning
Owns task/attempt/episode capture, distillation contracts, dedupe, procedure/skill synthesis.

### WP-F Retrieval/context
Owns FTS, semantic adapter, structural retrieval, ranker, token budgeting, context compiler, why explanations.

### WP-G Proxy/runtime
Owns OpenAI/Anthropic surfaces, streaming, runtime adapters, MTPLX capabilities, fail-open behavior.

### WP-H MCP/CLI
Owns external MCP tools and CLI commands; no domain logic duplicated.

### WP-I Evaluation/performance
Owns benchmark harness, metrics, ablations, statistics, perf regression scripts.

### WP-J Security/observability
Owns redaction, local-only enforcement, audit metrics, tracing, doctor diagnostics.

### WP-K Integration QA
Owns E2E fixtures, fault injection, release gates, cross-work-package contract validation.

Rules for all sub-agents:
1. Do not bypass domain interfaces by directly writing another module's tables.
2. Add schema changes through migrations and update contracts.
3. Unit tests required with implementation.
4. No network dependency in default test path.
5. Provide deterministic fixtures.
6. Log architectural deviations in ADRs.

---

# 39. Agent handoff protocol

Each implementation agent should receive:
- this spec;
- the work package definition;
- current interfaces/types;
- tests it owns;
- dependencies already implemented;
- explicit out-of-scope list.

Each agent returns:
1. code;
2. tests;
3. migration/schema changes;
4. benchmark/perf implications;
5. ADR if it changed an architecture assumption;
6. known limitations;
7. next-agent handoff notes.

No agent should be asked to “build the whole thing” once parallel development begins.

---

# 40. Architectural decision records required early

Create ADRs for:
- ADR-001 canonical SQLite/event model;
- ADR-002 graph logical model and physical persistence;
- ADR-003 project identity/fork/worktree semantics;
- ADR-004 evidence confidence policy;
- ADR-005 bitemporal validity semantics;
- ADR-006 context insertion location/order;
- ADR-007 token accounting/tokenizer strategy;
- ADR-008 vector backend abstraction;
- ADR-009 raw transcript retention/privacy;
- ADR-010 proxy fail-open/fail-closed behavior;
- ADR-011 benchmark isolation/control methodology;
- ADR-012 skill/invariant promotion rules.

---

# 41. Release-quality observability dashboard

The eventual local dashboard should show:

**Brain status**
- active project/branch;
- memory count by state/type;
- current index freshness;
- DB/vector/blob size;
- background queue;
- local-only/network status.

**Current context**
- selected items;
- token allocation;
- why selected;
- stable-prefix hash/cache status.

**Learning**
- recent candidate lessons;
- promoted/rejected/superseded;
- skills and reuse success;
- contradictions requiring review.

**Effectiveness**
- success delta on/off;
- token delta;
- repeated-failure delta;
- harmful-memory count;
- compiler overhead.

---

# 42. Memory lifecycle examples

## 42.1 Good path

```text
Agent hypothesis
"Abort cleanup is skipped because child receives no signal"
        |
        v
CANDIDATE hypothesis (.42)
        |
 edits child signal path
        |
 regression test passes
        |
 diff + test evidence
        v
SUPPORTED (.86)
        |
 user accepts + repeated test pass
        v
VALIDATED lesson (.97)
        |
 later similar task
        v
retrieved/injected
        |
 successful reuse
        v
utility increases
```

## 42.2 Poisoning prevention

```text
Agent says "Redis is required"
        |
 no code/test/human evidence
        v
CANDIDATE only
        |
 repeated by same agent
        v
STILL CANDIDATE
```

## 42.3 Invalidation

```text
VALIDATED: SessionStore -> Redis
        |
 commit removes RedisSessionStore
        |
 structural contradiction
        v
SUSPECT
        |
 new implementation verified
        v
SUPERSEDED by SessionStore -> Postgres
```

---

# 43. Context format guidance

Do not dump machine-oriented JSON unless the model benefits. Render concise structured text with IDs.

Example:

```text
[LEB PROJECT]
Project: scheduler-service @ 8ea91c
Architecture: API -> Scheduler -> Reactor -> ResourceManager

[VALIDATED LESSONS]
M-183 (.98, valid at HEAD): Abort cleanup must execute in the child lifecycle path; killing only the parent executor previously left reservations allocated.
Evidence: test_abort_cleanup, commit 17af3c.

[KNOWN FAILED APPROACH]
M-091 (.96): Replacing the outer ProcessPoolExecutor did not release active reservations.

[INVARIANT]
I-04: Every reservation acquired by Reactor must be released exactly once.
Required validation: pytest tests/test_abort_cleanup.py tests/test_timeout_cleanup.py

[CURRENT IMPACT]
Current diff touches Reactor.abort and ResourceManager.release, both linked to I-04.
```

Context IDs allow later `leb context why M-183`.

---

# 44. Performance implementation guidance

### Database
- WAL mode.
- prepared statements.
- batch event writes.
- indexes on `(project_id,status,kind)`, validity fields, evidence links.
- FTS table stores compact memory claims, not full transcripts.

### Graph
- integer internal IDs for hot adjacency.
- edge-type bitsets/enums where practical.
- bounded traversal.
- cache graph neighborhoods keyed by `(project, commit, node, edge-mask, depth)`.

### Embeddings
- embed memories/lessons, architecture summaries, task summaries; not every line of source by default.
- batch background embedding.
- lower-dimensional local embedding model initially.
- mmap or on-disk HNSW if available.

### Context
- memoize token counts.
- canonical formatting.
- no repeated verbose provenance; IDs + compact evidence names.
- raw evidence fetched only on demand.

### Runtime
- do not touch model memory/KV unless runtime API provides supported feature.
- allow background jobs to pause while inference is active.

---

# 45. Testing the claim that LEB is worthwhile

The project is successful only if at least one well-designed benchmark demonstrates meaningful benefit without unacceptable cost.

Initial target for a repository-specific repeated-task benchmark after sufficient learned episodes:
- >=10 percentage point absolute task-success improvement OR equivalent statistically credible efficiency gain;
- >=20% reduction in repeated known-bad attempts;
- <=10% increase in total wall time on unrelated control tasks;
- stale/harmful automatic injection <2% of injections in curated test set;
- context compiler p95 <=150 ms target;
- daemon active RSS <=600 MB medium-repo target;
- average injected context <= configured 6k-token budget.

These are product targets, not guaranteed outcomes. If benchmarks fail, architecture/ranking/learning must change rather than redefining success post hoc.

---

# 46. Risk register

| Risk | Impact | Mitigation |
|---|---|---|
| Memory poisoning | High | Evidence-gated promotion; candidate states |
| Stale knowledge | High | Commit/symbol validity + contradiction engine |
| Retrieval noise | High | Hybrid ranker, utility, diversity, strict budgets |
| Context bloat | High | Compiler/knapsack, token metrics, ablations |
| Negative transfer | High | Transfer as hypothesis; applicability model |
| Local latency | High | incremental indexing, background work, caches |
| Excess RAM | High | bounded LRU/mmap/on-disk indexes |
| Graph staleness | Medium | changed-file invalidation + live-source precedence |
| Agent ignores memory | Medium | automatic injection + explicit skills/policies |
| Vendor/runtime coupling | Medium | adapter interfaces |
| Benchmark leakage | High | isolated brain snapshots and disposable worktrees |
| Privacy leakage | High | local-only default, scope isolation, redaction |
| Overfitting to benchmark | Medium | unrelated controls + held-out repositories |
| Complexity explosion | High | staged milestones; no premature multi-agent internals |

---

# 47. Future research directions

1. Learn retrieval weights from causal/paired benchmark data.
2. Graph neural or graph-aware rerankers over engineering world model.
3. Counterfactual memory evaluation: would task succeed without item M?
4. Automated invariant mining from tests and runtime traces.
5. Causal failure graphs rather than mere correlation.
6. Learned skill applicability classifiers.
7. Compiling validated skills into tool/harness policies.
8. LoRA dataset generation from validated successful trajectories.
9. Model-size substitution research: whether experience narrows frontier-vs-local gap in a bounded repo.
10. Team knowledge merge/conflict resolution with signed provenance.
11. Temporal architecture drift detection from graph deltas.
12. Memory economics: optimize accuracy gain per token, joule, and millisecond.

---

# 48. Definition of done for V1

V1 is done when a clean machine can:

1. install LEB without cloud account;
2. register a Git repository;
3. build a local structural world model;
4. run an OpenAI-compatible local model through the proxy;
5. capture an engineering episode;
6. link a passing/failing test to an attempt;
7. promote a technically supported lesson only with evidence;
8. invalidate that lesson when its referenced implementation changes;
9. retrieve and compile relevant valid context on a later task;
10. explain why each injected item appeared;
11. purge a memory and all derived retrieval material;
12. run the same benchmark with brain OFF and ON;
13. produce a machine-readable and human-readable comparison report;
14. pass the required unit/integration/E2E suite;
15. remain within configured local latency/RAM/token budgets.

---

# Appendix A. Suggested SQLite tables

```sql
projects(id, name, canonical_remote, root_commit, created_at, ...)
project_aliases(project_id, path, ...)
commits(id, project_id, hash, parents_json, authored_at, ...)
nodes(id, project_id, kind, stable_key, label, current_digest, ...)
edges(id, project_id, source_id, relation, target_id, confidence, ...)
edge_provenance(edge_id, source_kind, source_ref, digest, ...)
validity(id, subject_type, subject_id, from_commit, to_commit, known_from, known_to, ...)
events(id, project_id, type, schema_version, payload_json, created_at, ...)
tasks(id, project_id, session_id, status, prompt_digest, ...)
attempts(id, task_id, ordinal, strategy_summary, outcome, ...)
evidence(id, project_id, kind, strength, commit_hash, artifact_digest, metadata_json, ...)
memories(id, project_id, scope_type, scope_id, kind, epistemic_state, status, claim, ...)
memory_evidence(memory_id, evidence_id, relation)
memory_sources(memory_id, episode_id)
memories_fts(memory_id, claim, tags, rationale)
skills(id, project_id, scope_type, scope_id, title, procedure_json, status, ...)
skill_sources(skill_id, episode_id)
invariants(id, project_id, statement, enforcement, status, ...)
invariant_nodes(invariant_id, node_id)
invariant_tests(invariant_id, test_node_id)
context_runs(id, request_id, project_id, mode, token_budget, compile_ms, ...)
context_items(context_run_id, ordinal, source_type, source_id, score, tokens, why_json)
usage_events(id, source_type, source_id, context_run_id, usage_kind, outcome_id, ...)
eval_runs(id, benchmark_id, mode, model_hash, config_hash, metrics_json, ...)
blobs(digest, size_bytes, content_type, created_at, refcount, ...)
jobs(id, type, priority, status, payload_json, ...)
```

---

# Appendix B. Core Python protocols

```python
class VectorIndex(Protocol):
    def upsert(self, items: list[VectorItem]) -> None: ...
    def search(self, vector: list[float], k: int, filters: dict) -> list[Hit]: ...
    def delete(self, ids: list[str]) -> None: ...
    def rebuild(self) -> None: ...

class EmbeddingProvider(Protocol):
    def embed(self, texts: list[str]) -> list[list[float]]: ...

class EvidenceVerifier(Protocol):
    async def verify(self, candidate: ClaimCandidate, ctx: VerificationContext) -> VerificationResult: ...

class Retriever(Protocol):
    async def candidates(self, query: RetrievalQuery) -> list[Candidate]: ...

class ContextCompiler(Protocol):
    async def compile(self, request: CompileRequest) -> CompiledContext: ...

class RuntimeAdapter(Protocol):
    def capabilities(self) -> RuntimeCapabilities: ...
    async def chat(self, request: ChatRequest) -> ChatResponse: ...
```

---

# Appendix C. Suggested development commands

```bash
uv sync --all-extras
pytest -q
pytest tests/unit -q
pytest tests/integration -q
pytest tests/e2e -q
pytest tests/perf -q
ruff check .
pyright src
leb doctor --json
leb eval run config/bench.example.yaml
```

---

# Appendix D. Benchmark reporting example

```text
Benchmark: scheduler-family-v1
Model: Qwen local / identical runtime config
Paired tasks: 40

Metric                    OFF          FULL         Delta
----------------------------------------------------------------
Task success              62.5%        77.5%        +15.0 pp
Median input tokens       31,200       23,800        -23.7%
Median tool calls         24           16            -33.3%
Known-bad repeats/task    1.80         0.42          -76.7%
Median wall time          11.8m        8.6m          -27.1%
Stale injections          -            0.8%          target <2%
LEB compile p95           -            112 ms        target <150ms
Daemon RSS p95            -            482 MB        target <600MB
```

Report confidence intervals and exact run configuration underneath. Never fabricate results; this table is format only.

---

# Appendix E. Implementation sequence recommendation

Do **not** start with sophisticated vector retrieval or automatic skills.

Recommended order:
1. event store + project/Git identity;
2. evidence + memory state machine;
3. simple structural graph;
4. validity/invalidation;
5. lexical + structural retrieval;
6. deterministic context compiler;
7. proxy + brain-off control;
8. benchmark harness;
9. only then embeddings/reranking;
10. only after measured benefit, add synthesis/skills/invariants.

This sequence ensures the team can measure whether each increment actually improves the agent rather than accumulating an impressive but unvalidated architecture.

---

# Appendix F. Sub-agent kickoff prompt template

Use the following structure when assigning a work package to an implementation agent:

```text
You are implementing Work Package <X> of Local Engineering Brain (LEB).

Authoritative spec: LOCAL_ENGINEERING_BRAIN_SPEC.md.
Your owned modules: <paths>.
Your owned test catalog: <tests>.
Dependencies you may call: <interfaces>.
Do not modify: <other teams' modules> except shared type contracts through reviewed changes.

Required output:
1. implementation;
2. all required unit tests plus useful additional tests;
3. integration fixtures if applicable;
4. docs/ADR for architectural deviations;
5. performance notes and measured local results;
6. explicit known limitations.

Hard rules:
- no cloud dependency by default;
- no direct writes into another domain's tables;
- no unsupported LLM claim can become validated knowledge;
- preserve project scope and bitemporal semantics;
- maintain brain-off control behavior;
- keep hot-path latency within the spec budget.
```

---

# Appendix G. Questions intentionally left empirical

These SHOULD be answered by benchmark data, not architectural taste:

- optimal retrieval scoring weights;
- best vector backend;
- whether embeddings materially improve coding-task outcomes beyond lexical+graph retrieval;
- optimal token budget per model size;
- number of episodes required before procedure synthesis;
- whether model-assisted reranking pays for its latency;
- which runtime traces are worth retaining;
- how aggressively utility should affect ranking;
- when an invariant should become blocking;
- whether MTPLX stable-prefix optimization materially changes total task economics.

The architecture is deliberately designed so these choices remain replaceable and measurable.

---

**End of specification.**
