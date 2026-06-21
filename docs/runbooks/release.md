# Runbook: Cutting a Release (RR-001 / RR-003)

Purpose: the end-to-end procedure for shipping a RONin release — preparing the
version/changelog, tagging the channel, verifying the published artifacts, and
rolling back a bad publish. This runbook also hosts the **release-readiness
checklist** (the human verification gate, SC-006) and the **secrets / token
model** (RR-003).

Audience: a maintainer with **write** permission on the repository (to merge the
Release PR and push tags) and **admin** on the crates.io crates (to yank).

Scope: Phase-1 distribution only (DDR-001) — GitHub Releases (binaries) +
crates.io (libraries), unsigned with checksums + keyless provenance (DDR-003).
Phase-2 package managers and Phase-3 signed installers are out of scope here.

The two release tools and their disjoint responsibilities (AD-002):

| Tool | Owns | Files |
|------|------|-------|
| `release-plz` | crate VERSIONING + changelog + the SOLE crates.io publish | `release-plz.toml`, `cliff.toml`, `.github/workflows/release-plz.yml` |
| `dist` (cargo-dist) | cross-platform BINARY build → GitHub Release + checksums + installers + provenance + SBOM | `dist-workspace.toml`, `.github/workflows/release.yml` |

`dist` never publishes a crate and `release-plz` never creates the binary
GitHub Release, so the two can never double-publish (a yank-only error).

---

## Overview

```text
prepare ──► tag ──► verify ──► (rollback if needed)
   │          │        │
   │          │        └─ Release has per-OS tarballs + checksums + installers
   │          │           + binstall + provenance + SBOM; crates on crates.io
   │          └─ push the umbrella vX.Y.Z (or -rc.N / -nightly) tag
   └─ merge the release-plz Release PR (version bumps + changelog)
```

A release is a **two-step maintainer action**: (1) merge the Release PR that
`release-plz` keeps open, then (2) push the umbrella product tag. Pushing the tag
is the single deliberate trigger (OR-001 / OR-008) — it fans out to both
`release.yml` (binaries) and the `publish` job in `release-plz.yml` (crates.io).

---

## 1. Prepare — merge the Release PR

`release-plz` runs on every push to `main` (the `release-pr` job in
`.github/workflows/release-plz.yml`) and keeps an open **Release PR** that
carries:

- **Independent per-crate version bumps** — each of `ronin-core`, `ronin-types`,
  `ronin-validate`, `ronin-app` is bumped from its own conventional-commit history
  (no shared version; AD-001 / OR-005).
- **The umbrella product version** — `ronin-app`'s version is the overall RONin
  release version (OR-008); it is what you will tag in step 2.
- **A regenerated changelog** — keepachangelog-style sections grouped by
  conventional-commit type, produced by git-cliff via `cliff.toml`.
- **A blocking `cargo-semver-checks` result** — the `cargo semver-checks
  (blocking)` step in the `release-pr` job (plus release-plz's own
  `semver_check = true`) FAILS the PR if a breaking API change lacks the matching
  bump (major for >= 1.0, minor in 0.x) (OR-004).

Steps:

1. Confirm the Release PR is green — in particular the `cargo semver-checks
   (blocking)` step. A red semver check means a crate's bump does not match its
   API change; fix the bump (or the API) and let `release-plz` recompute, do not
   override.
2. **Review the changelog** in the PR diff for accuracy and completeness (this is
   a maintainer-attested checklist item, not a mechanical one — see the
   readiness checklist below).
3. Confirm the **umbrella version** on `ronin-app` is the version you intend to
   ship; note it for the tag in step 2.
4. Merge the Release PR into `main`.

Merging the PR publishes **nothing** — it only lands the version bumps,
`Cargo.lock` update, and changelog. Publishing happens only on the tag (step 2).

---

## 2. Tag — push the umbrella tag (and pick the channel)

The umbrella product tag is the **single release trigger** (OR-001 / OR-008 /
STF-001). It must match `ronin-app`'s freshly-merged version.

