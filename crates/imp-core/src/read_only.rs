//! Read-only shell-command classification and the runtime auto-approval switch.
//!
//! `run_command` is always [`Risk::Execute`](crate::tool::Risk), so every shell
//! command would prompt by default even when it only observes — `ls`,
//! `git status`, `grep`. This module recognises the commands that are *provably*
//! read-only and lets the policy engine approve them without a prompt. It is
//! opt-in (`[policy.read_only].enabled`) and can be flipped at runtime by the
//! `/auto` REPL command.
//!
//! The classifier is a conservative whitelist, not a smarter denylist. It first
//! refuses any command that uses shell syntax it cannot reason about — pipes,
//! redirection, `;`/`&&`, command substitution, quotes, a leading `VAR=value`
//! assignment — because `ls; rm -rf x` must never be mistaken for `ls`. Only a
//! single, unadorned command whose verb is in the table is read-only. A command
//! the classifier does not recognise is *not* refused; it merely falls back to
//! the ordinary approval path, so an incomplete table is a missing convenience,
//! never a hole.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

/// Characters that mean a command is doing something a read-only classifier
/// cannot model: separators, redirection, substitution, quoting, history.
const FORBIDDEN: &[char] = &[
    ';', '&', '|', '>', '<', '$', '`', '(', ')', '{', '}', '\\', '"', '\'', '!', '\n', '\r',
];

/// Verbs whose invocations only observe, whatever their arguments.
const READ_ONLY_VERBS: &[&str] = &[
    "ls", "pwd", "cat", "head", "tail", "wc", "file", "stat", "du", "df", "tree", "which",
    "whereis", "type", "printenv", "whoami", "id", "uname", "uptime", "echo", "rg", "grep",
];

/// `find` predicates that write; any one of them makes the whole command unsafe.
const FIND_WRITE_ARGS: &[&str] = &[
    "-delete", "-exec", "-execdir", "-ok", "-okdir", "-fprint", "-fprint0", "-fprintf", "-fls",
];

/// `git` subcommands that only read.
const GIT_READ_ONLY_SUBCOMMANDS: &[&str] = &[
    "status",
    "log",
    "diff",
    "show",
    "blame",
    "shortlog",
    "describe",
    "rev-parse",
    "rev-list",
    "ls-files",
    "ls-tree",
    "cat-file",
    "show-ref",
    "whatchanged",
    "grep",
];

/// Whether `word` is a bare command verb the classifier can consider.
///
/// Used to validate `[policy.read_only].extra`, so a typo fails at startup
/// rather than silently never matching.
pub fn is_bare_verb(word: &str) -> bool {
    !word.is_empty()
        && !word.contains('/')
        && !word.contains('=')
        && !word.chars().any(|c| FORBIDDEN.contains(&c))
        && !word.chars().any(char::is_whitespace)
}

/// The verb of `command`, when `command` is a single unadorned command.
///
/// `None` means shell syntax the classifier will not reason about, so the
/// command must take the ordinary approval path.
fn bare_verb(command: &str) -> Option<&str> {
    if command.chars().any(|c| FORBIDDEN.contains(&c)) {
        return None;
    }
    let verb = command.split_whitespace().next()?;
    if is_bare_verb(verb) { Some(verb) } else { None }
}

/// Whether `command` is a recognised read-only shell command.
pub fn is_read_only_command(command: &str) -> bool {
    let Some(verb) = bare_verb(command) else {
        return false;
    };
    if READ_ONLY_VERBS.contains(&verb) {
        return true;
    }
    let args: Vec<&str> = command.split_whitespace().skip(1).collect();
    match verb {
        "git" => git_is_read_only(&args),
        "find" => !args.iter().any(|arg| FIND_WRITE_ARGS.contains(arg)),
        _ => false,
    }
}

