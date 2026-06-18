#!/usr/bin/env python3
# Tag -> channel routing verification check — feature 00011-release-distribution
# (T031 / OR-015(d) / OR-008 / SC-002).
#
# Mirrors the routing rule encoded in `.github/workflows/release.yml`'s `plan`
# job (`Derive release channel from tag suffix` step):
#
#     if [[ "$TAG" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then prerelease=false
#     else prerelease=true
#
# i.e. a BARE semver tag `vX.Y.Z` (no `-` pre-release component) -> STABLE; ANY
# tag carrying a pre-release component (`-rc.N`, `-nightly`, or ANY other `-...`
# suffix) -> PRE-RELEASE. The rule keys on presence/absence of the pre-release
# component, so an unrecognized suffix routes to pre-release, NEVER silently to
# stable (OR-008 — "no silent default").
#
# This check asserts:
#   1. each sample tag routes to its expected channel (incl. the plan-required
#      samples v1.4.0 -> stable, v1.4.0-rc.1 / v1.4.0-nightly -> pre-release), and
#   2. the INVERSE-NEVER property (Edge Cases / SC-002): no bare-semver tag ever
#      routes to pre-release, and no pre-release/suffixed tag ever routes to
#      stable.
#
# To guarantee the check stays faithful to the workflow, it EXTRACTS the regex
# from release.yml at runtime (so a drift in the workflow's regex is caught here
# rather than silently diverging from a hard-coded copy).
#
# PURE / offline (stdlib only). Exit 0 = routing correct + inverse-never holds;
# exit 1 = any mismatch or the regex could not be located in release.yml.

from __future__ import annotations

import re
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
RELEASE_YML = REPO_ROOT / ".github" / "workflows" / "release.yml"

# The expected bare-semver pattern the workflow uses. We also extract the actual
# one from release.yml and assert they agree, so a workflow edit cannot silently
# change routing without this check noticing.
EXPECTED_STABLE_RE = r"^v[0-9]+\.[0-9]+\.[0-9]+$"

# Locate the `[[ "$TAG" =~ <regex> ]]` test inside release.yml. The regex value
# itself contains `[...]` character classes, so we anchor on the trailing ` ]]`
# of the bash test: capture everything between `=~ ` and the final ` ]]`.
WORKFLOW_REGEX_RE = re.compile(r'\[\[\s*"\$TAG"\s*=~\s*(?P<re>\^v\S.*?)\s*\]\]\s*;')

# Sample fixtures (plan.md "Sample fixtures"): (tag, expected_channel).
# "stable" == prerelease:false ; "prerelease" == prerelease:true.
SAMPLES: list[tuple[str, str]] = [
    # Bare semver -> stable.
    ("v1.4.0", "stable"),
    ("v0.1.0", "stable"),
    ("v10.20.30", "stable"),
    ("v1.0.0", "stable"),
    # Pre-release component -> pre-release (rc / nightly / arbitrary suffix).
    ("v1.4.0-rc.1", "prerelease"),
    ("v1.4.0-rc.2", "prerelease"),
    ("v1.4.0-nightly", "prerelease"),
    ("v1.4.0-beta", "prerelease"),
    ("v1.4.0-alpha.3", "prerelease"),
    ("v1.4.0-2026.06.15", "prerelease"),
    ("v1.4.0-unrecognized-suffix", "prerelease"),  # never silent-stable (OR-008)
    # Build metadata after `+` is still NOT a bare X.Y.Z match -> pre-release
    # (the workflow's `$` anchor rejects anything after PATCH).
    ("v1.4.0+build.7", "prerelease"),
]


def extract_workflow_regex() -> str | None:
    if not RELEASE_YML.is_file():
        print(f"ERROR: release.yml not found: {RELEASE_YML}", file=sys.stderr)
        return None
    text = RELEASE_YML.read_text(encoding="utf-8")
    m = WORKFLOW_REGEX_RE.search(text)
    if not m:
        return None
    return m.group("re")


def route(tag: str, stable_re: re.Pattern[str]) -> str:
    """Return 'stable' or 'prerelease' for a tag, per the workflow rule."""
    return "stable" if stable_re.match(tag) else "prerelease"


def main() -> int:
    # 1. Confirm the workflow's regex matches the expected one (anti-drift).
    wf_regex = extract_workflow_regex()
    if wf_regex is None:
        print(
            "ERROR: could not locate the tag-routing regex "
            '(`[[ "$TAG" =~ ... ]]`) in release.yml — routing rule not found.',
            file=sys.stderr,
        )
        return 1
    # Bash regex uses `\.`; Python's re treats `\.` identically. Normalize the
    # comparison on the literal string.
    if wf_regex != EXPECTED_STABLE_RE:
        print(
            "ERROR: release.yml stable-tag regex drifted.\n"
            f"  workflow:  {wf_regex}\n"
            f"  expected:  {EXPECTED_STABLE_RE}",
            file=sys.stderr,
        )
        return 1
    print(f"OK: release.yml stable-tag regex == {EXPECTED_STABLE_RE}")

    stable_re = re.compile(EXPECTED_STABLE_RE)
    violations: list[str] = []

    # 2. Each sample routes to its expected channel.
    for tag, expected in SAMPLES:
        got = route(tag, stable_re)
        status = "OK " if got == expected else "BAD"
        print(f"  {status} {tag:<28} -> {got:<10} (expected {expected})")
        if got != expected:
            violations.append(f"{tag}: routed to {got}, expected {expected}")

    # 3. Inverse-never property (SC-002 / Edge Cases): partition the samples and
    # assert the two impossible transitions never occur.
    for tag, expected in SAMPLES:
        got = route(tag, stable_re)
        if expected == "stable" and got == "prerelease":
            violations.append(f"INVERSE-NEVER: bare-semver {tag} routed to pre-release")
        if expected == "prerelease" and got == "stable":
            violations.append(
                f"INVERSE-NEVER: pre-release/suffixed {tag} routed to stable "
                "(silent-stable forbidden — OR-008)"
            )

    n_stable = sum(1 for _, e in SAMPLES if e == "stable")
    n_pre = sum(1 for _, e in SAMPLES if e == "prerelease")
    print(
        f"\nRouted {len(SAMPLES)} sample tag(s) "
        f"({n_stable} stable, {n_pre} pre-release); {len(violations)} violation(s)."
    )

    if violations:
        print("\nCHANNEL-ROUTING CHECK FAILED:", file=sys.stderr)
        for v in violations:
            print(f"  - {v}", file=sys.stderr)
        return 1

    print(
        "CHANNEL-ROUTING CHECK PASSED: bare vX.Y.Z -> stable, any pre-release "
        "component -> pre-release, and the inverse never occurs (OR-008/SC-002)."
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
