"""Codex CLI arm for the real-repository replay runner.

Each attempt runs `codex exec` headless with a fresh, private CODEX_HOME and a
private HOME for the Codex process, so no user configuration, credentials file,
skills, memories, hooks or MCP servers leak in. Both arms share one generated
configuration; only the Brain arm enables Codex hooks (installed with
`engram hooks codex --install`, verified, then trusted in that isolated home)
and registers the engram-mcp server. Tool commands run under a Codex
permission profile that mirrors the Claude arm's sandbox: private roots are
unreadable, writes are limited to the checkout and the trial scratch
directory, and network access is disabled.

Credentials come only from the CODEX_API_KEY environment variable, which Codex
documents for `codex exec`. Its value is passed to the Codex process and is
never logged or written to disk.
"""

from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import shlex
import shutil
import signal
import subprocess
import tempfile
import time

import run_real_repo_pilot as runner
from real_repo_metrics import parse_codex_trace

AUTH_ENV = "CODEX_API_KEY"
PROFILE = "erp26"
SESSION_GLOB = "rollout-*.jsonl"
# Desktop builds bundle the CLI; it is usually not on PATH.
BUNDLE_IDS = ("com.openai.codex", "com.openai.chat")
APP_CANDIDATES = (
    Path("/Applications/Codex.app/Contents/Resources/codex"),
    Path("/Applications/ChatGPT.app/Contents/Resources/codex"),
    Path.home() / "Applications/Codex.app/Contents/Resources/codex",
    Path.home() / "Applications/ChatGPT.app/Contents/Resources/codex",
)
RUNTIME_CACHE = Path.home() / ".cache/codex-runtimes"
# Identical in both arms. Names come from `codex features list` of the pinned
# release; --strict-config rejects keys that release does not recognize.
# Unbounded connection retries would turn a provider outage into a wall-clock
# timeout, which scores as an agent failure; bounded retries end in turn.failed.
DISABLED_FEATURES = (
    "memories", "multi_agent", "apps", "plugins", "remote_plugin", "browser_use",
    "browser_use_external", "computer_use", "image_generation", "shell_snapshot",
    "skill_search", "tool_suggest", "guardian_approval", "unbounded_connection_retries",
)
# Hook event -> Engram subcommand, as written by `engram hooks codex --install`.
HOOK_EVENTS = {
    "SessionStart": "session-start", "UserPromptSubmit": "prompt", "PreToolUse": "pretool",
    "PostToolUse": "bash", "Stop": "stop", "SessionEnd": "session-end",
}
# Never visible to tool commands, whatever the arm.
EXCLUDED_ENV = ("CODEX_API_KEY", "OPENAI_API_KEY", "CODEX_ACCESS_TOKEN", "ENGRAM_DB_PATH", "ENGRAM_SOCKET_PATH")
# A hook process that outlives Codex (for example a SessionEnd hook) must
# finish while the arm's daemon still runs: a hook that finds no daemon loses
# its record and auto-starts a replacement daemon on the trial's state.
HOOK_DRAIN_SECONDS = 45
HOOK_KILL_GRACE_SECONDS = 5


def _signal_group(group: int, signum: int) -> bool:
    try:
        os.killpg(group, signum)
        return True
    except ProcessLookupError:
        return False


def _drain_group(group: int, seconds: float) -> bool:
    """Wait for every process in the group to exit; stop stragglers at the deadline."""
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        if not _signal_group(group, 0):
            return True
        time.sleep(0.2)
    _signal_group(group, signal.SIGTERM)
    time.sleep(HOOK_KILL_GRACE_SECONDS)
    _signal_group(group, signal.SIGKILL)
    return False


def run_codex(argv, *, cwd, env, prompt: str, stdout, stderr, timeout: float) -> dict:
    """Run `codex exec` in its own process group, then drain hook stragglers.

    Agent wall time ends when Codex exits; the drain is reported separately.
    """
    started = time.monotonic()
    timed_out = False
    proc = subprocess.Popen(list(map(str, argv)), cwd=cwd, env=env, stdin=subprocess.PIPE, stdout=stdout,
                            stderr=stderr, text=True, start_new_session=True)
    try:
        proc.communicate(prompt, timeout=timeout)
    except subprocess.TimeoutExpired:
        timed_out = True
        _signal_group(proc.pid, signal.SIGTERM)
        try:
            proc.wait(10)
        except subprocess.TimeoutExpired:
            _signal_group(proc.pid, signal.SIGKILL)
            proc.wait()
    wall = round(time.monotonic() - started, 3)
    drain_started = time.monotonic()
    drained = _drain_group(proc.pid, HOOK_DRAIN_SECONDS)
    return {"agent_wall_seconds": wall, "agent_exit_code": proc.returncode, "agent_timed_out": timed_out,
            "agent_hook_drain_seconds": round(time.monotonic() - drain_started, 3),
            "agent_hook_stragglers_stopped": not drained}


