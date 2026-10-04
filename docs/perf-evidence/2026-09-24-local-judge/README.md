# Local evidence judge, 24 September 2026

Runtime: MTPLX 2.9.2, llguidance 1.8.0, Apple Silicon with 48 GiB unified memory.
Model: `Youssofal/Qwen3.5-4B-MTPLX-Optimized-Speed`, 4-bit, 2.6 GB download.
Served as `engram-judge` at `http://127.0.0.1:8011/v1/chat/completions`.
Model catalog: [MTPLX publisher](https://github.com/youssofal/MTPLX).

Configuration: thinking disabled, temperature zero, JSON output, 1,200 output-token
limit, 16,384-token context, serial requests. Retained prompt cache: 512 MiB total,
256 MiB per entry, two entries. Disk session caching and runtime request/flight
recording are disabled. Engram retains its own redacted decision and usage audits.

## Observed behavior

The [raw 15-case report](qwen35-4b.json) contains every verdict, exact validation
error, provider-reported token count, latency and prompt/fixture digest. There are
four supported claims and eleven unsupported, conflicting, stale or adversarial
cases. Three cases were added independently after the initial model trial.

- Twelve of fifteen responses met both the expected decision and validation rules.
- Thirteen responses had valid schema and citations where acceptance required them.
- Median request duration was 4.639 seconds. A process RSS sample during this run
  was 3,256,656 KiB (about 3.1 GiB); this is not a measured peak or a memory ceiling.
- One supported answer double-escaped a newline in its quote. Exact citation
  validation blocks this response.
- One correctly rejected injection case returned an empty claim. Field validation
  blocks this response.
- One stale-revision case was incorrectly accepted by the raw model and passed
  citation checks. The fixture deliberately labels old evidence eligible to probe
  model behavior. Production checks revision mismatch against the live checkout
  before inference and blocks this packet. The Rust freshness tests cover that gate.

This is a small synthetic smoke evaluation. It does not establish a real-world
accuracy rate, complete prompt-injection resistance or token/cost savings. In
particular, the raw model is not a trustworthy authority for source freshness.
Automatic review remains conditional on Engram's independent provenance, revision,
exact-citation, conflict and source-change checks. Invalid responses are audited
without approving a lesson. Seventeen focused Rust judge tests passed.

An exploratory reasoning-enabled comparison took 12.3 seconds on the newline case
and still produced an invalid citation, then 23.5 seconds on the injection case.
The comparison was stopped before completing the suite. These two observations
do not establish general performance; the installed service uses thinking disabled.

## Installed service

The local user LaunchAgent is `~/Library/LaunchAgents/local.engram.judge.plist`.
It starts at login and restarts after process exit. It binds only to loopback.
Stdout is discarded because the runtime prints completion previews there; stderr
goes to `~/.engram/logs/local-judge-error.log` with owner-only permissions.
It uses its own port and does not change the coding model on port 8010.

The default workspace has automatic local review enabled, two proposals per pass
every 300 seconds. All `engram doctor` checks passed. The first two live proposals
were deferred before inference because they lacked readable independent evidence;
they consumed no model tokens. The live usage ledger therefore still had zero
samples at that check. Eligible model calls will record provider-reported usage.
The [runtime snapshot](runtime.json) records the final service state and a separate
idle RSS observation. Neither this setup nor the synthetic trial establishes
savings in engineering tokens, tool calls or time.

Inspect or stop it:

```sh
launchctl print "gui/$(id -u)/local.engram.judge"
launchctl bootout "gui/$(id -u)/local.engram.judge"
```

Start it after stopping:

```sh
launchctl bootstrap "gui/$(id -u)" "$HOME/Library/LaunchAgents/local.engram.judge.plist"
launchctl kickstart "gui/$(id -u)/local.engram.judge"
```

Set `judge.enabled = false` and restart Engram before leaving the model stopped;
otherwise pending reviews consume their bounded runtime retries. To remove login
startup, unload the service and remove its plist. Model weights remain in the
MTPLX cache until explicitly removed.
