# ronin-types

The normalized **type model** for [RONin](https://github.com/jsh562/RONin) — a schema-optional, progressive type-awareness layer for RON.

`ronin-types` acquires type information from multiple sources (Rust source via `syn`, `schemars`-derived JSON Schema, and user-supplied JSON Schema) and normalizes them into a single `TypeModel` shaped on **JSON Schema 2020-12** plus `x-ron-*` extension keywords for RON-only constructs (tuples, `char`, non-string-key maps, `Option`/implicit-some, bytes, newtype-unwrap). Sources merge by a deterministic precedence; unresolved types degrade to first-class `unknown` nodes rather than errors.

The serialized `TypeModel` is the cross-boundary interchange the WASM-clean validator consumes; this crate is native-gated (it carries `syn`/`schemars`/`walkdir`) and is never pulled into the WASM-clean core.

Licensed under MIT OR Apache-2.0.