# Timeout Codex hashes for a command hook that sets none.
DEFAULT_HOOK_TIMEOUT_SECS = 600


def event_snake_case(event: str) -> str:
    """`SessionStart` -> `session_start`, `PreToolUse` -> `pre_tool_use`."""
    return "".join(("_" if index else "") + character.lower() if "A" <= character <= "Z" else character
                   for index, character in enumerate(event))


def hook_trust_key(hooks_file: Path, event: str, group: int, handler: int) -> str:
    """The `hooks.state` key Codex uses for one handler of a user-level hooks.json."""
    return f"{hooks_file}:{event}:{group}:{handler}"


def hook_trust_hash(event: str, matcher, handler) -> str | None:
    """The `sha256:<hex>` trust hash Codex computes for one command handler.

    Port of src/codex_trust.rs (`trust_hash`), whose test vectors came from
    the pinned Codex CLI's app-server `hooks/list`: compact JSON with sorted
    keys of `event_name`, a one-handler `hooks` array (`type`, `command`,
    `timeout` defaulting to 600, `async` defaulting to false, and
    `statusMessage`/`additionalContextLimit` when set) and a non-empty
    `matcher`. `commandWindows` is not hashed on macOS. Returns None for
    handlers this rule does not cover.
    """
    if not isinstance(handler, dict) or handler.get("type") != "command" or "command" not in handler:
        return None
    timeout = handler.get("timeout")
    if timeout is None:
        timeout = DEFAULT_HOOK_TIMEOUT_SECS
    elif isinstance(timeout, bool) or not isinstance(timeout, int) or timeout < 0:
        return None
    asynchronous = handler.get("async")
    identity = {"type": "command", "command": handler["command"], "timeout": timeout,
                "async": asynchronous if isinstance(asynchronous, bool) else False}
    for optional in ("statusMessage", "additionalContextLimit"):
        if handler.get(optional) is not None:
            identity[optional] = handler[optional]
    group = {"event_name": event, "hooks": [identity]}
    if isinstance(matcher, str) and matcher:
        group["matcher"] = matcher
    canonical = json.dumps(group, sort_keys=True, separators=(",", ":"), ensure_ascii=False)
    return "sha256:" + hashlib.sha256(canonical.encode()).hexdigest()


def codex_hook_trust_entries(codex_home: Path) -> dict[str, str]:
    """Trust every handler in an isolated benchmark CODEX_HOME's hooks.json.

    Returns `{key: trusted_hash}` for `[hooks.state."<key>"]` tables, which
    Codex checks before running a handler. Refuses the user's real ~/.codex
    and any home outside a runner-owned active root: trust is written only
    for hooks this runner installed and verified.
    """
    home = Path(codex_home).resolve()
    if home == (Path.home() / ".codex").resolve() or not (home.parent / ".pilot-owned").is_file():
        raise ValueError("Hook trust is written only into an isolated, runner-owned CODEX_HOME, never ~/.codex")
    hooks_file = home / "hooks.json"
    document = json.loads(hooks_file.read_text())
    entries = {}
    for event, groups in document["hooks"].items():
        snake = event_snake_case(event)
        for group_index, group in enumerate(groups):
            for handler_index, handler in enumerate(group.get("hooks", [])):
                digest = hook_trust_hash(snake, group.get("matcher"), handler)
                if digest is None:
                    raise ValueError(f"Codex hook {event} is not a command handler this runner can trust")
                entries[hook_trust_key(hooks_file, snake, group_index, handler_index)] = digest
    return entries


def _executable(path: Path) -> bool:
    return path.is_file() and os.access(path, os.X_OK)