/// Whether a `git …` invocation only reads.
fn git_is_read_only(args: &[&str]) -> bool {
    // `-c`/`--config`/`--exec-path` let a command name a helper that runs an
    // arbitrary program (an alias, an external diff), and `--output[=…]` writes
    // a file. None of these are reads, so the whole invocation is refused.
    if args.iter().any(|arg| {
        matches!(*arg, "-c" | "--config" | "--exec-path")
            || arg.starts_with("--config=")
            || arg.starts_with("--exec-path=")
            || arg.starts_with("--output")
    }) {
        return false;
    }
    // The subcommand is the first token that is not a flag. A global option that
    // takes a value would shift this, and the result is a refusal, not a wrong
    // allow, so the simple scan fails closed.
    match args.iter().find(|arg| !arg.starts_with('-')) {
        Some(sub) => GIT_READ_ONLY_SUBCOMMANDS.contains(sub),
        None => false,
    }
}

/// How automatically shell commands are approved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoMode {
    /// The configured policy decides; nothing extra is auto-approved.
    Off,
    /// Recognised read-only commands run without a prompt.
    ReadOnly,
    /// Reserved for a future System One "full auto" mode; never set today.
    Full,
}

impl AutoMode {
    fn as_u8(self) -> u8 {
        match self {
            AutoMode::Off => 0,
            AutoMode::ReadOnly => 1,
            AutoMode::Full => 2,
        }
    }

    fn from_u8(value: u8) -> Self {
        match value {
            1 => AutoMode::ReadOnly,
            2 => AutoMode::Full,
            _ => AutoMode::Off,
        }
    }

    /// Stable lowercase name, used in the `/auto` reply and logs.
    pub fn as_str(self) -> &'static str {
        match self {
            AutoMode::Off => "off",
            AutoMode::ReadOnly => "read-only",
            AutoMode::Full => "full",
        }
    }
}

/// The shared, session-scoped auto-approval switch the gate consults and
/// `/auto` flips.
///
/// One `Arc` is handed to the [`PolicyEngine`](crate::policy::PolicyEngine) and
/// kept by the session, so a REPL command changes what the very next `check`
/// sees without rebuilding anything. A cron gate gets its own instance seeded
/// from config, so an interactive `/auto` never reaches a scheduled job.
#[derive(Debug)]
pub struct AutoCommands {
    mode: AtomicU8,
    extra: Vec<String>,
}

impl AutoCommands {
    /// A switch started at `enabled` (`[policy.read_only].enabled`), carrying the
    /// operator's extra verbs.
    pub fn new(enabled: bool, extra: Vec<String>) -> Arc<Self> {
        let mode = if enabled {
            AutoMode::ReadOnly
        } else {
            AutoMode::Off
        };
        Arc::new(Self {
            mode: AtomicU8::new(mode.as_u8()),
            extra,
        })
    }

    /// The current mode.
    pub fn mode(&self) -> AutoMode {
        AutoMode::from_u8(self.mode.load(Ordering::Relaxed))
    }

    /// Set the mode.
    pub fn set_mode(&self, mode: AutoMode) {
        self.mode.store(mode.as_u8(), Ordering::Relaxed);
    }

    /// Whether the read-only shortcut is active.
    pub fn read_only(&self) -> bool {
        self.mode() == AutoMode::ReadOnly
    }

