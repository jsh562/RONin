//! The [`TypeSource`] adapter contract {TR-009}.
//!
//! Every input source (Rust source via `syn`, `schemars`-derived JSON Schema,
//! user-supplied JSON Schema, and the future Bevy registry) implements this one
//! trait: it converts a single input into a **partial** [`TypeModel`] plus
//! diagnostics. The contract is the open/closed seam that keeps source-specific
//! logic isolated — a new source plugs in without touching [`TypeModel`],
//! [`TypeNode`](crate::model::TypeNode), or any existing adapter (SC-004).
//!
//! Two invariants the contract enforces:
//!
//! - **Never-fail output.** [`TypeSource::acquire`] always returns a (possibly
//!   empty) [`Acquired`]. Unresolvable input becomes `unknown` nodes plus
//!   diagnostics — adapters do not panic and do not surface a fatal error
//!   (TR-006, TR-011).
//! - **Deterministic precedence.** Each source declares a fixed
//!   [`SourcePrecedence`] used at merge time so the normalized model is
//!   independent of source iteration order (TR-010): user-schema > schemars >
//!   syn, with future sources slotting into the total order.
//!
//! The concrete adapters land in later waves; this wave defines only the
//! contract and its precedence ordering.

use crate::diagnostics::AcquisitionDiagnostic;
use crate::model::TypeModel;

mod bevy_source;
mod json_schema_ingest;
mod json_schema_source;
mod schemars_source;
mod syn_source;

pub use bevy_source::{BevyRegistry, BevySource, ReflectKind, ReflectSchema};
pub use json_schema_source::JsonSchemaSource;
pub use schemars_source::SchemarsSource;
pub use syn_source::SynSource;

/// The deterministic merge rank of a [`TypeSource`] (TR-010).
///
/// Ordering is a **total order**: `Syn < Schemars < UserSchema < Bevy`. Higher
/// ranks win on conflict. Derived `Ord` follows declaration order, so
/// comparisons and sorts reflect precedence directly. The Bevy registry source
/// (E009) slots in as the highest-ranked variant without disturbing the existing
/// ranks' relative order: in Bevy mode the registry is the authoritative type
/// source and *replaces* the serde sources rather than merging with them
/// (FR-013), so its rank only needs to be well-defined and total.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SourcePrecedence {
    /// Static Rust source analysis via `syn` (lowest precedence).
    Syn,
    /// `schemars`-derived JSON Schema (middle precedence).
    Schemars,
    /// User-supplied JSON Schema 2020-12 (higher precedence).
    UserSchema,
    /// Exported Bevy type registry consumed as data (E009; highest precedence —
    /// authoritative for `.scn.ron` scenes in Bevy mode).
    Bevy,
}

impl SourcePrecedence {
    /// A stable lowercase rank label (for diagnostics/provenance).
    #[inline]
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            SourcePrecedence::Syn => "syn",
            SourcePrecedence::Schemars => "schemars",
            SourcePrecedence::UserSchema => "user-schema",
            SourcePrecedence::Bevy => "bevy-registry",
        }
    }
}

impl std::fmt::Display for SourcePrecedence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The output of one [`TypeSource::acquire`] run: a partial [`TypeModel`] plus
/// the diagnostics produced while acquiring it.
///
/// Kept separate from `TypeModel` (rather than reusing its `diagnostics` field)
/// so the contract makes "partial model + findings" explicit; the normalizer
/// folds these into the merged model in a later wave.
#[derive(Debug, Clone, Default)]
pub struct Acquired {
    /// The partial model this source contributed.
    pub model: TypeModel,
    /// Findings produced during this acquisition.
    pub diagnostics: Vec<AcquisitionDiagnostic>,
}

impl Acquired {
    /// An empty acquisition result (no types, no diagnostics).
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }
}

/// The single adapter contract every type source implements (TR-009).
///
/// Implementors are native-only; their serialized output is what the WASM core
/// ultimately consumes (TR-013).
pub trait TypeSource {
    /// A stable identifier for this source instance, used for provenance and
    /// conflict diagnostics (e.g. `"syn"`, `"user-schema:foo.json"`).
    fn source_id(&self) -> String;

    /// This source's deterministic merge precedence (TR-010).
    fn precedence(&self) -> SourcePrecedence;

    /// Acquire a partial [`TypeModel`] + diagnostics from this source.
    ///
    /// MUST NOT fail: unresolvable or malformed input yields `unknown` nodes
    /// and/or diagnostics inside the returned [`Acquired`], never a panic or a
    /// fatal error (TR-006, TR-011).
    fn acquire(&self) -> Acquired;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::TypeNode;

    /// A trivial in-test source proving the contract is implementable without
    /// any source-specific type leaking into the trait (SC-004 extensibility).
    struct MockSource {
        id: String,
        precedence: SourcePrecedence,
        type_name: String,
    }

    impl TypeSource for MockSource {
        fn source_id(&self) -> String {
            self.id.clone()
        }
        fn precedence(&self) -> SourcePrecedence {
            self.precedence
        }
        fn acquire(&self) -> Acquired {
            let mut model = TypeModel::new();
            model.insert_named(self.type_name.clone(), TypeNode::unknown());
            Acquired {
                model,
                diagnostics: Vec::new(),
            }
        }
    }

    /// The contract is satisfiable and `acquire` yields a partial model.
    #[test]
    fn mock_source_produces_partial_model() {
        let src = MockSource {
            id: "mock".into(),
            precedence: SourcePrecedence::Syn,
            type_name: "Foo".into(),
        };
        assert_eq!(src.source_id(), "mock");
        assert_eq!(src.precedence(), SourcePrecedence::Syn);
        let acquired = src.acquire();
        assert!(acquired.model.contains("Foo"));
        assert!(acquired.diagnostics.is_empty());
    }

    /// TR-010: precedence is a deterministic total order, user-schema > schemars
    /// > syn, regardless of the order sources are listed.
    #[test]
    fn precedence_total_order() {
        assert!(SourcePrecedence::Syn < SourcePrecedence::Schemars);
        assert!(SourcePrecedence::Schemars < SourcePrecedence::UserSchema);
        assert!(SourcePrecedence::Syn < SourcePrecedence::UserSchema);
        // E009: the Bevy registry source is the highest-ranked variant.
        assert!(SourcePrecedence::UserSchema < SourcePrecedence::Bevy);
        assert_eq!(SourcePrecedence::Bevy.as_str(), "bevy-registry");

        // Sorting a shuffled list yields the canonical low→high precedence order.
        let mut ranks = [
            SourcePrecedence::UserSchema,
            SourcePrecedence::Syn,
            SourcePrecedence::Schemars,
        ];
        ranks.sort();
        assert_eq!(
            ranks,
            [
                SourcePrecedence::Syn,
                SourcePrecedence::Schemars,
                SourcePrecedence::UserSchema
            ]
        );
    }

    /// The trait is object-safe (usable as `dyn TypeSource`) so the normalizer
    /// can hold a heterogeneous list of sources.
    #[test]
    fn type_source_is_object_safe() {
        let sources: Vec<Box<dyn TypeSource>> = vec![
            Box::new(MockSource {
                id: "a".into(),
                precedence: SourcePrecedence::Syn,
                type_name: "A".into(),
            }),
            Box::new(MockSource {
                id: "b".into(),
                precedence: SourcePrecedence::UserSchema,
                type_name: "B".into(),
            }),
        ];
        let highest = sources
            .iter()
            .max_by_key(|s| s.precedence())
            .map(|s| s.source_id());
        assert_eq!(highest.as_deref(), Some("b"));
    }
}
