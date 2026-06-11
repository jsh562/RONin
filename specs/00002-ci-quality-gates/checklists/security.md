# Security Requirements Checklist: CI Quality Gates
**Created**: 2026-06-11 | **Feature**: [spec.md](../spec.md)

## Supply-Chain Scanning Completeness

- [X] CHK001 Are both supply-chain tools (cargo-audit for RustSec advisories AND cargo-deny) named as required, rather than treating either as optional or interchangeable? [Completeness, Spec §OR-005] <!-- Evaluator: Covered by spec.md §OR-005 (both `cargo audit` and `cargo deny check` named as MUST) and §OBJ3 -->
- [X] CHK002 Is the full scope of cargo-deny coverage specified (licenses, bans, AND advisories via deny.toml), rather than left as a generic "license check"? [Completeness, Spec §OR-005/IP-002] <!-- Evaluator: Covered by spec.md §OBJ3 rationale ("license/ban/advisory policy via deny.toml") and §Glossary (license/advisory/ban-policy) -->
- [X] CHK003 Is the relationship between deny.toml as the policy source and the cargo-deny job behavior stated unambiguously (which checks deny.toml governs)? [Clarity, Spec §IP-002/OR-005] <!-- Evaluator: Covered by spec.md §IP-002 ("cargo deny check consumes deny.toml at the repository root") and §OR-005 (waiver/license policy sourced from deny.toml) -->
- [X] CHK004 Does a requirement state that both scanners run within a single named `supply-chain` job, removing ambiguity about whether they are separate gate checks? [Clarity, Spec §OR-005/AD-003] <!-- Evaluator: Covered by spec.md §OR-005 ("in a single named `supply-chain` job") and Clarifications Q6; plan.md §AD-003 -->
- [X] CHK005 Is the trigger surface for supply-chain scanning fully enumerated (pull_request, push, AND scheduled cron), with no gap that would let a change skip scanning? [Completeness, Spec §OR-005/SC-004] <!-- Evaluator: Covered by spec.md §OR-005 ("on every pull request and push, AND on a recurring schedule (cron)") -->
- [X] CHK006 Are the tool versions required to be pinned, and is the pinning mechanism (marketplace action or locked install) defined unambiguously? [Clarity, Spec §OR-005/AD-003] <!-- Evaluator: Covered by spec.md §OR-005 ("with pinned tool versions"; "marketplace action or locked install") and plan.md §AD-003 (taiki-e/install-action, pinned) -->
- [X] CHK007 Does the spec define what "disallowed license" means by reference to deny.toml policy, rather than leaving the allowed/denied set undefined? [Completeness, Spec §OR-005] <!-- Evaluator: Covered by spec.md §OR-005 / §IP-002 — disallowed-license set is governed by deny.toml policy -->

## Advisory Hard-Fail Policy and Waiver Mechanism

- [X] CHK008 Is the hard-fail policy for any advisory or disallowed license stated as an absolute (MUST fail), with no implied soft-warning path? [Clarity, Spec §OR-005/SC-004] <!-- Evaluator: Covered by spec.md §OR-005 ("MUST hard-fail the run") and Clarifications ("Always hard-fail") -->
- [X] CHK009 Is the only permitted waiver path defined precisely (explicit, PR-reviewed, dated entry in deny.toml or the cargo-audit ignore-list)? [Completeness, Spec §OR-005/RR-002] <!-- Evaluator: Covered by spec.md §OR-005 and §RR-002 ("explicit, PR-reviewed, dated entry in deny.toml / the cargo-audit ignore-list") -->
- [X] CHK010 Does a requirement explicitly forbid silent overrides and CI-level overrides as waiver mechanisms? [Completeness, Spec §OR-005] <!-- Evaluator: Covered by spec.md §OR-005 ("never a silent or CI-level override") and Clarifications -->
- [X] CHK011 Is the "dated" attribute of a waiver entry stated as a requirement, so a waiver's age and review status are auditable? [Testability, Spec §OR-005/RR-002] <!-- Evaluator: Covered by spec.md §OR-005 / §RR-002 — waiver MUST be a "dated entry" -->
- [X] CHK012 Was the prior ambiguity between OR-005 hard-fail and the RR-002 waiver path resolved consistently across all sections (no surviving contradiction)? [Consistency, Spec §Clarifications/STF] <!-- Evaluator: Covered by spec.md §Clarifications (Q3) and §Stress-Test Findings (STF-001 note: "advisory hard-fail policy (OR-005) and the RR-002 waiver path are now consistent") -->
- [X] CHK013 Does the RR-002 runbook requirement define the escalation order (triage -> patch/upgrade -> dated waiver as last resort) rather than presenting the waiver as a co-equal option? [Clarity, Spec §RR-002] <!-- Evaluator: Covered by spec.md §RR-002 ("triage, patch/upgrade, or (only as a last resort) ... dated waiver") -->
- [X] CHK014 Is the failure-reporting expectation defined (the offending advisory/license is reported), so a hard-fail is actionable and not opaque? [Testability, Spec §SC-004] <!-- Evaluator: Covered by spec.md §SC-004 and §OBJ3 VC1 ("fails with the offending advisory/license reported") -->

