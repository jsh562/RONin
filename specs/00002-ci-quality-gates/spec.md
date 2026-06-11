---
feature_branch: "00002-ci-quality-gates"
created: "2026-06-11"
input: "E002"
spec_type: "operational"
spec_maturity: "clarified"
epic_id: "E002"
epic_sources: "{DOD:DDR-005}"
---

# Feature Specification: CI Quality Gates

**Feature Branch**: `00002-ci-quality-gates`
**Created**: 2026-06-11
**Status**: Draft
**Spec Type**: operational
**Spec Maturity**: clarified
**Epic ID**: E002
**Epic Sources**: {DOD:DDR-005}
**Product Document**: specs/prd.md

## Problem Statement *(mandatory)*

RONin's quality and architecture invariants — byte-for-byte losslessness, a WASM-clean core, clean lints, and a vulnerability-free dependency set — are only trustworthy if they are verified automatically on every change. Today those checks run only when a contributor remembers to run them locally, and the WASM-clean invariant (ADR-0002) can silently regress the moment a native dependency leaks into `ron-core`. Without an automated CI gate, regressions reach the default branch undetected, eroding the data-integrity guarantees the whole product rests on, and making the later release pipeline (E011) unsafe to automate. This feature establishes the continuous-integration gates that enforce those invariants on every pull request and push, at zero infrastructure cost.

## Clarifications

### Session 2026-06-11

- Q: Scheduled scan cadence? -> A: Deferred to the plan — the spec requires a recurring schedule without pinning the interval.
- Q: How to treat merge-blocking when branch protection is a repo-admin setting CI doesn't own? -> A: CI verifies the named gate jobs exist and go red on failure; enforcing them as merge blocks (required status checks) is an admin step documented in RR-001, out of CI-config scope.
- Q: Advisory-failure policy (RR-002 waiver vs OR-005 hard-fail)? -> A: Always hard-fail; the only waiver is an explicit, PR-reviewed, dated entry in `deny.toml` / the cargo-audit ignore-list — never a silent or CI-level override.
- Q: OS/job matrix breadth? -> A: Run the test suite on Windows/macOS/Linux; run OS-independent jobs (fmt, clippy, wasm32) once on Linux to conserve free-tier minutes.
- Q: Dedicated SCs for OR-009/OR-010? -> A: Yes — added SC-007 (fuzz crate excluded from the stable matrix) and SC-008 (named gate jobs present and individually selectable as required checks).
- Q: Supply-chain job structure? -> A: A single named `supply-chain` job runs both cargo-audit and cargo-deny with pinned tool versions.

## Scope *(mandatory)*

### Included

- A GitHub Actions pipeline that, on every pull request and push, runs `rustfmt --check`, `clippy -D warnings`, and the test suite across Windows, macOS, and Linux.
- A dedicated job that builds `ron-core` for `wasm32-unknown-unknown` and fails the run if it does not compile (enforcing the WASM-clean invariant, ADR-0002).
- Supply-chain scanning (`cargo-audit` + `cargo-deny`) on every change and on a recurring schedule.
- Build caching (rust-cache) and a committed `Cargo.lock` for fast, reproducible CI builds.
- Discrete, named jobs suitable for use as required status checks (merge gates), plus runbooks for branch protection and advisory response.

### Excluded

- Release builds, binary packaging, crates.io publishing, and signing — owned by E011 (Release & distribution).
- The nightly `cargo fuzz` ≥1M-iteration run — requires the nightly toolchain; deferred to an optional separate workflow (the stable proptest fallback already runs in the test suite).
- Code-coverage enforcement — coverage is advisory per project policy (no threshold); a coverage job is optional, not a gate.
- Configuring GitHub branch-protection rules themselves — that is a repository-admin setting, not workflow YAML; CI provides the gating jobs and a runbook for enabling them.
- Deployment, hosting, or any non-CI infrastructure (the product is local-first; there is no server to operate).

