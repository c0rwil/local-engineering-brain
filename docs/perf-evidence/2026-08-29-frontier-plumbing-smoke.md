# Frontier-arm plumbing smoke — 2026-08-29

Isolated verification of the remote-runtime path before any paid benchmark:
debug-build `leb-proxy --allow-remote-runtime --no-local-only --runtime
https://api.anthropic.com` on a scratch state root, Claude Code 2.1.247
(bundled binary) with `ANTHROPIC_BASE_URL` pointed at the proxy, stored
claude.ai subscription login (no API key), model `claude-haiku-4-5`, trivial
prompts.

Findings:

1. reqwest was built with `default-features = false` and NO TLS backend, so
   every HTTPS runtime request failed with a 502 before send. Fixed by
   enabling the `rustls` feature (platform verifier). This never surfaced
   locally because all prior runtimes were loopback HTTP.
2. Off mode: exact passthrough works against the real API — stored-login
   Authorization survived the proxy, response correct, `forwarded: 3`.
3. Brain mode, one agentic session (tool use → multiple API calls):
   `injected: 4, session_cache_misses: 1, session_cache_hits: 3` — the
   injected block was byte-stable across the session, keyed by Claude Code's
   NATIVE `X-Claude-Code-Session-Id` header (verified present in the 2.1.247
   binary's default header set; no wrapper wiring needed). This is the
   prefix-stability property that lets provider/runtime prompt caches hold.
4. Auth accounting: this machine uses the Max subscription stored login;
   frontier runs are subscription-billed, not Console-key spend.

Total external traffic: 6 requests, trivial prompts, synthetic content only.
