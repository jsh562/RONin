#!/usr/bin/env python3
# Release-workflow hardening (SC-007) static check — feature
# 00011-release-distribution (T036 / OR-015 / SC-007).
#
# Asserts the cross-workflow SC-007 invariants over release.yml + release-plz.yml
# (and confirms the gate-reuse for the release pipeline):
#
#   1. NEEDS-GATES: release.yml re-runs the E002 CI gates via
#      `uses: ./.github/workflows/gates.yml`, and every publish/upload-capable job
#      (`host`) transitively `needs:` that gate job — no publish from a red build
#      (OR-009).
#   2. LEAST-PRIVILEGE: each workflow declares a top-level `permissions:` that
#      grants NO write scope globally; write scopes appear only per-job.
#   3. NO FORK-PR SECRETS: neither workflow uses `pull_request` or
#      `pull_request_target`; release.yml triggers on tags only, release-plz on
#      push:main + tags. No `secrets.*` is referenced in any `pull_request*` path
#      (there is none).
#   4. LOCKED: every `cargo <build|test|publish|clippy>` invocation across the
#      release workflows passes `--locked` (OR-010). (cargo-semver-checks has no
#      `--locked` flag, so it is excluded; release-plz's publish wrapper is
#      exempted with a documented note — see release-plz.yml.)
#
# This is a STATIC YAML check (pyyaml). It does NOT run actionlint/dist/etc.
# PURE / offline. Exit 0 = all SC-007 statics hold; exit 1 otherwise.

from __future__ import annotations

import re
import sys
from pathlib import Path

try:
    import yaml
except ImportError:  # pragma: no cover
    print("ERROR: pyyaml is required (import yaml failed).", file=sys.stderr)
    sys.exit(1)

REPO_ROOT = Path(__file__).resolve().parent.parent
WORKFLOWS = REPO_ROOT / ".github" / "workflows"
RELEASE_YML = WORKFLOWS / "release.yml"
RELEASE_PLZ_YML = WORKFLOWS / "release-plz.yml"

GATES_REF = "./.github/workflows/gates.yml"

# cargo subcommands that build/resolve and therefore MUST be --locked.
# (cargo-semver-checks is excluded: it has no `--locked` flag and resolves against
# the committed Cargo.lock already.)
LOCKED_REQUIRED = re.compile(
    r"\bcargo\s+(build|test|clippy|publish)\b"
)
# `release-plz release`/`release-pr` wrap cargo publish internally; the committed
# lockfile + the blocking `cargo semver-checks` gate provide the equivalent
# guarantee (documented in release-plz.yml). Exempt those lines.
LOCKED_EXEMPT = re.compile(r"\brelease-plz\s+(release|release-pr)\b")


def load_yaml(path: Path):
    with path.open("rb") as fh:
        return yaml.safe_load(fh)


def check_no_fork_pr_triggers(name: str, wf: dict, violations: list[str]) -> None:
    # PyYAML parses the bareword `on:` key as boolean True in some YAML 1.1
    # readers; handle both 'on' and True keys.
    on = wf.get("on", wf.get(True))
    triggers: set[str] = set()
    if isinstance(on, dict):
        triggers = set(on.keys())
    elif isinstance(on, list):
        triggers = set(on)
    elif isinstance(on, str):
        triggers = {on}
    for forbidden in ("pull_request", "pull_request_target"):
        if forbidden in triggers:
            violations.append(
                f"{name}: forbidden trigger `{forbidden}` (no fork-PR secret "
                "exposure — OR-011 / SC-007)"
            )
    if not triggers:
        violations.append(f"{name}: could not determine workflow triggers")
    else:
        print(f"OK: {name} triggers = {sorted(triggers)} (no pull_request*)")


def check_top_level_least_privilege(name: str, wf: dict, violations: list[str]) -> None:
    perms = wf.get("permissions")
    if perms in ({}, None) or (isinstance(perms, dict) and not perms):
        print(f"OK: {name} top-level permissions grants nothing (least-privilege)")
        return
    if isinstance(perms, dict):
        write_scopes = [k for k, v in perms.items() if v == "write"]
        if write_scopes:
            violations.append(
                f"{name}: top-level permissions grants write scope(s) "
                f"{write_scopes} globally (must be per-job — OR-011 / SC-007)"
            )
        else:
            print(f"OK: {name} top-level permissions has no global write scope")
    else:
        violations.append(f"{name}: unexpected top-level permissions form: {perms!r}")


