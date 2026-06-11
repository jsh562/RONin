---
adr_id: ADR-0005
status: accepted
date: 2026-06-10
tags: [reliability, persistence, data-integrity]
supersedes: []
superseded_by: ""
related_artifacts: [specs/prd.md, specs/sad.md]
---

# ADR-0005: Non-Destructive Persistence and Crash Safety

## Status

Accepted.

## Context

An editor that can corrupt files is unusable; the PRD mandates "never silently corrupt." RONin needs a safe save path, crash recovery, and reliable undo/redo over the lossless CST, all for a local-first/offline desktop tool with filesystem-only storage.

## Decision Drivers

- Zero data loss on crash, power loss, or out-of-disk conditions.
- Preserve the original file until a write is known to have succeeded.
- Recoverable autosave that never clobbers the user's file.
- Reliable undo/redo.

## Considered Options

### Option A: Atomic save (temp-write + rename) with sidecar autosave/recovery and CST-backed undo/redo

Write to a temp file in the same directory, fsync, then atomically rename over the target; periodically autosave to a separate sidecar/swap recovery file (never overwriting the user file); prompt for crash recovery on reopen; and back undo/redo with the CST.

- **Pros**: Proven-safe; the original is untouched until commit; in-progress work is recoverable.
- **Cons**: Must manage temp-file and sidecar lifecycles and the same-filesystem rename constraint.

### Option B: In-place overwrite saves with autosave written directly onto the user's file

Overwrite the user's file in place on save, and write periodic autosaves directly onto the same file.

- **Pros**: Simplest possible implementation; no temp/sidecar bookkeeping.
- **Cons**: Power-loss/out-of-disk corruption (Atom-class data-loss failures).

## Decision Outcome

Chosen option: **A: atomic temp-write+rename, sidecar autosave/recovery, and CST-backed undo/redo** — it is the only option that delivers durability and recoverability, keeps the original file intact until a write is known to have succeeded, and never clobbers the user's file with autosave, satisfying the PRD mandate to never silently corrupt.

## Consequences

### Positive

- Durability and recoverability across crash, power loss, and out-of-disk conditions; safe, granular undo.

### Negative

- Must manage temp/sidecar files and honor the same-filesystem rename constraint.

### Neutral

- The undo unit pairs naturally with the CST from ADR-0001.

## Links

- PRD principle "Correctness is non-negotiable"
- PRD "large-file responsiveness / never corrupt" quality posture
- Related ADR-0001 (CST as the undo unit)
- External: atomic temp-write+rename save semantics (https://wiki.geany.org/config/all_you_never_wanted_to_know_about_file_saving)
- External: Atom data-loss case (https://github.com/atom/atom/issues/11406)
