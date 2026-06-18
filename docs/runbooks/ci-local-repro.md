# Runbook: Reproduce a CI Failure Locally (RR-003)

Purpose: run the exact gate commands from `.github/workflows/ci.yml` on your own
machine so you can fix a red check without round-tripping through GitHub.

Each command below is byte-for-byte the command the corresponding CI job runs.
Run them from the **repository root**.

## Prerequisites

- The pinned stable toolchain installs automatically from `rust-toolchain.toml`
  (channel `stable`, components `rustfmt`/`clippy`, target
  `wasm32-unknown-unknown`). Verify with `rustc --version` and
  `rustup target list --installed`.
- Supply-chain tools for the `supply-chain` job:
  ```bash
  cargo install cargo-audit cargo-deny
  ```
  (CI installs these prebuilt and version-pinned via `taiki-e/install-action`;
  local versions only need to be recent enough.)

## Gate commands

### `check` job — rustfmt + clippy (OR-001 / OR-002)

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
```

- `cargo fmt --all -- --check` fails (non-zero) if any file is mis-formatted; it
  does not modify files. To auto-fix, run `cargo fmt --all` (no `-- --check`).
- The clippy command treats every warning as an error (`-D warnings`), scans all
  targets (lib/bins/tests/examples), and the whole workspace.

### `test` job — test suite (OR-003 / OR-009)

```bash
cargo test --workspace --locked
```

- Runs on `ubuntu-latest`, `windows-latest`, and `macos-latest` in CI; run it on
  your OS locally. If a failure is OS-specific (paths, CRLF vs LF), reproduce on
  that OS.
- `--workspace` never builds `src/ronin-core/fuzz` (it is `exclude`d in the root
  `Cargo.toml`), so the nightly-only fuzz crate stays off the stable build
  (OR-009 / SC-007).

### `wasm` job — WASM-clean build (OR-004, ADR-0002)

```bash
cargo build -p ronin-core --target wasm32-unknown-unknown --locked
```

- Fails if a filesystem / UI / async / native dependency has leaked into
  `ronin-core`. If the target is missing locally:
  `rustup target add wasm32-unknown-unknown`.

### `supply-chain` job — audit + deny (OR-005)

```bash
cargo audit
cargo deny check
```

- `cargo audit` checks dependencies against the RustSec advisory database.
- `cargo deny check` reads `deny.toml` at the repo root (advisories, licenses,
  bans, sources). See `advisory-response.md` (RR-002) for how to respond to a
  failure.

## Run everything at once

```bash
cargo fmt --all -- --check \
  && cargo clippy --workspace --all-targets --locked -- -D warnings \
  && cargo test --workspace --locked \
  && cargo build -p ronin-core --target wasm32-unknown-unknown --locked \
  && cargo audit \
  && cargo deny check
```

The chain stops at the first failing gate, mirroring how CI reds that job.

## `--locked` and the lockfile

CI builds/tests with `--locked` against the committed `Cargo.lock` (OR-007), so
the dependency set is identical across runs of a commit. If `--locked` errors
with "the lock file needs to be updated", your `Cargo.toml` changed without
updating the lockfile — run `cargo generate-lockfile` (or `cargo update` for the
specific crate) and commit the updated `Cargo.lock`.

## Optional: run the workflow itself with `act`

[`nektos/act`](https://github.com/nektos/act) runs GitHub Actions workflows
locally in Docker:

```bash
# Run the jobs that fire on a pull_request event:
act pull_request

# Run a single job:
act -j check
act -j wasm
```

Caveats: `act` uses Docker Linux images, so the Windows/macOS matrix legs cannot
be reproduced this way (use a native machine for those); cache behavior and
prebuilt-tool installs may differ from GitHub-hosted runners. For a fast inner
loop, prefer the raw `cargo` commands above; use `act` mainly to validate
workflow wiring.
