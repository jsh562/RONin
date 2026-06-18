# ronin-validate

The **WASM-clean, type-aware validation engine** for [RONin](https://github.com/jsh562/RONin).

`ronin-validate` takes the serialized `TypeModel` interchange (JSON Schema 2020-12 + `x-ron-*`), projects a RON CST to a `serde_json` value with a JSON-Pointer → CST-`TextRange` index, runs [`jsonschema`](https://crates.io/crates/jsonschema) (with `default-features = false`, fully offline), and emits `ronin-core`-compatible diagnostics with precise byte ranges under a stable `RON-V####` code namespace. It also exposes a generic subtree-vs-named-type entry used by scene-aware validation.

It depends only on `ronin-core`, `jsonschema`, and `serde_json` — no network/TLS resolver — so it stays fully offline and builds for `wasm32-unknown-unknown`, reusable by a future LSP.

Licensed under MIT OR Apache-2.0.
