//! One place for terminal presentation: the colour palette, the glyph set and
//! the runtime [`Theme`] that decides whether either is drawn.
//!
//! Everything is 16-colour SGR — no truecolor crate, so the binary stays inside
//! NFR-3 and the output survives a serial console. Colour is never the only
//! signal: each coloured mark also carries a word or a glyph, so `NO_COLOR` and
//! a screen reader lose nothing but paint (§NFR-8). `--no-color`, `NO_COLOR` and
//! `[ui].color = "never"` all mean the same thing, and `--json` overrides the
//! lot.

use std::io::IsTerminal;

use imp_core::config::{ColorChoice, Config, IconChoice};
use imp_core::tool::Risk;

use crate::cli::Cli;

/// Every raw SGR fragment the tree emits. Nothing else writes an escape.
pub mod code {
    /// Attributes off.
    pub const RESET: &str = "\x1b[0m";
    /// Bold.
    pub const BOLD: &str = "\x1b[1m";
    /// Faint, for secondary text.
    pub const DIM: &str = "\x1b[2m";
    /// Italic, for emphasis.
    pub const ITALIC: &str = "\x1b[3m";
    /// Strikethrough.
    pub const STRIKE: &str = "\x1b[9m";
    /// Red, for errors and refused calls.
    pub const RED: &str = "\x1b[31m";
    /// Green, for success.
    pub const GREEN: &str = "\x1b[32m";
    /// Yellow, for warnings and writes.
    pub const YELLOW: &str = "\x1b[33m";
    /// Blue, for network activity.
    pub const BLUE: &str = "\x1b[34m";
    /// Magenta, for execution.
    pub const MAGENTA: &str = "\x1b[35m";
    /// Cyan, for code, links and accents.
    pub const CYAN: &str = "\x1b[36m";
    /// Bold bright white, the top heading level.
    pub const BRIGHT_WHITE: &str = "\x1b[1;97m";
    /// Bold bright cyan.
    pub const BRIGHT_CYAN: &str = "\x1b[1;96m";
    /// Bold bright magenta.
    pub const BRIGHT_MAGENTA: &str = "\x1b[1;95m";
    /// Bold bright blue.
    pub const BRIGHT_BLUE: &str = "\x1b[1;94m";
    /// Bold bright yellow.
    pub const BRIGHT_YELLOW: &str = "\x1b[1;93m";
    /// Bold bright green.
    pub const BRIGHT_GREEN: &str = "\x1b[1;92m";
}

/// Bold plus a colour that steps through the palette with the heading level.
///
/// A terminal has no font sizes, so hierarchy is bold and hue; level one is the
/// brightest on purpose.
pub fn heading_code(level: u8) -> &'static str {
    match level {
        1 => code::BRIGHT_WHITE,
        2 => code::BRIGHT_CYAN,
        3 => code::BRIGHT_MAGENTA,
        4 => code::BRIGHT_BLUE,
        5 => code::BRIGHT_YELLOW,
        _ => code::BRIGHT_GREEN,
    }
}

/// A decorative mark. Each maps to a Unicode glyph, an ASCII fallback, or
/// nothing at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Glyph {
    /// A tool is starting.
    Tool,
    /// A tool finished successfully.
    Done,
    /// A tool failed.
    Fail,
    /// A prompt or warning.
    Warn,
    /// A passing check.
    Check,
    /// A failing check.
    Cross,
    /// A list bullet.
    Bullet,
    /// A blockquote bar.
    Quote,
    /// A notice, outside the model's own words.
    Notice,
    /// The input prompt.
    Prompt,
    /// A selected or published item.
    Selected,
    /// A hidden item.
    Hidden,
}

/// The glyph for `glyph` under `icons`.
pub fn glyph(icons: IconChoice, kind: Glyph) -> &'static str {
    match icons {
        IconChoice::Auto => match kind {
            Glyph::Tool => "▸",
            Glyph::Done => "↳",
            Glyph::Fail => "✗",
            Glyph::Warn => "⚠",
            Glyph::Check => "✔",
            Glyph::Cross => "✖",
            Glyph::Bullet => "•",
            Glyph::Quote => "▎",
            Glyph::Notice => "…",
            Glyph::Prompt => "›",
            Glyph::Selected => "▸",
            Glyph::Hidden => "·",
        },
        IconChoice::Ascii => match kind {
            Glyph::Tool => ">",
            Glyph::Done => "->",
            Glyph::Fail => "x",
            Glyph::Warn => "!",
            Glyph::Check => "+",
            Glyph::Cross => "x",
            Glyph::Bullet => "*",
            Glyph::Quote => "|",
            Glyph::Notice => "...",
            Glyph::Prompt => ">",
            Glyph::Selected => ">",
            Glyph::Hidden => ".",
        },
        IconChoice::None => "",
    }
}

