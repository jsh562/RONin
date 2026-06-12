//! EOL re-emission round-trip tests (T033, FR-023/FR-020).
//!
//! The editor's `TextEdit` normalises every line ending in the live buffer to a
//! bare `\n`. On save, [`save_bytes`] must re-apply the file's load-time byte
//! fidelity so a load → (no edits) → save is **byte-identical** for uniform-EOL
//! files (CRLF or LF), regardless of BOM or trailing-newline presence.
//!
//! Each test below: takes raw original bytes → builds the fidelity profile from
//! them → simulates the widget normalising the decoded text to `\n` in the buffer
//! (stripping a leading BOM, which the editor keeps out of the buffer) → calls
//! `save_bytes` → asserts the output equals the **original** bytes.
//!
//! The single MIXED-EOL case is the documented limitation: a mixed file does NOT
//! round-trip byte-for-byte; it normalises to the dominant style (ties → LF).

use ronin_app::document::ByteFidelityProfile;
use ronin_app::fileio::save_bytes;

/// Decode `raw` to the editor buffer the way the shell does: validate UTF-8, drop
/// a leading BOM (kept on the profile, not in the buffer), and normalise every
/// line ending to a bare `\n` (what the `TextEdit` widget produces).
fn widget_buffer(raw: &[u8]) -> String {
    let text = std::str::from_utf8(raw).expect("fixture must be UTF-8");
    let without_bom = text.strip_prefix('\u{FEFF}').unwrap_or(text);
    without_bom.replace("\r\n", "\n").replace('\r', "\n")
}

/// Load → save with no edits must be byte-identical for `raw`.
fn assert_byte_identical(raw: &[u8], label: &str) {
    let profile = ByteFidelityProfile::from_bytes(raw);
    let buffer = widget_buffer(raw);
    let out = save_bytes(&buffer, &profile);
    assert_eq!(
        out, raw,
        "{label}: load→save must be byte-identical\n  expected: {raw:?}\n  got:      {out:?}"
    );
}

#[test]
fn uniform_crlf_roundtrips_byte_identical() {
    // CRLF throughout, trailing CRLF.
    assert_byte_identical(b"Config(\r\n    level: 3,\r\n)\r\n", "uniform CRLF");
}

#[test]
fn uniform_lf_roundtrips_byte_identical() {
    // LF throughout, trailing LF.
    assert_byte_identical(b"Config(\n    level: 3,\n)\n", "uniform LF");
}

#[test]
fn bom_present_roundtrips_byte_identical() {
    // UTF-8 BOM + LF content; the BOM must be re-emitted exactly once.
    let mut raw = vec![0xEF, 0xBB, 0xBF];
    raw.extend_from_slice(b"List([1, 2, 3])\n");
    assert_byte_identical(&raw, "BOM present (LF)");
}

#[test]
fn bom_present_crlf_roundtrips_byte_identical() {
    // BOM + CRLF content: both BOM and CRLF style must survive.
    let mut raw = vec![0xEF, 0xBB, 0xBF];
    raw.extend_from_slice(b"List([1, 2, 3])\r\n");
    assert_byte_identical(&raw, "BOM present (CRLF)");
}

#[test]
fn bom_absent_roundtrips_byte_identical() {
    // No BOM; must not gain one on save.
    assert_byte_identical(b"List([1, 2, 3])\n", "BOM absent");
}

#[test]
fn trailing_newline_present_roundtrips_byte_identical() {
    assert_byte_identical(b"Foo(x: 1)\n", "trailing newline present (LF)");
    assert_byte_identical(b"Foo(x: 1)\r\n", "trailing newline present (CRLF)");
}

#[test]
fn trailing_newline_absent_roundtrips_byte_identical() {
    // No trailing newline must not gain one on save.
    assert_byte_identical(b"Foo(x: 1)", "trailing newline absent (LF)");
    assert_byte_identical(b"a\r\nb", "trailing newline absent (CRLF)");
}

#[test]
fn empty_file_roundtrips_byte_identical() {
    assert_byte_identical(b"", "empty file");
}

#[test]
fn mixed_eol_normalises_to_dominant_not_byte_identical() {
    // DOCUMENTED LIMITATION (FR-020/FR-023): a genuinely mixed-EOL file does NOT
    // round-trip byte-for-byte. It normalises to the DOMINANT style.
    //
    // Two CRLF + one LF → dominant is CRLF; output is uniform CRLF, NOT the mixed
    // original.
    let raw = b"a\r\nb\r\nc\nd\r\n";
    let profile = ByteFidelityProfile::from_bytes(raw);
    let buffer = widget_buffer(raw);
    let out = save_bytes(&buffer, &profile);

    let expected_uniform = b"a\r\nb\r\nc\r\nd\r\n".to_vec();
    assert_eq!(
        out, expected_uniform,
        "mixed (CRLF-dominant) must normalise to uniform CRLF"
    );
    assert_ne!(
        out, raw,
        "mixed input is intentionally NOT byte-identical after save"
    );
}

#[test]
fn mixed_eol_tie_normalises_to_lf() {
    // One CRLF + one LF → a tie; the documented rule resolves ties to LF.
    let raw = b"a\r\nb\nc\n";
    let profile = ByteFidelityProfile::from_bytes(raw);
    let buffer = widget_buffer(raw);
    let out = save_bytes(&buffer, &profile);

    let expected_uniform = b"a\nb\nc\n".to_vec();
    assert_eq!(out, expected_uniform, "mixed-EOL tie must normalise to LF");
    assert_ne!(out, raw, "mixed tie input is not byte-identical after save");
}
