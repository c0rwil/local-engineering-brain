#!/usr/bin/env python3
"""Calibrate the h03 grader against disposable historical exports.

python3 -B preflight.py --source-repo /path/to/agents --output preflight.json
"""
from pathlib import Path
import sys

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent / "_shared"))
sys.dont_write_bytecode = True
from preflight_common import Control, Edit, Task, main  # noqa: E402

A = "src/agi_agents"
SERVER = f"{A}/framework/agent_server_state.py"
CDP = f"{A}/framework/base/cdp_agent.py"
ROUTER = f"{A}/framework/router.py"
CUA = f"{A}/agi_0_cua/parser.py"
HELPER = f"{A}/shared/url_utils.py"
SITES = ("agent_server_first_navigation", "cdp_base_goto_action", "claude_goto_tool", "qwen_goto_tool",
         "qwen1_goto_tool", "cua_goto_action")
SERVER_SITE = "                url = normalize_url(url)\n                await self._page.goto(url)"
URLSPLIT_HELPER = '''"""URL helpers shared across agent navigation handlers."""

from urllib.parse import urlsplit

_KNOWN_SCHEMES = {
    "http", "https", "file", "about", "data", "chrome", "chrome-extension", "blob",
    "view-source", "javascript", "mailto", "tel", "ws", "wss", "ftp",
}


def normalize_url(url: str) -> str:
    """Default scheme-less navigation targets to https."""
    stripped = url.strip()
    if not stripped:
        return url
    if urlsplit(stripped).scheme.lower() in _KNOWN_SCHEMES:
        return stripped
    return "https://" + stripped
'''

CONTROLS = (
    Control("base", "base", "base", "fail", "Historical base: every navigation path passes bare hosts through.",
            required_failures=SITES),
    Control("reference", "reference", "reference", "pass", "Historical fix."),
    Control("tool-handlers-only", "mutant", "reference", "fail",
            "Normalizes the agents' goto tools but not the server's first navigation or the CDP base action.",
            edits=(Edit(SERVER, SERVER_SITE, "                await self._page.goto(url)"),
                   Edit(CDP, "                await page.goto(normalize_url(url))", "                await page.goto(url)")),
            required_failures=SITES[:2]),
    Control("skip-cua-parser", "mutant", "reference", "fail",
            "Misses the parser-based agent's goto action.",
            edits=(Edit(CUA, "            url = normalize_url(action.content)", "            url = action.content.strip()"),),
            required_failures=("cua_goto_action",)),
    Control("separator-heuristic", "mutant", "reference", "fail",
            "Treats any URL without '://' as scheme-less, which breaks about:blank and data: URLs.",
            edits=(Edit(HELPER, "    if _SCHEME_RE.match(stripped) or _SCHEMELESS_SCHEME_RE.match(stripped):",
                        "    if \"://\" in stripped:"),),
            required_failures=SITES),
    Control("no-trim", "mutant", "reference", "fail",
            "Prefixes the scheme without trimming surrounding whitespace.",
            edits=(Edit(HELPER, "    stripped = url.strip()", "    stripped = url"),),
            required_failures=SITES),
    Control("urlsplit-allowlist", "alternative", "reference", "pass",
            "Legitimate alternative helper: urlsplit plus an allowlist of real schemes.",
            new_files=((HELPER, URLSPLIT_HELPER),)),
    Control("normalize-at-route", "alternative", "reference", "pass",
            "Legitimate alternative: the server normalizes the start URL at its user-message route instead of in the loop.",
            edits=(Edit(SERVER, SERVER_SITE, "                await self._page.goto(url)"),
                   Edit(ROUTER, "from agi_agents.framework.sse import SSEEvent, SSEEventType\n",
                        "from agi_agents.framework.sse import SSEEvent, SSEEventType\n"
                        "from agi_agents.shared.url_utils import normalize_url\n"),
                   Edit(ROUTER, '                url=getattr(req, "start_url", None),\n',
                        '                url=normalize_url(getattr(req, "start_url", None) or "") or None,\n'))),
)

TASK = Task(
    task_id="h03_schemeless_navigation",
    base_commit="9adccea20038f89381f0c49efa25909dc4d76f57",
    reference_commit="d3ab3e0222244dc133b508153a65f8bbbfa24d8b",
    grader=HERE / "grade.py",
    preflight=Path(__file__).resolve(),
    private_files=(),
    expected_check_count=7,
    controls=CONTROLS,
    grader_timeout_seconds=600,
)

if __name__ == "__main__":
    sys.exit(main(TASK))
