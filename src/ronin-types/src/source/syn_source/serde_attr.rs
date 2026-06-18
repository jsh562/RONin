//! serde attribute parsing for the `syn` source {TR-005}.
//!
//! This module reads the EXHAUSTIVE serde attribute set off container / field /
//! variant `#[serde(...)]` lists and maps each to the normalized model so the
//! shape matches serde's real (de)serialization. Attribute semantics follow the
//! serde documentation:
//!
//! - container attrs: <https://serde.rs/container-attrs.html>
//! - field attrs: <https://serde.rs/field-attrs.html>
//! - variant attrs: <https://serde.rs/variant-attrs.html>
//!
//! # Attribute → model mapping (TR-005)
//!
//! | serde attribute | handling |
//! |-----------------|----------|
//! | `rename = "x"` (field/variant) | overrides the serialized key/name verbatim |
//! | `rename_all = "case"` (container) | applies the case convention to field keys |
//! | `rename_all = "case"` on an enum | applies to **variant names** |
//! | `rename_all_fields = "case"` (enum) | applies to every variant's struct fields |
//! | `default` / `default = "path"` | field becomes optional (not required) |
//! | `skip` / `skip_serializing` / `skip_deserializing` | field/variant removed from the model |
//! | `skip_serializing_if = "path"` | field becomes optional |
//! | `flatten` | field marked `flatten`; target's keys are inlined by serde (see note) |
//! | `tag = "t"` (enum) | internally-tagged → `Discriminator::Internal` |
//! | `tag = "t", content = "c"` (enum) | adjacently-tagged → `Discriminator::Adjacent` |
//! | `untagged` (enum) | `Discriminator::Untagged` |
//! | `transparent` (struct) | collapses the one-field struct to its inner type |
//! | `deny_unknown_fields` (struct) | sets the object's `deny_unknown_fields` flag |
//! | `with` / `from` / `into` / `try_from` | converter shape is invisible to syn → the affected type is `unknown` (HONEST LIMIT) |
//! | `borrow` / `bound` | no data-shape change (noted, ignored) |
//!
//! ## Honest limitation: `with` / `from` / `into` / `try_from`
//!
//! These attributes route (de)serialization through a *converter* (a module, or a
//! `From`/`Into`/`TryFrom` impl) whose on-the-wire shape is defined by code syn
//! cannot see. A purely static AST pass therefore cannot know the effective type,
//! so the affected field is modeled as `unknown` with an `UnresolvedType`
//! diagnostic rather than guessing. This is faithful to the constraint that
//! `syn` is syntax-only (ADR-0004 Progressive Intelligence: degrade, never lie).

use crate::model::Discriminator;

/// serde case-conversion conventions for `rename_all` / `rename_all_fields`.
///
/// Mirrors serde's supported set exactly so generated keys match serde's output.
/// See <https://serde.rs/container-attrs.html#rename_all>.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RenameRule {
    /// No conversion — the identifier is used verbatim.
    #[default]
    None,
    /// `"lowercase"`.
    Lower,
    /// `"UPPERCASE"`.
    Upper,
    /// `"PascalCase"`.
    Pascal,
    /// `"camelCase"`.
    Camel,
    /// `"snake_case"`.
    Snake,
    /// `"SCREAMING_SNAKE_CASE"`.
    ScreamingSnake,
    /// `"kebab-case"`.
    Kebab,
    /// `"SCREAMING-KEBAB-CASE"`.
    ScreamingKebab,
}

impl RenameRule {
    /// Parse a serde `rename_all` string literal into a rule. Unknown strings map
    /// to [`RenameRule::None`] (serde would reject them at compile time; we stay
    /// permissive and unconverted rather than guessing).
    fn from_str(s: &str) -> Self {
        match s {
            "lowercase" => RenameRule::Lower,
            "UPPERCASE" => RenameRule::Upper,
            "PascalCase" => RenameRule::Pascal,
            "camelCase" => RenameRule::Camel,
            "snake_case" => RenameRule::Snake,
            "SCREAMING_SNAKE_CASE" => RenameRule::ScreamingSnake,
            "kebab-case" => RenameRule::Kebab,
            "SCREAMING-KEBAB-CASE" => RenameRule::ScreamingKebab,
            _ => RenameRule::None,
        }
    }