## Action Pinning and Update Mechanism

- [X] CHK015 Is the SHA-pinning requirement scoped to ALL third-party actions, with no carve-out for "trusted" publishers? [Completeness, Plan §AD-005/HINT-001] <!-- Evaluator: Covered by plan.md §AD-005 ("Pin every third-party action by full commit SHA") and §HINT-001; reinforced by spec.md §OR-012 -->
- [X] CHK016 Does the requirement specify full commit SHA pinning rather than tag/major-version pinning, removing the weaker-pin ambiguity? [Clarity, Plan §AD-005] <!-- Evaluator: Covered by plan.md §AD-005 ("by full commit SHA", chosen over "by major tag") -->
- [X] CHK017 Is an update mechanism for pinned actions defined (Dependabot for github-actions) so pins do not silently rot or block security bumps? [Completeness, Plan §AD-005/HINT-001] <!-- Evaluator: Covered by plan.md §HINT-001 ("add Dependabot for `github-actions` to bump them") and §AD-005; reinforced by spec.md §OR-012 -->
- [X] CHK018 Is the SHA-pinning policy traceable to a stated security rationale (supply-chain hardening + reproducibility) rather than asserted without justification? [Traceability, Plan §AD-005] <!-- Evaluator: Covered by plan.md §AD-005 rationale ("supply-chain hardening + reproducibility") -->
- [X] CHK019 Does any requirement or success criterion make SHA-pinning verifiable (e.g., reviewable in the workflow YAML), or is it stated only as a plan hint? [Testability, Plan §AD-005/HINT-001] <!-- Evaluator: Resolved — added SC-009 (every third-party action pinned to a full commit SHA, reviewable in workflow YAML) and OR-012 to spec.md -->

## Least-Privilege and Token Posture

- [X] CHK020 Is a least-privilege GITHUB_TOKEN permissions requirement stated (read-only / minimal scope) for the gate jobs, or is token scope left unspecified? [Completeness, Spec §OR-011] <!-- Evaluator: Resolved — added OR-012 to spec.md (explicit least-privilege `permissions:` block granting GITHUB_TOKEN read-only scope; no gate job may request write scopes) + plan.md coverage-map row -->
- [X] CHK021 Is the default vs. explicit permissions posture addressed, so the workflow does not inherit broad write permissions implicitly? [Clarity, Spec §OR-011] <!-- Evaluator: Resolved — OR-012 requires an explicit `permissions:` block "rather than inheriting the repository's default token permissions"; SC-009 makes it verifiable -->
- [X] CHK022 Is the requirement that no gate job references any repository secret stated as an absolute, covering every job rather than only the supply-chain job? [Completeness, Spec §OR-011] <!-- Evaluator: Covered by spec.md §OR-011 ("All gate jobs MUST run without any repository secret") -->
- [X] CHK023 Is the boundary between gate-job secret posture and the E011 release-job secret needs (CARGO_REGISTRY_TOKEN) drawn clearly, so secrets are not assumed available in gates? [Consistency, Spec §OR-011/Scope-Excluded] <!-- Evaluator: Covered by spec.md §Operational Constraints ("publish tokens belong to E011") and §Scope/Excluded (release/publishing owned by E011) -->

## Fork-PR Safety and No-Secrets-in-Gates

- [X] CHK024 Is fork-PR safety stated as a first-class requirement (all gate jobs fully validate fork PRs without secrets) rather than only an edge case? [Completeness, Spec §OR-011/Edge Cases] <!-- Evaluator: Covered by spec.md §OR-011 (requirement: all gate jobs run without secrets so fork PRs are fully validated) and §Edge Cases -->
- [X] CHK025 Is the trigger choice for fork safety made explicit (pull_request, not pull_request_target), so fork PRs do not gain secret access? [Clarity, Spec §OR-011/HINT-005] <!-- Evaluator: Covered by plan.md §HINT-005 ("Use the `pull_request` trigger ... so fork PRs are fully validated") -->
- [X] CHK026 Does a success criterion or verification step make fork-PR validation observable (a fork PR produces the same gate results as an internal PR)? [Testability, Spec §OR-011] <!-- Evaluator: Resolved — added SC-010 to spec.md (a fork PR produces the same gate-job results as an equivalent internal PR, no job skipped/erroring for lack of a secret) -->
- [X] CHK027 Is the consequence of a fork PR lacking secrets reconciled with every gate job's requirements (no job depends on a secret to pass)? [Consistency, Spec §OR-011/Operational Constraints] <!-- Evaluator: Covered by spec.md §OR-011 and §Operational Constraints ("No repository secrets are required for any gate job") -->

## Scheduled Scan for Commit-Independent Detection

