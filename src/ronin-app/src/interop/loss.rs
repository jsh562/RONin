//! The lossy-construct map (FR-004/007) — kinds + source `TextRange` + recovery flag.
//!
//! This module defines the **single source of truth** for everything that cannot
//! be represented losslessly when a RON document is projected to standard JSON: a
//! flat, ordered list of [`LossyConstruct`]s aggregated into one [`LossReport`].
//! That ONE list drives **both** user-facing surfaces — the pre-conversion loss
//! dialog (FR-005) **and** the inline E006 diagnostics (FR-006) — so a loss can
//! never reach one surface but not the other (FR-007, data-model §LossReport
//! "Drives both surfaces from one list").
//!
//! # Loss-code namespace (`RON-I####`)
//!
//! Each [`LossKind`] carries a **stable** `RON-I####` code (its [`LossKind::code`])
//! and a human label ([`LossKind::label`]). This mirrors E009's `BVY-S####`
//! [`crate::bevy::SceneDiagnosticCode`] namespace: a `ronin-app`-local diagnostic
//! registry kept native-side so the WASM-clean `ronin-core`/`ronin-validate` stay free
//! of any interop concern (FR-012). Tests and snapshots key on the **stable code +
//! kind**, never on the human-readable [`LossyConstruct::detail`] wording (plan
//! "Snapshot vs assertion scope").
//!
//! # Lossy ≠ unrecoverable (FR-004 / STF-001)
//!
//! A construct is **lossy to an external JSON consumer** (a tuple emitted as an
//! array) yet may still be **round-trip-safe within RONin** when a `TypeModel` is
//! bound (the array re-read as a tuple by arity). The two facts are not
//! contradictory: a [`LossRecovery::RoundTripSafeWithinRonin`] construct is **still**
//! reported — it is lossy to the outside world. The [`LossRecovery`] flag is kept
//! distinct from "report it"; everything in the list is reported (HINT-004).

use std::collections::BTreeMap;

use ronin_core::TextRange;

/// The category of one construct that cannot be represented losslessly in standard
/// JSON (FR-004), plus the convert-remainder placeholder kind (FR-013).
///
/// Each variant has a **stable** `RON-I####` [`code`](LossKind::code) and a human
/// [`label`](LossKind::label). The codes are an append-only, `ronin-app`-local
/// namespace (mirroring E009's `BVY-S####`); do **not** renumber an existing
/// variant — tests and snapshots pin these strings (plan "Snapshot vs assertion
/// scope").
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum LossKind {
    /// `RON-I0001` — a named struct/tuple-struct name is dropped when the value is
    /// emitted as an anonymous JSON object/array (FR-004/015).
    StructName,
    /// `RON-I0002` — a RON tuple is emitted as a JSON array, losing the tuple-vs-list
    /// distinction for an external consumer (FR-004/015).
    TupleVsList,
    /// `RON-I0003` — a RON `char` is emitted as a one-character JSON string, losing
    /// the char-vs-string distinction (FR-004).
    Char,
    /// `RON-I0004` — a named enum variant is emitted via a serde-tagging convention
    /// (external-tag `{"V": …}` by default), losing the variant's RON-native shape to
    /// an external consumer (FR-004/015).
    EnumTagging,
    /// `RON-I0005` — a non-string map key is stringified (to a canonical RON literal),
    /// since JSON object keys are strings only (FR-004/015).
    NonStringKey,
    /// `RON-I0006` — a RON unit `()` is emitted as JSON `null`, losing the unit-vs-null
    /// distinction (FR-004).
    UnitVsNull,
    /// `RON-I0007` — a RON raw string is emitted as a standard JSON string, losing the
    /// raw-string form (the value is preserved; the syntactic form is not) (FR-004).
    RawString,
    /// `RON-I0008` — a trailing comma in a RON collection has no JSON representation
    /// and is dropped (FR-004).
    TrailingComma,
    /// `RON-I0009` — a comment is dropped because the chosen carrier is pure standard
    /// JSON (no JSONC inline, no sidecar) (FR-004/008).
    DroppedComment,
    /// `RON-I0010` — an unparseable RON region carried over as a flagged placeholder
    /// under the convert-remainder policy (FR-013, SC-008).
    UnparseableRegion,
}