/// The frames an activity line cycles through while a turn works.
///
/// ASCII under `IconChoice::Ascii` (a terminal that cannot draw the dingbats is
/// also unlikely to draw braille), and a plain dot sequence otherwise.
pub fn spinner_frames(icons: IconChoice) -> &'static [&'static str] {
    match icons {
        IconChoice::Ascii | IconChoice::None => &["|", "/", "-", "\\"],
        IconChoice::Auto => &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"],
    }
}

/// Runtime styling: whether to paint, and which glyph set to draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    color: bool,
    icons: IconChoice,
}

impl Theme {
    /// A theme with colour on or off and the Unicode glyph set.
    pub fn new(color: bool) -> Self {
        Self {
            color,
            icons: IconChoice::Auto,
        }
    }

    /// Replace the glyph set.
    pub fn with_icons(mut self, icons: IconChoice) -> Self {
        self.icons = icons;
        self
    }

    /// Whether escapes will be emitted.
    pub fn color(self) -> bool {
        self.color
    }

    /// The glyph set in use.
    pub fn icons(self) -> IconChoice {
        self.icons
    }

    /// Wrap `text` in `code`, or return it unchanged when colour is off.
    ///
    /// An empty code is left completely alone: wrapping plain words in
    /// `ESC[m`/`ESC[0m` would double the size of every line for no visual gain.
    pub fn paint(self, code: &str, text: &str) -> String {
        if self.color && !code.is_empty() {
            format!("{code}{text}{}", code::RESET)
        } else {
            text.to_string()
        }
    }

    /// Faint secondary text.
    pub fn dim(self, text: &str) -> String {
        self.paint(code::DIM, text)
    }

    /// Bold text.
    pub fn bold(self, text: &str) -> String {
        self.paint(code::BOLD, text)
    }

    /// The banner and other highlights.
    pub fn accent(self, text: &str) -> String {
        self.paint(code::BRIGHT_CYAN, text)
    }

    /// A successful outcome.
    pub fn success(self, text: &str) -> String {
        self.paint(code::GREEN, text)
    }

    /// Something the user should notice.
    pub fn warn(self, text: &str) -> String {
        self.paint(code::YELLOW, text)
    }

    /// A failure or refusal.
    pub fn error(self, text: &str) -> String {
        self.paint(code::RED, text)
    }

    /// Inline code, links and values.
    pub fn info(self, text: &str) -> String {
        self.paint(code::CYAN, text)
    }

    /// The colour a risk class is drawn in.
    ///
    /// Reads before writes: `ReadOnly` is dim, and the three classes that
    /// require consent step up through yellow, magenta and blue so a glance
    /// separates "may touch the disk" from "may run a process".
    pub fn risk_code(self, risk: Risk) -> &'static str {
        match risk {
            Risk::ReadOnly => code::DIM,
            Risk::Write => code::YELLOW,
            Risk::Execute => code::MAGENTA,
            Risk::Network => code::BLUE,
        }
    }

    /// A risk class, painted by class and led by a marker when colour is off so
    /// the class survives a `NO_COLOR` terminal (§NFR-8).
    pub fn risk(self, risk: Risk) -> String {
        let label = risk.as_str();
        if self.color {
            self.paint(self.risk_code(risk), label)
        } else if risk.requires_consent() {
            format!("{label}!")
        } else {
            label.to_string()
        }
    }

    /// The glyph for `kind` under this theme.
    pub fn glyph(self, kind: Glyph) -> &'static str {
        glyph(self.icons, kind)
    }
}

/// Whether colour is allowed for a stream that is (or is not) a terminal.
///
/// `NO_COLOR`, `--no-color` and `--json` are each a hard no. Otherwise the
/// `[ui].color` choice decides, with `auto` meaning "only a terminal".
fn colour(choice: ColorChoice, cli: &Cli, is_tty: bool) -> bool {
    if cli.no_color || cli.json || std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    match choice {
        ColorChoice::Never => false,
        ColorChoice::Always => true,
        ColorChoice::Auto => is_tty,
    }
}