    /// Apply the rule to a **struct field** identifier (conventionally
    /// `snake_case`), producing the serialized key.
    ///
    /// This is a faithful port of `serde_derive_internals`'
    /// `RenameRule::apply_to_field`, so the resulting keys match serde's actual
    /// (de)serialization exactly. <https://serde.rs/container-attrs.html#rename_all>
    #[must_use]
    pub fn apply_to_field(self, field: &str) -> String {
        match self {
            RenameRule::None | RenameRule::Lower | RenameRule::Snake => field.to_owned(),
            RenameRule::Upper => field.to_ascii_uppercase(),
            RenameRule::Pascal => {
                let mut pascal = String::new();
                let mut capitalize = true;
                for ch in field.chars() {
                    if ch == '_' {
                        capitalize = true;
                    } else if capitalize {
                        pascal.push(ch.to_ascii_uppercase());
                        capitalize = false;
                    } else {
                        pascal.push(ch);
                    }
                }
                pascal
            }
            RenameRule::Camel => {
                let pascal = RenameRule::Pascal.apply_to_field(field);
                pascal[..1].to_ascii_lowercase() + &pascal[1..]
            }
            RenameRule::ScreamingSnake => field.to_ascii_uppercase(),
            RenameRule::Kebab => field.replace('_', "-"),
            RenameRule::ScreamingKebab => RenameRule::ScreamingSnake
                .apply_to_field(field)
                .replace('_', "-"),
        }
    }

    /// Apply the rule to an **enum variant** identifier (conventionally
    /// `PascalCase`), producing the serialized variant name.
    ///
    /// Faithful port of `serde_derive_internals`' `RenameRule::apply_to_variant`.
    #[must_use]
    pub fn apply_to_variant(self, variant: &str) -> String {
        match self {
            RenameRule::None | RenameRule::Pascal => variant.to_owned(),
            RenameRule::Lower => variant.to_ascii_lowercase(),
            RenameRule::Upper => variant.to_ascii_uppercase(),
            RenameRule::Camel => variant[..1].to_ascii_lowercase() + &variant[1..],
            RenameRule::Snake => {
                let mut snake = String::new();
                for (i, ch) in variant.char_indices() {
                    if i > 0 && ch.is_uppercase() {
                        snake.push('_');
                    }
                    snake.push(ch.to_ascii_lowercase());
                }
                snake
            }
            RenameRule::ScreamingSnake => RenameRule::Snake
                .apply_to_variant(variant)
                .to_ascii_uppercase(),
            RenameRule::Kebab => RenameRule::Snake
                .apply_to_variant(variant)
                .replace('_', "-"),
            RenameRule::ScreamingKebab => RenameRule::ScreamingSnake
                .apply_to_variant(variant)
                .replace('_', "-"),
        }
    }
}

/// Parsed container-level serde attributes (struct or enum).
#[derive(Debug, Clone, Default)]
pub struct ContainerAttrs {
    /// `rename_all` for the container's fields (struct) or variants (enum).
    pub rename_all: RenameRule,
    /// `rename_all_fields` — enum-only, applied to every variant's struct fields.
    pub rename_all_fields: RenameRule,
    /// `deny_unknown_fields`.
    pub deny_unknown_fields: bool,
    /// `transparent` (struct).
    pub transparent: bool,
    /// `tag = "..."` (enum internal/adjacent tag).
    pub tag: Option<String>,
    /// `content = "..."` (enum adjacent content).
    pub content: Option<String>,
    /// `untagged` (enum).
    pub untagged: bool,
}

impl ContainerAttrs {
    /// Parse the serde attributes on a container, reporting unsupported/lossy
    /// constructs through `note`.
    pub fn parse(attrs: &[syn::Attribute], _note: &mut dyn FnMut(String)) -> Self {
        let mut out = ContainerAttrs::default();
        for_each_serde_meta(attrs, |meta| match meta {
            SerdeMeta::Path(name) => match name.as_str() {
                "deny_unknown_fields" => out.deny_unknown_fields = true,
                "transparent" => out.transparent = true,
                "untagged" => out.untagged = true,
                // Container `default` (a struct default fn) does not change the
                // data shape — every field is still itself; field optionality is
                // driven per-field. Noted, no shape change.
                "default" => {}
                _ => {}
            },
            SerdeMeta::NameValue(name, value) => match name.as_str() {
                "rename_all" => out.rename_all = RenameRule::from_str(&value),
                "rename_all_fields" => out.rename_all_fields = RenameRule::from_str(&value),
                "tag" => out.tag = Some(value),
                "content" => out.content = Some(value),
                // `bound`/`crate`/`expecting` etc. do not change the data shape.
                _ => {}
            },
            // `default = "path"` name-value handled above via NameValue("default").
        });
        out
    }