The tag suffix selects the **channel** (DDR-004 / OR-008). Channel is derived
from the presence vs. absence of a semver pre-release component, so there is no
silent default:

| Tag form | Channel | GitHub Release marked |
|----------|---------|------------------------|
| `vX.Y.Z` (bare semver) | **stable** | normal release |
| `vX.Y.Z-rc.N` | pre-release | pre-release |
| `vX.Y.Z-nightly` | pre-release | pre-release |
| `vX.Y.Z-<anything>` (any other suffix) | pre-release | pre-release |

The routing logic lives in the `plan` job's `Derive release channel from tag
suffix` step in `.github/workflows/release.yml`: only an exact bare-semver match
(`^v[0-9]+\.[0-9]+\.[0-9]+$`) clears the pre-release flag; everything else routes
to pre-release (never silently to stable — OR-008 / SC-002).

Steps (from a clean checkout of the merged `main`):

```bash
git checkout main
git pull --ff-only

# Stable release (bare semver — must equal ronin-app's merged version):
git tag v1.4.0
git push origin v1.4.0

# OR a pre-release (release candidate / nightly), on demand:
git tag v1.4.0-rc.1
git push origin v1.4.0-rc.1
```

Pushing the tag triggers, in parallel:

- **`.github/workflows/release.yml`** (`on: push: tags: ['v*']`) — re-runs the
  E002 gates (`needs: gates`), then `dist` builds the four MVP target binaries,
  attaches checksums + install scripts + the SBOM, and emits per-binary
  provenance. A failure on **any** target fails the whole workflow with no
  partial GitHub Release (fail-fast matrix — OR-002).
- **`.github/workflows/release-plz.yml`** `publish` job (`on: push: tags: ['v*']`)
  — runs `release-plz release`, publishing every crate whose manifest version is
  not yet on crates.io, **in dependency order**: `ronin-core` → `ronin-types` →
  `ronin-validate` → `ronin-app` (OR-005). This is the only crates.io publish step.

Do **not** push per-crate tags as a trigger. Any `<crate>-v<version>` tags
release-plz records are provenance-only (OR-008 / STF-001).

---

## 3. Verify — confirm the published release

After both workflows go green, verify the published outputs before announcing.

### 3a. GitHub Release assets

On the Release page for the tag (`https://github.com/jsh562/RONin/releases/tag/<tag>`),
confirm `dist` attached, for each of the four MVP targets
(`x86_64-pc-windows-msvc`, `x86_64-apple-darwin`, `aarch64-apple-darwin`,
`x86_64-unknown-linux-gnu`):

- a per-OS **`.tar.gz` tarball**,
- a **`.sha256` checksum** per tarball,
- the **`install.sh`** (shell) and **`install.ps1`** (PowerShell) installers,
- the **`cargo binstall` metadata** — `cargo binstall ronin-app` resolves to the
  release tarball via `[package.metadata.binstall]` in `src/ronin-app/Cargo.toml`
  (`pkg-url` → `.../releases/download/v{version}/{name}-{target}.tar.gz`,
  `pkg-fmt = "tar.gz"`),
- the single **CycloneDX SBOM** (`ronin-app.cdx.json`),
- the correct **channel** (a `-rc`/`-nightly`/other tag shows as *pre-release*; a
  bare `vX.Y.Z` shows as a stable release — never the reverse).

### 3b. Provenance attestation

Each tarball carries a keyless Sigstore/SLSA build-provenance attestation
(produced by the `Attest build provenance (per binary)` step in `release.yml`).
Verify one (or each) tarball:

```bash
gh attestation verify ronin-app-x86_64-unknown-linux-gnu.tar.gz \
  --repo jsh562/RONin
```

A successful result confirms the artifact was built by this repo's release
workflow. No paid signing is involved (DDR-003). See the README "Verifying your
download" section for the full user-facing flow (checksum + provenance +
Gatekeeper/SmartScreen).

### 3c. SBOM + cargo-auditable

