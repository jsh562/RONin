# `ronin-core` round-trip corpus

This directory holds the RON fixture corpus for the byte-for-byte round-trip
gate (SC-001) and the corpus harness in `../corpus.rs` (TR-003, TR-015).

## Honesty note — these are hand-authored, representative fixtures

Real upstream Bevy `.scn.ron` scenes are **not bundled** here: they are not
available offline in this environment, and vendoring third-party scene files
would add provenance/licensing concerns. Every file in this corpus is
**hand-authored** (or, for the large file, **programmatically generated**) to be
*representative* of the RON surface RONin must round-trip — serde-style structs
and enums, plus Bevy-`DynamicScene`-shaped documents (`resources` + `entities`
maps). They are deliberately shaped like real files but are **not verbatim
copies** of any upstream source. When real corpus files become available they
should be added alongside these without removing them.

## Layout

- `valid/` — well-formed RON, one or more file per TR-004 construct group.
- `bevy/` — Bevy-scene-shaped (`*.scn.ron`) documents (hand-authored).
- `malformed/` — intentionally broken inputs (≥ 3). Their **error-recovered**
  trees must still re-print byte-for-byte and cover every input byte (INV-3),
  so they count toward the 100% round-trip denominator (SC-001).

## TR-004 construct coverage

| Construct group | Fixture(s) |
|-----------------|-----------|
| Struct (named) | `valid/01_struct_named.ron`, `valid/03_struct_nested.ron`, `valid/27_comments_line.ron` |
| Struct (anonymous) | `valid/02_struct_anonymous.ron`, `valid/20_bools.ron`, `valid/22_option_some_none.ron` |
| Enum / variant (unit) | `valid/04_enum_unit_variant.ron`, `bevy/scene_enums.scn.ron` |
| Enum / variant (tuple) | `valid/05_enum_tuple_variant.ron` |
| Enum / variant (struct) | `valid/06_enum_struct_variant.ron` |
| Tuple | `valid/07_tuple_simple.ron`, `valid/08_tuple_mixed.ron` |
| List / seq | `valid/09_list_simple.ron`, `valid/10_list_trailing_comma.ron`, `valid/11_list_nested.ron` |
| Map (string keys) | `valid/12_map_string_keys.ron` |
| Map (non-string keys) | `valid/13_map_nonstring_keys.ron`, `valid/14_map_tuple_keys.ron` |
| Char literals | `valid/15_chars.ron` |
| Strings (escapes/unicode) | `valid/16_strings_escapes.ron` |
| Raw strings (quotes/hashes) | `valid/17_raw_strings.ron` |
| Numbers (int: hex/oct/bin/underscores/suffix) | `valid/18_numbers_int_forms.ron` |
| Numbers (float: exponent/suffix) | `valid/19_numbers_float_forms.ron` |
| Bools | `valid/20_bools.ron` |
| Unit `()` | `valid/21_unit.ron` |
| Option / implicit_some | `valid/22_option_some_none.ron`, `valid/23_implicit_some.ron` |
| Extension attrs (known/multiple/unknown) | `valid/23_implicit_some.ron`, `valid/24_extension_unwrap_newtypes.ron`, `valid/25_extension_multiple.ron`, `valid/26_extension_unknown.ron` |
| Comments / trivia (line/block/nested) | `valid/27_comments_line.ron`, `valid/28_comments_block.ron`, `valid/29_comments_only.ron` |
| Edge: empty / whitespace-only | `valid/31_empty.ron`, `valid/32_whitespace_only.ron` |
| Edge: CRLF / BOM / no trailing newline | `valid/33_crlf_line_endings.ron`, `valid/34_bom_prefixed.ron`, `valid/35_no_trailing_newline.ron` |
| Deep mixed (many constructs) | `valid/30_deep_mixed.ron` |
| Large (≥ 1 MB) | `valid/36_large_scene.scn.ron` (~1.1 MB, generated) |
| Bevy-scene-shaped | `bevy/scene_small.scn.ron`, `bevy/scene_enums.scn.ron` |
| Malformed (recovery) | `malformed/01_unclosed_delimiters.ron`, `malformed/02_stray_tokens.ron`, `malformed/03_partial_value.ron`, `malformed/04_missing_separators.ron` |

## Counts

- Total fixtures: **42** (36 `valid/` + 2 `bevy/` + 4 `malformed/`).
- Malformed: **4** (≥ 3 required).
- Large file ≥ 1 MB: `valid/36_large_scene.scn.ron` (≥ 1 required).

## Regenerating the large file

`valid/36_large_scene.scn.ron` is generated, not authored by hand. It is
committed so tests are deterministic and offline. To regenerate it, run the
generator described in `../corpus.rs` (the harness asserts the file is ≥ 1 MB).