impl LossKind {
    /// Every [`LossKind`] variant, in stable code order. Useful for exhaustive
    /// per-kind iteration (counts tables, fixtures) without re-listing variants.
    pub const ALL: &'static [LossKind] = &[
        LossKind::StructName,
        LossKind::TupleVsList,
        LossKind::Char,
        LossKind::EnumTagging,
        LossKind::NonStringKey,
        LossKind::UnitVsNull,
        LossKind::RawString,
        LossKind::TrailingComma,
        LossKind::DroppedComment,
        LossKind::UnparseableRegion,
    ];

    /// The stable `RON-I####` code string for this kind (FR-004/006).
    ///
    /// This is the identity tests, snapshots, and the inline diagnostic surface key
    /// on — it is append-only and never renumbered. Mirrors E009's
    /// [`crate::bevy::SceneDiagnosticCode::code`].
    #[inline]
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            LossKind::StructName => "RON-I0001",
            LossKind::TupleVsList => "RON-I0002",
            LossKind::Char => "RON-I0003",
            LossKind::EnumTagging => "RON-I0004",
            LossKind::NonStringKey => "RON-I0005",
            LossKind::UnitVsNull => "RON-I0006",
            LossKind::RawString => "RON-I0007",
            LossKind::TrailingComma => "RON-I0008",
            LossKind::DroppedComment => "RON-I0009",
            LossKind::UnparseableRegion => "RON-I0010",
        }
    }

    /// Alias for [`code`](Self::code) — the stable `RON-I####` string. Provided for
    /// call-site symmetry with code that expects an `as_str` accessor.
    #[inline]
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.code()
    }

    /// A short, human-readable label for this kind, shown in the loss dialog's
    /// per-kind summary (FR-005). Distinct from the per-construct
    /// [`LossyConstruct::detail`]; tests pin [`code`](Self::code), never this label.
    #[inline]
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            LossKind::StructName => "struct name dropped",
            LossKind::TupleVsList => "tuple emitted as array",
            LossKind::Char => "char emitted as string",
            LossKind::EnumTagging => "enum variant tagged",
            LossKind::NonStringKey => "non-string map key stringified",
            LossKind::UnitVsNull => "unit () emitted as null",
            LossKind::RawString => "raw string emitted as standard string",
            LossKind::TrailingComma => "trailing comma dropped",
            LossKind::DroppedComment => "comment dropped",
            LossKind::UnparseableRegion => "unparseable region placeholdered",
        }
    }

    /// The producing-component `source` tag for the inline diagnostic surface — the
    /// interop boundary is `ronin-app`-native, so every loss code is tagged
    /// `"ronin-interop"` (FR-006), mirroring E009's `"ronin-bevy"` tag.
    #[inline]
    #[must_use]
    pub fn source(self) -> &'static str {
        "ronin-interop"
    }
}

/// Whether RONin can losslessly recover a [`LossyConstruct`] on a round-trip, or it
/// is irrecoverable to an external JSON consumer (FR-004, STF-001).
///
/// **Both states are still reported.** "Lossy to external JSON" and "round-trip-safe
/// within RONin" coexist: a tuple emitted as a JSON array is lossy to an outside
/// reader yet recoverable by RONin when a `TypeModel` is bound (the expanded
/// round-trip tier, FR-011). This flag records *which* — it does **not** decide
/// whether the construct is listed; everything in a [`LossReport`] is listed
/// (HINT-004, data-model §LossyConstruct "Reported even when recoverable").
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum LossRecovery {
    /// The information is lost for an **external** JSON consumer and RONin cannot
    /// recover it on round-trip (the base-tier irrecoverable case, e.g. a dropped
    /// comment under pure standard JSON) (FR-004).
    LossyToExternal,
    /// Lossy to an external consumer, but RONin can losslessly **recover** it on a
    /// round-trip via the FR-015 conventions and a bound `TypeModel` (the expanded
    /// round-trip tier, FR-011) — still reported (STF-001).
    RoundTripSafeWithinRonin,
}

impl LossRecovery {
    /// The stable lowercase label for this recovery state.
    #[inline]
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            LossRecovery::LossyToExternal => "lossy-to-external",
            LossRecovery::RoundTripSafeWithinRonin => "round-trip-safe-within-ronin",
        }
    }

    /// `true` when RONin can recover this construct on a round-trip (the expanded
    /// round-trip tier). It is **still** a reported loss (STF-001).
    #[inline]
    #[must_use]
    pub fn is_round_trip_safe(self) -> bool {
        matches!(self, LossRecovery::RoundTripSafeWithinRonin)
    }
}

/// One entry of the lossy-construct map (FR-004): a single construct that cannot be
/// represented losslessly in standard JSON, with its [`kind`](Self::kind), its real
/// source CST [`source_range`](Self::source_range), whether RONin can
/// [`recover`](Self::recovery) it on round-trip, and an optional human
/// [`detail`](Self::detail).
///
/// It is the atomic unit shared by **one** loss-report line **and** **one** inline
/// E006 diagnostic (FR-005/006/007). The `source_range` is a real byte span sourced
/// from the projection's Pointer→`TextRange` index — never fabricated/empty
/// (data-model §LossyConstruct "Always carries a real source location").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LossyConstruct {
    /// The category of unrepresentable construct (FR-004).
    kind: LossKind,
    /// The exact offending byte span in the source CST (the value span, or the key
    /// span for a non-string key), from the projection's Pointer→`TextRange` index
    /// (FR-004/006). Never fabricated.
    source_range: TextRange,
    /// Whether RONin can losslessly recover this on round-trip, or it is lost to an
    /// external consumer (FR-004, STF-001).
    recovery: LossRecovery,
    /// An optional human-readable explanation of what was lost / how it is carried
    /// (e.g. "tuple → JSON array") shown in the loss dialog (FR-005). Tests key on
    /// [`kind`](Self::kind) / [`LossKind::code`], never on this wording.
    detail: Option<String>,
}

impl LossyConstruct {
    /// Build a lossy-construct entry with no [`detail`](Self::detail) string.
    ///
    /// `source_range` MUST be a real span from the projection index (never an
    /// empty/fabricated range) (data-model §LossyConstruct).
    #[inline]
    #[must_use]
    pub fn new(kind: LossKind, source_range: TextRange, recovery: LossRecovery) -> Self {
        Self {
            kind,
            source_range,
            recovery,
            detail: None,
        }
    }