- [X] CHK028 Is the purpose of the scheduled scan stated explicitly (detect newly published advisories without a new commit), not just "run on a schedule"? [Clarity, Spec §OR-005/Edge Cases] <!-- Evaluator: Covered by spec.md §OBJ3 deliverable ("so newly published advisories are detected without a new commit") and §Edge Cases -->
- [X] CHK029 Is the scheduled cadence resolved to a concrete value (daily cron) rather than left as "deferred to planning"? [Completeness, Plan §AD-004] <!-- Evaluator: Covered by plan.md §AD-004 ("Daily cron (00:00 UTC)") -->
- [X] CHK030 Is the scheduled scan required to run the same checks as the per-change scan, so schedule and PR coverage cannot diverge? [Consistency, Spec §OR-005/SC-004] <!-- Evaluator: Covered by spec.md §OBJ3 ("A scheduled (cron) run of the same job") and §OR-005 (single job runs on PR/push AND schedule); SC-004 -->
- [X] CHK031 Is a measurable outcome defined for the scheduled scan surfacing a new advisory on the next run, making advisory-detection latency verifiable? [Testability, Spec §SC-004/AD-004] <!-- Evaluator: Covered by spec.md §SC-004 ("surfaces any newly published advisory on the next scheduled run") and §OBJ3 VC2 -->

## Reproducibility and Locked Dependency Set

- [X] CHK032 Is the committed-Cargo.lock requirement paired with a mandated `--locked` flag on build/test, so reproducibility is enforced not just intended? [Completeness, Spec §OR-007/SC-006] <!-- Evaluator: Covered by spec.md §OR-007 ("Cargo.lock MUST be committed and CI MUST build/test with `--locked`") and §SC-005/SC-006 -->
- [X] CHK033 Is "no silent dependency drift" stated as a verifiable outcome (same commit resolves an identical dependency set)? [Testability, Spec §SC-006] <!-- Evaluator: Covered by spec.md §SC-006 ("Re-running CI on the same commit resolves an identical dependency set (no silent dependency drift)") -->
- [X] CHK034 Is the ordering dependency stated (Cargo.lock must be committed before CI uses `--locked`) so a missing lockfile is treated as a defined failure, not a surprise? [Clarity, Spec §OR-007/HINT-003] <!-- Evaluator: Covered by plan.md §HINT-003 ("Ensure `Cargo.lock` is committed before CI uses `--locked` ... else CI fails on a missing lockfile") -->
- [X] CHK035 Is the cache-key derivation defined (toolchain + lockfile + OS/target) so caching cannot mask a dependency change relevant to security scanning? [Completeness, Spec §OR-006] <!-- Evaluator: Covered by spec.md §OR-006 ("keyed on toolchain, lockfile, and OS/target") and plan.md §AD-002 -->

## WASM-Clean Gate Security/Architecture Intent

- [X] CHK036 Is the security/architecture intent of the wasm32 gate stated (proving ron-core carries no filesystem/UI/async/native dependency per ADR-0002), not just "it compiles"? [Clarity, Spec §OBJ2/IP-003/Glossary] <!-- Evaluator: Covered by spec.md §OBJ2 ("enforce the ron-core WASM-clean invariant"), §IP-003, and §Glossary ("WASM-clean gate ... proves it carries no filesystem/UI/async/native dependency (ADR-0002)") -->
- [X] CHK037 Is the wasm32 gate's regression-detection behavior specified (a native/fs/async dependency leaking into ron-core fails the job)? [Testability, Spec §OBJ2/SC-003] <!-- Evaluator: Covered by spec.md §OBJ2 VC1 (introducing a filesystem/UI/async/native dependency fails the wasm32 job) and §SC-003 -->
- [X] CHK038 Is the wasm32 gate's scope correctly limited to ron-core (`-p ron-core`), so the architecture boundary it protects is unambiguous? [Clarity, Spec §OR-004/Plan §IP-003] <!-- Evaluator: Covered by spec.md §OBJ2 deliverable / §OR-004 and plan.md §IP-003 ("cargo build -p ron-core --target wasm32-unknown-unknown") -->

## Job Boundaries, Exclusions, and Traceability

- [X] CHK039 Is the exclusion of the nightly/cargo-fuzz `fuzz` crate from the stable matrix stated as a security/scope requirement, so it cannot silently break or be scanned out of context? [Completeness, Spec §OR-009/SC-007] <!-- Evaluator: Covered by spec.md §OR-009 ("MUST exclude the workspace-excluded fuzz crate from the stable build/test matrix") and §SC-007 -->
- [X] CHK040 Are all security-relevant gate jobs (supply-chain, wasm32) required to exist as discrete, individually selectable named checks, so each can be made a required merge gate? [Traceability, Spec §OR-010/SC-008] <!-- Evaluator: Covered by spec.md §OR-010 ("discrete, named jobs ... suitable for configuration as required status checks") and §SC-008 -->