### Edge Cases & Boundaries

- Pull requests from forks (no repository secrets) — all gate jobs must run without secrets.
- The workspace-excluded `fuzz` crate must not be built on the stable matrix (it needs nightly/cargo-fuzz) and must not break the run.
- Cross-OS differences (path separators, CRLF vs LF) must not cause spurious test failures.
- A transient dependency-fetch network failure — mitigated by caching and standard retry.
- A new security advisory published between merges — caught by the scheduled scan even with no new commits.
- Runner free-tier minute limits as the matrix grows — mitigated by caching and fail-fast.

## Operational Objectives *(mandatory for operational specs only)*

### Objective 1 - Cross-platform validation pipeline (Priority: P1)

Run format, lint, and test gates on every pull request and push across all three target operating systems.

**Why this priority**: This is the core merge gate; without it, lint/test regressions reach the default branch and the project's "Verified Quality" principle is unenforced.

**Rationale**: project-instructions §V mandates rustfmt, clippy `-D warnings`, and a passing test matrix on Windows/macOS/Linux before merge.

**Deliverables**:
- A GitHub Actions workflow triggered on `pull_request` and `push` to the default branch.
- A test job (`cargo test --workspace --locked`) on a `{windows, macos, ubuntu}` matrix; the OS-independent jobs (`cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`) run once on Linux to conserve free-tier minutes.
- Stable-toolchain pinning consistent across all jobs (via `rust-toolchain.toml`).

**Verification Criteria**:
1. **Given** a change that fails formatting, a clippy lint, or any OS's tests, **When** CI runs, **Then** the corresponding gate job fails (red). (Enforcing a red job as a merge block is the branch-protection admin step in RR-001.)
2. **Given** a clean commit, **When** CI runs, **Then** the test job passes green on Windows, macOS, and Linux and the fmt/clippy jobs pass green on Linux.

### Objective 2 - WASM-clean gate (Priority: P1)

Enforce the `ron-core` WASM-clean invariant by compiling it to `wasm32-unknown-unknown` as a required job.

**Why this priority**: ADR-0002 makes WASM-cleanliness a load-bearing architecture invariant for future LSP/VSCode reuse; it can regress silently and only a real wasm build proves it.

**Rationale**: The only reliable proof of WASM-cleanliness is a successful `wasm32-unknown-unknown` build (research/DOD); CI must perform it on every change.

**Deliverables**:
- A dedicated CI job that adds the `wasm32-unknown-unknown` target and runs `cargo build -p ron-core --target wasm32-unknown-unknown`.
- The job is a discrete named gate suitable for configuration as a required check (enabling branch protection is the RR-001 admin step).

**Verification Criteria**:
1. **Given** a change that introduces a filesystem/UI/async/native dependency into `ron-core`, **When** the wasm32 job runs, **Then** it fails (red).
2. **Given** a WASM-clean change, **When** the wasm32 job runs, **Then** it compiles successfully.

### Objective 3 - Supply-chain scanning (Priority: P1)

Scan dependencies for vulnerabilities and license/advisory/ban-policy violations on every change and on a schedule.

**Why this priority**: Security scanning is a PI-mandated QC category; advisories also appear independently of commits, so a recurring scan is required.

**Rationale**: `cargo-audit` (RustSec advisories) and `cargo-deny` (license/ban/advisory policy via `deny.toml`) are the established zero-cost Rust supply-chain controls (DOD).

**Deliverables**:
- A single named `supply-chain` job running `cargo audit` and `cargo deny check` on `pull_request`/`push`, with the tool versions pinned (marketplace action or locked install).
- A scheduled (cron) run of the same job so newly published advisories are detected without a new commit (exact cadence chosen during planning).
- Hard-fail on any advisory/disallowed license; the only waiver is an explicit, PR-reviewed, dated entry in `deny.toml` / the cargo-audit ignore-list.

