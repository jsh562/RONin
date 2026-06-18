//! Integration test for [`SynSource`]'s serde-attribute fidelity {TR-005, SC-003}.
//!
//! Parses a single fixture file exercising the serde attribute set and asserts
//! the resulting model matches serde's ACTUAL (de)serialization behavior:
//! `rename`/`rename_all` keys, `default`/`skip`/`skip_serializing_if`
//! optionality, `flatten`, enum `tag`/`tag`+`content`/`untagged`
//! representations, `transparent` collapse, and the documented `with` →
//! `unknown` limitation.

use std::path::PathBuf;

use ronin_types::model::{Discriminator, NodeKind, VariantShape};
use ronin_types::source::TypeSource;
use ronin_types::SynSource;

fn fixture() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests");
    p.push("fixtures");
    p.push("serde_attrs");
    p.push("types.rs");
    p
}

fn model() -> ronin_types::TypeModel {
    SynSource::from_path(fixture()).acquire().model
}

fn object_fields(node: &ronin_types::TypeNode) -> &[ronin_types::model::Field] {
    match &node.kind {
        NodeKind::Object { fields, .. } => fields,
        other => panic!("expected object, got {other:?}"),
    }
}

#[test]
fn rename_all_camel_case_produces_camel_keys() {
    let model = model();
    let account = model.lookup("Account").unwrap();
    let fields = object_fields(account);
    let keys: Vec<&str> = fields.iter().map(|f| f.serialized_key.as_str()).collect();

    // rename_all = "camelCase": first_name -> firstName, last_name -> lastName.
    assert!(keys.contains(&"firstName"), "keys: {keys:?}");
    assert!(keys.contains(&"lastName"), "keys: {keys:?}");
    // per-field rename = "ID" wins over rename_all.
    assert!(keys.contains(&"ID"), "keys: {keys:?}");
    // skip removes `cached` entirely.
    assert!(!keys.contains(&"cached"), "skip should drop field");
}

#[test]
fn deny_unknown_fields_sets_object_flag() {
    let model = model();
    let account = model.lookup("Account").unwrap();
    let NodeKind::Object {
        deny_unknown_fields,
        ..
    } = &account.kind
    else {
        panic!("object");
    };
    assert!(*deny_unknown_fields);
}

#[test]
fn default_and_skip_serializing_if_make_fields_optional() {
    let model = model();
    let account = model.lookup("Account").unwrap();
    let fields = object_fields(account);
    let by_key = |k: &str| fields.iter().find(|f| f.serialized_key == k).unwrap();

    // `default` -> optional even though String is not Option.
    assert!(by_key("nickname").optional, "default field is optional");
    // `skip_serializing_if` -> optional.
    assert!(
        by_key("bio").optional,
        "skip_serializing_if field is optional"
    );
    // required (no optionality attr, not Option).
    assert!(!by_key("firstName").optional, "plain field is required");
}

#[test]
fn with_converter_collapses_to_unknown() {
    let model = model();
    let account = model.lookup("Account").unwrap();
    let fields = object_fields(account);
    let created = fields
        .iter()
        .find(|f| f.serialized_key == "createdAt")
        .unwrap();
    // serde `with` converter shape is invisible to syn -> unknown.
    let node = model.resolve(&created.value).unwrap();
    assert!(node.is_unknown(), "`with` field must be unknown");
}

#[test]
fn internally_tagged_enum_discriminator() {
    let model = model();
    let event = model.lookup("Event").unwrap();
    let NodeKind::Enum { discriminator, .. } = &event.kind else {
        panic!("enum");
    };
    assert_eq!(
        *discriminator,
        Discriminator::Internal {
            tag: "type".to_string()
        }
    );
}

#[test]
fn adjacently_tagged_enum_discriminator() {
    let model = model();
    let message = model.lookup("Message").unwrap();
    let NodeKind::Enum { discriminator, .. } = &message.kind else {
        panic!("enum");
    };
    assert_eq!(
        *discriminator,
        Discriminator::Adjacent {
            tag: "kind".to_string(),
            content: "data".to_string(),
        }
    );
}

#[test]
fn untagged_enum_discriminator() {
    let model = model();
    let value = model.lookup("Value").unwrap();
    let NodeKind::Enum { discriminator, .. } = &value.kind else {
        panic!("enum");
    };
    assert_eq!(*discriminator, Discriminator::Untagged);
}

#[test]
fn variant_rename_all_and_per_variant_rename_and_skip() {
    let model = model();
    let status = model.lookup("Status").unwrap();
    let NodeKind::Enum { variants, .. } = &status.kind else {
        panic!("enum");
    };
    let names: Vec<&str> = variants
        .iter()
        .map(|v| v.serialized_name.as_str())
        .collect();

    // rename_all = "SCREAMING_SNAKE_CASE": Active -> ACTIVE, InProgress -> IN_PROGRESS.
    assert!(names.contains(&"ACTIVE"), "names: {names:?}");
    assert!(names.contains(&"IN_PROGRESS"), "names: {names:?}");
    // per-variant rename wins.
    assert!(names.contains(&"done!"), "names: {names:?}");
    // skipped variant removed.
    assert!(!names.contains(&"Internal"), "skip drops variant");
}

#[test]
fn transparent_struct_collapses_to_inner_shape() {
    let model = model();
    let wrapper = model.lookup("Wrapper").unwrap();
    // Collapsed to InnerData's object shape (an object with a, b), not a 1-field
    // wrapper object, and tagged unwrap_newtypes.
    let NodeKind::Object { fields, .. } = &wrapper.kind else {
        panic!(
            "transparent should collapse to inner object, got {:?}",
            wrapper.kind
        );
    };
    let keys: Vec<&str> = fields.iter().map(|f| f.serialized_key.as_str()).collect();
    assert_eq!(keys, vec!["a", "b"]);
    assert!(wrapper.ron_extension.as_ref().unwrap().unwrap_newtypes);
}

#[test]
fn flatten_field_is_marked() {
    let model = model();
    let env = model.lookup("Envelope").unwrap();
    let fields = object_fields(env);
    let payload = fields
        .iter()
        .find(|f| f.serialized_key == "payload")
        .unwrap();
    // The flattened field is referenced by name (target keys not inlined by syn);
    // the field is marked `flatten` so a consumer can expand it with the model.
    assert!(payload.flatten);
    assert_eq!(payload.value.as_named(), Some("InnerData"));
}

#[test]
fn rename_all_fields_applies_to_variant_struct_fields() {
    let model = model();
    let command = model.lookup("Command").unwrap();
    let NodeKind::Enum { variants, .. } = &command.kind else {
        panic!("enum");
    };
    let VariantShape::Struct(fields) = &variants[0].shape else {
        panic!("struct variant");
    };
    let keys: Vec<&str> = fields.iter().map(|f| f.serialized_key.as_str()).collect();
    // rename_all_fields = "camelCase": exit_code -> exitCode, working_dir -> workingDir.
    assert_eq!(keys, vec!["exitCode", "workingDir"]);
}