    /// Build a lossy-construct entry with a human-readable [`detail`](Self::detail)
    /// string for the loss dialog (FR-005).
    #[inline]
    #[must_use]
    pub fn with_detail(
        kind: LossKind,
        source_range: TextRange,
        recovery: LossRecovery,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            source_range,
            recovery,
            detail: Some(detail.into()),
        }
    }

    /// This construct's [`LossKind`] (FR-004).
    #[inline]
    #[must_use]
    pub fn kind(&self) -> LossKind {
        self.kind
    }

    /// The exact offending byte span in the source CST (FR-004/006).
    #[inline]
    #[must_use]
    pub fn source_range(&self) -> TextRange {
        self.source_range
    }

    /// Whether RONin can recover this on round-trip (FR-004, STF-001).
    #[inline]
    #[must_use]
    pub fn recovery(&self) -> LossRecovery {
        self.recovery
    }

    /// The optional human-readable explanation, if one was supplied (FR-005).
    #[inline]
    #[must_use]
    pub fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }

    /// The stable `RON-I####` code for this construct's kind — the identity for the
    /// inline diagnostic and tests/snapshots (FR-006).
    #[inline]
    #[must_use]
    pub fn code(&self) -> &'static str {
        self.kind.code()
    }
}

/// The aggregate lossy-construct map for one conversion (FR-004/005): an ordered
/// list of every [`LossyConstruct`], plus per-kind counts and the
/// confirmation gate.
///
/// This ONE [`constructs`](Self::constructs) list is the **single source of truth**
/// that feeds **both** the pre-conversion loss dialog (FR-005) **and** the inline
/// E006 diagnostics (FR-006), so the two surfaces can never disagree (FR-007).
/// An empty report ([`is_empty`](Self::is_empty)) means the document is within the
/// applicable round-trip-safe tier — no confirmation required (FR-011, SC-001).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LossReport {
    /// The full, source-ordered lossy-construct map — the one list both surfaces
    /// read (FR-004/006/007).
    constructs: Vec<LossyConstruct>,
}

impl LossReport {
    /// An empty loss report — no losses, no confirmation required (the round-trip-safe
    /// tier, FR-011). Equivalent to [`LossReport::default`].
    #[inline]
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a report from a pre-collected list of constructs (their existing source
    /// order is preserved).
    #[inline]
    #[must_use]
    pub fn from_constructs(constructs: Vec<LossyConstruct>) -> Self {
        Self { constructs }
    }

    /// Append one [`LossyConstruct`] to the map (the builder operation).
    ///
    /// The single point through which every loss enters the report — there is no
    /// other path to a surface, which is what makes "never silently drop data"
    /// enforceable (FR-007).
    #[inline]
    pub fn push(&mut self, construct: LossyConstruct) {
        self.constructs.push(construct);
    }

    /// The full lossy-construct map, in source order — the one list that drives BOTH
    /// the loss dialog AND the inline diagnostics (FR-004/006/007).
    #[inline]
    #[must_use]
    pub fn constructs(&self) -> &[LossyConstruct] {
        &self.constructs
    }

    /// `true` when there are no losses — the conversion is within the applicable
    /// round-trip-safe tier and needs no confirmation (FR-011, SC-001).
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.constructs.is_empty()
    }

    /// The number of lossy constructs in the map.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.constructs.len()
    }

    /// `true` when the conversion is lossy and the user must confirm/cancel before a
    /// single byte is written — i.e. the report is non-empty (FR-005, SC-002).
    /// Cancelling a lossy conversion leaves every document and file byte-identical
    /// (FR-005, SC-003).
    #[inline]
    #[must_use]
    pub fn requires_confirmation(&self) -> bool {
        !self.is_empty()
    }

    /// The number of constructs of a single [`LossKind`] (FR-005, SC-002).
    #[must_use]
    pub fn count_of(&self, kind: LossKind) -> usize {
        self.constructs.iter().filter(|c| c.kind() == kind).count()
    }

    /// Per-kind counts for the loss dialog's summary ("2 tuples → arrays, 1 char →
    /// string, …") (FR-005, SC-002). Only kinds with at least one occurrence appear;
    /// the map is ordered by [`LossKind`]'s stable code order.
    #[must_use]
    pub fn counts_by_kind(&self) -> BTreeMap<LossKind, usize> {
        let mut counts: BTreeMap<LossKind, usize> = BTreeMap::new();
        for construct in &self.constructs {
            *counts.entry(construct.kind()).or_insert(0) += 1;
        }
        counts
    }
}

// ===========================================================================
// T009 — the lossy-construct map BUILDER (FR-004/007).
// ===========================================================================
//
// Walk the RON CST value tree detecting every construct that cannot be
// represented losslessly in standard JSON, and push a `LossyConstruct` for each
// with:
//   * the precise source `TextRange` from the projection's Pointer→`TextRange`
//     index (HINT-002) — the value span for a value-typed loss, the key span for
//     a non-string-key loss — falling back to the node's own real span when the
//     index has no entry (still a real CST span, never fabricated);
//   * the per-kind `recovery` flag (HINT-004): the expanded-tier kinds
//     (tuple/char/enum-tagging/non-string-key) are `RoundTripSafeWithinRonin`
//     when a `TypeModel` is bound (the FR-015 conventions recover them), else
//     `LossyToExternal`; the base-irrecoverable kinds (struct name, unit→null,
//     raw string, trailing comma, dropped comment) are always `LossyToExternal`.
//
// EVERY loss is reported even when recoverable (FR-007 / STF-001) — the
// `recovery` flag records *which*, never *whether* to list it.

use ronin_core::syntax::ast::{EnumVariant, List, Map, MapEntry, Struct, Tuple, Value};
use ronin_core::{CstDocument, SyntaxElement, SyntaxKind, SyntaxNode, SyntaxToken};
use ronin_validate::PointerRangeIndex;

use crate::interop::comments::CommentCarrier;
use crate::interop::pointer::projection_key_string;

