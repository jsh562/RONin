# Runbook: Branch Protection / Required Status Checks (RR-001)

Purpose: make the CI gate jobs in `.github/workflows/ci.yml` merge-blocking on
the default branch. CI defines and runs the jobs; turning them into required
checks is a **repository-admin** step (it is a GitHub setting, not workflow YAML),
so it lives here rather than in the workflow.

Audience: a maintainer with **Admin** permission on the repository.

Prerequisite: the `CI` workflow has run at least once on a branch/PR so GitHub
knows the check names (required-check selection lists only checks it has seen).

## Required status checks to mark

Mark every gate job below as a required status check. The names are the job
`name:` values reported by GitHub; the matrix `test` job reports one check per
OS.

| Check (as shown in the GitHub UI) | Source job | Enforces |
|-----------------------------------|------------|----------|
| `check (fmt + clippy)`            | `check`        | rustfmt + clippy `-D warnings` (OR-001/002) |
| `test (ubuntu-latest)`           | `test` matrix  | Linux test suite (OR-003) |
| `test (windows-latest)`          | `test` matrix  | Windows test suite (OR-003) |
| `test (macos-latest)`            | `test` matrix  | macOS test suite (OR-003) |
| `wasm (ronin-core wasm32)`         | `wasm`         | WASM-clean invariant, ADR-0002 (OR-004) |
| `supply-chain (audit + deny)`    | `supply-chain` | cargo-audit + cargo-deny (OR-005) |

All six must be required so a red gate blocks merge (SC-001 / SC-008 / E011
SC-007). The `supply-chain` job also runs on the daily schedule; scheduled runs
report on the default branch but do not gate an individual PR — that is expected.

### E011: `release-verify` is a MANUAL gate — do NOT mark it required

`release-verify` is the release-pipeline verification gate
(`.github/workflows/release-verify.yml`). As of the workflow split it runs ONLY on
`workflow_dispatch` — a *before-a-release* readiness check the maintainer runs by
hand (from the Actions tab) before cutting a release, NOT an every-push PR gate. It
proves the release config without cutting a live release: `dist plan`,
`actionlint`, the SHA-pin + manifest-metadata + tag→channel-routing +
provenance/SBOM + SC-007-hardening check scripts, `cargo publish --dry-run` per
crate, and blocking `cargo-semver-checks` (OR-015..018).

Because it does **not** run on `pull_request`, it MUST NOT be added to the required
status checks above: a manual-only workflow never reports a result on a PR, so
requiring it would leave every PR's merge button blocked on a check that never
arrives. Run it manually before a release instead (see
`docs/runbooks/release.md`). The every-push correctness gates that DO block merge
are the six `gates.yml` jobs listed above (the after-any-push bucket).

## Steps (GitHub web UI)

1. Repository → **Settings** → **Branches** (or **Rules → Rulesets**).
2. Add a branch protection rule / ruleset targeting the default branch (`main`).
3. Enable **Require status checks to pass before merging**.
4. Enable **Require branches to be up to date before merging** (re-runs checks
   against the latest base so a green check reflects the merged result).
5. In the status-check search box, add each of the six checks listed above.
6. (Recommended) Enable **Require a pull request before merging** and **Do not
   allow bypassing the above settings** so admins are gated too.
7. Save.

## Steps (GitHub CLI alternative)

```bash
# Classic branch-protection API. Adjust OWNER/REPO and the default branch name.
gh api -X PUT repos/OWNER/REPO/branches/main/protection \
  -H "Accept: application/vnd.github+json" \
  -f 'required_status_checks[strict]=true' \
  -f 'required_status_checks[checks][][context]=check (fmt + clippy)' \
  -f 'required_status_checks[checks][][context]=test (ubuntu-latest)' \
  -f 'required_status_checks[checks][][context]=test (windows-latest)' \
  -f 'required_status_checks[checks][][context]=test (macos-latest)' \
  -f 'required_status_checks[checks][][context]=wasm (ronin-core wasm32)' \
  -f 'required_status_checks[checks][][context]=supply-chain (audit + deny)' \
  -F 'enforce_admins=true' \
  -F 'required_pull_request_reviews=null' \
  -F 'restrictions=null'
```

## Verification

- Open a throwaway PR that fails one gate (see `ci-local-repro.md` for cheap
  ways to trigger each). The PR's **Merge** button must be blocked until the
  failing check goes green.
- Confirm a fork PR shows the same six checks (no check skipped for lack of a
  secret) — see SC-010; the workflow uses `pull_request` and references no
  `secrets.*`.

## Notes

- If you rename a job's `name:` in `ci.yml`, the required-check selection breaks
  silently (the old name is still "required" but never reported, so PRs hang).
  Update this table and the branch-protection rule in the same change.
- New OSes added to the `test` matrix create new check names; add them as
  required here too.