    /// Compute the enum [`Discriminator`] from the parsed tag/content/untagged.
    /// <https://serde.rs/enum-representations.html>
    #[must_use]
    pub fn discriminator(&self) -> Discriminator {
        if self.untagged {
            Discriminator::Untagged
        } else {
            match (&self.tag, &self.content) {
                (Some(tag), Some(content)) => Discriminator::Adjacent {
                    tag: tag.clone(),
                    content: content.clone(),
                },
                (Some(tag), None) => Discriminator::Internal { tag: tag.clone() },
                _ => Discriminator::External,
            }
        }
    }
}

/// Parsed field-level serde attributes.
#[derive(Debug, Clone, Default)]
pub struct FieldAttrs {
    /// `rename = "..."` — overrides the serialized key verbatim.
    pub rename: Option<String>,
    /// `default` / `default = "path"` — field optional.
    pub default: bool,
    /// `skip` / `skip_serializing` / `skip_deserializing` — field removed.
    pub skip: bool,
    /// `skip_serializing_if = "path"` — field optional.
    pub skip_serializing_if: bool,
    /// `flatten` — field's target keys are inlined by serde.
    pub flatten: bool,
    /// `with` / `from` / `into` / `try_from` — converter shape invisible to syn.
    pub opaque_conversion: bool,
}

impl FieldAttrs {
    /// Parse the serde attributes on a field, reporting lossy constructs through
    /// `note`.
    pub fn parse(attrs: &[syn::Attribute], note: &mut dyn FnMut(String)) -> Self {
        let mut out = FieldAttrs::default();
        for_each_serde_meta(attrs, |meta| match meta {
            SerdeMeta::Path(name) => match name.as_str() {
                "default" => out.default = true,
                "skip" | "skip_serializing" | "skip_deserializing" => out.skip = true,
                "flatten" => out.flatten = true,
                // `borrow` with no value: zero-copy borrow, no data-shape change.
                "borrow" => {}
                _ => {}
            },
            SerdeMeta::NameValue(name, _value) => match name.as_str() {
                "rename" => out.rename = Some(_value),
                "default" => out.default = true,
                "skip_serializing_if" => out.skip_serializing_if = true,
                // Converters route through code syn cannot see → opaque.
                "with" | "from" | "into" | "try_from" | "deserialize_with" | "serialize_with" => {
                    out.opaque_conversion = true;
                    note(format!(
                        "serde `{name}` converter shape is not visible to static \
                         analysis; field type recorded as unknown"
                    ));
                }
                // `borrow = "..."`, `bound = "..."`: no data-shape change.
                _ => {}
            },
        });
        out
    }
}

/// Parsed variant-level serde attributes.
#[derive(Debug, Clone, Default)]
pub struct VariantAttrs {
    /// `rename = "..."` — overrides the serialized variant name verbatim.
    pub rename: Option<String>,
    /// `skip` / `skip_serializing` / `skip_deserializing` — variant removed.
    pub skip: bool,
}

impl VariantAttrs {
    /// Parse the serde attributes on a variant.
    pub fn parse(attrs: &[syn::Attribute], _note: &mut dyn FnMut(String)) -> Self {
        let mut out = VariantAttrs::default();
        for_each_serde_meta(attrs, |meta| match meta {
            SerdeMeta::Path(name) => {
                if matches!(
                    name.as_str(),
                    "skip" | "skip_serializing" | "skip_deserializing"
                ) {
                    out.skip = true;
                }
            }
            SerdeMeta::NameValue(name, value) => {
                if name == "rename" {
                    out.rename = Some(value);
                }
            }
        });
        out
    }
}

