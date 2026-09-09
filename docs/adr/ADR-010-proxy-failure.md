# ADR-010: Proxy fail-open and fail-closed behavior

- Status: accepted
- Date: 2026-08-28

## Decision

The default is `fail_open`: if the daemon is unavailable, the proxy forwards the original semantic
request unchanged and marks the runtime response with `x-leb-degraded: context_unavailable`.
Malformed client requests still fail closed. Policy-sensitive use and controlled evaluation can set
`proxy.failure_mode = "fail_closed"`, which returns a clear 502 if scoped context cannot be
compiled. Explicit off mode without project scope bypasses the daemon and forwards unchanged.
Streaming and tool schemas are preserved in every path.
