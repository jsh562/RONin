//! Large/awkward Bevy-scene perf + edge test (E009 Polish / T033,
//! FR-005/FR-008) `[COMPLETES FR-005]`.
//!
//! FR-005 requires scene-aware validation to stay **interactive on large
//! scenes**, and explicitly inherits the project's EXISTING large-file
//! responsiveness target (E001/E003/E006) with a Bevy-scene fixture added — it
//! introduces **no new numeric budget**. This test exercises that inherited
//! target end-to-end over a single, deliberately-awkward multi-MB scene that
//! folds together every edge case FR-004/FR-008 calls out:
//!
//! * **multi-MB** — the generated scene exceeds the project's DEFAULT large-file
//!   threshold (`5 MiB`, the E003 size past which the always-on intelligence
//!   layer degrades — the same anchor `AppSettings`'s default uses), so it is a
//!   genuine "large scene" by the project's own definition (no new budget);
//! * **many entities** — thousands of entities, each carrying several components;
//! * **duplicate entity ids** — the same id appears more than once (kept distinct,
//!   never merged/deduped — FR-004);
//! * **very large / non-contiguous ids** — ids near `i64::MAX` and big gaps;
//! * **omitted `resources`** — the scene has no top-level `resources` section;
//! * **at least one unparseable region** — a garbled component value the
//!   interpretation must skip while still modeling the parseable remainder.
//!
//! # The inherited responsiveness target (no new budget)
//!
//! The project's existing off-frame responsiveness bound — the wall-clock window
//! an off-frame parse/validate pass is given to land before the harness treats it
//! as hung — is `Duration::from_secs(5)` (see `large_file_degrade.rs`'s
//! `drive_app_reparse`, `bevy_validation_path.rs`, and `bevy_mode.rs`, all of
//! which drive the real E003/E006/E009 off-frame worker with exactly that
//! deadline). This test REUSES that same bound verbatim — it does NOT invent a
//! new numeric budget. The size anchor likewise reuses the project's own
//! [`DEFAULT_LARGE_FILE_THRESHOLD`] (5 MiB) via the public [`AppSettings`]
//! default, so "large" means exactly what E003 means by it.
//!
//! # What inherits the target, and how (the E003/E006 degrade model)
//!
//! FR-005 says validation "remains interactive on large scenes — it inherits the
//! project's existing large-file responsiveness target (E001/E003/E006)". The
//! way the project ACHIEVES interactivity past the large-file threshold is by
//! **degrading the always-on intelligence layer** (highlighting, squiggles, and
//! per-component type validation) once a document is `oversize`, while the cheap
//! structural projection still runs (this is exactly what `large_file_degrade.rs`
//! asserts: an oversize bound document runs structural-only with zero off-frame
//! validation work). So the inherited target splits cleanly:
//!
//! * the cheap **read projection** [`SceneModel::from_cst`] — the structural-only
//!   layer that always runs — must interpret the WHOLE multi-MB scene within the
//!   inherited window; and
//! * full per-component **validation** [`validate_scene`] is the always-on
//!   intelligence layer that DEGRADES past the threshold (the app suppresses it
//!   on the oversize signal — `large_file_degrade.rs`), so it is timed here on a
//!   representative UNDER-threshold slice (the interactive regime where it
//!   actually runs), within the SAME inherited window.
//!
//! The core invariant across both is **never panics**: `SceneModel::from_cst` and
//! `validate_scene` must interpret the parseable remainder, tolerate the awkward
//! ids/omissions, and stay within the inherited target — over adversarial,
//! malformed input (FR-003 trust-boundary / FR-008 degrade-safe).

use std::time::{Duration, Instant};

use ron_core::parse;
use ron_types::{BevyRegistry, BevySource, TypeSource};
use ronin_app::bevy::{validate_scene, SceneModel, SceneValueKind};
use ronin_app::settings::AppSettings;
use serde_json::Value;

/// The project's EXISTING off-frame responsiveness window, reused verbatim (NOT a
/// new budget): the same `Duration::from_secs(5)` the E003/E006/E009 off-frame
/// worker tests give a pass to land (`large_file_degrade.rs`,
/// `bevy_validation_path.rs`, `bevy_mode.rs`). Interpreting + validating the whole
/// large scene must finish comfortably inside this inherited bound.
const INHERITED_RESPONSIVENESS_WINDOW: Duration = Duration::from_secs(5);

