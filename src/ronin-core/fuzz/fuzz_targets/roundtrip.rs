//! cargo-fuzz no-panic + lossless-round-trip target for `ronin-core` (SC-003).
//!
//! For arbitrary raw bytes this asserts the OBJ2 contract:
//!
//! * `parse` / `parse_bytes` never panic on any input (TR-001/TR-005);
//! * for every **accepted** (valid-UTF-8) input — valid or malformed RON — the
//!   concatenation of the tree's token texts equals the input byte-for-byte
//!   (INV-1/INV-2/INV-3);
//! * non-UTF-8 input is rejected cleanly via `Err` (never a panic) (INV-4).
//!
//! # Running (NIGHTLY only)
//!
//! ```text
//! cargo install cargo-fuzz
//! cargo +nightly fuzz run roundtrip -- -runs=1000000   # SC-003 CI gate
//! ```
//!
//! This requires the nightly toolchain + libFuzzer and is NOT runnable on a
//! stable-only machine. The identical assertions are mirrored on **stable** via
//! proptest in `src/ronin-core/tests/roundtrip.rs` (the "T028 — stable-toolchain
//! fuzz fallback" section), which is what runs in CI on stable; the
//! ≥ 1,000,000-iteration seeded run here is the dedicated nightly fuzz gate.

#![no_main]

use libfuzzer_sys::fuzz_target;

fn token_concat(doc: &ronin_core::CstDocument) -> String {
    doc.root()
        .descendant_tokens()
        .map(|t| t.text().to_string())
        .collect()
}

fuzz_target!(|data: &[u8]| {
    match ronin_core::parse_bytes(data) {
        Ok(doc) => {
            // Accepted ⇒ valid UTF-8. The tree must cover every input byte and
            // round-trip byte-for-byte (INV-1/INV-2/INV-3).
            let s = std::str::from_utf8(data).expect("parse_bytes Ok implies valid UTF-8");
            let concat = token_concat(&doc);
            assert_eq!(concat, s, "token concat must equal input");
            assert_eq!(ronin_core::print(&doc), s, "print must round-trip");
            // Diagnostics never point outside the source.
            for d in doc.diagnostics() {
                assert!(d.range().end() <= doc.source_len());
            }
        }
        Err(_) => {
            // Rejected ⇒ the bytes were not valid UTF-8 (INV-4). Clean Err, no
            // panic, and the input is never represented in a tree.
            assert!(std::str::from_utf8(data).is_err());
        }
    }
});
