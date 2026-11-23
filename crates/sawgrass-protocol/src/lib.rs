//! Wire format shared by the Sawgrass server, gateways, clients and bots.
//!
//! Only the protocol version lives here for now. The packet layout, quantization and delta encoding
//! are tasks NET-1 and NET-2 (see `docs/roadmap.md`).

/// Bumped on every change that an older peer cannot read. CI compares it with the base branch when the
/// protocol crate changes (task OPS-3).
pub const PROTOCOL_VERSION: u32 = 0;

/// Whether a peer speaking `theirs` can talk to one speaking [`PROTOCOL_VERSION`].
///
/// Until the first release only identical versions are compatible.
#[must_use]
pub const fn is_compatible(theirs: u32) -> bool {
    theirs == PROTOCOL_VERSION
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_version_is_compatible() {
        assert!(is_compatible(PROTOCOL_VERSION));
    }

    #[test]
    fn other_version_is_not_compatible() {
        assert!(!is_compatible(PROTOCOL_VERSION + 1));
    }
}
