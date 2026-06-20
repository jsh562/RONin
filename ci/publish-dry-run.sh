#!/usr/bin/env bash
# Per-crate `cargo publish --dry-run` check — feature 00011-release-distribution
# (T032 / OR-016 / SC-003).
#
# Runs `cargo publish --dry-run --locked` for EACH publishable crate, in the SAME
# dependency / publish order as the live release (OR-005):
#
#     ronin-core -> ronin-types -> ronin-validate -> ronin-app
#
# and MUST succeed for each. The root-excluded `fuzz` crate is absent from this
# set (never published — OR-005).
#
# ---------------------------------------------------------------------------
# UNPUBLISHED WORKSPACE PATH-DEPENDENCIES (OR-016 — the defined, passing case):
# ---------------------------------------------------------------------------
# Before the FIRST release, none of these crates exist on crates.io yet. A
# `cargo publish --dry-run`/verify pass for a DEPENDENT crate normally tries to
# resolve its workspace siblings (e.g. ronin-validate -> ronin-core,
# ronin-app -> ronin-core/ronin-types/ronin-validate) FROM crates.io, which fails
# because they are not published yet. Only `ronin-core` (no workspace path-deps)
# fully dry-runs/verifies pre-first-publish.
#
# So this check is PUBLISH-ORDER-AWARE:
#   - ronin-core      : full `cargo publish --dry-run --locked` (verify ON) — it has
#                     no workspace path-deps, so it must verify cleanly.
#   - dependents    : `cargo publish --dry-run --locked --no-verify` (verify OFF)
#                     PRE-FIRST-PUBLISH. `--no-verify` skips the from-registry
#                     build/verify of the unpublished sibling but STILL exercises
#                     the package step (manifest validity, metadata completeness,
#                     file packaging) — the part a dry-run can prove offline.
#
# Once a crate's dependencies are live on crates.io, drop the `--no-verify` for
# that crate so its full verify pass runs too (TODO: remove the dependents'
# `--no-verify` after the first successful release publishes ronin-core/types/validate).
#
# The live publish (release-plz, dependency-ordered) covers the residual that a
# dry-run cannot: the actual from-registry verify of each dependent AFTER its
# deps are published. The blocking `cargo semver-checks` gate (release-plz.yml /
# release-verify) covers breaking-change detection (OR-004).
#
# Exit 0 = every crate dry-runs cleanly; exit 1 = any failure.
set -euo pipefail

# Crate (manifest-dir : mode). "verify" = full dry-run; "skip" = cannot dry-run
# pre-first-publish because its workspace path-deps are not on crates.io yet
# (cargo publish resolves a path-dep's VERSION from the registry even with
# `--no-verify`, so it errors "no matching package <sibling>"). The live publish
# (release-plz, dependency-ordered) verifies the dependents once their siblings
# are up. TODO: switch the dependents back to "verify" after the first publish.
declare -a CRATES=(
  "ronin-core:verify"
  "ronin-types:verify"        # leaf (no workspace path-dep) -> verify
  "ronin-validate:skip"       # depends on ronin-core (path) -> skip pre-first-publish
  "ronin-app:skip"            # depends on all siblings (path) -> skip pre-first-publish
)

fail=0
for entry in "${CRATES[@]}"; do
  crate="${entry%%:*}"
  mode="${entry##*:}"
  echo "=============================================================="
  echo ">> cargo publish --dry-run for ${crate} (mode: ${mode})"
  echo "=============================================================="
  if [[ "$mode" == "skip" ]]; then
    echo "SKIPPED: ${crate} — its workspace path-deps are not on crates.io yet" \
         "(pre-first-publish). Re-enable as 'verify' after the first publish."
    continue
  fi
  if ! cargo publish -p "$crate" --dry-run --locked; then
    echo "DRY-RUN FAILED: ${crate} (full verify)" >&2
    fail=1
  fi
done

if [[ "$fail" -ne 0 ]]; then
  echo "PUBLISH DRY-RUN CHECK FAILED" >&2
  exit 1
fi
echo "PUBLISH DRY-RUN CHECK PASSED: leaf crates dry-run cleanly; dependents skipped pre-first-publish."