**Verification Criteria**:
1. **Given** a dependency with a known RustSec advisory or a disallowed license, **When** the scan job runs, **Then** it fails with the offending advisory/license reported.
2. **Given** a clean dependency set, **When** the scheduled scan runs, **Then** it completes green and surfaces any newly published advisory on the next scheduled run.

### Objective 4 - Build caching and reproducibility (Priority: P2)

Cache Rust build artifacts and lock the dependency set so CI is fast and reproducible.

**Why this priority**: Significant value (keeps the free-tier matrix viable and builds reproducible) but the gates in OBJ1–OBJ3 function correctly without it.

**Rationale**: rust-cache plus a committed `Cargo.lock` (`--locked`) keep runs within free-tier minutes and guarantee the same dependency set per commit (DOD: "commit Cargo.lock").

**Deliverables**:
- rust-cache (or equivalent) configured per job, keyed on toolchain + lockfile + OS/target.
- `Cargo.lock` committed; CI builds/tests use `--locked`.

**Verification Criteria**:
1. **Given** a prior run populated the cache, **When** CI re-runs on an unchanged dependency set, **Then** the rust-cache step reports a cache hit (restored registry/target) in the job log.
2. **Given** a committed `Cargo.lock`, **When** CI runs with `--locked`, **Then** the dependency set is identical across runs of the same commit (no silent drift).

### Operational Constraints

- Zero infrastructure budget — GitHub-hosted runners on the free tier only.
- Stable Rust toolchain only in the required gates (nightly-only checks such as `cargo fuzz` are out of scope here).
- No repository secrets are required for any gate job (so fork PRs run fully); publish tokens belong to E011.
- The workspace-excluded `fuzz` crate must remain outside the stable build/test.

## Integration Points *(mandatory for technical and operational specs)*

- **IP-001**: CI builds and tests the E001 Cargo workspace (`ron-core` plus `ron-types`/`ronin-app` stubs); depends on the buildable workspace produced by E001.
- **IP-002**: `cargo deny check` consumes `deny.toml` at the repository root (produced in E001).
- **IP-003**: The wasm32 job enforces ADR-0002 (`ron-core` WASM-clean); it consumes that architecture constraint from the SAD.
- **IP-004**: Release & distribution (E011) depends on green CI as a precondition for releasing.
- **IP-005**: The toolchain pin (`rust-toolchain.toml`, E001) governs the Rust version used by every CI job.

## Requirements *(mandatory)*

### Operational Requirements *(operational specs only)*

- **OR-001**: CI MUST run `cargo fmt --check` (once, on Linux — OS-independent) on every pull request and push; a formatting violation MUST fail the run.
- **OR-002**: CI MUST run `cargo clippy --workspace --all-targets -- -D warnings` (once, on Linux — OS-independent); any lint MUST fail the run.
- **OR-003**: CI MUST run the test suite on Windows, macOS, and Linux; any test failure on any OS MUST fail the run.
- **OR-004**: CI MUST build `ron-core` for `wasm32-unknown-unknown` in a dedicated job; a build failure MUST fail the run.
- **OR-005**: CI MUST run `cargo audit` and `cargo deny check` in a single named `supply-chain` job (with pinned tool versions) on every pull request and push, AND on a recurring schedule (cron; exact cadence chosen during planning). A known advisory or disallowed license MUST hard-fail the run; the only permitted waiver is an explicit, PR-reviewed, dated entry in `deny.toml` / the cargo-audit ignore-list — never a silent or CI-level override.
- **OR-006**: CI MUST cache the Cargo registry and build output (rust-cache or equivalent), keyed on toolchain, lockfile, and OS/target.
- **OR-007**: `Cargo.lock` MUST be committed and CI MUST build/test with `--locked` for reproducible dependency resolution.
- **OR-008**: CI MUST use the pinned stable toolchain (`rust-toolchain.toml`) consistently across all jobs.
- **OR-009**: CI MUST exclude the workspace-excluded `fuzz` crate from the stable build/test matrix.
- **OR-010**: CI MUST expose discrete, named jobs (fmt, clippy, test-per-OS, wasm32, supply-chain) suitable for configuration as required status checks (merge gates).
- **OR-011**: All gate jobs MUST run without any repository secret (so pull requests from forks are fully validated).
- **OR-012**: The workflow MUST declare an explicit least-privilege `permissions:` block granting the `GITHUB_TOKEN` read-only scope (e.g. `contents: read`) for the gate jobs, rather than inheriting the repository's default token permissions; no gate job may request write scopes. Third-party actions MUST be pinned by full commit SHA (AD-005), with a Dependabot `github-actions` configuration to update those pins.