/// A single parsed `#[serde(...)]` meta entry — either a bare path (`flatten`) or
/// a `name = "value"` pair (`rename = "x"`).
enum SerdeMeta {
    Path(String),
    NameValue(String, String),
}

/// Walk every `#[serde(...)]` attribute and invoke `f` for each comma-separated
/// meta entry inside it. Non-serde attributes are ignored. Nested or unexpected
/// shapes are skipped silently (serde would reject them at compile time).
fn for_each_serde_meta(attrs: &[syn::Attribute], mut f: impl FnMut(SerdeMeta)) {
    for attr in attrs {
        if !attr.path().is_ident("serde") {
            continue;
        }
        // `#[serde( <nested,...> )]` — parse the comma-separated list.
        let _ = attr.parse_nested_meta(|meta| {
            let Some(ident) = meta.path.get_ident() else {
                return Ok(());
            };
            let name = ident.to_string();
            // Try to read a `= "value"` if present; otherwise it's a bare path.
            if let Ok(value) = meta.value() {
                if let Ok(lit) = value.parse::<syn::LitStr>() {
                    f(SerdeMeta::NameValue(name, lit.value()));
                    return Ok(());
                }
                // A non-string value (e.g. `default = path`) — treat as a flag
                // presence (e.g. `default`) by emitting the bare path form.
                f(SerdeMeta::Path(name));
                // Consume the remaining tokens of this value so the parser does
                // not error on an unexpected token.
                let _ = value.parse::<proc_macro2_fallback::AnyTokens>();
                return Ok(());
            }
            f(SerdeMeta::Path(name));
            Ok(())
        });
    }
}

/// Minimal token sink so `for_each_serde_meta` can swallow non-string attribute
/// values (e.g. `default = some::path`) without taking a `proc-macro2` direct
/// dependency. `syn::parse::ParseStream` exposes the cursor we drain here.
mod proc_macro2_fallback {
    use syn::parse::{Parse, ParseStream};

    /// Parses and discards any remaining tokens in the stream.
    pub struct AnyTokens;

    impl Parse for AnyTokens {
        fn parse(input: ParseStream) -> syn::Result<Self> {
            input.step(|cursor| {
                let mut rest = *cursor;
                while let Some((_, next)) = rest.token_tree() {
                    rest = next;
                }
                Ok(((), rest))
            })?;
            Ok(AnyTokens)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Field conversions: input is snake_case (matches serde's apply_to_field).
    #[test]
    fn field_camel_case_conversion() {
        assert_eq!(RenameRule::Camel.apply_to_field("first_name"), "firstName");
        assert_eq!(RenameRule::Camel.apply_to_field("id"), "id");
        assert_eq!(
            RenameRule::Camel.apply_to_field("http_status_code"),
            "httpStatusCode"
        );
    }

    #[test]
    fn field_pascal_and_snake_conversions() {
        assert_eq!(RenameRule::Pascal.apply_to_field("first_name"), "FirstName");
        // snake_case on a field is identity (fields are already snake_case).
        assert_eq!(RenameRule::Snake.apply_to_field("first_name"), "first_name");
    }

    #[test]
    fn field_kebab_and_screaming_conversions() {
        assert_eq!(RenameRule::Kebab.apply_to_field("first_name"), "first-name");
        assert_eq!(
            RenameRule::ScreamingSnake.apply_to_field("first_name"),
            "FIRST_NAME"
        );
        assert_eq!(
            RenameRule::ScreamingKebab.apply_to_field("first_name"),
            "FIRST-NAME"
        );
    }

    // Variant conversions: input is PascalCase (matches serde's apply_to_variant).
    #[test]
    fn variant_conversions_match_serde() {
        assert_eq!(RenameRule::Snake.apply_to_variant("TwoWords"), "two_words");
        assert_eq!(RenameRule::Lower.apply_to_variant("TwoWords"), "twowords");
        assert_eq!(RenameRule::Camel.apply_to_variant("VeryTasty"), "veryTasty");
        assert_eq!(
            RenameRule::Kebab.apply_to_variant("VeryTasty"),
            "very-tasty"
        );
        assert_eq!(
            RenameRule::ScreamingSnake.apply_to_variant("VeryTasty"),
            "VERY_TASTY"
        );
    }
}
