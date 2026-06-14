//! Binding-resolution unit tests (E006 US2 — T023; FR-008, FR-009, FR-012,
//! FR-013).
//!
//! Covers the deterministic resolution algorithm:
//! * most-specific wins (more literal chars beats fewer / fewer wildcards),
//! * equal-specificity tie-break = last-declared rule,
//! * an exclusion removes a rule from candidacy entirely (even the more specific
//!   one),
//! * override > config,
//! * no matching rule and no override → `NoBinding`,
//! * absent / empty config → `NoBinding`,
//! * resolution is order-independent for the specificity comparison (only exact
//!   ties consult declaration order).

use std::path::{Path, PathBuf};

use ronin_app::binding::{
    resolve, resolve_binding, BindingConfig, BindingOrigin, BindingRule, BindingState,
    DocumentOverride, TypeSourceLocator, BINDING_CONFIG_VERSION,
};

/// Build a `BindingRule` with no exclusions, a Rust-source locator, and a
/// `type_name` derived from the source path so the chosen rule is identifiable.
fn rule(pattern: &str, type_name: &str) -> BindingRule {
    BindingRule {
        pattern: pattern.to_string(),
        exclude: None,
        type_name: type_name.to_string(),
        type_source: TypeSourceLocator::RustSource(PathBuf::from(format!("src/{type_name}.rs"))),
    }
}

/// Build a `BindingRule` with exclusion globs.
fn rule_excl(pattern: &str, excludes: &[&str], type_name: &str) -> BindingRule {
    BindingRule {
        pattern: pattern.to_string(),
        exclude: Some(excludes.iter().map(|s| s.to_string()).collect()),
        type_name: type_name.to_string(),
        type_source: TypeSourceLocator::RustSource(PathBuf::from(format!("src/{type_name}.rs"))),
    }
}

fn config(rules: Vec<BindingRule>) -> BindingConfig {
    BindingConfig {
        rules,
        version: BINDING_CONFIG_VERSION,
    }
}

fn path(p: &str) -> &Path {
    Path::new(p)
}

// ---------------------------------------------------------------------------
// Most-specific wins (FR-012)
// ---------------------------------------------------------------------------

#[test]
fn most_specific_more_literal_chars_wins() {
    // Both match `config/app.ron`. The exact path has far more literal chars than
    // the broad `**/*.ron`, so it wins regardless of declaration order.
    let broad = rule("**/*.ron", "Broad");
    let exact = rule("config/app.ron", "Exact");

    let cfg = config(vec![broad.clone(), exact.clone()]);
    let b = resolve_binding(&cfg, path("config/app.ron"));
    assert_eq!(b.type_name(), Some("Exact"));
    assert_eq!(b.origin(), Some(BindingOrigin::Config));

    // Order-independent: swapping declaration order keeps the same winner because
    // the comparison is by specificity, not position.
    let cfg_rev = config(vec![exact, broad]);
    let b_rev = resolve_binding(&cfg_rev, path("config/app.ron"));
    assert_eq!(b_rev.type_name(), Some("Exact"));
}

#[test]
fn fewer_wildcards_outranks_more() {
    // `config/*.ron` (one wildcard, more literal chars) beats `**/*.ron`
    // (more wildcards, fewer literal chars) for `config/app.ron`.
    let many_wild = rule("**/*.ron", "ManyWild");
    let few_wild = rule("config/*.ron", "FewWild");

    let cfg = config(vec![many_wild, few_wild]);
    let b = resolve_binding(&cfg, path("config/app.ron"));
    assert_eq!(b.type_name(), Some("FewWild"));

    // Order-independent.
    let cfg_rev = config(vec![
        rule("config/*.ron", "FewWild"),
        rule("**/*.ron", "ManyWild"),
    ]);
    let b_rev = resolve_binding(&cfg_rev, path("config/app.ron"));
    assert_eq!(b_rev.type_name(), Some("FewWild"));
}

