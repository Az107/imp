//! Session identity.
//!
//! A session id names one conversation. It is stable for the whole conversation
//! so that gateways can pin routing and reuse prompt caches: OpenCode Go, for
//! example, requires it in the `x-opencode-session` header and rejects requests
//! without it.

use uuid::Uuid;

/// Generate an identifier for a new conversation.
///
/// Version 7 UUIDs are time-ordered, so the ids the M1 store persists as
/// `sessions.id` sort by creation time while staying globally unique.
pub fn new_session_id() -> String {
    Uuid::now_v7().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique() {
        assert_ne!(new_session_id(), new_session_id());
    }

    #[test]
    fn ids_are_time_ordered_so_they_sort_by_creation() {
        let earlier = new_session_id();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let later = new_session_id();
        assert!(earlier < later, "{earlier} should sort before {later}");
    }
}
