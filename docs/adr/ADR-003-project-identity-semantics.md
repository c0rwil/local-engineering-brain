# ADR-003: Project, fork, worktree, and component identity

- Status: accepted
- Date: 2026-08-28

## Context

Filesystem paths, branch names, and worktree directories are unstable. A moved repository must keep
its knowledge, multiple worktrees must share it, and a fork must not silently collapse into its
upstream project. Remote URLs may also contain credentials and may represent the same repository via
different transports.

## Decision

Resolve project identity in this order:

1. persisted `.leb/project.toml` UUID;
2. normalized canonical `origin` remote;
3. initial root commit only when no canonical remote exists;
4. an alias match or a newly generated UUID persisted to `.leb/project.toml`.

Remote normalization structurally removes credentials, URL queries/fragments, transport differences,
trailing `.git`, and a trailing slash. Relative/local remotes are canonicalized against the current
repository root so identical strings in unrelated directory trees cannot collide. Root commit
matching is never used to collapse two repositories that have different remotes. Callers can
explicitly link a newly registered fork through `parent_project_id`.

Repository discovery delegates to Git so nested repositories and worktrees use Git's nearest-root
semantics. Branch names are recorded as current state but never participate in permanent project
identity. Canonical root paths are evented as aliases. A nested `.leb/component.toml` marker provides
a logical monorepo scope without creating a separate project.

## Consequences

- Moving a project preserves identity because its metadata moves with it.
- Worktrees lacking the metadata file still converge through the canonical remote.
- Two offline clones without remotes may converge through their root commit; users who need them to
  remain distinct should persist an explicit project UUID before cloning.
- Registering a project writes `.leb/project.toml`; read-only source trees must be made writable or
  explicitly provisioned before registration.