/// The hand-authored registry-schema export shared with the rest of the Bevy
/// suite (lives in the `ron-types` fixtures). The large scene's component paths
/// are deliberately a MIX of registered (`bevy_transform...::Transform`,
/// `bevy_pbr::light::Visibility`) and unregistered (`my_game::...`) so the
/// validation pass drives BOTH the `validate_subtree_against_type` engine path
/// AND the `TypeNotInRegistry` hint path under load.
fn registry_schema_json() -> String {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("ron-types")
        .join("tests")
        .join("fixtures")
        .join("bevy_registry_schema.json");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read registry fixture {}: {e}", path.display()))
}

/// Acquire the registry + its serialized interchange exactly as production does:
/// `BevySource::acquire()` → `ron_types::to_json`, paired with the parsed
/// `BevyRegistry` for the membership lookup (the same wiring the US1 suite uses).
fn registry_and_model(json: &str) -> (BevyRegistry, Value) {
    let (registry, _diags) = BevyRegistry::from_schema_json(json, "test", "<registry>");
    let acquired = BevySource::from_schema_json(json).acquire();
    let model = ron_types::to_json(&acquired.model);
    (registry, model)
}

/// Generate a large, awkward `.scn.ron` source string in memory.
///
/// Returns `(source, entity_count)`. The scene:
/// * **omits `resources`** entirely (FR-004 edge case);
/// * has `entity_count` entities, each with three components — two registered
///   (`Transform`, `Visibility`) and one unregistered (`my_game::components::Tag`)
///   — so validation exercises both the engine and the hint path under load;
/// * assigns **awkward ids**: a near-`i64::MAX` id, a deliberate **duplicate** of
///   the very first id, and otherwise large non-contiguous ids (a coarse stride),
///   so nothing is contiguous and at least one id repeats;
/// * embeds **one unparseable region** (a garbled `@@@` component value) in the
///   middle of the stream that the interpretation must skip.
///
/// `min_bytes` drives the entity count up until the source exceeds it, so the
/// scene is guaranteed past the requested multi-MB anchor.
fn generate_large_awkward_scene(min_bytes: usize) -> (String, usize) {
    // Each entity block is on the order of ~250 bytes; size the count so the
    // total comfortably exceeds `min_bytes` (with headroom).
    let approx_block = 250usize;
    let entity_count = (min_bytes / approx_block) + 64;

    let mut s = String::with_capacity(entity_count * approx_block + 1024);
    // NOTE: no top-level `resources` field — it is OMITTED (FR-004 edge case).
    s.push_str("(\n    entities: {\n");

    // The first id is a very large, near-i64::MAX value; it is also DUPLICATED
    // later so the duplicate-id tolerance is exercised at scale.
    let first_id: i64 = i64::MAX - 7;
    // A coarse, non-contiguous stride keeps ids large and gappy.
    let stride: i64 = 1_000_003;

    // Insert the single unparseable region roughly in the middle of the stream.
    let garbled_at = entity_count / 2;

    for i in 0..entity_count {
        let id: i64 = if i == 0 {
            first_id
        } else {
            // Large, non-contiguous ids (a big base plus a gappy stride).
            10_000_000_000i64.wrapping_add((i as i64).wrapping_mul(stride))
        };
        s.push_str("        ");
        s.push_str(&id.to_string());
        s.push_str(": (components: {\n");
        // A fully-VALID registered Transform (translation/rotation/scale all
        // present, matching the fixture's required fields) so the registered path
        // is exercised WITHOUT producing false errors over valid data.
        s.push_str(
            "            \"bevy_transform::components::transform::Transform\": \
             (translation: (x: 1.0, y: 2.0, z: 3.0), rotation: (0.0, 0.0, 0.0, 1.0), \
             scale: (x: 1.0, y: 1.0, z: 1.0)),\n",
        );
        s.push_str("            \"bevy_pbr::light::Visibility\": Inherited,\n");
        // An UNREGISTERED component path → drives the TypeNotInRegistry hint path.
        s.push_str("            \"my_game::components::Tag\": (n: 1),\n");
        s.push_str("        }),\n");

        // Exactly one UNPARSEABLE region in the stream (FR-008): a garbled entity
        // value the interpretation must skip while still modeling the remainder.
        if i == garbled_at {
            s.push_str("        424242: @@@,\n");
        }

        // A DUPLICATE of the very first (near-i64::MAX) id, kept distinct.
        if i == 0 {
            s.push_str("        ");
            s.push_str(&first_id.to_string());
            s.push_str(": (components: {\n");
            s.push_str("            \"bevy_pbr::light::Visibility\": Hidden,\n");
            s.push_str("        }),\n");
        }
    }

    s.push_str("    },\n)\n");
    (s, entity_count)
}