/// Build the lossy-construct map for one RON→JSON conversion (FR-004/007).
///
/// Walks the document's CST value tree (in the projection's pointer coordinate
/// space, HINT-002) detecting each lossy construct, looks up its precise source
/// span in `projection_index`, and aggregates them — plus any dropped comments
/// from `comments` — into one [`LossReport`]. That one list drives BOTH the
/// pre-conversion loss dialog AND the inline diagnostics (FR-007).
///
/// * `doc` — the source document (read-only).
/// * `projection_index` — the [`PointerRangeIndex`] from the same
///   [`CstJsonProjection`](ronin_validate::CstJsonProjection) used to build the
///   JSON value (HINT-002); supplies the real source spans.
/// * `comments` — the comment carrier for this conversion; its dropped comments
///   (only when the carrier is pure-standard-JSON) become `DroppedComment` losses
///   so a dropped comment is never silent (FR-007/008).
/// * `bound` — whether a `TypeModel` is bound to the document. Drives the
///   expanded-tier `recovery` flag (HINT-004): tuple/char/enum-tagging/
///   non-string-key are `RoundTripSafeWithinRonin` when bound, else
///   `LossyToExternal`.
///
/// Every loss is reported even when `recovery` is `RoundTripSafeWithinRonin`
/// (FR-007, STF-001). The walk is read-only over the CST.
#[must_use]
pub fn build_loss_report(
    doc: &CstDocument,
    projection_index: &PointerRangeIndex,
    comments: &CommentCarrier,
    bound: bool,
) -> LossReport {
    let mut report = LossReport::new();
    let root = doc.root();
    if let Some(value) =
        ronin_core::syntax::ast::Document::cast(root.clone()).and_then(|d| d.value())
    {
        let mut walker = LossWalker {
            index: projection_index,
            bound,
            report: &mut report,
            pointer: String::new(),
            segment_starts: Vec::new(),
        };
        walker.walk(&value);
    }
    // Dropped comments (pure-standard-JSON only) are losses too — never silent
    // (FR-007/008). Each gets its real comment-token span and is irrecoverable to
    // an external consumer.
    for comment in comments.dropped_comments() {
        report.push(LossyConstruct::with_detail(
            LossKind::DroppedComment,
            comment.source_range,
            LossRecovery::LossyToExternal,
            "comment dropped — pure standard JSON, sidecar declined",
        ));
    }
    report
}

/// The recursive CST walker that detects lossy constructs and pushes them onto a
/// [`LossReport`], tracking the current JSON Pointer so it can look up the
/// projection index's span for each value (HINT-002).
struct LossWalker<'a> {
    index: &'a PointerRangeIndex,
    bound: bool,
    report: &'a mut LossReport,
    pointer: String,
    segment_starts: Vec<usize>,
}

