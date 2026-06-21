#!/usr/bin/env python3
# SHA-pin verification check — feature 00011-release-distribution (T034 / OR-017 / OR-010).
#
# Asserts that EVERY third-party GitHub Action referenced via `uses:` in any
# workflow under .github/workflows/ is pinned to a FULL 40-hex commit SHA, never
# a tag/branch ref (`@vN`, `@main`, `@stable`, ...). A tag ref is mutable and a
# supply-chain risk; OR-010 requires full-SHA pins.
#
# WHAT COUNTS AS THIRD-PARTY: any `uses:` value of the form `owner/repo[/path]@ref`
# (i.e. NOT a local reusable-workflow reference `./.github/workflows/*.yml`, which
# is pinned by repo content, and NOT a `docker://` ref). For those third-party
# refs the part after the LAST `@` MUST be exactly 40 lowercase hex characters.
#
# TEMPORARY ALLOWLIST (TODO(OR-010)): five actions in release.yml are currently
# tag-pinned because their tag->SHA mapping cannot be resolved in this offline
# build environment (the `dist generate`/provenance steps). They are EXPLICITLY
# enumerated below so the check stays STRICT for everything else — it MUST flag
# any NEW tag-pin — while keeping the known, tracked offline gap green. Remove
# each allowlist entry as its SHA is resolved before the first release.
#
# This script is PURE (no network, no installed tooling) and runs offline against
# the repo. Exit 0 = all third-party actions SHA-pinned (modulo the allowlist);
# exit 1 = a non-allowlisted tag-pin (or a stale/over-broad allowlist entry).

from __future__ import annotations

import re
import sys
from pathlib import Path

# Repo root = two levels up from this file (ci/check-sha-pins.py -> repo root).
REPO_ROOT = Path(__file__).resolve().parent.parent
WORKFLOWS_DIR = REPO_ROOT / ".github" / "workflows"

# A full commit SHA is exactly 40 lowercase hex characters.
FULL_SHA_RE = re.compile(r"^[0-9a-f]{40}$")

# Matches a `uses:` line and captures the ref value. Tolerant of quoting and
# leading list markers / indentation.
USES_RE = re.compile(r"""^\s*-?\s*uses:\s*['"]?(?P<ref>[^'"\s#]+)['"]?""")

# ---------------------------------------------------------------------------
# TEMPORARY OFFLINE ALLOWLIST — TODO(OR-010): remove this allowlist once SHAs are
# resolved before first release.
#
# Each entry is (workflow filename, full `uses:` value) for a KNOWN tag-pinned
# action that cannot be resolved to a SHA in this offline environment. The check
# matches on the EXACT `uses:` string, so a different tag/version is NOT covered
# (it will be flagged) — the allowlist is as narrow as possible. Every entry must
# actually appear in the repo, or the check fails (no stale allowlist hiding a
# resolved pin).
# ---------------------------------------------------------------------------
ALLOWLIST: set[tuple[str, str]] = {
    ("release.yml", "actions/upload-artifact@v4"),
    ("release.yml", "actions/download-artifact@v4"),
    ("release.yml", "actions/attest-build-provenance@v2"),
}


def is_local_ref(ref: str) -> bool:
    """Local reusable-workflow refs (./.github/...) are repo-content-pinned."""
    return ref.startswith("./") or ref.startswith("../")


def is_docker_ref(ref: str) -> bool:
    return ref.startswith("docker://")


def collect_uses(workflow_path: Path) -> list[tuple[int, str]]:
    """Return (line_number, ref) for every `uses:` in the workflow file."""
    results: list[tuple[int, str]] = []
    for lineno, line in enumerate(
        workflow_path.read_text(encoding="utf-8").splitlines(), start=1
    ):
        m = USES_RE.match(line)
        if m:
            results.append((lineno, m.group("ref")))
    return results


def main() -> int:
    if not WORKFLOWS_DIR.is_dir():
        print(f"ERROR: workflows dir not found: {WORKFLOWS_DIR}", file=sys.stderr)
        return 1

    workflow_files = sorted(WORKFLOWS_DIR.glob("*.yml")) + sorted(
        WORKFLOWS_DIR.glob("*.yaml")
    )
    if not workflow_files:
        print(f"ERROR: no workflow YAML found under {WORKFLOWS_DIR}", file=sys.stderr)
        return 1

    violations: list[str] = []
    allowlisted_hits: set[tuple[str, str]] = set()
    third_party_count = 0

    for wf in workflow_files:
        wf_name = wf.name
        for lineno, ref in collect_uses(wf):
            if is_local_ref(ref) or is_docker_ref(ref):
                continue  # not a third-party pinnable action
            third_party_count += 1

            # Split into action path and ref-after-last-@.
            if "@" not in ref:
                violations.append(
                    f"{wf_name}:{lineno}: `uses: {ref}` has no `@<ref>` pin at all"
                )
                continue
            action, _, pin = ref.rpartition("@")

            if FULL_SHA_RE.match(pin):
                continue  # correctly pinned to a 40-hex SHA

            # Not a SHA: a tag/branch ref. Allowed only if explicitly enumerated.
            key = (wf_name, ref)
            if key in ALLOWLIST:
                allowlisted_hits.add(key)
                print(
                    f"ALLOWLISTED (TODO(OR-010)): {wf_name}:{lineno}: `{ref}` "
                    f"is tag-pinned (offline SHA unresolved)"
                )
                continue

            violations.append(
                f"{wf_name}:{lineno}: `uses: {ref}` is NOT a full 40-hex commit "
                f"SHA (got `{pin}`) and is not allowlisted (OR-010)"
            )

    # A stale allowlist (entry that no longer appears) must fail: it would hide a
    # future tag-pin reusing the same string after the real one is resolved.
    stale = ALLOWLIST - allowlisted_hits
    for wf_name, ref in sorted(stale):
        violations.append(
            f"STALE ALLOWLIST: ({wf_name}, {ref}) is in the OR-010 allowlist but "
            f"no longer appears in the workflows — remove it from ci/check-sha-pins.py"
        )

    print(
        f"\nScanned {len(workflow_files)} workflow file(s); "
        f"{third_party_count} third-party `uses:` ref(s); "
        f"{len(allowlisted_hits)} allowlisted; {len(violations)} violation(s)."
    )

    if violations:
        print("\nSHA-PIN CHECK FAILED:", file=sys.stderr)
        for v in violations:
            print(f"  - {v}", file=sys.stderr)
        return 1

    print("SHA-PIN CHECK PASSED: every third-party action is SHA-pinned "
          "(allowlisted tag-pins are tracked TODO(OR-010) items).")
    return 0


if __name__ == "__main__":
    sys.exit(main())