- Confirm exactly **one** CycloneDX SBOM (`ronin-app.cdx.json`) is attached to
  the Release (OR-007).
- Confirm the shipped binary is `cargo-auditable` (set by `cargo-auditable =
  true` in `dist-workspace.toml`): `cargo audit bin <path-to-ronin-app>` reads
  the embedded dependency metadata.

### 3d. crates.io publish (dependency order)

Confirm each crate's new version is live on crates.io, published in dependency
order:

```bash
# Replace versions with the per-crate versions from the merged Release PR.
cargo search ronin-core
cargo search ronin-types
cargo search ronin-validate
cargo search ronin-app
```

`ronin-core` must appear before `ronin-validate` and `ronin-app` (the latter depend
on it). If `release-plz release` published a partial set (some crates live,
others not), see Rollback (step 4) — the dependency-order publish minimizes blast
radius but a partial publish still needs handling.

---

## 4. Rollback — yank + supersede

A published crates.io version **cannot be deleted, only yanked.** A yank hides a
version from new dependency resolution without breaking existing lockfiles. Plan
rollback around this constraint.

### 4a. crates.io — yank

For a bad or partial crate publish:

```bash
# Yank a bad version (new resolves skip it; existing lockfiles still work):
cargo yank --version 1.4.0 ronin-core

# Un-yank if you yanked in error:
cargo yank --version 1.4.0 --undo ronin-core
```

- A bad publish is **yank-only** — there is no delete. The remedy for a defective
  version is to yank it and publish a corrected **higher** version (you cannot
  republish the same version number).
- For a **partial** publish (dependency-order publish failed partway), yank any
  versions that did publish if they are unusable on their own, fix the cause, and
  re-run the publish for the remaining crates (re-pushing the tag re-runs the
  `publish` job; release-plz publishes only the not-yet-published versions).
- Yank in **reverse dependency order** where it matters (yank dependents before
  the crates they depend on) so you do not strand a half-yanked graph.

### 4b. GitHub Release — delete or supersede

The binary GitHub Release **can** be deleted (unlike crates.io):

```bash
# Delete a bad Release (and optionally its tag):
gh release delete v1.4.0 --repo jsh562/RONin --yes
git push origin :refs/tags/v1.4.0   # delete the remote tag, if re-cutting it

# OR mark it a pre-release / draft instead of deleting (supersede):
gh release edit v1.4.0 --repo jsh562/RONin --prerelease
```

- Prefer **superseding** (publish a fixed `vX.Y.Z+1`) over deleting a release
  users may already have downloaded; delete only an obviously broken release that
  shipped nothing usable.
- Re-cutting the **same** tag requires deleting the remote tag first
  (`git push origin :refs/tags/<tag>`) and re-pushing — but prefer a new
  higher version to avoid confusing downloaders and crates.io (which forbids
  version reuse).

### 4c. Recovery order summary

1. Stop the bleeding: yank the bad crate version(s); mark/delete the bad Release.
2. Fix the cause (code, version bump, or config).
3. Re-prepare (Release PR) with a corrected higher version.
4. Re-tag and re-verify (steps 2–3).

---

## Release-readiness checklist (SC-006)

This checklist is the **human verification gate**: a release MUST satisfy every
item before announcing it. Each item is labelled **mechanically re-checkable**
(verifiable by re-running a tool or inspecting an artifact/workflow) or
**maintainer-attested** (requires maintainer judgment that cannot be fully
automated).

### Mechanically re-checkable

- [ ] **CI gates green, including BOTH wasm32 builds** — the tag's `release.yml`
  run shows `gates (E002 CI re-run)` green (`check`, `test` on all three OSes,
  `wasm (ronin-core wasm32)`, `wasm (ronin-validate wasm32)`, `supply-chain`). The
  release `needs:` these, so a red gate blocks publish (OR-009). *Re-check:* the
  `gates` job conclusion on the tag run, or re-run the gates workflow.
