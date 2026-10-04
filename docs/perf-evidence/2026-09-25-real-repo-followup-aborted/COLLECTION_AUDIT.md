# Aborted collection audit

This collection stopped after four coding attempts and one interrupted Rust preparation. All four patches passed their private behavioral graders. However, all 18 attempted Bash calls failed before execution because the generated sandbox profile exceeded the macOS process argument-size limit (E2BIG). There were zero successful shell calls. The agents could edit files but could not run their own shell checks or tests.

The terminal model results reported success and no permission denials, so terminal-only validation missed the tool failure. The frozen raw records, private grades, model usage and traces remain unchanged. Publication now applies a trace-hash-bound operational exclusion: four infrastructure/unscorable outcomes, zero eligible savings pairs. Complete reported usage is retained; it is not falsely relabeled partial solely because a tool failed.

The preliminary token, tool-call and timing interpretations are withdrawn. These attempts will not be retried inside this frozen collection or mixed with a repaired collection. The fifth attempt stopped during preparation before a model call. The stopped protocol fingerprint is `7dc7ae1795915103604bf01ea772ad7f08a4f070a3b70c90a19a0593859e14bf`.

The [runtime audit](runtime-verification.json) matches tool-use IDs to error results and retains trace hashes without publishing command or source payloads. It also checks all 24 original v1 traces: 372 Bash calls, 364 successes, eight other command errors, and zero E2BIG failures. Original v1 evidence and native tracking remain unchanged; its 416-value native verification still passes.

The repaired collector must prove that the real CLI can execute Bash, read and edit its active checkout, and cannot read protected canaries. A local deterministic provider can drive those checks without a paid model call. Shell spawn failures must automatically preserve the behavioral verdict, invalidate the attempt and stop collection before another attempt begins.

Raw stopped evidence: `/private/tmp/erp26-v2-data/`. No native import of this incomplete collection was performed.

The owner-only [archive verification](archive-verification.json) checks all 46,396 file and symlink entries and all four candidate trees. The compressed archive is 498,663,205 bytes; its SHA-256 is `fae69f12490ee024ef7ee526ab341a689ed8a7eab7f05730339f55bc0754b2bc`. The archive includes original frozen collector sources, all four traces and grades, source snapshots, calibration evidence, and the interrupted fifth preparation. Build/dependency caches are excluded. The verification proof is external to the archive.
