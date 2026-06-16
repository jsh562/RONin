# ron-core

The lossless, error-tolerant **RON** (Rusty Object Notation) concrete-syntax-tree engine that powers [RONin](https://github.com/jsh562/RONin).

`ron-core` parses RON text into a full-fidelity CST (built on [`rowan`](https://crates.io/crates/rowan)) that preserves **every byte** — comments, whitespace, ordering, and trailing commas — so `print(parse(src)) == src`. Parsing is error-tolerant (it recovers and reports diagnostics rather than failing), and the crate exposes a typed AST, structural-edit transforms, a deterministic formatter, and a bounded CST-backed undo stack.

It is the **WASM-clean core** of the workspace: its only dependency is `rowan`, with no filesystem, UI, async-runtime, or network code, so it builds for `wasm32-unknown-unknown` and is reusable by future frontends (LSP, browser/VSCode).

Licensed under MIT OR Apache-2.0.
