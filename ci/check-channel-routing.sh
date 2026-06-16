#!/usr/bin/env bash
# Tag -> channel routing check (bash mirror) — feature 00011-release-distribution
# (T031 / OR-015(d) / OR-008 / SC-002).
#
# Executes the EXACT bash `[[ "$TAG" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]` test used by
# the `plan` job's `Derive release channel from tag suffix` step in
# `.github/workflows/release.yml`, against the plan-required sample tags, so CI
# exercises the real SHELL routing semantics (not just a Python re-implementation).
#
# The Python sibling `ci/check-channel-routing.py` additionally EXTRACTS the
# regex from release.yml and asserts the broader sample set + inverse-never
# property; run both in CI. This bash script is intentionally minimal and uses
# the same regex literal as the workflow.
#
# Exit 0 = every sample routed correctly (incl. inverse-never); exit 1 otherwise.
set -euo pipefail

# Routing rule — keep this regex byte-identical to release.yml's plan step.
route() {
  local tag="$1"
  if [[ "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    echo "stable"
  else
    echo "prerelease"
  fi
}

# (tag, expected) samples — the plan.md-required fixtures + inverse-never cases.
declare -a TAGS=(
  "v1.4.0:stable"
  "v0.1.0:stable"
  "v1.4.0-rc.1:prerelease"
  "v1.4.0-nightly:prerelease"
  "v1.4.0-beta:prerelease"
  "v1.4.0-unrecognized-suffix:prerelease"
)

fail=0
for entry in "${TAGS[@]}"; do
  tag="${entry%%:*}"
  expected="${entry##*:}"
  got="$(route "$tag")"
  if [[ "$got" == "$expected" ]]; then
    printf 'OK  %-30s -> %s\n' "$tag" "$got"
  else
    printf 'BAD %-30s -> %s (expected %s)\n' "$tag" "$got" "$expected" >&2
    fail=1
  fi
  # inverse-never assertions.
  if [[ "$expected" == "stable" && "$got" == "prerelease" ]]; then
    echo "INVERSE-NEVER: bare-semver $tag routed to pre-release" >&2
    fail=1
  fi
  if [[ "$expected" == "prerelease" && "$got" == "stable" ]]; then
    echo "INVERSE-NEVER: pre-release $tag routed to stable (silent-stable forbidden)" >&2
    fail=1
  fi
done

if [[ "$fail" -ne 0 ]]; then
  echo "CHANNEL-ROUTING (bash) CHECK FAILED" >&2
  exit 1
fi
echo "CHANNEL-ROUTING (bash) CHECK PASSED"
