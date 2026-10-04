#!/usr/bin/env python3
"""Private behavioral grader for h03 (standard library only).

Drives every agent navigation path in the candidate's own code with a fake
browser page that records what it was asked to open: the agent server's
first navigation through its user-message route, the CDP base agent's goto
action, and the goto tool or action of the agi_0_claude, agi_0_qwen,
agi_1_qwen and agi_0_cua agents. Third-party packages are replaced by inert
stubs; nothing is installed, launched or contacted. Run only against an
expendable grading copy.
"""
from __future__ import annotations

import asyncio
import contextlib
import enum
import importlib
import importlib.util
from pathlib import Path
import sys
from types import SimpleNamespace

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent / "_shared"))
sys.dont_write_bytecode = True
from py_harness import main, module, package  # noqa: E402

TASK_ID = "h03_schemeless_navigation"
ENVIRONMENT = {"ENV": "test", "ENABLE_TRACE_ANALYSIS": "false", "PYTHONDONTWRITEBYTECODE": "1"}

# (input, expected navigation target)
BATTERY = (
    ("example.com", "https://example.com"),
    ("www.example.com/path?q=1#frag", "https://www.example.com/path?q=1#frag"),
    ("  example.com  ", "https://example.com"),
    ("localhost:3000", "https://localhost:3000"),
    ("127.0.0.1:8080/health", "https://127.0.0.1:8080/health"),
    ("http://example.com/a", "http://example.com/a"),
    ("https://example.com", "https://example.com"),
    ("about:blank", "about:blank"),
    ("file:///tmp/page.html", "file:///tmp/page.html"),
    ("chrome://settings", "chrome://settings"),
    ("data:text/html,hello", "data:text/html,hello"),
)

# ---------------------------------------------------------------------------
# Inert third-party stubs
# ---------------------------------------------------------------------------


class _Inert:
    """Accepts any call or attribute access; used for objects the tests never inspect."""

    def __init__(self, *args, **kwargs):
        pass

    def __call__(self, *args, **kwargs):
        return _Inert()

    def __getattr__(self, name):
        if name.startswith("__"):
            raise AttributeError(name)
        return _Inert()

    def __enter__(self):
        return self

    def __exit__(self, *args):
        return False

    def __getitem__(self, item):
        return self

    def __or__(self, other):
        return self

    __ror__ = __or__


def _inert_attribute(name):
    if name.startswith("__"):
        raise AttributeError(name)
    return _Inert


def _permissive(name, **attrs):
    value = module(name, **attrs)
    value.__getattr__ = _inert_attribute
    return value


class _Observation:
    def __init__(self, *args, **kwargs):
        self.session = None
        self.id = "trace"
        self.timestamp = "0"

    def update(self, **kwargs):
        pass

    def __enter__(self):
        return self

    def __exit__(self, *args):
        return False


def _observe(*args, **kwargs):
    # Supports @observe, @observe(...), and observe("name", "kind", flag).
    if len(args) == 1 and callable(args[0]) and not kwargs:
        return args[0]
    return lambda function: function


class _BaseModel:
    model_config: dict = {}

    def __init__(self, **data):
        for key, value in data.items():
            setattr(self, key, value)

    def model_copy(self, update=None):
        copy = object.__new__(type(self))
        copy.__dict__.update(self.__dict__)
        copy.__dict__.update(update or {})
        return copy

    def model_dump(self, **kwargs):
        return dict(self.__dict__)

    @classmethod
    def model_validate(cls, data):
        return cls(**data)


def _field(default=None, *args, **kwargs):
    if "default_factory" in kwargs:
        return kwargs["default_factory"]()
    return None if default is Ellipsis else default


class _HTTPException(Exception):
    def __init__(self, status_code=500, detail=None, headers=None):
        super().__init__(detail)
        self.status_code, self.detail = status_code, detail


class _APIRouter:
    def __init__(self, *args, **kwargs):
        self.routes = {}

    def _register(self, method, path, **kwargs):
        def decorator(function):
            self.routes[(method, path)] = function
            return function
        return decorator

    def get(self, path, **kwargs):
        return self._register("GET", path, **kwargs)

    def post(self, path, **kwargs):
        return self._register("POST", path, **kwargs)

    def put(self, path, **kwargs):
        return self._register("PUT", path, **kwargs)

    def delete(self, path, **kwargs):
        return self._register("DELETE", path, **kwargs)

    def websocket(self, path, **kwargs):
        return self._register("WEBSOCKET", path, **kwargs)