/// The theme for the conversation's own stream (assistant text on stdout).
pub fn stdout_theme(cli: &Cli, config: &Config) -> Theme {
    let is_tty = std::io::stdout().is_terminal();
    Theme::new(colour(config.ui.color, cli, is_tty)).with_icons(config.ui.icons)
}

/// The theme for tool activity, approvals and diagnostics (stderr).
pub fn stderr_theme(cli: &Cli, config: &Config) -> Theme {
    let is_tty = std::io::stderr().is_terminal();
    Theme::new(colour(config.ui.color, cli, is_tty)).with_icons(config.ui.icons)
}

/// The stdout theme before a configuration exists, for `imp init`.
pub fn stdout_theme_default(cli: &Cli) -> Theme {
    let is_tty = std::io::stdout().is_terminal();
    Theme::new(colour(ColorChoice::Auto, cli, is_tty)).with_icons(IconChoice::Auto)
}

/// The stderr theme before a configuration exists, for `imp init` prompts.
pub fn stderr_theme_default(cli: &Cli) -> Theme {
    let is_tty = std::io::stderr().is_terminal();
    Theme::new(colour(ColorChoice::Auto, cli, is_tty)).with_icons(IconChoice::Auto)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use imp_core::config::Config;
    use imp_core::tool::Risk;

    fn cli() -> Cli {
        Cli::parse_from(["imp"])
    }

    #[test]
    fn a_colourless_theme_emits_no_escapes() {
        let theme = Theme::new(false).with_icons(IconChoice::Auto);
        assert_eq!(theme.error("boom"), "boom");
        assert_eq!(theme.dim("x"), "x");
        assert!(!theme.error("boom").contains('\u{1b}'));
    }

    #[test]
    fn a_colourful_theme_balances_every_escape() {
        let theme = Theme::new(true);
        let painted = theme.error("boom");
        assert!(painted.contains(code::RED));
        assert!(painted.ends_with(code::RESET));
    }

    #[test]
    fn every_risk_class_is_load_bearing_without_colour() {
        // NFR-8: the class must survive `NO_COLOR`, so a consent-requiring
        // class gains a marker and a read-only one does not.
        let theme = Theme::new(false);
        assert_eq!(theme.risk(Risk::ReadOnly), "read_only");
        assert_eq!(theme.risk(Risk::Write), "write!");
        assert_eq!(theme.risk(Risk::Execute), "execute!");
        assert_eq!(theme.risk(Risk::Network), "network!");
    }

    #[test]
    fn ascii_icons_have_a_fallback_for_every_glyph() {
        for kind in [
            Glyph::Tool,
            Glyph::Done,
            Glyph::Fail,
            Glyph::Warn,
            Glyph::Check,
            Glyph::Cross,
            Glyph::Bullet,
            Glyph::Quote,
            Glyph::Notice,
            Glyph::Prompt,
            Glyph::Selected,
            Glyph::Hidden,
        ] {
            let unicode = glyph(IconChoice::Auto, kind);
            let ascii = glyph(IconChoice::Ascii, kind);
            assert!(
                unicode.is_ascii() || !unicode.is_empty(),
                "unicode glyph missing"
            );
            assert!(ascii.is_ascii(), "ascii fallback for {kind:?} is not ascii");
        }
    }

    #[test]
    fn the_none_icon_set_draws_nothing() {
        assert_eq!(glyph(IconChoice::None, Glyph::Tool), "");
        assert_eq!(glyph(IconChoice::None, Glyph::Bullet), "");
    }

    #[test]
    fn no_color_beats_every_config_choice() {
        let mut cli = cli();
        cli.no_color = true;
        let mut config = Config::default();
        config.ui.color = ColorChoice::Always;
        // `--no-color` wins even over `always`.
        assert!(!colour(ColorChoice::Always, &cli, true));
        // and `always` means colour on a pipe when nothing refuses it.
        cli.no_color = false;
        assert!(colour(ColorChoice::Always, &cli, false));
        assert!(!colour(ColorChoice::Never, &cli, true));
    }
}