def find_codex(explicit: Path | None = None) -> Path:
    """Locate the Codex CLI: explicit path, PATH, runtime cache, app bundles, Spotlight."""
    if explicit is not None:
        path = Path(explicit).expanduser().resolve(strict=True)
        if not _executable(path):
            raise ValueError(f"Codex CLI is not executable: {path}")
        return path
    candidates = []
    if found := shutil.which("codex"):
        candidates.append(Path(found))
    if RUNTIME_CACHE.is_dir():
        candidates += sorted(path for path in RUNTIME_CACHE.rglob("codex") if path.is_file())
    candidates += APP_CANDIDATES
    for bundle_id in BUNDLE_IDS:
        try:
            result = subprocess.run(["mdfind", f"kMDItemCFBundleIdentifier == '{bundle_id}'"],
                                    capture_output=True, text=True, timeout=10)
        except (OSError, subprocess.SubprocessError):
            continue
        candidates += [Path(line) / "Contents/Resources/codex" for line in result.stdout.splitlines() if line]
    for candidate in candidates:
        if _executable(candidate):
            return candidate.resolve()
    raise FileNotFoundError("Codex CLI not found; pass --codex /path/to/codex "
                            "(Codex Desktop bundles it as <App>.app/Contents/Resources/codex)")


def version(executable: Path) -> str:
    """`codex --version` with a throwaway CODEX_HOME and HOME; no user state is read."""
    with tempfile.TemporaryDirectory(prefix="codex-version-") as home:
        env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "HOME": home, "CODEX_HOME": home}
        return runner.command([executable, "--version"], env=env).strip()


def config_template_sha256(model: str, effort: str) -> str:
    """Digest of both arms' generated settings with placeholder paths."""
    active = Path("/ERP26_ACTIVE")
    texts = [config_text(arm=arm, model=model, effort=effort, work=active / "work", denied=[Path("/ERP26_DENIED")],
                         globs=["/ERP26_GLOB"], state=active / "state",
                         engram_mcp=Path("/ERP26_RELEASE/bin/engram-mcp") if arm == "brain" else None)
             for arm in ("off", "brain")]
    return hashlib.sha256("\n".join(texts).encode()).hexdigest()


def toml_string(value) -> str:
    # JSON string escapes are valid TOML basic-string escapes.
    return json.dumps(str(value))


def private_paths(work: Path, data: Path, active: Path, state: Path, codex_home: Path, user_home: Path):
    """Absolute roots tool commands must not read or write, and deny globs.

    Mirrors the Claude arm's compact sandbox: evidence and fixtures, the
    Engram checkout with private graders, source repositories, user agent
    state, this trial's Engram state, and other trial roots.
    """
    home = Path.home()
    exact = [data.resolve(), runner.ROOT.resolve(), runner.SOURCE_ROOT.resolve(), home / ".engram",
             home / ".codex", home / ".claude", state.resolve(), codex_home.resolve(), user_home.resolve()]
    exact += sorted(path.resolve() for path in Path("/private/tmp").glob("erp26work-*")
                    if path.resolve() != active.resolve())
    # Seatbelt evaluates globs at access time, so later trial roots stay denied.
    globs = [f"/private/tmp/{prefix}{suffix}" for prefix in ("engram-", "erp26-", "claude-")
             for suffix in ("*", "*/**")]
    return exact, globs


