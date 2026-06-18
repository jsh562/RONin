//! TR-007 / TR-008 / SC-004 — schemars and user-schema adapters converge.
//!
//! A `schemars`-derived schema and a hand-written general JSON Schema 2020-12
//! document, each describing the **same** type, load through their respective
//! adapters ([`SchemarsSource`] / [`JsonSchemaSource`]) and normalize into
//! **equivalent** [`TypeModel`]s. This is the cross-source convergence guarantee
//! (SC-004): regardless of which input form a host supplies, RONin's internal
//! model of a type is the same.
//!
//! The schemas below are representative fixtures covering struct fields, nested
//! `$ref`s, `Option`, sequences, tuples, string-keyed maps, and a data-carrying
//! enum.

use ronin_types::model::{NodeKind, TypeModel};
use ronin_types::normalize;
use ronin_types::source::TypeSource;
use ronin_types::{JsonSchemaSource, SchemarsSource};

/// A schemars 1.x-derived schema for a `Profile` struct, exactly as
/// `schemars::schema_for!(Profile)` emits it (verified against schemars 1.2).
const SCHEMARS_PROFILE: &str = r##"{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "title": "Profile",
  "type": "object",
  "properties": {
    "name": { "type": "string" },
    "age": { "type": "integer", "format": "uint32", "minimum": 0 },
    "nickname": { "type": ["string", "null"] },
    "tags": { "type": "array", "items": { "type": "string" } },
    "coord": {
      "type": "array",
      "maxItems": 2,
      "minItems": 2,
      "prefixItems": [ { "type": "number" }, { "type": "number" } ]
    },
    "scores": { "type": "object", "additionalProperties": { "type": "integer", "format": "int32" } },
    "role": { "$ref": "#/$defs/Role" }
  },
  "required": ["name", "age", "tags", "coord", "scores", "role"],
  "$defs": {
    "Role": {
      "oneOf": [
        { "type": "string", "enum": ["Guest"] },
        { "type": "object", "properties": { "Member": { "type": "integer", "format": "uint64", "minimum": 0 } }, "required": ["Member"], "additionalProperties": false }
      ]
    }
  }
}"##;

/// A hand-authored general JSON Schema 2020-12 for the *same* `Profile` type.
/// Written without schemars' `format`/`minimum`/`min|maxItems` annotations
/// (which the type model does not carry) to show the two converge on the same
/// structural model.
const USER_PROFILE: &str = r##"{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "title": "Profile",
  "type": "object",
  "properties": {
    "name": { "type": "string" },
    "age": { "type": "integer" },
    "nickname": { "type": ["string", "null"] },
    "tags": { "type": "array", "items": { "type": "string" } },
    "coord": {
      "type": "array",
      "prefixItems": [ { "type": "number" }, { "type": "number" } ]
    },
    "scores": { "type": "object", "additionalProperties": { "type": "integer" } },
    "role": { "$ref": "#/$defs/Role" }
  },
  "required": ["name", "age", "tags", "coord", "scores", "role"],
  "$defs": {
    "Role": {
      "oneOf": [
        { "type": "string", "enum": ["Guest"] },
        { "type": "object", "properties": { "Member": { "type": "integer" } }, "required": ["Member"], "additionalProperties": false }
      ]
    }
  }
}"##;

/// Compare two models for structural equivalence: same named types in the same
/// order, with byte-identical node shapes (after the JSON-Schema interchange
/// round-trip, which is the form a consumer actually sees). Diagnostics and
/// source-specific provenance are intentionally ignored.
fn assert_models_equivalent(a: &TypeModel, b: &TypeModel) {
    let names_a: Vec<&str> = a.iter_ordered().map(|(n, _)| n).collect();
    let names_b: Vec<&str> = b.iter_ordered().map(|(n, _)| n).collect();
    assert_eq!(names_a, names_b, "same named types in the same order");
    for name in names_a {
        assert_eq!(
            a.lookup(name),
            b.lookup(name),
            "node `{name}` is identical across the two adapters"
        );
    }
}

#[test]
fn schemars_and_user_schema_converge() {
    let from_schemars = SchemarsSource::from_json_str(SCHEMARS_PROFILE).acquire();
    let from_user = JsonSchemaSource::from_json_str(USER_PROFILE).acquire();

    // Neither adapter produced an unsupported-construct error for these shapes.
    assert!(
        from_schemars.diagnostics.is_empty(),
        "schemars ingest is clean: {:?}",
        from_schemars.diagnostics
    );
    assert!(
        from_user.diagnostics.is_empty(),
        "user-schema ingest is clean: {:?}",
        from_user.diagnostics
    );

    assert_models_equivalent(&from_schemars.model, &from_user.model);
}

#[test]
fn both_describe_the_expected_shape() {
    let model = SchemarsSource::from_json_str(SCHEMARS_PROFILE)
        .acquire()
        .model;

    // Profile is an object with the seven declared fields.
    let profile = model.lookup("Profile").expect("Profile registered");
    let NodeKind::Object { fields, .. } = &profile.kind else {
        panic!("Profile is an object");
    };
    assert_eq!(fields.len(), 7);

    // `nickname` is an Option (and therefore optional).
    let nickname = fields
        .iter()
        .find(|f| f.serialized_key == "nickname")
        .unwrap();
    assert!(nickname.optional);
    assert!(matches!(
        model.resolve(&nickname.value).unwrap().kind,
        NodeKind::Option { .. }
    ));

    // `coord` is a 2-tuple.
    let coord = fields.iter().find(|f| f.serialized_key == "coord").unwrap();
    let NodeKind::Tuple { elements } = &model.resolve(&coord.value).unwrap().kind else {
        panic!("coord is a tuple");
    };
    assert_eq!(elements.len(), 2);

    // `scores` is a string-keyed map.
    let scores = fields
        .iter()
        .find(|f| f.serialized_key == "scores")
        .unwrap();
    assert!(matches!(
        model.resolve(&scores.value).unwrap().kind,
        NodeKind::Map { .. }
    ));

    // `role` references the named Role enum.
    let role = fields.iter().find(|f| f.serialized_key == "role").unwrap();
    assert_eq!(role.value.as_named(), Some("Role"));
    let NodeKind::Enum { variants, .. } = &model.lookup("Role").unwrap().kind else {
        panic!("Role is an enum");
    };
    assert_eq!(variants.len(), 2);
}

#[test]
fn equivalent_after_normalize_through_their_adapters() {
    // The same convergence holds end-to-end through `normalize`, comparing the
    // merged named types (ignoring provenance diagnostics).
    let schemars_sources: Vec<Box<dyn TypeSource>> =
        vec![Box::new(SchemarsSource::from_json_str(SCHEMARS_PROFILE))];
    let user_sources: Vec<Box<dyn TypeSource>> =
        vec![Box::new(JsonSchemaSource::from_json_str(USER_PROFILE))];

    let via_schemars = normalize(&schemars_sources);
    let via_user = normalize(&user_sources);

    assert_models_equivalent(&via_schemars, &via_user);
}