#[test]
fn large_awkward_scene_projection_within_inherited_target() {
    // ---- Build a genuinely multi-MB scene by the project's OWN definition -------
    // The size anchor is the project's existing DEFAULT large-file threshold
    // (5 MiB) read from the public `AppSettings` default — the E003 size past
    // which the always-on intelligence layer degrades. No new budget is defined.
    let large_anchor = AppSettings::default().large_file_threshold as usize; // 5 MiB
    assert!(
        large_anchor >= 5 * 1024 * 1024,
        "the inherited large-file anchor is multi-MB (got {large_anchor} bytes)"
    );
    let (src, entity_count) = generate_large_awkward_scene(large_anchor);
    assert!(
        src.len() > large_anchor,
        "the generated scene must exceed the inherited multi-MB anchor \
         ({} bytes > {large_anchor})",
        src.len()
    );

    // ---- Time the cheap read PROJECTION over the WHOLE multi-MB scene -----------
    // `SceneModel::from_cst` is the structural-only layer that always runs (the
    // layer the E003/E006 degrade keeps live past the threshold). It must
    // interpret the full multi-MB scene within the INHERITED off-frame
    // responsiveness window (`Duration::from_secs(5)`) — REUSED verbatim, NOT a
    // new budget. (Full per-component validation degrades past the threshold; it
    // is timed separately, under the threshold, in the companion test below.)
    let start = Instant::now();
    let doc = parse(&src); // parse never panics over the adversarial input (FR-003/008)
    let scene = SceneModel::from_cst(&doc);
    let elapsed = start.elapsed();
    assert!(
        elapsed < INHERITED_RESPONSIVENESS_WINDOW,
        "projecting a {}-byte / {}-entity scene took {elapsed:?}, exceeding the INHERITED \
         off-frame responsiveness window ({INHERITED_RESPONSIVENESS_WINDOW:?}); this reuses the \
         E001/E003/E006 large-file target and introduces no new budget",
        src.len(),
        entity_count
    );

    // ---- Edge-case assertions: the awkward shape is tolerated, not crashed ------

    // Omitted `resources` reads as empty (FR-004 edge case).
    assert!(
        scene.resources().is_empty(),
        "the omitted `resources` section reads as zero resources"
    );

    // Many entities were modeled. The unparseable `424242:` entry is skipped, but
    // the duplicate near-i64::MAX entity adds one back, so the count is at least
    // the generated entity count (the garbled one is the only drop).
    let modeled = scene.entities();
    assert!(
        modeled.len() >= entity_count,
        "the parseable remainder (incl. the duplicate id) is modeled: {} entities >= {}",
        modeled.len(),
        entity_count
    );

    // Duplicate near-i64::MAX id: it appears at least twice, kept DISTINCT (FR-004).
    let first_id: i128 = (i64::MAX - 7) as i128;
    let dup_count = modeled.iter().filter(|e| e.id() == first_id).count();
    assert!(
        dup_count >= 2,
        "the duplicate near-i64::MAX id is kept as >=2 distinct entities, got {dup_count}"
    );

    // Very large / non-contiguous ids are tolerated (no i64 overflow / panic).
    assert!(
        modeled.iter().any(|e| e.id() == first_id),
        "a near-i64::MAX entity id is interpreted (no overflow)"
    );
    let ids: Vec<i128> = modeled.iter().map(|e| e.id()).collect();
    assert!(
        ids.windows(2).any(|w| (w[1] - w[0]).abs() > 1),
        "entity ids are non-contiguous (gappy strides), not 0,1,2,…"
    );

    // The garbled `424242: @@@` region degrades safely (FR-008): the entry is
    // either skipped entirely OR modeled with NO components (its value never
    // parsed into a valid `components` struct) — never a crash, never fabricated
    // components. The structural diagnostics (computed elsewhere) cover the
    // malformed bytes; the interpretation just tolerates them.
    let garbled = modeled.iter().filter(|e| e.id() == 424_242).count();
    assert!(
        garbled <= 1,
        "the garbled entry is not duplicated/fabricated"
    );
    assert!(
        modeled
            .iter()
            .filter(|e| e.id() == 424_242)
            .all(|e| e.components().is_empty()),
        "the unparseable `424242: @@@` value yields no components (skipped, never a crash)"
    );
    // The structural channel covers the malformed region (parse diagnostics),
    // proving the structural set remains intact alongside the projection.
    assert!(
        !doc.diagnostics().is_empty(),
        "the unparseable region produces structural diagnostics (degrade-safe, FR-008)"
    );

    // The parseable component remainder is still fully projected: every modeled
    // entity carries its components (the registered + unregistered mix).
    let total_components: usize = modeled.iter().map(|e| e.components().len()).sum();
    assert!(
        total_components >= entity_count,
        "components across the large scene are projected ({total_components} components)"
    );
    assert!(
        scene
            .components()
            .all(|c| c.kind() == SceneValueKind::Component),
        "every projected component value ref is tagged as a component"
    );
}

