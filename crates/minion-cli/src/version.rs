//! The version string `minion --version` prints (SDD §9).
//!
//! Assembled at compile time from the stamps in `build.rs`: the workspace
//! version, the commit SHA, the commit date and the capability families. It
//! requires no git, no network and no config at run time, and `minion update`
//! reads the same shape back out of a downloaded binary's `--version` to tie it
//! to the commit its release declares.
//!
//! Shape: `0.1.0 (abc1234def01 2026-10-07) [features: cron,guard,mcp,update]`.

/// The full `--version` value, without the program name.
pub const VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("MINION_GIT_SHA"),
    " ",
    env!("MINION_GIT_DATE"),
    ") [features: ",
    env!("MINION_FEATURES"),
    "]",
);

#[cfg(test)]
mod tests {
    use super::*;
    use minion_core::update::{sha_from_output, version_from_output};

    #[test]
    fn the_version_line_carries_what_update_needs_to_read_back() {
        let line = format!("minion {VERSION}");
        assert_eq!(version_from_output(&line).as_deref(), Some("0.1.0"));
        // The build stamps a SHA unless git and the environment are both absent.
        if !VERSION.contains("(unknown unknown)") {
            assert!(
                sha_from_output(&line).is_some(),
                "a stamped build must report a commit: {line}"
            );
        }
    }
}