impl LossWalker<'_> {
    /// The recovery flag for an expanded-tier construct: round-trip-safe within
    /// RONin when a `TypeModel` is bound (FR-011/015), else lossy to external
    /// (HINT-004).
    fn expanded_recovery(&self) -> LossRecovery {
        if self.bound {
            LossRecovery::RoundTripSafeWithinRonin
        } else {
            LossRecovery::LossyToExternal
        }
    }

    /// The projection-index value span for the current pointer, falling back to
    /// `node`'s own real span (never a fabricated/empty range).
    fn value_range(&self, node: &SyntaxNode) -> TextRange {
        self.index
            .value_range(&self.pointer)
            .unwrap_or_else(|| node.text_range())
    }

    fn push_key(&mut self, key: &str) {
        self.segment_starts.push(self.pointer.len());
        self.pointer.push('/');
        for ch in key.chars() {
            match ch {
                '~' => self.pointer.push_str("~0"),
                '/' => self.pointer.push_str("~1"),
                other => self.pointer.push(other),
            }
        }
    }

    fn push_index(&mut self, index: usize) {
        self.segment_starts.push(self.pointer.len());
        self.pointer.push('/');
        self.pointer.push_str(&index.to_string());
    }

    fn pop(&mut self) {
        if let Some(start) = self.segment_starts.pop() {
            self.pointer.truncate(start);
        }
    }

    /// Detect lossy constructs at `value` and recurse into its children, keeping
    /// the pointer in sync with the projection's walk (HINT-002).
    fn walk(&mut self, value: &Value) {
        match value {
            Value::Struct(s) => self.walk_struct(s),
            Value::Tuple(t) => self.walk_tuple(t),
            Value::List(l) => self.walk_list(l),
            Value::Map(m) => self.walk_map(m),
            Value::EnumVariant(v) => self.walk_enum_variant(v),
            Value::Unit(u) => self.detect_unit(u.syntax()),
            Value::Literal(lit) => self.detect_literal(lit),
            Value::Error(_) => {}
        }
    }

    fn walk_struct(&mut self, s: &Struct) {
        // A NAMED struct loses its name in the anonymous JSON object (FR-004/015).
        if s.name().is_some() {
            let range = self.value_range(s.syntax());
            self.report.push(LossyConstruct::with_detail(
                LossKind::StructName,
                range,
                LossRecovery::LossyToExternal,
                "struct name dropped — JSON object is anonymous",
            ));
        }
        // A trailing comma in the struct body is dropped (FR-004).
        self.detect_trailing_comma(s.syntax());
        for field in s.fields() {
            let Some(name_tok) = field.name() else {
                continue;
            };
            self.push_key(name_tok.text());
            if let Some(v) = field.value() {
                self.walk(&v);
            }
            self.pop();
        }
    }

    fn walk_tuple(&mut self, t: &Tuple) {
        let name = tuple_name(t);

        // `Some(x)` is the Option unwrap — handled as an Option loss, then unwrap.
        if name.as_deref() == Some("Some") {
            // `Some`/`None` is round-trip-safe only when bound (Option tier).
            // The projection unwraps it to the inner value at the same pointer, so
            // recurse without pushing a segment. (No dedicated Option loss kind in
            // FR-004's enumeration — Some/None is recovered via the bound type.)
            if let Some(inner) = t.items().next() {
                self.walk(&inner);
            }
            return;
        }

        let items: Vec<Value> = t.items().collect();
        if let Some(variant) = name {
            // A named tuple/newtype enum variant — tagged in JSON (FR-004/015).
            let range = self.value_range(t.syntax());
            self.report.push(LossyConstruct::with_detail(
                LossKind::EnumTagging,
                range,
                self.expanded_recovery(),
                "named tuple variant — emitted via serde tagging",
            ));
            self.detect_trailing_comma(t.syntax());
            self.push_key(&variant);
            match items.len() {
                0 => {}
                1 => self.walk(&items[0]),
                _ => {
                    for (i, item) in items.iter().enumerate() {
                        self.push_index(i);
                        self.walk(item);
                        self.pop();
                    }
                }
            }
            self.pop();
            return;
        }

        // An ANONYMOUS tuple → JSON array (tuple-vs-list lost, FR-004/015).
        let range = self.value_range(t.syntax());
        self.report.push(LossyConstruct::with_detail(
            LossKind::TupleVsList,
            range,
            self.expanded_recovery(),
            "tuple emitted as JSON array",
        ));
        self.detect_trailing_comma(t.syntax());
        for (i, item) in items.iter().enumerate() {
            self.push_index(i);
            self.walk(item);
            self.pop();
        }
    }

    fn walk_list(&mut self, l: &List) {
        self.detect_trailing_comma(l.syntax());
        for (i, item) in l.items().enumerate() {
            self.push_index(i);
            self.walk(&item);
            self.pop();
        }
    }

    fn walk_map(&mut self, m: &Map) {
        self.detect_trailing_comma(m.syntax());
        for entry in m.entries() {
            let Some(key_value) = entry.key() else {
                continue;
            };
            // A NON-STRING map key is stringified to a canonical RON literal in
            // JSON (FR-004/015). Report it at the KEY span (HINT-002).
            if !is_string_key(&key_value) {
                let pointer_key = projection_key_string(&key_value);
                self.push_key(&pointer_key);
                let range = self
                    .index
                    .key_range(&self.pointer)
                    .unwrap_or_else(|| key_value.syntax().text_range());
                self.report.push(LossyConstruct::with_detail(
                    LossKind::NonStringKey,
                    range,
                    self.expanded_recovery(),
                    "non-string map key stringified to canonical RON literal",
                ));
                if let Some(v) = entry.value() {
                    self.walk(&v);
                }
                self.pop();
            } else {
                let key = projection_key_string(&key_value);
                self.push_key(&key);
                if let Some(v) = entry.value() {
                    self.walk(&v);
                }
                self.pop();
            }
        }
    }

    fn walk_enum_variant(&mut self, v: &EnumVariant) {
        let name = v.name_text().unwrap_or_default();
        let node = v.syntax();

        // `None` (no payload) → JSON null; recovered via a bound Option type.
        if name == "None" && payload_values(node).next().is_none() && v.entries().next().is_none() {
            // Option None is round-trip-safe only when bound — but there is no
            // dedicated FR-004 kind for it; the projection maps it to null and a
            // bound Option re-types it. No loss kind is enumerated, so it is not
            // pushed (consistent with FR-004's ten kinds).
            return;
        }
        // `Some(x)` → unwrap at the same pointer.
        if name == "Some" {
            if let Some(inner) = payload_values(node).next() {
                self.walk(&inner);
                return;
            }
        }

        // Any other named variant is emitted via serde tagging (FR-004/015).
        let range = self.value_range(node);
        self.report.push(LossyConstruct::with_detail(
            LossKind::EnumTagging,
            range,
            self.expanded_recovery(),
            "enum variant emitted via serde tagging",
        ));
        self.detect_trailing_comma(node);

        self.push_key(&name);
        let struct_entries: Vec<MapEntry> = v.entries().collect();
        if !struct_entries.is_empty() || has_brace(node) {
            for entry in &struct_entries {
                let Some(key) = entry_key_name(entry) else {
                    continue;
                };
                self.push_key(&key);
                if let Some(val) = entry.value() {
                    self.walk(&val);
                }
                self.pop();
            }
        } else {
            let payload: Vec<Value> = payload_values(node).collect();
            match payload.len() {
                0 => {}
                1 => self.walk(&payload[0]),
                _ => {
                    for (i, item) in payload.iter().enumerate() {
                        self.push_index(i);
                        self.walk(item);
                        self.pop();
                    }
                }
            }
        }
        self.pop();
    }

    /// A RON unit `()` → JSON `null` (unit-vs-null lost, FR-004).
    fn detect_unit(&mut self, node: &SyntaxNode) {
        let range = self.value_range(node);
        self.report.push(LossyConstruct::with_detail(
            LossKind::UnitVsNull,
            range,
            LossRecovery::LossyToExternal,
            "unit () emitted as JSON null",
        ));
    }

    /// A `char` literal → one-char JSON string (expanded tier); a raw string →
    /// standard JSON string (the value survives, the raw form does not) (FR-004).
    fn detect_literal(&mut self, lit: &ronin_core::syntax::ast::Literal) {
        match lit.token_kind() {
            Some(SyntaxKind::Char) => {
                let range = self.value_range(lit.syntax());
                self.report.push(LossyConstruct::with_detail(
                    LossKind::Char,
                    range,
                    self.expanded_recovery(),
                    "char emitted as one-character JSON string",
                ));
            }
            Some(SyntaxKind::RawString) => {
                let range = self.value_range(lit.syntax());
                self.report.push(LossyConstruct::with_detail(
                    LossKind::RawString,
                    range,
                    LossRecovery::LossyToExternal,
                    "raw string emitted as standard JSON string",
                ));
            }
            _ => {}
        }
    }

    /// Detect a trailing comma immediately before a collection's closing
    /// delimiter (FR-004). A trailing comma has no JSON representation; it is pure
    /// formatting (the round-trip oracle ignores it) but is still lossy to an
    /// external consumer, so it is reported.
    ///
    /// Scans only the collection's **direct** children (the delimiters, commas,
    /// and value nodes live directly under the collection node), so a comma deep
    /// inside a nested child is never mistaken for this collection's trailing one.
    fn detect_trailing_comma(&mut self, node: &SyntaxNode) {
        // The candidate is the most recent direct `Comma` token; a closing
        // delimiter immediately after it (ignoring trivia) confirms a trailing
        // comma, while any intervening direct value node clears the candidate.
        let mut comma_before_close: Option<SyntaxToken> = None;
        for el in node.children_with_tokens() {
            match el {
                SyntaxElement::Node(_) => {
                    // A child value node resets candidacy (a comma after it might
                    // be a trailing one, detected on the next iteration).
                    comma_before_close = None;
                }
                SyntaxElement::Token(tok) => {
                    let kind = tok.kind();
                    if kind.is_trivia() {
                        continue;
                    }
                    match kind {
                        SyntaxKind::Comma => comma_before_close = Some(tok),
                        SyntaxKind::RParen | SyntaxKind::RBracket | SyntaxKind::RBrace => {
                            if let Some(comma) = comma_before_close.take() {
                                self.report.push(LossyConstruct::with_detail(
                                    LossKind::TrailingComma,
                                    comma.text_range(),
                                    LossRecovery::LossyToExternal,
                                    "trailing comma dropped — no JSON representation",
                                ));
                            }
                        }
                        _ => comma_before_close = None,
                    }
                }
            }
        }
    }
}