class _MessageRoleMeta(type):
    def __getattr__(cls, name):
        if name.startswith("__"):
            raise AttributeError(name)
        return name.lower()


class _AgentMessageRole(metaclass=_MessageRoleMeta):
    pass


class _BaseAgent:
    def __init__(self, *args, **kwargs):
        pass


def install_stubs():
    _permissive("patchright")
    _permissive("patchright.async_api", Page=type("Page", (), {}))
    _permissive("llmapi", observe=_observe, observation=_Observation, trace=_Observation,
                update_trace=lambda **kwargs: None, Trace=_Observation)
    _permissive("PIL")
    _permissive("PIL.Image", Image=type("Image", (), {}))
    _permissive("PIL.ImageDraw")
    sys.modules["PIL"].Image = sys.modules["PIL.Image"]
    sys.modules["PIL"].ImageDraw = sys.modules["PIL.ImageDraw"]
    _permissive("agent_tools")
    _permissive("agent_tools.llm_on_page_text", llm_on_page_text=_Inert())
    _permissive("arena", BaseAgent=_BaseAgent, AgentBrowser=_Inert, AgentState=_Inert)
    _permissive("arena.image", Base64Image=type("Base64Image", (), {}))
    _permissive("clickmodels")
    _permissive("clickmodels.base", ClickModel=type("ClickModel", (), {}))
    _permissive("httpx")
    errors = {name: type(name, (Exception,), {}) for name in
              ("APIConnectionError", "APIStatusError", "APITimeoutError", "RateLimitError", "APIError")}
    _permissive("openai", **errors)
    _permissive("anthropic")
    _permissive("fastapi", HTTPException=_HTTPException, APIRouter=_APIRouter)
    _permissive("fastapi.responses", StreamingResponse=_Inert)
    _permissive("pydantic", BaseModel=_BaseModel, Field=_field, ConfigDict=dict)
    _permissive("db")
    _permissive("db.messages", add_agent_message=lambda *args, **kwargs: None)
    _permissive("db.models", AgentMessageRole=_AgentMessageRole)


def exec_package(name, directory):
    spec = importlib.util.spec_from_file_location(name, directory / "__init__.py",
                                                  submodule_search_locations=[str(directory)])
    value = importlib.util.module_from_spec(spec)
    sys.modules[name] = value
    if "." in name:
        parent, child = name.rsplit(".", 1)
        setattr(sys.modules[parent], child, value)
    spec.loader.exec_module(value)
    return value


def load_candidate(repo: Path):
    src = repo / "src"
    install_stubs()
    # Light packages run their real initializers so package-level exports work.
    exec_package("agi_agents", src / "agi_agents")
    exec_package("agi_agents.shared", src / "agi_agents/shared")
    exec_package("agi_agents.framework", src / "agi_agents/framework")
    exec_package("agi_agents.framework.base", src / "agi_agents/framework/base")
    # Agent packages import their full model clients on init; load modules directly.
    for name in ("agi_0_claude", "agi_0_qwen", "agi_1_qwen", "agi_0_cua"):
        package(f"agi_agents.{name}", src / "agi_agents" / name)
    return SimpleNamespace(src=src)


# ---------------------------------------------------------------------------
# Fakes
# ---------------------------------------------------------------------------


class FakePage:
    def __init__(self):
        self.visits = []
        self.url = "about:blank"

    async def goto(self, url, *args, **kwargs):
        self.visits.append(url)
        self.url = url

    async def wait_for_load_state(self, *args, **kwargs):
        return None

    async def wait_for_timeout(self, *args, **kwargs):
        return None


def state():
    return SimpleNamespace(messages=[], images=[], step=0, url=None, finished=False, awaiting_user=False)


def check_battery(site, navigate):
    failures = []
    for raw, expected in BATTERY:
        visits = asyncio.run(navigate(raw))
        if visits != [expected]:
            failures.append(f"{raw!r} -> {visits!r}, expected [{expected!r}]")
    assert not failures, f"{site}: " + "; ".join(failures)


# ---------------------------------------------------------------------------
# Navigation paths
# ---------------------------------------------------------------------------


def claude_goto_tool(context):
    tools = importlib.import_module("agi_agents.agi_0_claude.tools")

    async def navigate(url):
        page = FakePage()
        executor = tools.ClaudeToolExecutor(page, None, state(), 1000, 1000)
        await executor.execute_tool("goto", {"url": url}, None)
        return page.visits

    check_battery("agi_0_claude goto tool", navigate)


