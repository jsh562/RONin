#!/usr/bin/env bash
# Per-crate `cargo publish --dry-run` check — feature 00011-release-distribution
# (T032 / OR-016 / SC-003).
#
# Runs `cargo publish --dry-run --locked` for EACH publishable crate, in the SAME
# dependency / publish order as the live release (OR-005):
#
#     ron-core -> ron-types -> ron-validate -> ronin-app
#
# and MUST succeed for each. The root-excluded `fuzz` crate is absent from this
# set (never published — OR-005).
#
# ---------------------------------------------------------------------------
# UNPUBLISHED WORKSPACE PATH-DEPENDENCIES (OR-016 — the defined, passing case):
# ---------------------------------------------------------------------------
# Before the FIRST release, none of these crates exist on crates.io yet. A
# `cargo publish --dry-run`/verify pass for a DEPENDENT crate normally tries to
# resolve its workspace siblings (e.g. ron-validate -> ron-core,
# ronin-app -> ron-core/ron-types/ron-validate) FROM crates.io, which fails
# because they are not published yet. Only `ron-core` (no workspace path-deps)
# fully dry-runs/verifies pre-first-publish.
#
# So this check is PUBLISH-ORDER-AWARE:
#   - ron-core      : full `cargo publish --dry-run --locked` (verify ON) — it has
#                     no workspace path-deps, so it must verify cleanly.
#   - dependents    : `cargo publish --dry-run --locked --no-verify` (verify OFF)
#                     PRE-FIRST-PUBLISH. `--no-verify` skips the from-registry
#                     build/verify of the unpublished sibling but STILL exercises
#                     the package step (manifest validity, metadata completeness,
#                     file packaging) — the part a dry-run can prove offline.
#
# Once a crate's dependencies are live on crates.io, drop the `--no-verify` for
# that crate so its full verify pass runs too (TODO: remove the dependents'
# `--no-verify` after the first successful release publishes ron-core/types/validate).
#
# The live publish (release-plz, dependency-ordered) covers the residual that a
# dry-run cannot: the actual from-registry verify of each dependent AFTER its
# deps are published. The blocking `cargo semver-checks` gate (release-plz.yml /
# release-verify) covers breaking-change detection (OR-004).
#
# Exit 0 = every crate dry-runs cleanly; exit 1 = any failure.
set -euo pipefail

# Crate (manifest-dir : verify-mode). "verify" = full dry-run; "noverify" =
# --no-verify (unpublished-path-dep-safe pre-first-publish).
declare -a CRATES=(
  "ron-core:verify"
  "ron-types:verify"        # ron-types has NO workspace path-dep (leaf) -> verify
  "ron-validate:noverify"   # depends on ron-core (path) -> noverify pre-publish
  "ronin-app:noverify"      # depends on ron-core/ron-types/ron-validate -> noverify
)

fail=0
for entry in "${CRATES[@]}"; do
  crate="${entry%%:*}"
  mode="${entry##*:}"
  echo "=============================================================="
  echo ">> cargo publish --dry-run for ${crate} (mode: ${mode})"
  echo "=============================================================="
  if [[ "$mode" == "verify" ]]; then
    if ! cargo publish -p "$crate" --dry-run --locked; then
      echo "DRY-RUN FAILED: ${crate} (full verify)" >&2
      fail=1
    fi
  else
    # --no-verify: package the crate (manifest + metadata + file list) without
    # the from-registry verify build of its as-yet-unpublished siblings (OR-016).
    if ! cargo publish -p "$crate" --dry-run --locked --no-verify; then
      echo "DRY-RUN FAILED: ${crate} (--no-verify, unpublished path-deps)" >&2
      fail=1
    fi
  fi
done

if [[ "$fail" -ne 0 ]]; then
  echo "PUBLISH DRY-RUN CHECK FAILED" >&2
  exit 1
fi
echo "PUBLISH DRY-RUN CHECK PASSED: all 4 crates dry-run cleanly in dep order."
