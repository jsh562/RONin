#!/usr/bin/env python3
# Static provenance + SBOM + cargo-auditable workflow-config check — feature
# 00011-release-distribution (T035 / OR-018 / SC-004 / SC-005).
#
# Because NO artifact is built in CI (OR-015), SC-004 (per-binary provenance) and
# SC-005 (one CycloneDX SBOM + cargo-auditable) are only PARTIALLY verifiable in
# CI — verification is limited to a STATIC workflow-configuration check (OR-018).
# This script asserts that `.github/workflows/release.yml` declares, scoped to the
# single publish/host job:
#
#   1. a per-binary keyless build-provenance step (actions/attest-build-provenance),
#   2. exactly ONE CycloneDX SBOM step (cargo cyclonedx),
#   3. the publish job carries `id-token: write` + `attestations: write`
#      + `contents: write` (and NO OTHER job carries id-token/attestations write),
#
# and that `dist-workspace.toml` sets `cargo-auditable = true`.
#
# The LIVE residual — a real attestation actually verifying via
# `gh attestation verify`, an SBOM actually attached, the binary's embedded
# metadata actually readable by `cargo audit bin`, and the OR-002 fail-whole
# orchestration — is DEFERRED to the release-readiness gate (SC-006 / RR-001),
# NOT silently dropped. See docs/runbooks/release.md.
#
# PURE / offline: parses YAML/TOML with pyyaml + tomllib (no installed CI tooling,
# no network). Exit 0 = all static assertions hold; exit 1 otherwise.

from __future__ import annotations

import sys
import tomllib
from pathlib import Path

try:
    import yaml
except ImportError:  # pragma: no cover - environment guard
    print("ERROR: pyyaml is required (import yaml failed).", file=sys.stderr)
    sys.exit(1)

REPO_ROOT = Path(__file__).resolve().parent.parent
RELEASE_YML = REPO_ROOT / ".github" / "workflows" / "release.yml"
DIST_TOML = REPO_ROOT / "dist-workspace.toml"

# The single write/publish job in release.yml (the only job allowed the write
# scopes). Modeled on dist's `host` job (T021).
PUBLISH_JOB = "host"

PROVENANCE_ACTION = "actions/attest-build-provenance"
SBOM_TOKEN = "cargo cyclonedx"


def job_steps(job: dict) -> list[dict]:
    return job.get("steps", []) or []


def step_uses(step: dict) -> str:
    return str(step.get("uses", "") or "")


def step_run(step: dict) -> str:
    return str(step.get("run", "") or "")


def main() -> int:
    if not RELEASE_YML.is_file():
        print(f"ERROR: release.yml not found: {RELEASE_YML}", file=sys.stderr)
        return 1
    with RELEASE_YML.open("rb") as fh:
        wf = yaml.safe_load(fh)

    jobs = (wf or {}).get("jobs", {}) or {}
    violations: list[str] = []

    if PUBLISH_JOB not in jobs:
        print(
            f"ERROR: expected publish job '{PUBLISH_JOB}' not found in release.yml "
            f"(jobs: {', '.join(jobs)})",
            file=sys.stderr,
        )
        return 1

    publish = jobs[PUBLISH_JOB]
    steps = job_steps(publish)

    # 1. provenance step present + scoped to the publish job.
    provenance_steps = [s for s in steps if PROVENANCE_ACTION in step_uses(s)]
    if not provenance_steps:
        violations.append(
            f"'{PUBLISH_JOB}' job has no `{PROVENANCE_ACTION}` provenance step "
            "(SC-004 / OR-006)"
        )
    else:
        print(f"OK: provenance step present in '{PUBLISH_JOB}' "
              f"({len(provenance_steps)} `{PROVENANCE_ACTION}` step)")

    # provenance must NOT appear in any non-publish job.
    for jname, jdef in jobs.items():
        if jname == PUBLISH_JOB:
            continue
        if any(PROVENANCE_ACTION in step_uses(s) for s in job_steps(jdef)):
            violations.append(
                f"provenance step found in non-publish job '{jname}' "
                "(must be scoped to the publish job only — OR-018)"
            )

    # 2. exactly ONE CycloneDX SBOM step in the publish job.
    sbom_steps = [s for s in steps if SBOM_TOKEN in step_run(s)]
    if len(sbom_steps) == 0:
        violations.append(
            f"'{PUBLISH_JOB}' job has no `{SBOM_TOKEN}` SBOM step (SC-005 / OR-007)"
        )
    elif len(sbom_steps) > 1:
        violations.append(
            f"'{PUBLISH_JOB}' job has {len(sbom_steps)} `{SBOM_TOKEN}` steps; "
            "exactly ONE SBOM per release is required (OR-007)"
        )
    else:
        print(f"OK: exactly one CycloneDX SBOM step in '{PUBLISH_JOB}'")

    # 3. publish job carries the three write scopes; no other job has id-token /
    #    attestations write.
    pub_perms = publish.get("permissions", {}) or {}
    for scope in ("contents", "id-token", "attestations"):
        if pub_perms.get(scope) != "write":
            violations.append(
                f"'{PUBLISH_JOB}' job permission `{scope}` is "
                f"{pub_perms.get(scope)!r}, expected 'write' (OR-018 / OR-011)"
            )
    if all(pub_perms.get(s) == "write" for s in ("contents", "id-token", "attestations")):
        print(f"OK: '{PUBLISH_JOB}' job has contents/id-token/attestations: write")

    for jname, jdef in jobs.items():
        if jname == PUBLISH_JOB:
            continue
        perms = jdef.get("permissions", {}) or {}
        for scope in ("id-token", "attestations"):
            if perms.get(scope) == "write":
                violations.append(
                    f"non-publish job '{jname}' has `{scope}: write` "
                    "(write attest/id-token scopes must be publish-job-only — OR-011)"
                )

    # workflow-level permissions must not grant the write scopes globally.
    top_perms = (wf or {}).get("permissions", {})
    if isinstance(top_perms, dict):
        for scope in ("contents", "id-token", "attestations"):
            if top_perms.get(scope) == "write":
                violations.append(
                    f"workflow-level `permissions.{scope}: write` grants a write "
                    "scope globally (must be per-job least-privilege — OR-011)"
                )

    # 4. dist-workspace.toml sets cargo-auditable = true.
    if not DIST_TOML.is_file():
        violations.append(f"dist-workspace.toml not found: {DIST_TOML}")
    else:
        with DIST_TOML.open("rb") as fh:
            dist_cfg = tomllib.load(fh)
        auditable = dist_cfg.get("dist", {}).get("cargo-auditable")
        if auditable is not True:
            violations.append(
                f"dist-workspace.toml [dist].cargo-auditable is {auditable!r}, "
                "expected true (SC-005 / OR-007)"
            )
        else:
            print("OK: dist-workspace.toml sets cargo-auditable = true")

    print(f"\n{len(violations)} violation(s).")
    if violations:
        print("\nPROVENANCE/SBOM STATIC CHECK FAILED:", file=sys.stderr)
        for v in violations:
            print(f"  - {v}", file=sys.stderr)
        return 1

    print(
        "PROVENANCE/SBOM STATIC CHECK PASSED: provenance + single CycloneDX SBOM "
        "+ cargo-auditable present and correctly scoped to the publish job "
        "(OR-018). Live verify deferred to the readiness gate (SC-006/RR-001)."
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
