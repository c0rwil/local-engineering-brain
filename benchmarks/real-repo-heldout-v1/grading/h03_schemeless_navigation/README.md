# Private scheme-less navigation grader (h03, draft)

```sh
python3 -B benchmarks/real-repo-heldout-v1/grading/h03_schemeless_navigation/grade.py \
  --repo /private/tmp/candidate-grading-copy --output /private/tmp/h03-grade.json
```

Standard library only; Python 3.11 or newer. The grader imports the candidate's
own agent modules from the grading copy and drives each navigation path with a
fake browser page that records every `goto` target. Third-party packages
(patchright, llmapi, PIL, openai, httpx, fastapi, pydantic and the database
layer) are replaced by inert stubs. The light package initializers
(`agi_agents`, `agi_agents.shared`, `agi_agents.framework`,
`agi_agents.framework.base`) run for real, so package-level exports work; the
agent packages are namespaces because their initializers load model clients.
Environment variables are cleared before import; sockets and subprocesses are
denied by an audit hook. The grader does not modify the candidate copy.

Six checks, one per navigation path, plus a module-load check. Each path runs
the same eleven-input battery: bare hosts with and without paths, padded
whitespace, `localhost:3000`, `127.0.0.1:8080/health`, and six already-schemed
targets (`http`, `https`, `about:blank`, `file://`, `chrome://`, `data:`).

| Check | Entry point driven |
| --- | --- |
| `agent_server_first_navigation` | The framework router's `/user_message` handler for a new session, through the real `AgentServerState` loop (one step) |
| `cdp_base_goto_action` | `CDPBaseAgent._execute_actions` with a goto `Action` |
| `claude_goto_tool` | `agi_0_claude` `ClaudeToolExecutor.execute_tool("goto", …)` |
| `qwen_goto_tool` | `agi_0_qwen` `QwenToolExecutor.execute_tool("goto", …)` |
| `qwen1_goto_tool` | `agi_1_qwen` `QwenToolExecutor.execute_tool("goto", …, state)` |
| `cua_goto_action` | `agi_0_cua` `ActionParser` model text then `ActionExecutor.execute_action` |

Driving the router (not only the loop) accepts a fix at the route, the state
object or the loop. Known limits for review: a candidate that imports a
third-party package absent from the stubs fails to load; normalization done
only inside a concrete agent's `step()` is not credited for the shared CDP
action path. Exit codes: 0 pass, 1 behavioral failure (including candidate
import errors), 2 infrastructure failure.

## Calibration

Base `9adccea2`, reference `d3ab3e02`. Recorded in `preflight.json`:

| Control | Kind | Expected |
| --- | --- | --- |
| `base` | base | All six paths fail (schemed inputs already pass through). |
| `reference` | reference | All pass. |
| `tool-handlers-only` | mutant | Server first navigation and CDP base action fail. |
| `skip-cua-parser` | mutant | Only the CUA action fails. |
| `separator-heuristic` | mutant | Treats anything without `://` as scheme-less: every path fails on `about:blank`/`data:`. |
| `no-trim` | mutant | Prefixes without trimming: every path fails. |
| `urlsplit-allowlist` | alternative | Different helper (urlsplit plus scheme allowlist): passes. |
| `normalize-at-route` | alternative | Server normalization moved from the loop to the route: passes. |