- [ ] **`cargo-semver-checks` green** — the `cargo semver-checks (blocking)` step
  in the Release PR's `release-pr` job passed (no breaking change without the
  matching bump) (OR-004). *Re-check:* re-run `cargo semver-checks check-release
  --locked --workspace`.
- [ ] **Per-OS tarballs + SHA-256 checksums attached** — one `.tar.gz` + one
  `.sha256` per MVP target on the Release (OR-003). *Re-check:* list the Release
  assets; recompute a checksum and compare.
- [ ] **Install scripts + binstall metadata present** — `install.sh`,
  `install.ps1`, and a resolvable `cargo binstall ronin-app` (OR-003).
  *Re-check:* inspect assets; dry-run `cargo binstall --dry-run ronin-app`.
- [ ] **Per-binary provenance attestations verify** — `gh attestation verify
  <tarball> --repo jsh562/RONin` succeeds for each tarball (OR-006 / SC-004).
  *Re-check:* re-run the command.
- [ ] **Exactly one CycloneDX SBOM attached + binary is cargo-auditable** —
  `ronin-app.cdx.json` present once; `cargo audit bin` reads embedded metadata
  (OR-007 / SC-005). *Re-check:* inspect the asset; run `cargo audit bin`.
- [ ] **Channel correct for the tag** — a `-rc`/`-nightly`/other tag is a
  GitHub *pre-release*; a bare `vX.Y.Z` is *stable* — never the reverse (OR-008 /
  SC-002). *Re-check:* the Release's pre-release flag vs the tag.
- [ ] **All four crates live on crates.io in dependency order** — `ronin-core` →
  `ronin-types` → `ronin-validate` → `ronin-app` (OR-005). *Re-check:* `cargo search`
  each crate.

### Maintainer-attested judgment

- [ ] **Changelog reviewed for accuracy/completeness** — the generated changelog
  in the merged Release PR correctly describes the user-visible changes (OR-004).
  *(Maintainer judgment — the generator cannot assess whether an entry is
  accurate or complete.)*
- [ ] **Per-OS binary smoke test passed** — on **each** of the four MVP target
  OSes (`x86_64` Windows, `x86_64` macOS, `aarch64` macOS, `x86_64` Linux):
  download the release tarball, run the `ronin-app` binary, and confirm it
  **launches** and performs a **basic open → validate → format** operation on a
  sample RON file **without crashing** (RR-001). *(Maintainer judgment —
  exercising a real GUI binary on each OS is not done in CI; OR-015 keeps the
  live release out of CI.)*

The mechanically-re-checkable items are also exercised — without a live release —
by the `release-verify` CI job (`.github/workflows/release-verify.yml`, run on
every PR + push to main): `dist plan` / `dist generate --check`, tag→channel
routing (`ci/check-channel-routing.{py,sh}`), `cargo publish --dry-run` per crate
(`ci/publish-dry-run.sh`), blocking `cargo-semver-checks`, the manifest-metadata
(`ci/check-manifest-metadata.py`) + SHA-pin (`ci/check-sha-pins.py`) checks,
`actionlint`, and the static provenance/SBOM-step check
(`ci/check-provenance-sbom.py`) + SC-007 hardening check
(`ci/check-release-hardening.py`), per OR-015..018.

The maintainer-attested items, plus the *live* artifact verification a dry-run
cannot prove, are this checklist's residual responsibility (OR-018) — these are
**deferred to this readiness gate** and run only on a real maintainer-pushed tag:

- a real per-binary attestation actually verifying via `gh attestation verify`
  (step 3b above) — CI only statically asserts the `attest-build-provenance` step
  is present + scoped (OR-018);
- a real CycloneDX SBOM actually attached + the binary's embedded metadata
  actually readable by `cargo audit bin` (step 3c) — CI only statically asserts
  the single-SBOM step + `cargo-auditable = true` (OR-018);
- the OR-002 **fail-whole orchestration** (any target/step failure fails the
  whole release with no partial publish), which only the live tag-triggered run
  demonstrates;
- the per-OS binary smoke test (maintainer-attested, above).