### Runbook Requirements *(include for operational specs if applicable)*

- **RR-001**: A runbook MUST exist for enabling branch protection / required status checks on the default branch (which jobs to mark required).
- **RR-002**: A runbook MUST exist for responding to a `cargo-audit`/`cargo-deny` failure — triage, patch/upgrade, or (only as a last resort) an explicit, PR-reviewed, dated waiver entry in `deny.toml` / the cargo-audit ignore-list.
- **RR-003**: A runbook MUST exist for reproducing a CI failure locally (the exact fmt/clippy/test/wasm32 commands).

## Assumptions & Risks *(mandatory)*

### Assumptions

- The repository is hosted on GitHub with Actions enabled on the free tier.
- A maintainer with admin rights can configure branch protection (required checks) — an admin step outside the workflow YAML.
- Stable Rust at or above the project MSRV (1.77) is available on all three hosted runner OSes.
- `deny.toml`, `rust-toolchain.toml`, and the Cargo workspace already exist (from E001).

### Risks

- **Free-tier minute limits** *(likelihood: medium, impact: medium)*: a growing matrix may exhaust free minutes. Mitigation: caching, fail-fast, and minimal redundant jobs.
- **Cross-OS test flakiness** *(likelihood: low, impact: medium)*: path/line-ending differences. Mitigation: deterministic tests and existing CRLF/BOM handling in `ron-core`.
- **Advisory churn / scan noise** *(likelihood: medium, impact: low)*: scheduled scans may surface advisories needing triage. Mitigation: the RR-002 advisory-response runbook and a curated `deny.toml`.

## Implementation Signals *(mandatory)*

- `NEW-CONFIG` — GitHub Actions workflow file(s) under `.github/workflows/` (per-PR validation + scheduled supply-chain scan); reuse of the existing `rust-toolchain.toml` and `deny.toml`; a committed `Cargo.lock`.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001** [OBJ1]: A change that fails formatting, a clippy lint, or any OS's tests produces a red gate-job check. (Enforcing red checks as merge blocks is the RR-001 branch-protection admin step.)
- **SC-002** [OBJ1]: A clean commit passes green — the test job on Windows, macOS, and Linux, and the fmt/clippy/wasm32 jobs on Linux.
- **SC-003** [OBJ2]: A change that makes `ron-core` non-WASM-clean fails the wasm32 job; a WASM-clean change passes it.
- **SC-004** [OBJ3]: `cargo audit` and `cargo deny check` run on every pull request and on the schedule; a dependency with a known advisory or disallowed license yields a failing check.
- **SC-005** [OBJ4]: A warm-cache CI run restores the Cargo registry/target cache (a cache hit is reported in the job log), and CI builds with `--locked` against a committed `Cargo.lock`.
- **SC-006** [OBJ4]: Re-running CI on the same commit resolves an identical dependency set (no silent dependency drift).
- **SC-007** [OBJ1]: The workspace-excluded `fuzz` crate is never built or tested by the stable CI matrix (authoritatively guaranteed by the root `[workspace].exclude` entry, so `--workspace` commands never build it).
- **SC-008** [OBJ1]: All gate jobs (fmt, clippy, per-OS test, wasm32, supply-chain) are present as discrete named jobs, each individually selectable as a required status check.
- **SC-009** [OBJ3]: Every third-party action reference in the workflow is pinned to a full commit SHA (reviewable in the workflow YAML — no tag/major-version-only pins); the workflow declares an explicit read-only `GITHUB_TOKEN` `permissions:` block with no write scopes and no per-job override that re-grants write; and a Dependabot `github-actions` configuration is present to keep the pinned SHAs updated.
- **SC-010** [OBJ1]: A pull request opened from a fork produces the same gate-job results (same passing/failing checks) as an equivalent internal-branch pull request, with no gate job skipped or erroring for lack of a repository secret.
- **SC-011** [OBJ1]: All CI jobs (check, test, wasm, supply-chain) resolve the same stable toolchain from `rust-toolchain.toml`, with no per-job channel override.