def _iter_run_commands(wf: dict):
    """Yield (job_name, step_index, command_line) for every line of every step
    `run:` block. Comments and YAML `name:` labels are NOT included — only the
    actual shell commands the runner executes."""
    for jname, jdef in (wf.get("jobs", {}) or {}).items():
        for idx, step in enumerate(jdef.get("steps", []) or []):
            run = step.get("run")
            if not run:
                continue
            for line in str(run).splitlines():
                yield jname, idx, line


def check_locked(name: str, wf: dict, violations: list[str]) -> None:
    """Scan only executed `run:` command lines (parsed from YAML) for cargo
    build/resolve invocations missing `--locked`. Operating on parsed `run:`
    blocks avoids false positives on comments / `name:` step labels."""
    bad: list[str] = []
    for jname, idx, line in _iter_run_commands(wf):
        stripped = line.strip()
        if stripped.startswith("#"):
            continue  # shell comment inside a run block
        if LOCKED_EXEMPT.search(stripped):
            continue
        if LOCKED_REQUIRED.search(stripped) and "--locked" not in stripped:
            bad.append(
                f"{name} [job {jname}, step {idx}]: cargo build/resolve without "
                f"--locked: {stripped}"
            )
    if bad:
        violations.extend(bad)
    else:
        print(f"OK: {name} all cargo build/resolve `run:` commands use --locked")


def check_release_needs_gates(wf: dict, violations: list[str]) -> None:
    jobs = wf.get("jobs", {}) or {}
    # find the job that uses gates.yml
    gate_jobs = [
        jname for jname, jdef in jobs.items()
        if str(jdef.get("uses", "")).strip() == GATES_REF
    ]
    if not gate_jobs:
        violations.append(
            f"release.yml: no job calls the reusable gates "
            f"(`uses: {GATES_REF}`) — release must re-run E002 gates (OR-009)"
        )
        return
    gate_job = gate_jobs[0]
    print(f"OK: release.yml re-runs E002 gates via job '{gate_job}' (OR-009)")

    # the publish job 'host' must transitively need the gate job.
    def needs_of(jname: str) -> list[str]:
        n = jobs.get(jname, {}).get("needs", [])
        if isinstance(n, str):
            return [n]
        return list(n or [])

    def transitively_needs(start: str, target: str) -> bool:
        seen: set[str] = set()
        stack = list(needs_of(start))
        while stack:
            cur = stack.pop()
            if cur == target:
                return True
            if cur in seen:
                continue
            seen.add(cur)
            stack.extend(needs_of(cur))
        return False

    if "host" not in jobs:
        violations.append("release.yml: publish job 'host' not found")
    elif not transitively_needs("host", gate_job):
        violations.append(
            f"release.yml: publish job 'host' does not (transitively) `needs:` "
            f"the gate job '{gate_job}' — could publish from a red build (OR-009)"
        )
    else:
        print(f"OK: release.yml 'host' transitively needs '{gate_job}' before publish")


def main() -> int:
    for p in (RELEASE_YML, RELEASE_PLZ_YML):
        if not p.is_file():
            print(f"ERROR: workflow not found: {p}", file=sys.stderr)
            return 1

    release = load_yaml(RELEASE_YML)
    release_plz = load_yaml(RELEASE_PLZ_YML)
    violations: list[str] = []

    # 1. needs-gates (release.yml).
    check_release_needs_gates(release, violations)

    # 2. least-privilege top-level perms.
    check_top_level_least_privilege("release.yml", release, violations)
    check_top_level_least_privilege("release-plz.yml", release_plz, violations)

    # 3. no fork-PR triggers.
    check_no_fork_pr_triggers("release.yml", release, violations)
    check_no_fork_pr_triggers("release-plz.yml", release_plz, violations)

    # 4. --locked everywhere required (scan parsed `run:` blocks only).
    gates = load_yaml(WORKFLOWS / "gates.yml")
    check_locked("release.yml", release, violations)
    check_locked("release-plz.yml", release_plz, violations)
    check_locked("gates.yml", gates, violations)

    print(f"\n{len(violations)} violation(s).")
    if violations:
        print("\nRELEASE-HARDENING (SC-007) CHECK FAILED:", file=sys.stderr)
        for v in violations:
            print(f"  - {v}", file=sys.stderr)
        return 1

    print(
        "RELEASE-HARDENING (SC-007) CHECK PASSED: release re-runs gates before "
        "publish; least-privilege top-level perms; no pull_request* triggers; "
        "--locked everywhere (OR-009/OR-010/OR-011/SC-007)."
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