/// Whether a map key is a string-typed key (string / raw-string / char keys are
/// JSON-string-native; everything else is a non-string key, FR-004/015).
fn is_string_key(key: &Value) -> bool {
    if let Value::Literal(lit) = key {
        matches!(
            lit.token_kind(),
            Some(SyntaxKind::String | SyntaxKind::RawString)
        )
    } else {
        false
    }
}

/// The leading `Ident` name of a named tuple (`Name(..)`), or `None` for an
/// anonymous tuple.
fn tuple_name(t: &Tuple) -> Option<String> {
    t.syntax()
        .first_token_of(SyntaxKind::Ident)
        .map(|tok| tok.text().to_string())
}

/// The positional payload values inside a variant `Variant(a, b, ..)`.
fn payload_values(node: &SyntaxNode) -> impl Iterator<Item = Value> {
    node.children().filter_map(Value::cast)
}

/// Whether a variant node uses brace-style payload `{ .. }` (struct-like).
fn has_brace(node: &SyntaxNode) -> bool {
    node.children_with_tokens()
        .any(|el| el.kind() == SyntaxKind::LBrace)
}

/// The field-name string of a struct-like variant entry.
fn entry_key_name(entry: &MapEntry) -> Option<String> {
    let key = entry.key()?;
    match &key {
        Value::EnumVariant(ev) => ev.name_text(),
        Value::Literal(lit) => lit.text(),
        other => Some(other.syntax().text()),
    }
}

#[cfg(test)]
mod tests {
    //! T004 — the lossy-construct map types: stable `RON-I####` codes, the
    //! recovery flag, and the one-list confirmation/count surface (FR-004/007).

    use super::*;

    fn range(start: usize, end: usize) -> TextRange {
        TextRange::new(start, end)
    }

    #[test]
    fn every_kind_has_a_distinct_stable_ron_i_code() {
        // The codes are the stable RON-I#### namespace tests/snapshots pin.
        for kind in LossKind::ALL {
            let code = kind.code();
            assert!(
                code.starts_with("RON-I"),
                "kind {kind:?} code `{code}` must be in the RON-I#### namespace"
            );
            assert_eq!(code, kind.as_str(), "as_str must alias code for {kind:?}");
            assert_eq!(kind.source(), "ronin-interop");
            assert!(!kind.label().is_empty());
        }
        // All ten codes are globally distinct (no accidental collision/renumber).
        let mut codes: Vec<&str> = LossKind::ALL.iter().map(|k| k.code()).collect();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(
            codes.len(),
            LossKind::ALL.len(),
            "loss codes must be unique"
        );
        assert_eq!(LossKind::ALL.len(), 10, "FR-004 enumerates ten lossy kinds");
    }

