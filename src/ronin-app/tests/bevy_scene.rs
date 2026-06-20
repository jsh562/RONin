//! Integration tests for the Bevy [`SceneModel`] over real `.scn.ron` fixtures
//! (E009 US1 cluster 3C-1 / T006+T013, FR-004/FR-008).
//!
//! Exercises the read-only scene interpretation against the sample scenes in
//! `tests/fixtures/scenes/`: a valid scene, an unregistered-component-path scene,
//! a wrong-typed/arity/variant scene, and an omitted-`resources` scene. Asserts
//! the resource/component refs, their fully-qualified type paths, precise CST
//! ranges, and degrade-safe handling — all with zero byte mutation.

use std::path::PathBuf;

use ronin_app::bevy::{SceneModel, SceneValueKind, SceneValueRef};
use ronin_core::parse;

/// Load a `.scn.ron` fixture's source text by file name.
fn fixture(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("scenes")
        .join(name);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read fixture {}: {e}", path.display()))
}

/// The type paths of a value-ref slice, in order.
fn paths(refs: &[SceneValueRef]) -> Vec<&str> {
    refs.iter().map(SceneValueRef::type_path).collect()
}

#[test]
fn valid_scene_projects_resources_and_components_with_paths() {
    let src = fixture("valid.scn.ron");
    let model = SceneModel::from_cst(&parse(&src));

    // One resource, keyed by its fully-qualified type path.
    assert_eq!(
        paths(model.resources()),
        vec!["bevy_utils::label::LabelMap"]
    );
    assert_eq!(model.resources()[0].kind(), SceneValueKind::Resource);

    // Two entities, in source order (ids 0 and 1).
    let entities = model.entities();
    assert_eq!(entities.len(), 2);
    assert_eq!(entities[0].id(), 0);
    assert_eq!(
        paths(entities[0].components()),
        vec![
            "bevy_transform::components::transform::Transform",
            "bevy_pbr::light::Visibility",
        ]
    );
    assert_eq!(entities[1].id(), 1);
    assert_eq!(
        paths(entities[1].components()),
        vec!["bevy_transform::components::transform::Transform"]
    );

    // Components carry their owning entity id.
    assert_eq!(entities[0].components()[0].entity_id(), Some(0));
    assert_eq!(entities[1].components()[0].entity_id(), Some(1));
}

#[test]
fn value_refs_carry_real_cst_ranges_into_the_source() {
    let src = fixture("valid.scn.ron");
    let model = SceneModel::from_cst(&parse(&src));
    for value in model.entries() {
        let range = value.range();
        // The recorded range is the value node's real extent (never fabricated).
        assert_eq!(value.value_node().text_range(), range);
        assert!(
            !range.is_empty(),
            "{} has an empty range",
            value.type_path()
        );
        // The span addresses real source bytes within the file.
        assert!(range.end() <= src.len());
    }
}

#[test]
fn unregistered_component_path_still_projects_as_a_component() {
    // The unregistered path is well-formed; the model surfaces it normally so
    // validation can mark it unconstrained (FR-006) — interpretation never errors.
    let src = fixture("unregistered_component.scn.ron");
    let model = SceneModel::from_cst(&parse(&src));
    let comps = paths(model.entities()[0].components());
    assert!(comps.contains(&"my_game::components::Health"));
    assert!(comps.contains(&"bevy_transform::components::transform::Transform"));
}

#[test]
fn wrong_typed_scene_projects_every_component_for_validation() {
    // All component paths ARE registered; the model still reads each (the value
    // mismatches are a validation concern, not an interpretation concern).
    let src = fixture("wrong_typed.scn.ron");
    let model = SceneModel::from_cst(&parse(&src));
    assert_eq!(
        paths(model.entities()[0].components()),
        vec![
            "bevy_transform::components::transform::Transform",
            "bevy_pbr::light::Visibility",
        ]
    );
}

#[test]
fn omitted_resources_scene_reads_empty_resources_keeps_entities() {
    let src = fixture("omitted_resources.scn.ron");
    let model = SceneModel::from_cst(&parse(&src));
    assert!(model.resources().is_empty(), "omitted resources → empty");
    assert_eq!(model.entities().len(), 1);
    assert_eq!(model.entities()[0].id(), 7);
    assert_eq!(
        paths(model.entities()[0].components()),
        vec!["bevy_transform::components::transform::Transform"]
    );
}

#[test]
fn interpretation_is_read_only_zero_bytes() {
    // Deriving the model from a fixture changes nothing about the source; the CST
    // round-trips byte-for-byte (SC-003).
    let src = fixture("valid.scn.ron");
    let cst = parse(&src);
    let _ = SceneModel::from_cst(&cst);
    assert_eq!(ronin_core::print(&cst), src, "byte-lossless: zero mutation");
}