def config_text(*, arm: str, model: str, effort: str, work: Path, denied: list, globs: list,
                state: Path, engram_mcp: Path | None, trusted_hooks: dict | None = None) -> str:
    """Generate one arm's config.toml. Arms differ only in hooks, hook trust and MCP."""
    lines = [
        "# Generated per trial by scripts/real_repo_codex.py. Both arms share every setting",
        "# except the Engram hooks feature, their trust records and the engram MCP server.",
        f"model = {toml_string(model)}",
        f"model_reasoning_effort = {toml_string(effort)}",
        'approval_policy = "never"',
        # Tool commands otherwise run as a login shell that sources user dotfiles.
        "allow_login_shell = false",
        f"default_permissions = {toml_string(PROFILE)}",
        'web_search = "disabled"',
        "check_for_update_on_startup = false",
        'cli_auth_credentials_store = "file"',
        "",
        "[history]",
        'persistence = "none"',
        "",
        "[features]",
        f"hooks = {'true' if arm == 'brain' else 'false'}",
        *(f"{name} = false" for name in DISABLED_FEATURES),
        "",
        "[shell_environment_policy]",
        'inherit = "all"',
        "ignore_default_excludes = true",
        f"exclude = [{', '.join(map(toml_string, EXCLUDED_ENV))}]",
        # The Codex process uses a private HOME; tool commands keep the real
        # one so toolchains resolve as they do in the Claude arm.
        f"set = {{ HOME = {toml_string(Path.home())} }}",
        "",
        f"[permissions.{PROFILE}]",
        'description = "Real-repo replay: checkout and scratch writable, private roots denied, no network"',
        'extends = ":workspace"',
        "",
        f"[permissions.{PROFILE}.filesystem]",
        '":slash_tmp" = "read"',
        f'{toml_string(work / ".engram")} = "read"',
        *(f"{toml_string(path)} = \"deny\"" for path in denied),
        *(f"{toml_string(pattern)} = \"deny\"" for pattern in globs),
        "",
        f"[permissions.{PROFILE}.network]",
        "enabled = false",
        "",
    ]
    if arm == "brain":
        args = ["--workspace", "default", "--db", state / "brain.sqlite", "--socket", state / "d.sock", "--no-autostart"]
        lines += [
            "[mcp_servers.engram]",
            f"command = {toml_string(engram_mcp)}",
            f"args = [{', '.join(map(toml_string, args))}]",
            "startup_timeout_sec = 30",
            "tool_timeout_sec = 120",
            "",
        ]
    for key, digest in sorted((trusted_hooks or {}).items()):
        lines += [f"[hooks.state.{toml_string(key)}]", f"trusted_hash = {toml_string(digest)}", ""]
    return "\n".join(lines)


def verify_hooks(document: dict, engram: Path) -> dict:
    """Accept only the Engram handlers the runner installed, for the pinned binary."""
    if not isinstance(document, dict) or set(document) != {"hooks"} or not isinstance(document["hooks"], dict):
        raise ValueError("Codex hooks.json must contain only the installed hooks table")
    events = document["hooks"]
    if set(events) != set(HOOK_EVENTS):
        raise ValueError(f"Unexpected Codex hook events: {sorted(events)}")
    summary = {}
    for event, groups in events.items():
        handlers = [handler for group in groups for handler in group.get("hooks", [])]
        if len(groups) != 1 or len(handlers) != 1:
            raise ValueError(f"Codex hook event {event} must have exactly one handler")
        handler = handlers[0]
        argv = shlex.split(handler.get("command", ""))
        if handler.get("type") != "command" or argv != [str(engram), "hook", HOOK_EVENTS[event], "--codex"]:
            raise ValueError(f"Codex hook {event} does not run the pinned Engram binary")
        summary[event] = {"command": argv[1:], "timeout": handler.get("timeout"), "matcher": groups[0].get("matcher")}
    return summary


def install_hooks(codex_home: Path, hook_home: Path) -> dict:
    """Run the real installer into this CODEX_HOME and verify what it wrote.

    The installer prefers ~/.local/bin/engram; a private HOME makes it record
    the pinned release binary instead.
    """
    hook_home.mkdir(mode=0o700, exist_ok=True)
    env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "HOME": str(hook_home), "CODEX_HOME": str(codex_home)}
    runner.command([runner.ENGRAM, "hooks", "codex", "--install"], env=env)
    return verify_hooks(json.loads((codex_home / "hooks.json").read_text()), runner.ENGRAM)


def validate_checkout(work: Path):
    """Project-level Codex configuration would change both arms; refuse it."""
    if (work / ".codex").exists() or (work / ".codex").is_symlink():
        raise ValueError("Fixture checkout contains .codex; project hooks or config would bypass arm controls")


def session_files(codex_home: Path) -> list[Path]:
    root = codex_home / "sessions"
    return sorted(root.rglob(SESSION_GLOB)) if root.is_dir() else []