def qwen_goto_tool(context):
    tools = importlib.import_module("agi_agents.agi_0_qwen.tools")

    async def navigate(url):
        page = FakePage()
        executor = tools.QwenToolExecutor(page, None, state(), 1000, 1000)
        await executor.execute_tool("goto", {"url": url})
        return page.visits

    check_battery("agi_0_qwen goto tool", navigate)


def qwen1_goto_tool(context):
    tools = importlib.import_module("agi_agents.agi_1_qwen.tools")

    async def navigate(url):
        page = FakePage()
        current = state()
        executor = tools.QwenToolExecutor(page, None, current, 1000, 1000)
        await executor.execute_tool("goto", {"url": url}, current)
        return page.visits

    check_battery("agi_1_qwen goto tool", navigate)


def cua_goto_action(context):
    parser = importlib.import_module("agi_agents.agi_0_cua.parser")

    async def navigate(url):
        page = FakePage()
        action = parser.ActionParser().parse_single_action(f"goto(url='{url}')")
        assert action is not None and action.action_type == "goto", f"model text was not parsed as goto: {action!r}"
        executor = parser.ActionExecutor(page, pointer_model=None)
        await executor.execute_action(action, state())
        return page.visits

    check_battery("agi_0_cua goto action", navigate)


def cdp_base_goto_action(context):
    cdp = importlib.import_module("agi_agents.framework.base.cdp_agent")
    types_module = importlib.import_module("agi_agents.framework.types")

    class Probe(cdp.CDPBaseAgent):
        async def step(self, step_context, agent_state):
            raise AssertionError("the probe never steps")

    async def navigate(url):
        page = FakePage()
        result = types_module.AgentStepResult(actions=[types_module.Action(type="goto", parameters={"url": url})])
        await Probe()._execute_actions(SimpleNamespace(page=page), state(), result, None)
        return page.visits

    check_battery("CDP base agent goto action", navigate)


def agent_server_first_navigation(context):
    server_state = importlib.import_module("agi_agents.framework.agent_server_state")
    router_module = importlib.import_module("agi_agents.framework.router")
    config_module = importlib.import_module("agi_agents.config")

    class Config(config_module.BaseAgentConfig):
        pass

    async def navigate(url):
        page = FakePage()
        config = Config(session_id="heldout-session", agent_session_type="managed-cdp", max_steps=1,
                        step_delay_seconds=0.0, cdp_url="", user_id=None, user_email=None)
        spec = SimpleNamespace(service_name="heldout", trace_name="heldout", session_id_factory=None,
                               requires_start_url=False, user_message_model=object,
                               create_initial_state=lambda cfg, start_url: state())
        server = server_state.AgentServerState(config, spec)
        server._browser = object()
        server._page = page

        class Llmapi:
            @staticmethod
            def trace(**kwargs):
                return _Observation()

            @staticmethod
            def observation(**kwargs):
                return _Observation()

            @staticmethod
            def update_trace(**kwargs):
                return None

        class Agent:
            async def step_cdp(self, browser, agent_state):
                agent_state.finished = True
                return agent_state

        class Browser:
            async def screenshot(self):
                return object()

        async def no_events(*args, **kwargs):
            return {}

        server._ensure_llmapi_session = lambda: Llmapi
        server._ensure_agent = lambda: Agent()
        server._ensure_browser = lambda: Browser()
        server._send_step_events = no_events
        server._broadcast_event = no_events
        server.snapshot = lambda: {}
        service = SimpleNamespace(spec=spec, _config=config, state=server)
        routes = router_module.build_agent_router(service).routes
        request = SimpleNamespace(content="Open the page", start_url=url, max_steps=1, step_delay_seconds=0.0,
                                  session_id=None, agent_name=None, user_id=None)
        await routes[("POST", "/user_message")](request)
        task = server._agent_task
        assert task is not None, "the user-message route did not start the agent loop"
        await asyncio.wait_for(task, timeout=10)
        return page.visits

    check_battery("agent server first navigation", navigate)


CHECKS = (
    ("agent_server_first_navigation", agent_server_first_navigation),
    ("cdp_base_goto_action", cdp_base_goto_action),
    ("claude_goto_tool", claude_goto_tool),
    ("qwen_goto_tool", qwen_goto_tool),
    ("qwen1_goto_tool", qwen1_goto_tool),
    ("cua_goto_action", cua_goto_action),
)

if __name__ == "__main__":
    sys.exit(main(TASK_ID, load_candidate, CHECKS, ENVIRONMENT))