    #[test]
    fn exact_codes_are_pinned() {
        // Pin the exact code strings so a renumber is a deliberate, breaking change.
        assert_eq!(LossKind::StructName.code(), "RON-I0001");
        assert_eq!(LossKind::TupleVsList.code(), "RON-I0002");
        assert_eq!(LossKind::Char.code(), "RON-I0003");
        assert_eq!(LossKind::EnumTagging.code(), "RON-I0004");
        assert_eq!(LossKind::NonStringKey.code(), "RON-I0005");
        assert_eq!(LossKind::UnitVsNull.code(), "RON-I0006");
        assert_eq!(LossKind::RawString.code(), "RON-I0007");
        assert_eq!(LossKind::TrailingComma.code(), "RON-I0008");
        assert_eq!(LossKind::DroppedComment.code(), "RON-I0009");
        assert_eq!(LossKind::UnparseableRegion.code(), "RON-I0010");
    }

    #[test]
    fn empty_report_needs_no_confirmation() {
        let report = LossReport::new();
        assert!(report.is_empty());
        assert!(!report.requires_confirmation());
        assert_eq!(report.len(), 0);
        assert!(report.counts_by_kind().is_empty());
    }

    #[test]
    fn non_empty_report_requires_confirmation() {
        let mut report = LossReport::new();
        report.push(LossyConstruct::new(
            LossKind::TupleVsList,
            range(3, 9),
            LossRecovery::RoundTripSafeWithinRonin,
        ));
        assert!(!report.is_empty());
        assert!(report.requires_confirmation());
        assert_eq!(report.len(), 1);
    }

    #[test]
    fn recovery_flag_is_distinct_from_being_reported() {
        // A round-trip-safe construct is STILL reported (STF-001 / HINT-004).
        let mut report = LossReport::new();
        report.push(LossyConstruct::new(
            LossKind::TupleVsList,
            range(0, 4),
            LossRecovery::RoundTripSafeWithinRonin,
        ));
        report.push(LossyConstruct::new(
            LossKind::DroppedComment,
            range(10, 20),
            LossRecovery::LossyToExternal,
        ));
        assert_eq!(report.len(), 2, "both are listed regardless of recovery");
        assert!(report.constructs()[0].recovery().is_round_trip_safe());
        assert!(!report.constructs()[1].recovery().is_round_trip_safe());
        assert_eq!(
            report.constructs()[0].recovery().as_str(),
            "round-trip-safe-within-ronin"
        );
        assert_eq!(
            report.constructs()[1].recovery().as_str(),
            "lossy-to-external"
        );
    }

    #[test]
    fn counts_by_kind_aggregates_the_one_list() {
        let mut report = LossReport::new();
        report.push(LossyConstruct::new(
            LossKind::TupleVsList,
            range(0, 4),
            LossRecovery::RoundTripSafeWithinRonin,
        ));
        report.push(LossyConstruct::new(
            LossKind::TupleVsList,
            range(5, 9),
            LossRecovery::RoundTripSafeWithinRonin,
        ));
        report.push(LossyConstruct::new(
            LossKind::Char,
            range(11, 14),
            LossRecovery::RoundTripSafeWithinRonin,
        ));
        let counts = report.counts_by_kind();
        assert_eq!(counts.get(&LossKind::TupleVsList), Some(&2));
        assert_eq!(counts.get(&LossKind::Char), Some(&1));
        assert_eq!(counts.get(&LossKind::DroppedComment), None);
        assert_eq!(report.count_of(LossKind::TupleVsList), 2);
        assert_eq!(report.count_of(LossKind::UnitVsNull), 0);
    }

    #[test]
    fn detail_and_source_range_are_carried() {
        let c = LossyConstruct::with_detail(
            LossKind::Char,
            range(7, 10),
            LossRecovery::RoundTripSafeWithinRonin,
            "char → string",
        );
        assert_eq!(c.kind(), LossKind::Char);
        assert_eq!(c.code(), "RON-I0003");
        assert_eq!(c.source_range(), range(7, 10));
        assert_eq!(c.detail(), Some("char → string"));
        let plain =
            LossyConstruct::new(LossKind::Char, range(7, 10), LossRecovery::LossyToExternal);
        assert_eq!(plain.detail(), None);
    }

    #[test]
    fn from_constructs_preserves_order() {
        let constructs = vec![
            LossyConstruct::new(LossKind::Char, range(0, 3), LossRecovery::LossyToExternal),
            LossyConstruct::new(
                LossKind::TupleVsList,
                range(4, 8),
                LossRecovery::LossyToExternal,
            ),
        ];
        let report = LossReport::from_constructs(constructs);
        assert_eq!(report.len(), 2);
        assert_eq!(report.constructs()[0].kind(), LossKind::Char);
        assert_eq!(report.constructs()[1].kind(), LossKind::TupleVsList);
    }

    // --- T009: build_loss_report walk (FR-004/007) -------------------------

    use crate::interop::comments::{CommentCarrier, CommentMode};

    /// Build the loss report for `src` using the same projection index the value
    /// map uses (HINT-002), with comments preserved (JSONC) so the only losses
    /// are the value constructs.
    fn report_for(src: &str, bound: bool) -> LossReport {
        let doc = ronin_core::parse(src);
        let proj = ronin_validate::CstJsonProjection::from_document(&doc);
        let comments = CommentCarrier::from_document(&doc, CommentMode::JsoncInline);
        build_loss_report(&doc, &proj.index, &comments, bound)
    }

    fn kinds(report: &LossReport) -> Vec<LossKind> {
        report.constructs().iter().map(|c| c.kind()).collect()
    }

    #[test]
    fn detects_named_struct_name_loss() {
        let r = report_for("Player(hp: 10)", false);
        assert_eq!(r.count_of(LossKind::StructName), 1);
        let c = r
            .constructs()
            .iter()
            .find(|c| c.kind() == LossKind::StructName)
            .unwrap();
        // The struct-name loss is irrecoverable to external JSON.
        assert_eq!(c.recovery(), LossRecovery::LossyToExternal);
        // Real, non-empty source span.
        assert!(!c.source_range().is_empty());
    }