#[test]
fn specificity_is_order_independent_across_permutations() {
    // Three rules of strictly increasing specificity all match the same path; the
    // most specific must win for every permutation (no ties involved, so order
    // never matters here).
    let a = rule("**/*.ron", "A"); // least specific
    let b = rule("config/*.ron", "B");
    let c = rule("config/app.ron", "C"); // most specific
    let target = path("config/app.ron");

    let perms = [
        vec![a.clone(), b.clone(), c.clone()],
        vec![a.clone(), c.clone(), b.clone()],
        vec![b.clone(), a.clone(), c.clone()],
        vec![b.clone(), c.clone(), a.clone()],
        vec![c.clone(), a.clone(), b.clone()],
        vec![c.clone(), b.clone(), a.clone()],
    ];
    for perm in perms {
        let cfg = config(perm);
        assert_eq!(
            resolve_binding(&cfg, target).type_name(),
            Some("C"),
            "most-specific rule must win regardless of declaration order"
        );
    }
}

// ---------------------------------------------------------------------------
// Equal-specificity tie-break = later-declared (FR-012)
// ---------------------------------------------------------------------------

#[test]
fn equal_specificity_tie_break_last_declared_wins() {
    // Two distinct patterns that both match `data/x.ron` and have IDENTICAL
    // literal-char counts: `data/*.ron` and `*ata/x.ron` (both 9 literal chars).
    // The later-declared rule must win.
    let first = rule("data/*.ron", "First");
    let second = rule("*ata/x.ron", "Second");
    assert_eq!(
        first.specificity(),
        second.specificity(),
        "fixture precondition: the two patterns must be equally specific"
    );

    let cfg = config(vec![first.clone(), second.clone()]);
    assert_eq!(
        resolve_binding(&cfg, path("data/x.ron")).type_name(),
        Some("Second"),
        "later-declared rule wins on an exact specificity tie"
    );

    // Reversing declaration order flips the winner — the ONLY case where order
    // matters.
    let cfg_rev = config(vec![second, first]);
    assert_eq!(
        resolve_binding(&cfg_rev, path("data/x.ron")).type_name(),
        Some("First"),
    );
}

// ---------------------------------------------------------------------------
// Exclusions remove a rule from candidacy entirely (FR-012)
// ---------------------------------------------------------------------------

#[test]
fn exclusion_removes_rule_from_candidacy() {
    // A single rule whose include matches but whose exclude also matches → no
    // candidate → NoBinding.
    let cfg = config(vec![rule_excl(
        "config/*.ron",
        &["config/secret.ron"],
        "Cfg",
    )]);

    // Excluded file → NoBinding.
    assert!(matches!(
        resolve_binding(&cfg, path("config/secret.ron")).state,
        BindingState::NoBinding
    ));
    // Non-excluded sibling → bound.
    assert_eq!(
        resolve_binding(&cfg, path("config/app.ron")).type_name(),
        Some("Cfg")
    );
}

#[test]
fn exclusion_beats_higher_specificity() {
    // The more-specific rule is excluded for this path, so the less-specific rule
    // (which still matches and is not excluded) wins — proving exclusion is
    // applied BEFORE specificity ranking.
    let specific = rule_excl("config/app.ron", &["config/app.ron"], "Specific");
    let broad = rule("**/*.ron", "Broad");

    let cfg = config(vec![specific, broad]);
    let b = resolve_binding(&cfg, path("config/app.ron"));
    assert_eq!(
        b.type_name(),
        Some("Broad"),
        "an excluded rule never participates, so the less-specific rule wins"
    );
}

// ---------------------------------------------------------------------------
// Override > config (FR-009)
// ---------------------------------------------------------------------------

