# LEB — next-session handoff

Refreshed 2026-09-02. Read `CLAUDE.md` first (verified gotchas), then the
memory file's `leb-project-state` entries for the full chronology.

## Where the product stands

LEB is prod-shaped: 210 tests, CI green on macOS + Linux, all gates
(`fmt`/`clippy -D warnings`/`test`) enforced. Schema at migration 11.

- **Proven core**: evidence-gated memory + budgeted injection. Flagship
  numbers (docs/perf-evidence/): Opus fidelity 24/24 vs off 5/12; local
  wall-calibrated brain 10/10 vs 7/10 at −33% wall; real-repo recall
  11/11; compact injection default ON (parity both families).
- **Living encyclopedia**: browser embedded in the daemon at
  127.0.0.1:11436 — service map, clickable service modals (guide section,
  facts, links, memories, module diagrams), activity feed, filterable
  memories, architecture guide with dossier-digest staleness badge.
  Obsidian vault mirrors it at ~/.leb/vault.
- **Freshness machinery**: daemon watcher re-indexes on committed HEAD
  changes (30 s poll; never on dirty edits) and refreshes shared-truth
  views on origin advance (900 s). Origin/main views are SHADOW projects
  (`derived_view = true` metadata; internal.origin_view/view_of facts) so
  origin and local graphs coexist; shadows are excluded from every listing
  surface and record no aliases.
- **Cross-repo knowledge**: explicit links + auto-discovery (manifest path
  deps, submodules, hostname references against `hosting.*` facts);
  linked-repo memories inject relevance-gated with source labels, validated
  against the linked repo's own HEAD.
- **Synthesis**: `leb architect --claude` (opus via claude CLI) or
  `--model` (local runtime) writes the vault ARCHITECTURE.md with an
  embedded dossier digest.

## Live environment

Four real repos registered + linked: agi-backend-rust
(androidagent.agi.tech, GCP), agents (api.agi.tech), platform
(platform.agi.tech), mobile (client). All have seeded origin views.
Launchers: claude-mtplx (Speed), claude-mtplx-quality (Q8). MTPLX on
:8010, proxy on :11435, browser on :11436.

## Standing discipline

- Push as `c0rwil` (`gh auth switch --user c0rwil` — it flips externally).
- Benchmarks: caffeinate from background commands, MTPLX restart before
  >10-run matrices, warmup task after restart (no `timeout` on macOS zsh),
  walls ≥2700 s local, evidence into docs/perf-evidence/.
- Long-running launches must be nohup+disown (session-tied background
  tasks die with Claude Code).
- Never send private repos through cloud arms; conveyor fixture only.
- Brain-off control stays semantically untouched (spec P10).

## Open threads

1. ~~Preference distillation~~ DONE 2026-09-02: deterministic
   fail-then-pass mining, unit-tested, benchmarked (distilled-brain 4/4
   vs off 3/4, −41% wall; docs/perf-evidence/2026-09-02).
2. Route-overlap link proposals (served vs consumed paths) as a
   complement to hostname inference; LLM-assisted proposals could ride
   the same confirm-before-link pathway.
3. Memory provenance drill-down in the browser (claim → evidence chain).
4. Alias/projection GC as proper events if replay fidelity ever matters.
5. Vault/dossier could include origin-view module maps per service.