    /// Whether `command` is recognised as read-only — the built-in table plus
    /// any configured `extra` verbs. An extra verb is still subject to the same
    /// shell-syntax rejection, so `mycmd; rm -rf x` is never a match.
    pub fn matches(&self, command: &str) -> bool {
        if let Some(verb) = bare_verb(command)
            && self.extra.iter().any(|extra| extra == verb)
        {
            return true;
        }
        is_read_only_command(command)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_readers_are_recognised() {
        for command in [
            "ls",
            "ls -la",
            "pwd",
            "cat Cargo.toml",
            "head -n 20 src/main.rs",
            "tail -f app.log",
            "wc -l README.md",
            "stat src/lib.rs",
            "grep -r TODO src",
            "rg --hidden needle",
            "git status",
            "git log --oneline -5",
            "git diff HEAD~1",
            "git show --stat",
            "find . -name *.rs",
            "echo hello",
        ] {
            assert!(
                is_read_only_command(command),
                "`{command}` should be read-only"
            );
        }
    }

    #[test]
    fn shell_syntax_is_never_read_only() {
        for command in [
            "ls; rm -rf build",
            "ls && rm -rf build",
            "ls || true",
            "ls | xargs rm",
            "ls > out.txt",
            "ls >> out.txt",
            "cat < /etc/passwd",
            "ls $(rm -rf x)",
            "ls `rm -rf x`",
            "ls (rm x)",
            "ls &",
            "echo \"hi\"",
            "git status\nrm -rf x",
            "LD_PRELOAD=evil.so git status",
            "FOO=bar ls",
        ] {
            assert!(
                !is_read_only_command(command),
                "`{command}` must not be read-only"
            );
        }
    }

    #[test]
    fn a_path_binary_is_not_a_bare_verb() {
        assert!(!is_read_only_command("/bin/ls"));
        assert!(!is_read_only_command("./ls"));
        assert!(!is_read_only_command("../scripts/grep x"));
    }

    #[test]
    fn mutating_git_invocations_are_refused() {
        for command in [
            "git checkout main",
            "git reset --hard HEAD~1",
            "git push origin main",
            "git commit -m x",
            "git add .",
            "git clean -fdx",
            "git config user.name",
            "git -c alias.x=!sh status",
            "git --config core.pager=less log",
            "git --exec-path=/tmp log",
            "git diff --output=out.patch",
            "git log --output out.patch",
        ] {
            assert!(
                !is_read_only_command(command),
                "`{command}` must not be read-only"
            );
        }
    }

    #[test]
    fn find_write_predicates_are_refused() {
        assert!(!is_read_only_command("find . -delete"));
        assert!(!is_read_only_command("find . -exec rm {} ;"));
        assert!(!is_read_only_command("find . -fprint list.txt"));
        assert!(!is_read_only_command("find . -fprintf out '%p'"));
        assert!(is_read_only_command("find . -type f -name *.rs"));
        // Quoting is shell syntax the classifier declines to reason about, so a
        // quoted argument is not exempt (it simply falls back to a prompt).
        assert!(!is_read_only_command("find . -name '*.rs'"));
    }

    #[test]
    fn an_unknown_verb_is_not_read_only() {
        for command in [
            "rm notes.txt",
            "python setup.py",
            "make install",
            "env",
            "xargs rm",
        ] {
            assert!(!is_read_only_command(command), "`{command}` is unknown");
        }
    }

    #[test]
    fn extra_verbs_are_honoured_but_still_metacharacter_checked() {
        let auto = AutoCommands::new(false, vec!["fd".to_string()]);
        assert!(auto.matches("fd -e rs"));
        assert!(!auto.matches("fd; rm -rf x"));
        // The built-ins keep working.
        assert!(auto.matches("ls -la"));
    }

    #[test]
    fn the_mode_starts_where_the_config_says_and_transitions() {
        let off = AutoCommands::new(false, Vec::new());
        assert_eq!(off.mode(), AutoMode::Off);
        assert!(!off.read_only());

        off.set_mode(AutoMode::ReadOnly);
        assert_eq!(off.mode(), AutoMode::ReadOnly);
        assert!(off.read_only());

        off.set_mode(AutoMode::Off);
        assert_eq!(off.mode(), AutoMode::Off);

        let on = AutoCommands::new(true, Vec::new());
        assert!(on.read_only());
    }

    #[test]
    fn is_bare_verb_rejects_shell_shapes() {
        assert!(is_bare_verb("ls"));
        assert!(is_bare_verb("git"));
        assert!(!is_bare_verb(""));
        assert!(!is_bare_verb("rm -rf"));
        assert!(!is_bare_verb("/bin/ls"));
        assert!(!is_bare_verb("a=b"));
        assert!(!is_bare_verb("ls;rm"));
    }
}