class CodexAgent:
    """Agent adapter consumed by run_real_repo_pilot.run_trial."""

    name = "codex"

    def __init__(self, executable: Path, model: str, effort: str = "high", hook_trust: str = "persisted"):
        if not model:
            raise ValueError("A pinned Codex model is required")
        if hook_trust not in ("bypass", "persisted"):
            raise ValueError("hook_trust must be 'bypass' or 'persisted'")
        self.executable = Path(executable)
        self.model = model
        self.effort = effort
        self.hook_trust = hook_trust

    def limits(self, args) -> dict:
        # Codex exec has no USD budget cap; the wall limit applies to both arms.
        return {"max_budget_usd_estimate": None, "model": self.model, "effort": self.effort, "agent": self.name}

    def require_credentials(self):
        if not os.environ.get(AUTH_ENV):
            raise ValueError(f"Set {AUTH_ENV} in the runner's environment; the isolated CODEX_HOME has no login")

    def prepare(self, *, trial: Path, work: Path, state: Path, arm: str, args) -> dict:
        validate_checkout(work)
        codex_home, user_home = args.active / "codex-home", args.active / "codex-user-home"
        codex_home.mkdir(mode=0o700)
        user_home.mkdir(mode=0o700)
        hooks = trusted = None
        if arm == "brain":
            hooks = install_hooks(codex_home, args.active / "hook-install-home")
            if self.hook_trust == "persisted":
                # Same record `/hooks` review writes, for handlers verify_hooks accepted.
                trusted = codex_hook_trust_entries(codex_home)
            shutil.copy2(codex_home / "hooks.json", trial / "codex-hooks.json")
        denied, globs = private_paths(work, args.data, args.active, state, codex_home, user_home)
        text = config_text(arm=arm, model=self.model, effort=self.effort, work=work, denied=denied, globs=globs,
                           state=state, engram_mcp=runner.ENGRAM.parent / "engram-mcp" if arm == "brain" else None,
                           trusted_hooks=trusted)
        (codex_home / "config.toml").write_text(text)
        (trial / "codex-config.toml").write_text(text)
        return {"arm": arm, "trial": trial, "work": work, "state": state, "codex_home": codex_home,
                "user_home": user_home, "scratch": args.active / "scratch", "hooks": hooks}

    def command(self, prepared: dict) -> list[str]:
        argv = [str(self.executable), "exec", "--json", "--strict-config", "--color", "never", "--ignore-rules",
                "--cd", str(prepared["work"]), "--output-last-message", str(prepared["trial"] / "last-message.txt")]
        if prepared["arm"] == "brain" and self.hook_trust == "bypass":
            # Alternative to persisted trust, documented for automation that
            # vets hook sources outside Codex; verify_hooks accepted exactly
            # the installed Engram handlers.
            argv.append("--dangerously-bypass-hook-trust")
        return [*argv, "-"]

    def environment(self, prepared: dict, *, compact: bool) -> dict:
        env = {key: value for key, value in runner.coding_env(prepared["work"], compact=compact).items()
               if not key.startswith("CLAUDE_CODE_")}
        env.update({"HOME": str(prepared["user_home"]), "CODEX_HOME": str(prepared["codex_home"])})
        if os.environ.get(AUTH_ENV):
            env[AUTH_ENV] = os.environ[AUTH_ENV]
        if prepared["arm"] == "brain":
            # Hooks also find these paths through the checkout's .engram/config.toml.
            env.update({"ENGRAM_DB_PATH": str(prepared["state"] / "brain.sqlite"),
                        "ENGRAM_SOCKET_PATH": str(prepared["state"] / "d.sock")})
        return env

    def execute(self, prepared: dict, prompt: str, args) -> dict:
        trial = prepared["trial"]
        prepared["scratch"].mkdir(exist_ok=True)
        with (trial / "trace.jsonl").open("w") as out, (trial / "stderr.log").open("w") as err:
            return run_codex(self.command(prepared), cwd=prepared["work"],
                             env=self.environment(prepared, compact=getattr(args, "compact_settings", False)),
                             prompt=prompt, stdout=out, stderr=err, timeout=args.agent_seconds)

    def metrics(self, prepared: dict) -> dict:
        return parse_codex_trace(prepared["trial"] / "trace.jsonl", session_files(prepared["codex_home"]))

    def record_flags(self, metrics: dict) -> dict:
        # Reset or missing counters leave a lower bound, never a savings claim.
        return {"usage_may_be_partial": True} if metrics.get("usage_complete") is False else {}

    def models(self, metrics: dict) -> list:
        return list(metrics.get("models") or [])

    def preserve(self, prepared: dict):
        """Keep sessions, config and hooks as private evidence, never credentials."""
        for home, name in ((prepared["codex_home"], "codex-home"), (prepared["user_home"], "codex-user-home")):
            target = prepared["trial"] / name
            if target.exists() or not home.is_dir():
                continue
            (home / "auth.json").unlink(missing_ok=True)
            runner.clone(home, target)
