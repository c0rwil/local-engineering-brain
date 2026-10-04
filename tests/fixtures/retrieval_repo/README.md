# Synthetic retrieval fixture

A small multi-language repository used only by Engram's hermetic retrieval
regression test (`tests/retrieval_regression.rs`). The code is synthetic and is
never compiled or executed.

## Layout

- `service/` Rust settings loading and a process-wide environment guard with
  three call sites.
- `worker/` Python queue publishing with a retry policy.
- `web/` TypeScript session refresh and an API client.
