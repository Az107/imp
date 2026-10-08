//! Memory namespacing.
//!
//! Memory is scoped per workspace: a fact learned in one project must not turn
//! up in another. The `memory` table stores a *hash* of the workspace path
//! rather than the path itself, because the database is a user artifact that
//! gets copied, synced and inspected, and an absolute path is both a privacy
//! leak and a machine-specific label that would not match after a move.
//!
//! The hash is FNV-1a rather than a cryptographic digest. It labels a handful of
//! workspaces and is not a security boundary — a 64-bit collision would need
//! roughly four billion distinct roots to be likely. What it does have to be is
//! *deterministic*: the same root must produce the same namespace on every
//! machine, every platform, and every future release, or remembered facts
//! would silently scatter. `DefaultHasher` cannot promise that (its algorithm is
//! explicitly not stable), and a dependency would be a poor trade for eight
//! lines.

use std::path::Path;

/// FNV-1a 64-bit offset basis.
const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
/// FNV-1a 64-bit prime.
const PRIME: u64 = 0x0000_0100_0000_01b3;

/// The namespace label for a workspace root.
///
/// Deterministic and path-agnostic beyond the exact string given, so callers
/// must pass an already-canonicalized root — otherwise `/home/me/project` and
/// `/home/me/project/` would be two namespaces. [`crate::tool::ToolCtx`] hands
/// tools a canonical root, which is what the CLI supplies here.
pub fn namespace_for(root: &Path) -> String {
    let mut hash = OFFSET_BASIS;
    for byte in root.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_root_always_gives_the_same_namespace() {
        let root = Path::new("/home/dev/project");
        assert_eq!(namespace_for(root), namespace_for(root));
    }

    #[test]
    fn different_roots_get_different_namespaces() {
        let one = namespace_for(Path::new("/home/dev/project"));
        let two = namespace_for(Path::new("/home/dev/other"));
        assert_ne!(one, two);
    }

    #[test]
    fn the_namespace_never_leaks_the_path() {
        let namespace = namespace_for(Path::new("/home/alice/secret-client-work"));
        assert!(!namespace.contains("alice"));
        assert!(!namespace.contains("secret"));
    }

    #[test]
    fn the_label_is_sixteen_hex_digits() {
        let namespace = namespace_for(Path::new("/tmp/ws"));
        assert_eq!(namespace.len(), 16);
        assert!(namespace.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn a_known_input_hashes_to_its_known_value() {
        // Pins the algorithm. If this changes, every workspace's memory becomes
        // unreachable, so the constant is a deliberate tripwire rather than an
        // incidental snapshot.
        assert_eq!(namespace_for(Path::new("")), "cbf29ce484222325");
        assert_eq!(namespace_for(Path::new("/")), "af63a24c860189fe");
    }
}
