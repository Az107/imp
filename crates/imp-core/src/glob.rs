//! Glob matching for allow and deny rules.
//!
//! The syntax is deliberately tiny: `*` is any run of characters, everything
//! else is literal, and the pattern must match from the start of the subject.
//! Anchoring at the start is what makes `ls *` mean "commands beginning `ls`"
//! without also matching `lsof`, and it leaves `*sudo*` working unchanged.
//!
//! This is not a general glob library. There is no `?`, no character classes and
//! no escaping, because an allowlist has to be readable enough that a user can
//! tell at a glance whether a rule is wider than they intended.

/// Whether `pattern` matches the start of `subject`.
pub fn glob_match(pattern: &str, subject: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if !pattern.contains('*') {
        return subject == pattern;
    }

    let mut segments = pattern.split('*');
    let first = segments.next().unwrap_or_default();
    if !subject.starts_with(first) {
        return false;
    }

    let rest = &subject[first.len()..];
    let middle: Vec<&str> = segments.clone().collect();
    let Some((last, middle)) = middle.split_last() else {
        // A single `*`: the leading literal was all that had to match.
        return true;
    };

    // Interior segments must appear in order; the final one must be a suffix,
    // because the pattern is anchored at both ends.
    let mut cursor = 0usize;
    for segment in middle {
        match rest[cursor..].find(segment) {
            Some(offset) => cursor += offset + segment.len(),
            None => return false,
        }
    }
    rest[cursor..].ends_with(last)
}

#[cfg(test)]
mod tests {
    use super::glob_match;

    #[test]
    fn a_bare_star_matches_everything() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("*", ""));
    }

    #[test]
    fn an_exact_pattern_matches_only_itself() {
        assert!(glob_match("git status", "git status"));
        assert!(!glob_match("git status", "git status --short"));
        assert!(!glob_match("git status", "git"));
    }

    #[test]
    fn a_trailing_star_matches_a_prefix() {
        assert!(glob_match("ls *", "ls -la"));
        assert!(glob_match("ls *", "ls "));
        assert!(!glob_match("ls *", "lsof"), "must not match a longer word");
    }

    #[test]
    fn a_leading_star_matches_a_substring() {
        assert!(glob_match("*sudo*", "sudo rm"));
        assert!(glob_match("*sudo*", "doas sudo"));
        assert!(!glob_match("*sudo*", "echo hi"));
    }

    #[test]
    fn a_star_in_the_middle_matches_a_gap() {
        assert!(glob_match("rm -rf *", "rm -rf /tmp/x"));
        assert!(!glob_match("rm -rf *", "rm -r /tmp/x"));
    }

    #[test]
    fn segments_must_appear_in_order() {
        assert!(glob_match("a*b*c", "axxbyyc"));
        assert!(!glob_match("a*b*c", "acb"));
    }

    #[test]
    fn an_empty_pattern_matches_only_an_empty_subject() {
        assert!(glob_match("", ""));
        assert!(!glob_match("", "x"));
    }

    #[test]
    fn a_double_star_behaves_like_two_wildcards() {
        assert!(glob_match("**sudo**", "sudo"));
        assert!(glob_match("**sudo**", "a sudo b"));
    }
}
