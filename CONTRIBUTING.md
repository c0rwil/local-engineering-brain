# Contributing

LEB is a local-first daemon. Almost everything you need runs on your own
machine, and CI runs exactly the same commands you do.

## Development setup

Requirements: macOS or Linux, Git, and `shasum`. macOS on Apple Silicon is the
primary target; Linux is supported and covered in CI.

Install the toolchain with [rustup](https://rustup.rs). The pinned version is
**1.96.0**, declared in [`rust-toolchain.toml`](rust-toolchain.toml); rustup
reads that file automatically, so a plain `cargo build` in this repo already
uses the right compiler. The same version is pinned in
`.github/workflows/ci.yml` as `RUST_VERSION` — when you bump one, bump
`rust-toolchain.toml`, the workflow, and the number in this paragraph together.

```bash
git clone <repo> && cd brain
cargo build            # rustup installs 1.96.0 on first use
./scripts/install.sh   # optional: build and install leb, lebd, leb-proxy, leb-mcp
```

`rusqlite` is built with the `bundled` feature, so SQLite compiles from source
and you need a working C compiler (Xcode command line tools on macOS, `build-essential` on Debian/Ubuntu). No database service, no Redis, no network
dependency.

## Quality gates

These four commands are the gate. Run them before you open a pull request; CI
runs them verbatim and nothing merges until they pass.

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
cargo build --release --locked
```

**Clippy warnings are hard errors.** `Cargo.toml` sets `clippy::all` and
`clippy::pedantic` to `warn`, and `-D warnings` promotes every one of them to a
failure. Fix the lint. If a lint is genuinely wrong for the code in question,
`#[allow(...)]` it at the narrowest possible scope with a comment saying why —
do not relax the workspace lint table, and do not leave a bare `allow`.

`unsafe_code` is `forbid` at the crate level. There is no exception process.

### Performance tests

`tests/performance.rs` asserts latency, RSS, and token-budget ceilings. Those
assertions are real gates on the target hardware but are noisy on shared CI
runners, so CI skips the two timing-sensitive test functions in the required
`test` job and measures them separately in an informational `perf` job that is
allowed to fail. Locally, run them in release mode where the bounds mean
something:

```bash
cargo test --test performance --release -- --nocapture
```

If you change anything on the context-compilation or daemon-concurrency path,
run that command and quote the numbers in your pull request.

### Dependency policy

New or updated dependencies are checked by `cargo deny` (see
[`deny.toml`](deny.toml) and `.github/workflows/audit.yml`):

- crates.io only — no git dependencies, no alternate registries;
- permissive licenses only, from an explicit allowlist. Adding a crate with a
  license that is not already listed requires adding it to `deny.toml` in the
  same commit, with a comment;
- security advisories are fatal; unmaintained advisories are warnings.

Prefer not adding a dependency at all. This is a local-first product whose
selling point is that it is one binary and one SQLite file.

## Writing a migration

Schema changes are append-only, and the running daemon verifies that. Every
applied migration's SHA-256 is recorded in `schema_migrations`, and a mismatch
makes the database refuse to open with `migration N checksum mismatch`.

**Never edit a migration that has been applied anywhere** — not to fix a typo,
not to add a column you forgot. Write the next one instead.

To add a migration:

1. Create `src/storage/migrations/NNNN_short_name.sql`, where `NNNN` is the
   next sequential four-digit number with no gaps (currently `0005`).
2. Register it in the `MIGRATIONS` table at the top of
   [`src/storage/mod.rs`](src/storage/mod.rs) as
   `(N, "short_name", include_str!("migrations/NNNN_short_name.sql"))`. The
   version and the name must match the filename; the loader compares both.
3. Bump `LATEST_MIGRATION_VERSION` in the same file to `N`.
4. Add or extend a test covering the new schema. Migration and downgrade
   protection is already tested — an older binary must refuse a newer database
   rather than corrupt it, so keep that path honest.

Migrations run inside one transaction via `execute_batch`, so a failing
migration leaves the database untouched. Write them so they are safe on a
database that already holds real data: prefer additive `CREATE TABLE` /
`ALTER TABLE ... ADD COLUMN` over destructive rewrites, and backfill in the
same file rather than in application code.

## Commits and pull requests

Use [Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/):

```
<type>(<optional scope>): <imperative summary>

<optional body explaining why>
```

Types in use: `feat`, `fix`, `perf`, `refactor`, `docs`, `test`, `build`,
`ci`, `chore`. Breaking changes take a `!` before the colon
(`feat(storage)!: ...`) and a `BREAKING CHANGE:` footer.

Write the summary in the imperative mood, under ~72 characters, and describe
the effect rather than the edit. The body is where you explain *why* — the
diff already shows what. Anything that changes on-disk format, a public CLI
surface, or a documented bound also needs a `CHANGELOG.md` entry under
`## [Unreleased]`.

Keep pull requests scoped to one change. If a refactor is needed to make a fix
possible, land the refactor first as its own commit.
