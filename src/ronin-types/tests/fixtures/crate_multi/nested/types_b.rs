// Fixture data (NOT compiled into the crate). Lives in a nested directory to
// prove the walkdir crate walk recurses into subdirectories (TR-004).

/// Referenced by `Player` in types_a.rs (different file, different directory).
pub struct Inventory {
    pub slots: Vec<Item>,
    pub gold: u64,
}

/// A unit-ish newtype referenced from the inventory.
pub struct Item {
    pub id: u32,
    pub label: String,
}

/// A tuple struct used as a cross-file field type.
pub struct Position(pub f32, pub f32, pub f32);

/// An enum referenced nowhere but unioned into the model (whole-crate union).
pub enum Faction {
    Neutral,
    Ally(u32),
    Enemy { strength: u32 },
}

/// A struct whose field is a generic type *parameter* (not a concrete type),
/// which syn cannot resolve -> unknown. Also references a generic instantiation
/// of a user type.
pub struct Container<T> {
    pub value: T,
    pub wrapped: Holder<Item>,
}

/// A generic user type; an *instantiation* of it (`Holder<Item>`) is not
/// statically resolvable to a concrete shape.
pub struct Holder<T> {
    pub inner: T,
}