## Stress-Test Findings

### Session 2026-06-11

- **STF-001** *(category: consistency, severity: low)*: After the matrix-breadth clarification (tests on all three OSes; fmt/clippy/wasm32 once on Linux), SC-002 and Objective 1's second verification criterion still claimed fmt/clippy run on all three OSes. **Resolution (accepted, applied inline)**: reworded SC-002 and OBJ1 Verification Criterion 2 to scope per-OS execution to the test job, with fmt/clippy/wasm32 on Linux.

No other contradictions found; the advisory hard-fail policy (OR-005) and the RR-002 waiver path are now consistent, and merge-blocking is uniformly scoped to the RR-001 branch-protection admin step.

## Glossary *(include when spec introduces 2+ domain-specific terms)*

| Term | Definition |
|------|------------|
| CI gate | A CI job whose failure blocks a change from merging when set as a required status check. |
| Required status check | A GitHub branch-protection setting that makes a named CI job mandatory before merge. |
| WASM-clean gate | The CI job that compiles `ron-core` to `wasm32-unknown-unknown` to prove it carries no filesystem/UI/async/native dependency (ADR-0002). |
| Supply-chain scanning | Checking dependencies for known vulnerabilities (`cargo-audit`) and license/advisory/ban-policy violations (`cargo-deny`). |
| rust-cache | A GitHub Actions cache for the Cargo registry and `target/` directory keyed on toolchain and lockfile. |

## Compliance Check

**Overall**: PASS — no contradictions with non-negotiable governance; zero CRITICAL items. Audited against `project-instructions.md` (Principles I–VI, Source Code Layout, Tech Stack, Testing & Quality Policy), DDR-005, the DOD CI design, and ADR-0002.

| Rule | Verdict | Evidence |
|------|---------|----------|
| Principle V — Verified Quality (fmt + clippy -D warnings + multi-OS tests + wasm32 gate) | Compliant | OBJ1/OBJ2, OR-001–004, SC-001–003 |
| Principle II / ADR-0002 — WASM-clean core via wasm32 gate | Compliant | OBJ2, OR-004, IP-003, SC-003 |
| Testing & Quality Policy — linting + security required; coverage advisory (no gate) | Compliant | OBJ3/OR-005; Excluded explicitly bars a coverage gate |
| Principle VI — Local-First & Private (no telemetry) | Compliant | CI-only scope; no product telemetry; OR-011 forbids required secrets |
| DDR-005 — mandatory wasm32 gate | Compliant | Epic source {DOD:DDR-005}; OBJ2/OR-004 implement it |
| DOD CI design — cargo-audit + cargo-deny, rust-cache, Cargo.lock, stable toolchain, free tier | Compliant | OR-005/006/007/008 + Operational Constraints |
| Source Code Layout — /src | Compliant | Workflows under `.github/workflows/` (config, correctly outside /src) |

**Advisory (non-blocking)**: OR-009 (exclude fuzz crate) and OR-010 (named required-check jobs) have no dedicated SC — coverage rides on OBJ1/OBJ2 criteria and SC-001's "required checks" clause; consider an explicit SC during Plan if tighter traceability is wanted.