    #[test]
    fn anonymous_struct_has_no_struct_name_loss() {
        let r = report_for("(hp: 10)", false);
        assert_eq!(r.count_of(LossKind::StructName), 0);
    }

    #[test]
    fn detects_tuple_char_unit_with_correct_recovery() {
        // Unbound: expanded-tier kinds are lossy-to-external.
        let unbound = report_for("(t: (1, 2), c: 'x', u: ())", false);
        assert_eq!(unbound.count_of(LossKind::TupleVsList), 1);
        assert_eq!(unbound.count_of(LossKind::Char), 1);
        assert_eq!(unbound.count_of(LossKind::UnitVsNull), 1);
        let tuple = unbound
            .constructs()
            .iter()
            .find(|c| c.kind() == LossKind::TupleVsList)
            .unwrap();
        assert_eq!(tuple.recovery(), LossRecovery::LossyToExternal);

        // Bound: the expanded-tier kinds (tuple, char) become round-trip-safe;
        // unit→null stays lossy-to-external (not in the expanded tier).
        let bound = report_for("(t: (1, 2), c: 'x', u: ())", true);
        let tuple = bound
            .constructs()
            .iter()
            .find(|c| c.kind() == LossKind::TupleVsList)
            .unwrap();
        assert_eq!(tuple.recovery(), LossRecovery::RoundTripSafeWithinRonin);
        let chr = bound
            .constructs()
            .iter()
            .find(|c| c.kind() == LossKind::Char)
            .unwrap();
        assert_eq!(chr.recovery(), LossRecovery::RoundTripSafeWithinRonin);
        let unit = bound
            .constructs()
            .iter()
            .find(|c| c.kind() == LossKind::UnitVsNull)
            .unwrap();
        assert_eq!(unit.recovery(), LossRecovery::LossyToExternal);
    }

    #[test]
    fn detects_enum_tagging_and_non_string_key() {
        let r = report_for("(state: Running, m: { 1: \"a\" })", true);
        assert_eq!(r.count_of(LossKind::EnumTagging), 1);
        assert_eq!(r.count_of(LossKind::NonStringKey), 1);
        let nsk = r
            .constructs()
            .iter()
            .find(|c| c.kind() == LossKind::NonStringKey)
            .unwrap();
        assert_eq!(nsk.recovery(), LossRecovery::RoundTripSafeWithinRonin);
        // The key loss is anchored at the key span (a real span).
        assert!(!nsk.source_range().is_empty());
    }

    #[test]
    fn detects_raw_string_loss() {
        let r = report_for("(s: r#\"raw\"#)", false);
        assert_eq!(r.count_of(LossKind::RawString), 1);
        let c = r
            .constructs()
            .iter()
            .find(|c| c.kind() == LossKind::RawString)
            .unwrap();
        assert_eq!(c.recovery(), LossRecovery::LossyToExternal);
    }

    #[test]
    fn detects_trailing_comma_loss() {
        let r = report_for("[1, 2,]", false);
        assert_eq!(r.count_of(LossKind::TrailingComma), 1);
        let c = r
            .constructs()
            .iter()
            .find(|c| c.kind() == LossKind::TrailingComma)
            .unwrap();
        // The span is exactly the trailing comma token (one byte).
        assert_eq!(c.source_range().len(), 1);
    }

    #[test]
    fn no_trailing_comma_when_absent() {
        let r = report_for("[1, 2]", false);
        assert_eq!(r.count_of(LossKind::TrailingComma), 0);
    }

    #[test]
    fn dropped_comments_are_losses_only_in_pure_json() {
        let doc = ronin_core::parse("// c\n(x: 1)");
        let proj = ronin_validate::CstJsonProjection::from_document(&doc);
        // JSONC: the comment is carried, not dropped.
        let jsonc = CommentCarrier::from_document(&doc, CommentMode::JsoncInline);
        let r = build_loss_report(&doc, &proj.index, &jsonc, false);
        assert_eq!(r.count_of(LossKind::DroppedComment), 0);
        // Pure standard JSON: the comment is dropped → reported (FR-007).
        let none = CommentCarrier::from_document(&doc, CommentMode::None);
        let r = build_loss_report(&doc, &proj.index, &none, false);
        assert_eq!(r.count_of(LossKind::DroppedComment), 1);
        let c = r
            .constructs()
            .iter()
            .find(|c| c.kind() == LossKind::DroppedComment)
            .unwrap();
        assert_eq!(c.recovery(), LossRecovery::LossyToExternal);
    }

    #[test]
    fn round_trip_safe_document_has_empty_report() {
        // Scalars + a string-keyed map + a list: the base tier, always safe.
        let r = report_for("(n: 1, s: \"x\", l: [1, 2], m: {\"k\": 3})", false);
        assert!(
            r.is_empty(),
            "base-tier document reports no losses: {:?}",
            kinds(&r)
        );
        assert!(!r.requires_confirmation());
    }

    #[test]
    fn every_loss_keeps_a_real_source_range() {
        // No fabricated/empty ranges (data-model §LossyConstruct).
        let r = report_for(
            "Player(t: (1, 2), c: 'x', u: (), s: r#\"r\"#, m: { 1: \"a\", }, e: Running)",
            false,
        );
        assert!(!r.is_empty());
        for c in r.constructs() {
            assert!(
                !c.source_range().is_empty(),
                "{:?} has an empty span",
                c.kind()
            );
        }
    }
}