#[test]
fn override_beats_config() {
    let cfg = config(vec![rule("config/app.ron", "Cfg")]);
    let ov = DocumentOverride {
        type_name: "Forced".to_string(),
        type_source: TypeSourceLocator::SchemaFile(PathBuf::from("schemas/forced.json")),
    };

    let b = resolve(&cfg, Some(path("config/app.ron")), Some(&ov));
    assert_eq!(b.type_name(), Some("Forced"));
    assert_eq!(b.origin(), Some(BindingOrigin::Override));
    assert_eq!(
        b.type_source(),
        Some(&TypeSourceLocator::SchemaFile(PathBuf::from(
            "schemas/forced.json"
        )))
    );
}

#[test]
fn override_wins_even_with_no_config_match() {
    // No rule matches, but an override is present → Bound via Override.
    let cfg = config(vec![rule("other/*.ron", "Other")]);
    let ov = DocumentOverride {
        type_name: "Forced".to_string(),
        type_source: TypeSourceLocator::RustSource(PathBuf::from("src/forced.rs")),
    };
    let b = resolve(&cfg, Some(path("unrelated/file.ron")), Some(&ov));
    assert!(b.is_bound());
    assert_eq!(b.origin(), Some(BindingOrigin::Override));
}

#[test]
fn override_applies_without_a_path() {
    // A not-yet-saved buffer (no path) still honours an override.
    let cfg = config(vec![rule("**/*.ron", "Cfg")]);
    let ov = DocumentOverride {
        type_name: "Forced".to_string(),
        type_source: TypeSourceLocator::RustSource(PathBuf::from("src/forced.rs")),
    };
    let b = resolve(&cfg, None, Some(&ov));
    assert_eq!(b.type_name(), Some("Forced"));
    assert_eq!(b.origin(), Some(BindingOrigin::Override));
}

// ---------------------------------------------------------------------------
// NoBinding states (FR-013, FR-015)
// ---------------------------------------------------------------------------

#[test]
fn no_matching_rule_and_no_override_is_no_binding() {
    let cfg = config(vec![rule("config/*.ron", "Cfg")]);
    let b = resolve(&cfg, Some(path("src/main.rs")), None);
    assert!(matches!(b.state, BindingState::NoBinding));
    assert!(!b.is_bound());
    assert_eq!(b.type_name(), None);
    assert_eq!(b.origin(), None);
}

#[test]
fn empty_config_is_no_binding() {
    let cfg = config(vec![]);
    assert!(matches!(
        resolve_binding(&cfg, path("config/app.ron")).state,
        BindingState::NoBinding
    ));
}

#[test]
fn default_config_is_empty_and_no_binding() {
    let cfg = BindingConfig::default();
    assert!(cfg.rules.is_empty());
    assert_eq!(cfg.version, BINDING_CONFIG_VERSION);
    assert!(matches!(
        resolve_binding(&cfg, path("anything.ron")).state,
        BindingState::NoBinding
    ));
}

#[test]
fn no_path_against_config_is_no_binding() {
    // Without an override and without a path, config resolution has nothing to
    // match → NoBinding.
    let cfg = config(vec![rule("**/*.ron", "Cfg")]);
    assert!(matches!(
        resolve(&cfg, None, None).state,
        BindingState::NoBinding
    ));
}

// ---------------------------------------------------------------------------
// Serde round-trip (FR-008, FR-013 persisted shape)
// ---------------------------------------------------------------------------

#[test]
fn config_serde_round_trips() {
    let cfg = config(vec![
        rule("config/*.ron", "Cfg"),
        rule_excl("data/**", &["data/tmp/**"], "Data"),
    ]);
    let json = serde_json::to_string(&cfg).expect("serialize");
    let back: BindingConfig = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(cfg, back);
}

#[test]
fn malformed_glob_degrades_to_no_match() {
    // An unparseable pattern must not crash and must simply never match (the
    // building block for adversarial-config hardening in Phase 4b). `[` opens an
    // unterminated character class.
    let cfg = config(vec![rule("config/[unterminated.ron", "Bad")]);
    assert!(matches!(
        resolve_binding(&cfg, path("config/whatever.ron")).state,
        BindingState::NoBinding
    ));
}
