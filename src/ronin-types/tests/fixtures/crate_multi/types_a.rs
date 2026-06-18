// Fixture data (NOT compiled into the crate) for the SynSource crate-walk test.
// Defines types that reference types declared in `types_b.rs`, exercising
// cross-file reference resolution (TR-004).

/// A struct in file A whose fields reference types defined across files.
pub struct Player {
    pub name: String,
    pub level: u32,
    /// Cross-file reference to `Inventory` (defined in types_b.rs).
    pub inventory: Inventory,
    /// Cross-file reference to `Position` (defined in types_b.rs).
    pub position: Position,
    /// A homogeneous sequence of a cross-file type.
    pub party: Vec<Player>,
    /// A foreign-crate type that syn cannot resolve -> unknown.
    pub clock: external_crate::Instant,
}

/// A tuple struct referencing a cross-file type and a primitive.
pub struct Spawn(pub Position, pub u32);