---

## Secrets / token model (RR-003)

The release pipeline uses exactly **two** credentials, both GitHub Actions
secrets, both least-privilege and never exposed to fork PRs.

### `CARGO_REGISTRY_TOKEN` — the only repository secret

`CARGO_REGISTRY_TOKEN` is the crates.io API token used by `release-plz release`
to publish crates. It is the **only** repository secret in the pipeline.

**Setup:**

1. Sign in to <https://crates.io>, open **Account Settings → API Tokens**.
2. Create a new token. Scope it as narrowly as crates.io allows — `publish-update`
   for the four RONin crates is sufficient (it never needs yank/owner scopes; do
   those interactively from the maintainer's own account).
3. In the GitHub repo: **Settings → Secrets and variables → Actions → New
   repository secret**, name it exactly `CARGO_REGISTRY_TOKEN`, paste the value.

**Rotation:**

- Rotate on any suspicion of compromise, when a maintainer with access leaves, or
  on a routine cadence (e.g. annually).
- To rotate: create a new crates.io token, update the
  `CARGO_REGISTRY_TOKEN` repository secret with the new value, then **revoke** the
  old token on crates.io. Rotation needs no code change — only the secret value.
- If a token leaks, revoke it on crates.io **first** (that immediately disables
  it), then replace the secret.

### `GITHUB_TOKEN` — the Actions-provided token

`GITHUB_TOKEN` is minted per-run by GitHub Actions (no manual setup). The
pipeline never grants it global write; each job re-declares only the scopes it
needs.

**Required repository setting (one-time, repo-admin).** The `release-pr` job opens
the Release PR via the API, which GitHub blocks by default — even with
`pull-requests: write` — until you enable:

> **Settings → Actions → General → Workflow permissions →
> ✅ "Allow GitHub Actions to create and approve pull requests" → Save**

Without it, `release-plz release-pr` fails with `GitHub Actions is not permitted to
create or approve pull requests` (403). `jsh562/RONin` is a personal account, so
the repo setting is sufficient; in an org, the same checkbox must also be enabled
at the org level (it can override the repo). Note: a PR opened by `GITHUB_TOKEN`
does **not** trigger `pull_request` workflows, so `ci.yml` does not run on the
Release PR itself — it runs when the PR is merged to `main`. (To CI-gate the
Release PR you would switch release-plz to a PAT / GitHub App token, adding a
second secret — deliberately not done here.)

### Least-privilege per-job token scopes

Every workflow sets `permissions: {}` at the top level (grant nothing) and each
job re-declares the minimum it needs (OR-011 / HINT-003 / AD-005):

| Workflow / job | `contents` | `id-token` | `attestations` | `pull-requests` | Secret |
|----------------|-----------|-----------|----------------|-----------------|--------|
| `release.yml` › `gates` (reusable) | read | — | — | — | none |
| `release.yml` › `plan` | read | — | — | — | none |
| `release.yml` › `build-local-artifacts` | read | — | — | — | none |
| `release.yml` › `build-global-artifacts` | read | — | — | — | none |
| `release.yml` › **`host`** (the only write job) | **write** | **write** | **write** | — | none |
| `release.yml` › `announce` | read | — | — | — | none |
| `release-plz.yml` › `release-pr` | write | — | — | write | none |
| `release-plz.yml` › **`publish`** | write | — | — | — | **`CARGO_REGISTRY_TOKEN`** |

Notes:

- In `release.yml`, the write scopes (`contents`/`id-token`/`attestations:
  write`) live **only** on the `host` job — `contents: write` to create the
  Release and upload assets, `id-token: write` to mint the Sigstore OIDC token,
  `attestations: write` to record the build-provenance attestation. Nothing else
  in the workflow can publish or attest.
- `release.yml` references **no** `CARGO_REGISTRY_TOKEN` — `dist` builds binaries
  only and never publishes a crate (AD-002).
- In `release-plz.yml`, `CARGO_REGISTRY_TOKEN` is injected on the **single**
  `release-plz release` step of the `publish` job and nowhere else. The
  `publish` job has `contents: write` so release-plz can push the per-crate
  provenance tags (`git_tag_enable = true`) — pushing a git tag inherently needs
  write. It still creates no GitHub Release (`git_release_enable = false`); the
  crates.io push uses the registry token, not `GITHUB_TOKEN`.
- The `release-pr` job gets `contents: write` + `pull-requests: write` to
  open/update the Release PR and nothing more; it never sees
  `CARGO_REGISTRY_TOKEN`.

### No fork-PR secret exposure (RR-003 / Edge Cases)

Neither release workflow uses `pull_request` or `pull_request_target`:

- `release.yml` triggers on `push: tags: ['v*']` only.
- `release-plz.yml` triggers on `push: branches: [main]` (the Release PR job) and
  `push: tags: ['v*']` (the publish job).

A tag can only be pushed by someone with write access, and a push to `main` runs
on the trusted branch — so a fork PR can never run either workflow or reach
`CARGO_REGISTRY_TOKEN`. The PR CI (`ci.yml` → `gates.yml`) does run on
`pull_request`, but it references no `secrets.*`, so fork PRs validate
identically with zero secret access.

---

## Zero runtime impact + zero infrastructure cost (OR-012)

The release pipeline is build-time only and adds nothing to the shipped
application or any standing infrastructure (OR-012 / SC-007 / DDR-001 / §VI
Local-First & Private).

### Zero runtime network / telemetry added to the shipped app

- Everything in `release.yml` / `release-plz.yml` runs at **release time on
  GitHub-hosted runners** — none of it executes inside `ronin-app`. The shipped
  binary is the same `ronin-app` produced from the workspace; the pipeline only
  builds, checksums, attests, and uploads it.
- The pipeline introduces **no new runtime dependency**. `cargo-auditable` embeds
  *dependency metadata* into the binary (read offline by `cargo audit`); it adds
  no network code path. The CycloneDX SBOM and provenance attestation are
  **release artifacts** attached to the GitHub Release, not code linked into the
  app.
- crates.io and Sigstore are contacted **only at release time** by the CI jobs
  (to publish crates and mint the keyless attestation) — never by the running
  application. This preserves the Local-First & Private invariant: the app makes
  no network calls and collects no telemetry by default.
- *Verification:* the E002 supply-chain gate (`cargo-deny`/`cargo-audit`) and the
  wasm32 gates (which fail on any network/TLS dependency leaking into the
  WASM-clean crates) re-run on every release (`needs: gates`), and no release
  step adds a runtime dependency to `ronin-app`'s manifest.

### Zero infrastructure cost

- The whole pipeline runs on free OSS tiers only (DDR-001 / cost estimate ~$0 in
  `specs/dod.md`):
  - **GitHub Actions** — free runner minutes for public repos (build matrix +
    publish jobs); caching (`Swatinem/rust-cache`) keeps runs within limits.
  - **GitHub Releases** — free artifact hosting + built-in CDN (binaries,
    checksums, installers, SBOM, attestations).
  - **crates.io** — free crate hosting/publishing.
  - **Sigstore** — free keyless provenance (no paid certificate; DDR-003).
- There is **no hosted runtime, server, container registry, or database** to
  operate or pay for. The only *future* cost is optional Phase-3 code-signing
  certificates, which are explicitly deferred (DDR-003).
- *Verification:* every external touchpoint above is a free tier; the repo
  declares no paid infrastructure; the unsigned-with-checksums+provenance posture
  (DDR-003) deliberately avoids the one paid item (signing).

---

## Related runbooks

- [`branch-protection.md`](branch-protection.md) — making the CI gates
  merge-blocking (required before relying on the green-CI readiness item).
- [`advisory-response.md`](advisory-response.md) — responding to a red
  `supply-chain` gate, which also blocks a release via `needs: gates`.
- [`ci-local-repro.md`](ci-local-repro.md) — running the gate commands locally.