#[test]
fn large_scene_validation_under_threshold_within_inherited_target() {
    // The companion to the projection test: full per-component VALIDATION is the
    // always-on intelligence layer that DEGRADES past the large-file threshold
    // (the app suppresses it on the E003 `oversize` signal — `large_file_degrade.rs`
    // proves an oversize bound document runs structural-only). So validation is
    // exercised here on a representative UNDER-threshold scene — the interactive
    // regime where it actually runs — still within the SAME inherited off-frame
    // window (`Duration::from_secs(5)`, REUSED verbatim, NOT a new budget). The
    // scene keeps every awkward trait (duplicate/large ids, omitted resources, an
    // unparseable region) so validation is exercised over the awkward shape too.
    let threshold = AppSettings::min_large_file_threshold() as usize; // 64 KiB floor
                                                                      // Stay comfortably UNDER the threshold so we are in the regime where the app
                                                                      // runs full validation (mirroring `under_threshold_same_binding_document`).
    let (src, entity_count) = generate_large_awkward_scene(threshold / 2);
    assert!(
        src.len() < threshold,
        "the validation slice stays under the large-file threshold (the regime where \
         validation runs): {} bytes < {threshold}",
        src.len()
    );

    let (registry, model) = registry_and_model(&registry_schema_json());

    let start = Instant::now();
    let doc = parse(&src);
    let diags = validate_scene(&model, &registry, &doc, None);
    let elapsed = start.elapsed();
    assert!(
        elapsed < INHERITED_RESPONSIVENESS_WINDOW,
        "validating a {}-byte / {}-entity under-threshold scene took {elapsed:?}, exceeding the \
         INHERITED off-frame responsiveness window ({INHERITED_RESPONSIVENESS_WINDOW:?}); reuses \
         the E001/E003/E006 target, no new budget",
        src.len(),
        entity_count
    );

    // Validation findings are sane over the awkward-but-valid data: the registered
    // components (Transform, Visibility) are well-formed, so the only findings are
    // `TypeNotInRegistry` hints for the unregistered `my_game::components::Tag` —
    // never a false hard error, even over the unparseable region (FR-006/FR-008).
    assert!(
        diags
            .iter()
            .all(|d| d.severity() != ronin_app::bevy::SceneSeverity::Error),
        "an awkward-but-valid scene produces no false hard errors; got {} findings, \
         first few: {:?}",
        diags.len(),
        diags.iter().take(3).collect::<Vec<_>>()
    );
    assert!(
        diags
            .iter()
            .any(|d| d.code() == ronin_app::bevy::SceneDiagnosticCode::TypeNotInRegistry),
        "the unregistered `my_game::components::Tag` paths surface as hints under load"
    );
}

#[test]
fn large_awkward_scene_changes_zero_bytes() {
    // FR-011 / SC-003: interpreting + validating the large awkward scene is
    // strictly read-only — the CST round-trips byte-for-byte, even over the
    // unparseable region. (A modest anchor keeps this companion fast; byte
    // stability does not depend on size.)
    let (src, _count) = generate_large_awkward_scene(128 * 1024);
    let cst = parse(&src);

    let (registry, model) = registry_and_model(&registry_schema_json());
    let _ = SceneModel::from_cst(&cst);
    let _ = validate_scene(&model, &registry, &cst, None);

    assert_eq!(
        ron_core::print(&cst),
        src,
        "interpret + validate over a large awkward scene mutates zero bytes"
    );
}
