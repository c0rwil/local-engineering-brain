# ADR-016: A cited product model and living documentation

Status: accepted

## Problem

Repository facts and one link per repository pair cannot describe product intent,
ordered user flows, or multiple contracts between services. A Markdown guide in
an optional Obsidian vault also cannot provide stable section identity, source
freshness or safe concurrent editing.

## Decision

Schema 12 adds a versioned product catalog in canonical SQLite. The daemon's
single-writer scheduler owns mutations. Product entities describe products,
capabilities, flows, services, repositories, APIs, queues, databases and schemas.
Typed dependency records have independent identities, contract names, optional
environments, and source anchors. Repository identities remain in the existing
project registry and appear as `repo:<project-id>` references.

Intent is declared, observed behavior cites independent evidence, and hypotheses
are inferred. A source anchor points to one indexed file, symbol or evidence row
in a specific project. Capture records its digest and indexed/evidence revision.
Freshness compares those particular sources, not a whole-workspace timestamp.
“Sources unchanged” means only that the recorded indexed bytes still match; it
is not a claim that prose is correct or production is unchanged. Missing or
purged previously known sources require review. Unknown and inferred sources
remain visible. Deployment observations require runtime evidence at their exact
project and revision; they are never presented as live cloud checks.

Documentation sections have stable IDs, separate curated and generated text,
review attestations and immutable revisions. Reindexing refreshes changed
sections' generated source inventories and clears their review attestation;
curated explanations survive. Generated text is never memory evidence. The
older whole-workspace guide also moves to SQLite. Existing vault guides are
imported once without making daemon startup depend on successful import.
Obsidian is an optional export. Architecture-guide invalidation includes indexed
source-content digests so behavior changes under unchanged module names count.

Every edit requires the version the editor read. Conflicts request a reload.
Archival preserves history and source stamps even if their sources are gone.
Source inspection reads canonical indexed data rather than arbitrary files.
Catalog captures apply the configured redactor.

## Coding-agent workflow

CLI, MCP and browser share product queries and mutation types. Task bundles use
recorded flows, contracts, source anchors, validated memory and prior successful
verification commands. Their text has a hard estimated-token budget, including
redaction and Unicode. MCP returns only that text, avoiding a second unbudgeted
copy of metadata. Missing knowledge and omitted records remain explicit.
Automatic injection's stable prefix and brain-off behavior remain unchanged
(ADR-014); these bundles are an explicit task-planning surface.

Change impact intersects supplied changed paths with source anchors, then walks
bounded dependencies. Producer writes/publications can affect downstream readers
and consumers. Direct intersections and possible impacts remain distinct;
absence of recorded impact is not proof of safety. Environment filtering occurs
before graph expansion. The analysis does not execute commands from diffs.

One completion call records actual verification, closes the episode, and queues
up to five lesson proposals. Failed verification stays on the task; only eligible
support is automatically attached to proposals. Retries deduplicate observations
and task-scoped proposals. The stages are separately durable, so partial failures
can be retried. Accepting a proposal passes through ordinary distillation and
stepwise evidence-gated promotion, with confidence capped at 0.50. Inactive or
rejected duplicates cannot silently reactivate. Similar text is a review hint;
only an explicit contradiction decision quarantines an older claim.

Optional expiry stops retrieval. Fresh supporting evidence and explicit renewed
expiry can renew a duplicate. Memory purge also scrubs dependent review content
and invalidates evidence whose artifact was purged. Tests, commit observations,
merge observations and explicit user acceptance remain distinct signals; a merge
or an uncorrected next prompt never invents acceptance.

Internal Claude synthesis disables hooks, tools, MCP and session persistence.
Documentation refreshes must not create engineering episodes or recursively call
the memory system. Existing cloud opt-in remains explicit.

## Limits

This catalog is scoped to one daemon. Existing cross-instance memory sync does
not automatically merge product catalogs. History is bounded to the latest 100
revisions per query; snapshots cap at 10,000 active records. Impact is conservative
and incomplete without recorded contracts. Source inventories do not infer
behavior from names. A fresh deployment probe or a human's first-correct-change
time must be measured, never inferred from these features being present.
