#!/usr/bin/env python3
# Manifest-metadata verification check — feature 00011-release-distribution
# (T033 / OR-017 / OR-014).
#
# Asserts that EACH of the four publishable workspace crates declares the
# crates.io-publishable metadata required for `cargo publish` to succeed:
#   repository, keywords, categories, readme
# A crate missing ANY of the four fails the check (OR-017). Fields inherited from
# `[workspace.package]` in the root Cargo.toml (via `field.workspace = true`)
# count as present, matching how cargo resolves them at publish time.
#
# The root-excluded `src/ronin-core/fuzz` crate is NOT a workspace member and is
# NEVER published (OR-005), so it is deliberately absent from the checked set.
#
# This script ALSO sanity-checks the surrounding release inputs named in OR-017:
#   - the committed Cargo.lock exists (reproducible --locked publish), and
#   - rust-toolchain.toml pins the stable channel (release toolchain — OR-014).
#
# This script is PURE (no network, no installed tooling beyond the stdlib) and
# runs offline. Python 3.11+ ships `tomllib`. Exit 0 = all crates complete;
# exit 1 = any crate missing a required field, or a missing release input.

from __future__ import annotations

import sys
import tomllib
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent

# The four publishable members, in dependency / publish order (OR-005). `fuzz` is
# intentionally NOT here (never published).
PUBLISHABLE_CRATES = [
    "src/ronin-core",
    "src/ronin-types",
    "src/ronin-validate",
    "src/ronin-app",
]

REQUIRED_FIELDS = ["repository", "keywords", "categories", "readme"]


def load_toml(path: Path) -> dict:
    with path.open("rb") as fh:
        return tomllib.load(fh)


def workspace_inherited_fields(root_manifest: dict) -> set[str]:
    """Fields available for `field.workspace = true` inheritance."""
    return set((root_manifest.get("workspace", {}).get("package", {})).keys())


def field_present(pkg: dict, field: str, inheritable: set[str]) -> bool:
    """A field is present if set directly, or inherited via `workspace = true`."""
    if field not in pkg:
        return False
    val = pkg[field]
    # Inherited form: { workspace = true }.
    if isinstance(val, dict) and val.get("workspace") is True:
        return field in inheritable
    # Directly set: any non-empty string / non-empty list counts.
    if isinstance(val, str):
        return val.strip() != ""
    if isinstance(val, list):
        return len(val) > 0
    return val is not None


def main() -> int:
    root_manifest_path = REPO_ROOT / "Cargo.toml"
    if not root_manifest_path.is_file():
        print(f"ERROR: root Cargo.toml not found: {root_manifest_path}", file=sys.stderr)
        return 1
    root_manifest = load_toml(root_manifest_path)
    inheritable = workspace_inherited_fields(root_manifest)

    violations: list[str] = []

    # --- the four publishable crates each declare all four fields (OR-017) ---
    for crate_dir in PUBLISHABLE_CRATES:
        manifest_path = REPO_ROOT / crate_dir / "Cargo.toml"
        if not manifest_path.is_file():
            violations.append(f"{crate_dir}/Cargo.toml: manifest not found")
            continue
        manifest = load_toml(manifest_path)
        pkg = manifest.get("package", {})
        name = pkg.get("name", crate_dir)

        # A publishable crate must NOT be marked publish = false.
        if pkg.get("publish") is False:
            violations.append(
                f"{name}: marked `publish = false` but is in the publishable set"
            )

        missing = [
            f for f in REQUIRED_FIELDS if not field_present(pkg, f, inheritable)
        ]
        if missing:
            violations.append(
                f"{name} ({crate_dir}/Cargo.toml): missing required publish "
                f"metadata: {', '.join(missing)}"
            )
        else:
            print(f"OK: {name} declares {', '.join(REQUIRED_FIELDS)}")

    # --- surrounding release inputs named in OR-017 ---
    lockfile = REPO_ROOT / "Cargo.lock"
    if not lockfile.is_file():
        violations.append("Cargo.lock not found (required for --locked publish, OR-010)")
    else:
        print("OK: committed Cargo.lock present")

    toolchain_path = REPO_ROOT / "rust-toolchain.toml"
    if not toolchain_path.is_file():
        violations.append("rust-toolchain.toml not found (release toolchain pin, OR-014)")
    else:
        toolchain = load_toml(toolchain_path)
        channel = toolchain.get("toolchain", {}).get("channel")
        if channel != "stable":
            violations.append(
                f"rust-toolchain.toml channel is '{channel}', expected 'stable' (OR-014)"
            )
        else:
            print("OK: rust-toolchain.toml pins the stable channel")

    print(
        f"\nChecked {len(PUBLISHABLE_CRATES)} publishable crate(s); "
        f"{len(violations)} violation(s)."
    )

    if violations:
        print("\nMANIFEST-METADATA CHECK FAILED:", file=sys.stderr)
        for v in violations:
            print(f"  - {v}", file=sys.stderr)
        return 1

    print(
        "MANIFEST-METADATA CHECK PASSED: every publishable crate declares "
        "repository + keywords + categories + readme (OR-014/OR-017)."
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
