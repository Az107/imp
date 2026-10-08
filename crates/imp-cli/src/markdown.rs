//! Markdown for the terminal.
//!
//! Two ideas shape this. First, the parser is a plain library but the *renderer*
//! is ours, so the output is exactly what we intend and the dependency stays
//! small enough to keep the binary within the SDD's size budget. Second,
//! rendering is per block rather than per token: a table cannot be drawn until
//! its last row has arrived, and re-drawing finished text to insert a border
//! would break the scrollback the REPL is built to preserve. So a block is
//! emitted the moment it is complete, and nothing already printed is revisited.
//!
//! "Font size" does not exist in a terminal, so heading levels are shown with
//! bold plus a colour from a small palette: distinct enough to convey hierarchy,
//! and the first level is deliberately the brightest.

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use crate::style::{code, heading_code};

/// How output should be drawn.
///
/// `enabled` and `color` are separate on purpose. Layout — table borders, list
/// markers, indentation — is worth having even when colour is refused, so
/// `NO_COLOR` still gets aligned tables. Rendering is switched off entirely only
/// when stdout is not a terminal, because then the markdown source is the more
/// useful thing to pipe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Style {
    /// Whether markdown is rendered at all.
    pub enabled: bool,
    /// Whether ANSI escapes may be emitted.
    pub color: bool,
    /// Wrap width in columns.
    pub width: usize,
    /// Which glyph set list bullets and quote bars use.
    pub icons: imp_core::config::IconChoice,
}

impl Style {
    /// Off: the raw markdown, for a pipe or a file.
    pub fn plain() -> Self {
        Self {
            enabled: false,
            color: false,
            width: 80,
            icons: imp_core::config::IconChoice::Auto,
        }
    }

    /// On, with the given colour and width choices.
    pub fn rendered(color: bool, width: usize) -> Self {
        Self {
            enabled: true,
            color,
            width: width.max(20),
            icons: imp_core::config::IconChoice::Auto,
        }
    }

    /// Replace the glyph set.
    pub fn with_icons(mut self, icons: imp_core::config::IconChoice) -> Self {
        self.icons = icons;
        self
    }

    /// Wrap a fragment in a style, returning it unchanged when colour is off.
    ///
    /// An empty code is left completely alone: wrapping plain words in
    /// `ESC[m`/`ESC[0m` would double the size of every line for no visual gain.
    fn paint(&self, code: &str, text: &str) -> String {
        if self.color && !code.is_empty() {
            format!("{code}{text}{}", crate::style::code::RESET)
        } else {
            text.to_string()
        }
    }

    fn dim(&self, text: &str) -> String {
        self.paint(code::DIM, text)
    }

    fn rule(&self, width: usize) -> String {
        self.dim(&"─".repeat(width.max(1)))
    }
}

/// Inline emphasis, flattened to a tree we can lay out.
#[derive(Debug, Clone, PartialEq)]
enum Inline {
    Text(String),
    Code(String),
    Strong(Vec<Inline>),
    Emphasis(Vec<Inline>),
    Strike(Vec<Inline>),
}

/// One top-level block.
#[derive(Debug, Clone, PartialEq)]
enum Block {
    Heading(u8, Vec<Inline>),
    Paragraph(Vec<Inline>),
    /// Fenced or indented code: (language, text).
    Code(String, String),
    List {
        ordered: bool,
        start: u64,
        items: Vec<Vec<Block>>,
    },
    Quote(Vec<Block>),
    Rule,
    Table(Table),
}

#[derive(Debug, Clone, PartialEq, Default)]
struct Table {
    header: Vec<Vec<Inline>>,
    rows: Vec<Vec<Vec<Inline>>>,
    align: Vec<Alignment>,
}

/// Render one complete block of markdown.
///
/// The streaming path goes through `render_parsed` directly so it can track the
/// spacing between blocks; this is the single-block entry point the tests use.
#[cfg(test)]
pub fn render_block(markdown: &str, style: &Style) -> String {
    render_parsed(parse(markdown), style).0
}

/// Render a parsed block list, reporting whether it ended on a horizontal rule.
///
/// A rule already separates what is above it, so it is not given a blank line of
/// its own before it.
fn render_parsed(blocks: Vec<Block>, style: &Style) -> (String, bool) {
    let mut out = String::new();
    for (index, block) in blocks.iter().enumerate() {
        if index > 0 && !matches!(block, Block::Rule) {
            out.push('\n');
        }
        render_block_into(block, style, 0, &mut out);
    }
    let ended_on_rule = blocks
        .last()
        .is_some_and(|block| matches!(block, Block::Rule));
    (out, ended_on_rule)
}

/// Parse markdown into the block/inline tree the renderer walks.
fn parse(markdown: &str) -> Vec<Block> {
    // `Options::empty()` disables the GFM extensions, tables included; the
    // default is `all()`, so the wanted set is named explicitly instead.
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let events: Vec<Event<'_>> = Parser::new_ext(markdown, options).collect();
    let mut cursor = 0usize;
    collect_blocks(&events, &mut cursor, false)
}

/// Consume events into blocks until the enclosing list or quote ends.
///
/// `nested` is true inside a list item, where a tight list emits bare text
/// events rather than paragraphs.
fn collect_blocks(events: &[Event<'_>], cursor: &mut usize, nested: bool) -> Vec<Block> {
    let mut blocks: Vec<Block> = Vec::new();

    while *cursor < events.len() {
        match &events[*cursor] {
            // Stop at the item's own end. Without this the loop runs on into the
            // *next* item's `Start(Item)`, which is not a block we can parse, and
            // that item is lost.
            Event::End(TagEnd::Item) if nested => break,
            Event::End(_) => break,
            Event::Rule => {
                *cursor += 1;
                blocks.push(Block::Rule);
            }
            Event::Html(text) => {
                // Raw HTML has no terminal equivalent; keep the text visible.
                let text = text.to_string();
                *cursor += 1;
                blocks.push(Block::Paragraph(vec![Inline::Text(text)]));
            }
            // A tight list item has no paragraph wrapper, so its inline events
            // arrive bare. The whole run has to be absorbed as *one* paragraph:
            // returning after the first run would split an item at every code
            // span, giving `~~old~~ is gone` two bullets instead of one.
            //
            // This arm must come *before* the generic `Start(tag)` arm below.
            // Match arms are tried in order, and `Start(Strong)` is both an
            // inline mark and a `Tag`, so a later arm would claim it first and
            // the emphasis would be dropped.
            Event::Text(_)
            | Event::Code(_)
            | Event::SoftBreak
            | Event::HardBreak
            | Event::Start(Tag::Emphasis | Tag::Strong | Tag::Strikethrough)
                if nested =>
            {
                blocks.push(Block::Paragraph(absorb_inlines(events, cursor)));
            }
            Event::Start(tag) => {
                if let Some(block) = start_block(events, cursor, tag) {
                    blocks.push(block);
                }
            }
            _ => {
                *cursor += 1;
            }
        }
    }
    blocks
}

/// Parse one block starting at the current `Start` event.
fn start_block(events: &[Event<'_>], cursor: &mut usize, tag: &Tag<'_>) -> Option<Block> {
    match tag {
        Tag::Paragraph => {
            *cursor += 1;
            let inlines = collect_inlines(events, cursor, true);
            Some(Block::Paragraph(inlines))
        }
        Tag::Heading { level, .. } => {
            let level = heading_level(*level);
            *cursor += 1;
            let inlines = collect_inlines(events, cursor, true);
            Some(Block::Heading(level, inlines))
        }
        Tag::CodeBlock(kind) => {
            let language = match kind {
                CodeBlockKind::Fenced(info) => info.to_string(),
                CodeBlockKind::Indented => String::new(),
            };
            *cursor += 1;
            let mut text = String::new();
            while *cursor < events.len() {
                match &events[*cursor] {
                    Event::End(TagEnd::CodeBlock) => {
                        *cursor += 1;
                        break;
                    }
                    Event::Text(chunk) => {
                        text.push_str(chunk);
                        *cursor += 1;
                    }
                    _ => *cursor += 1,
                }
            }
            Some(Block::Code(language, text))
        }
        Tag::BlockQuote(_) => {
            *cursor += 1;
            let inner = collect_blocks(events, cursor, false);
            if *cursor < events.len() {
                *cursor += 1;
            }
            Some(Block::Quote(inner))
        }
        Tag::List(start) => {
            *cursor += 1;
            let mut items = Vec::new();
            while *cursor < events.len() {
                match &events[*cursor] {
                    Event::End(TagEnd::List(_)) => {
                        *cursor += 1;
                        break;
                    }
                    Event::Start(Tag::Item) => {
                        *cursor += 1;
                        items.push(collect_blocks(events, cursor, true));
                        if *cursor < events.len() {
                            *cursor += 1;
                        }
                    }
                    _ => *cursor += 1,
                }
            }
            Some(Block::List {
                ordered: start.is_some(),
                start: start.unwrap_or(1),
                items,
            })
        }
        Tag::Table(align) => {
            *cursor += 1;
            Some(Block::Table(collect_table(events, cursor, align.clone())))
        }
        _ => {
            *cursor += 1;
            None
        }
    }
}

/// Read a table's header and body.
fn collect_table(events: &[Event<'_>], cursor: &mut usize, align: Vec<Alignment>) -> Table {
    let mut table = Table {
        header: Vec::new(),
        rows: Vec::new(),
        align,
    };
    let mut in_head = false;
    let mut row: Vec<Vec<Inline>> = Vec::new();

    while *cursor < events.len() {
        match &events[*cursor] {
            Event::End(TagEnd::Table) => {
                *cursor += 1;
                break;
            }
            Event::Start(Tag::TableHead) => {
                in_head = true;
                *cursor += 1;
            }
            Event::Start(Tag::TableRow) => {
                row = Vec::new();
                *cursor += 1;
            }
            Event::Start(Tag::TableCell) => {
                *cursor += 1;
                let cell = collect_inlines(events, cursor, true);
                row.push(cell);
            }
            Event::End(TagEnd::TableHead) => {
                // The header carries no TableRow wrapper in pulldown-cmark, so
                // it has to be taken here or the header is silently lost.
                table.header = std::mem::take(&mut row);
                in_head = false;
                *cursor += 1;
            }
            Event::End(TagEnd::TableRow) => {
                let cells = std::mem::take(&mut row);
                if in_head {
                    table.header = cells;
                } else {
                    table.rows.push(cells);
                }
                *cursor += 1;
            }
            _ => *cursor += 1,
        }
    }
    table
}

/// Consume every consecutive inline event as one run.
///
/// A tight list item interleaves text with code spans and emphasis, and
/// `collect_inlines` returns at the first `End` it sees — which for a bare text
/// run is the end of the item. Absorbing repeatedly stitches those runs back
/// into a single paragraph instead of one paragraph per run.
fn absorb_inlines(events: &[Event<'_>], cursor: &mut usize) -> Vec<Inline> {
    let mut all = Vec::new();
    while *cursor < events.len() {
        match &events[*cursor] {
            Event::Text(_)
            | Event::Code(_)
            | Event::SoftBreak
            | Event::HardBreak
            | Event::Start(Tag::Emphasis | Tag::Strong | Tag::Strikethrough) => {
                all.extend(collect_inlines(events, cursor, false));
            }
            _ => break,
        }
    }
    all
}

/// Read inline runs until the matching `End`.
/// `consume_end` decides who steps over the run's terminating `End`.
///
/// Block callers want it consumed so the outer loop moves on to the next block.
/// The list-item caller must *not* consume it, because the event it stops on is
/// the item's own `End` and the enclosing list is the part that has to see it —
/// otherwise the cursor lands inside the next item and that item is swallowed.
fn collect_inlines(events: &[Event<'_>], cursor: &mut usize, consume_end: bool) -> Vec<Inline> {
    let mut out: Vec<Inline> = Vec::new();
    let mut run = String::new();

    macro_rules! flush {
        () => {
            if !run.is_empty() {
                out.push(Inline::Text(std::mem::take(&mut run)));
            }
        };
    }

    // Links and images have no terminal equivalent, so their syntax is dropped
    // and only the text kept. That means their `End` must not be mistaken for
    // the end of the run, or the rest of the sentence is discarded with it.
    let mut transparent = 0usize;

    while *cursor < events.len() {
        match &events[*cursor] {
            Event::End(_) => {
                if transparent > 0 {
                    transparent -= 1;
                    *cursor += 1;
                    continue;
                }
                if consume_end {
                    *cursor += 1;
                }
                break;
            }
            Event::Text(text) => {
                run.push_str(text);
                *cursor += 1;
            }
            Event::Code(text) => {
                flush!();
                out.push(Inline::Code(text.to_string()));
                *cursor += 1;
            }
            Event::SoftBreak => {
                run.push(' ');
                *cursor += 1;
            }
            Event::HardBreak => {
                flush!();
                out.push(Inline::Text("\n".to_string()));
                *cursor += 1;
            }
            Event::Start(Tag::Strong) => {
                flush!();
                *cursor += 1;
                let inner = collect_inlines(events, cursor, true);
                out.push(Inline::Strong(inner));
            }
            Event::Start(Tag::Emphasis) => {
                flush!();
                *cursor += 1;
                let inner = collect_inlines(events, cursor, true);
                out.push(Inline::Emphasis(inner));
            }
            Event::Start(Tag::Strikethrough) => {
                flush!();
                *cursor += 1;
                let inner = collect_inlines(events, cursor, true);
                out.push(Inline::Strike(inner));
            }
            Event::Start(
                Tag::Link { .. } | Tag::Image { .. } | Tag::Superscript | Tag::Subscript,
            ) => {
                transparent += 1;
                *cursor += 1;
            }
            _ => {
                // Task markers and anything else: keep the text, drop the syntax.
                *cursor += 1;
            }
        }
    }
    flush!();
    out
}

fn heading_level(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

// ---------------------------------------------------------------- rendering

/// Render a block, possibly indented, into `out`.
fn render_block_into(block: &Block, style: &Style, indent: usize, out: &mut String) {
    let pad = " ".repeat(indent);
    match block {
        Block::Heading(level, inlines) => {
            let text = plain_of(inlines);
            let painted = style.paint(heading_code(*level), &text);
            out.push_str(&pad);
            out.push_str(&painted);
            out.push('\n');
        }
        Block::Paragraph(inlines) => {
            let mut line = LineWriter::new(style, indent);
            write_inlines(inlines, &mut line);
            line.finish(out);
        }
        Block::Code(language, text) => {
            if !language.is_empty() {
                out.push_str(&pad);
                out.push_str(&style.dim(&format!("[{language}]")));
                out.push('\n');
            }
            for line in text.trim_end_matches('\n').split('\n') {
                out.push_str(&pad);
                out.push_str(&style.paint(code::CYAN, line));
                out.push('\n');
            }
        }
        Block::List {
            ordered,
            start,
            items,
        } => {
            for (offset, item) in items.iter().enumerate() {
                let marker = if *ordered {
                    format!("{}. ", start + offset as u64)
                } else {
                    let bullet = crate::style::glyph(style.icons, crate::style::Glyph::Bullet);
                    if bullet.is_empty() {
                        String::new()
                    } else {
                        format!("{bullet} ")
                    }
                };
                let marker_width = marker.chars().count();
                let mut first = true;
                for inner in item {
                    // A tight item is a single paragraph; render its first line
                    // beside the bullet and the rest aligned under the text.
                    let mut sub = String::new();
                    render_block_into(inner, style, 0, &mut sub);
                    for (index, line) in sub.trim_end_matches('\n').split('\n').enumerate() {
                        if index == 0 {
                            out.push_str(&pad);
                            out.push_str(&style.paint(code::BOLD, &marker));
                        } else {
                            out.push_str(&pad);
                            out.push_str(&" ".repeat(marker_width));
                        }
                        out.push_str(line);
                        out.push('\n');
                    }
                    first = false;
                }
                if first {
                    out.push_str(&pad);
                    out.push_str(&style.paint(code::BOLD, &marker));
                    out.push('\n');
                }
            }
        }
        Block::Quote(inner) => {
            let mut sub = String::new();
            for block in inner {
                render_block_into(block, style, 0, &mut sub);
            }
            let bar = crate::style::glyph(style.icons, crate::style::Glyph::Quote);
            let bar = if bar.is_empty() {
                String::new()
            } else {
                style.dim(&format!("{bar} "))
            };
            for line in sub.trim_end_matches('\n').split('\n') {
                out.push_str(&pad);
                out.push_str(&bar);
                out.push_str(line);
                out.push('\n');
            }
        }
        Block::Rule => {
            out.push_str(&pad);
            out.push_str(&style.rule(style.width.saturating_sub(indent)));
            out.push('\n');
        }
        Block::Table(table) => render_table(table, style, indent, out),
    }
}

/// Draw a table with box-drawing borders and aligned columns.
fn render_table(table: &Table, style: &Style, indent: usize, out: &mut String) {
    let columns = table
        .header
        .len()
        .max(table.rows.iter().map(Vec::len).max().unwrap_or(0));
    if columns == 0 {
        return;
    }

    // A table wider than the terminal is what makes box drawing fall apart, so
    // columns are measured and, if the total overflows, the table is left plain.
    let mut widths = vec![0usize; columns];
    let mut measure = |cells: &[Vec<Inline>]| {
        for (index, cell) in cells.iter().enumerate() {
            widths[index] = widths[index].max(plain_width(cell));
        }
    };
    measure(&table.header);
    for row in &table.rows {
        measure(row);
    }

    let pad = " ".repeat(indent);
    let total: usize = widths.iter().map(|w| w + 3).sum::<usize>() + 1;
    if total + indent > style.width {
        // Too wide for the terminal: alignment would be meaningless.
        for cells in std::iter::once(&table.header).chain(table.rows.iter()) {
            let text: Vec<String> = cells.iter().map(|cell| plain_of(cell)).collect();
            out.push_str(&pad);
            out.push_str(text.join("  ").trim_end());
            out.push('\n');
        }
        return;
    }

    let border = |left: &str, mid: &str, right: &str| {
        let cells: Vec<String> = widths.iter().map(|w| "─".repeat(w + 2)).collect();
        style.dim(&format!("{left}{}{right}", cells.join(mid)))
    };

    out.push_str(&pad);
    out.push_str(&border("┌", "┬", "┐"));
    out.push('\n');

    let row_line = |cells: &[Vec<Inline>], out: &mut String| {
        out.push_str(&pad);
        out.push_str(&style.dim("│"));
        for (index, width) in widths.iter().enumerate() {
            let empty: Vec<Inline> = Vec::new();
            let cell = cells.get(index).unwrap_or(&empty);
            let painted = render_inlines_inline(cell, style);
            let padding = width.saturating_sub(plain_width(cell));
            let align = table.align.get(index).copied().unwrap_or(Alignment::None);
            out.push(' ');
            match align {
                Alignment::Right => {
                    out.push_str(&" ".repeat(padding));
                    out.push_str(&painted);
                }
                Alignment::Center => {
                    let left = padding / 2;
                    out.push_str(&" ".repeat(left));
                    out.push_str(&painted);
                    out.push_str(&" ".repeat(padding - left));
                }
                _ => {
                    out.push_str(&painted);
                    out.push_str(&" ".repeat(padding));
                }
            }
            out.push(' ');
            out.push_str(&style.dim("│"));
        }
        out.push('\n');
    };

    row_line(&table.header, out);
    out.push_str(&pad);
    out.push_str(&border("├", "┼", "┤"));
    out.push('\n');
    for row in &table.rows {
        row_line(row, out);
    }
    out.push_str(&pad);
    out.push_str(&border("└", "┴", "┘"));
    out.push('\n');
}

/// Writes inline runs into a line, wrapping at the style's width.
///
/// Styling is kept as a stack of ANSI codes so nested emphasis composes, and
/// each word is closed and reopened at its boundaries. Words are never split, so
/// a wrapped bold run is not left unterminated.
struct LineWriter<'a> {
    style: &'a Style,
    indent: usize,
    limit: usize,
    column: usize,
    out: String,
    codes: Vec<&'static str>,
}

impl<'a> LineWriter<'a> {
    fn new(style: &'a Style, indent: usize) -> Self {
        Self {
            style,
            indent,
            limit: style.width.max(20),
            column: 0,
            out: String::new(),
            codes: Vec::new(),
        }
    }

    /// Start a fresh line at the indent.
    fn hard_break(&mut self) {
        self.out.push('\n');
        self.out.push_str(&" ".repeat(self.indent));
        self.column = self.indent;
    }

    /// Append one word, wrapping first if it would overflow.
    fn word(&mut self, word: &str) {
        let width = word.chars().count();
        let at_start = self.column <= self.indent;

        if !at_start && self.column + width + 1 > self.limit {
            self.hard_break();
        } else if !at_start {
            self.out.push(' ');
            self.column += 1;
        }

        let code = self.codes.concat();
        self.out.push_str(&self.style.paint(&code, word));
        self.column += width;
    }

    fn finish(self, out: &mut String) {
        out.push_str(&self.out);
    }
}

/// Write inline runs through a [`LineWriter`].
fn write_inlines(inlines: &[Inline], line: &mut LineWriter<'_>) {
    for inline in inlines {
        match inline {
            Inline::Text(text) => {
                for (index, part) in text.split('\n').enumerate() {
                    if index > 0 {
                        line.hard_break();
                    }
                    for word in part.split_whitespace() {
                        line.word(word);
                    }
                }
            }
            Inline::Code(text) => {
                line.codes.push(code::CYAN);
                for word in text.split_whitespace() {
                    line.word(word);
                }
                line.codes.pop();
            }
            Inline::Strong(inner) => {
                line.codes.push(code::BOLD);
                write_inlines(inner, line);
                line.codes.pop();
            }
            Inline::Emphasis(inner) => {
                line.codes.push(code::ITALIC);
                write_inlines(inner, line);
                line.codes.pop();
            }
            Inline::Strike(inner) => {
                line.codes.push(code::STRIKE);
                write_inlines(inner, line);
                line.codes.pop();
            }
        }
    }
}

/// Render inlines onto a single line, with no wrapping.
fn render_inlines_inline(inlines: &[Inline], style: &Style) -> String {
    let mut out = String::new();
    for inline in inlines {
        match inline {
            Inline::Text(text) => out.push_str(text),
            Inline::Code(text) => out.push_str(&style.paint(code::CYAN, text)),
            Inline::Strong(inner) => out.push_str(&style.paint(code::BOLD, &plain_of(inner))),
            Inline::Emphasis(inner) => out.push_str(&style.paint(code::ITALIC, &plain_of(inner))),
            Inline::Strike(inner) => out.push_str(&style.paint(code::STRIKE, &plain_of(inner))),
        }
    }
    out
}

/// The visible text of an inline run, with no styling.
fn plain_of(inlines: &[Inline]) -> String {
    let mut out = String::new();
    for inline in inlines {
        match inline {
            Inline::Text(text) | Inline::Code(text) => out.push_str(text),
            Inline::Strong(inner) | Inline::Emphasis(inner) | Inline::Strike(inner) => {
                out.push_str(&plain_of(inner));
            }
        }
    }
    out
}

/// Visible width of an inline run. Escapes are not counted.
fn plain_width(inlines: &[Inline]) -> usize {
    plain_of(inlines).chars().count()
}

/// Progressive renderer: turns a stream of text deltas into whole blocks.
#[derive(Debug)]
pub struct Stream {
    style: Style,
    /// Complete lines of the block being assembled.
    block: String,
    /// Trailing text with no newline yet.
    pending: String,
    /// Inside a fenced code block, where blank lines do not end a block.
    in_fence: bool,
    /// Whether a block has already been emitted, so the next needs air above it.
    emitted: bool,
}

impl Stream {
    /// A stream that draws with `style`.
    pub fn new(style: Style) -> Self {
        Self {
            style,
            block: String::new(),
            pending: String::new(),
            in_fence: false,
            emitted: false,
        }
    }

    /// Feed a delta, returning any block that is now complete.
    pub fn push(&mut self, chunk: &str) -> String {
        self.pending.push_str(chunk);
        let mut out = String::new();

        while let Some(newline) = self.pending.find('\n') {
            let line: String = self.pending.drain(..=newline).collect();
            self.consume(&line, &mut out);
        }
        out
    }

    /// Render whatever is left over at the end of a turn.
    pub fn flush(&mut self) -> String {
        if !self.pending.is_empty() {
            let tail = std::mem::take(&mut self.pending);
            self.block.push_str(&tail);
        }
        if self.block.trim().is_empty() {
            self.block.clear();
            return String::new();
        }
        let block = std::mem::take(&mut self.block);
        self.emit(&block)
    }

    /// Render a finished block, with a blank line before it if one is due.
    ///
    /// Each block is rendered on its own, so the blank line that `render_block`
    /// puts *between* blocks has to be added here instead. Without it a heading
    /// arrives hard against the paragraph above it.
    fn emit(&mut self, block: &str) -> String {
        let (mut text, _) = render_parsed(parse(block.trim_end()), &self.style);
        if text.is_empty() {
            return String::new();
        }
        // Every block comes back line-terminated. A paragraph ends without a
        // newline of its own, so a caller that appended a single separator would
        // run the next block straight into it.
        if !text.ends_with('\n') {
            text.push('\n');
        }
        let out = if self.emitted {
            format!("\n{text}")
        } else {
            text
        };
        self.emitted = true;
        out
    }

    fn consume(&mut self, line: &str, out: &mut String) {
        let bare = line.trim();
        let is_fence = bare.starts_with("```") || bare.starts_with("~~~");
        if is_fence {
            self.in_fence = !self.in_fence;
        }

        // A blank line ends a block, except inside a fence where it is content.
        if bare.is_empty() && !self.in_fence {
            if !self.block.trim().is_empty() {
                let block = std::mem::take(&mut self.block);
                out.push_str(&self.emit(&block));
            }
            return;
        }
        self.block.push_str(line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn coloured() -> Style {
        Style::rendered(true, 80)
    }

    fn plain() -> Style {
        Style::rendered(false, 80)
    }

    fn strip(text: &str) -> String {
        // Remove ANSI sequences so assertions can look at the visible text.
        let mut out = String::new();
        let mut chars = text.chars();
        while let Some(c) = chars.next() {
            if c == '\u{1b}' {
                for c in chars.by_ref() {
                    if c == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn plain_prose_is_unchanged_when_colour_is_off() {
        let rendered = render_block("just a sentence.", &plain());
        assert_eq!(rendered.trim_end(), "just a sentence.");
    }

    #[test]
    fn emphasis_marks_are_not_leaked_into_the_text() {
        let rendered = strip(&render_block("some **bold** and `code` here", &plain()));
        assert_eq!(rendered.trim_end(), "some bold and code here");
    }

    #[test]
    fn no_colour_means_no_escape_sequences_at_all() {
        let markdown = "# Title\n\ntext with **bold** and `code`\n\n- one\n- two\n\n> quoted\n\n```rust\nfn main() {}\n```";
        let rendered = render_block(markdown, &plain());
        assert!(
            !rendered.contains('\u{1b}'),
            "colour is off but escapes were emitted"
        );
    }

    #[test]
    fn colour_is_used_when_enabled() {
        let rendered = render_block("some **bold** text", &coloured());
        assert!(
            rendered.contains("\u{1b}[1m"),
            "expected bold, got: {rendered:?}"
        );
        assert!(rendered.contains("\u{1b}[0m"), "styles must be reset");
    }

    #[test]
    fn headings_use_distinct_codes_per_level() {
        let mut codes = Vec::new();
        for level in 1..=6 {
            let markdown = format!("{} Heading", "#".repeat(level as usize));
            let rendered = render_block(&markdown, &coloured());
            let code = heading_code(level);
            assert!(rendered.contains(code), "level {level} used the wrong code");
            codes.push(code);
        }
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), 6, "each level should be visually distinct");
    }

    #[test]
    fn a_table_draws_borders_and_aligns_columns() {
        let markdown = "| name | count |\n|---|--:|\n| alpha | 1 |\n| a much longer name | 22 |";
        let rendered = strip(&render_block(markdown, &plain()));

        let lines: Vec<&str> = rendered.lines().collect();
        assert!(
            lines[0].starts_with('┌'),
            "expected a top border, got {:?}",
            lines[0]
        );
        assert!(
            lines.iter().any(|line| line.starts_with('├')),
            "expected a row separator"
        );
        assert!(
            lines.last().unwrap().starts_with('└'),
            "expected a bottom border"
        );

        // Every row between the borders is the same width.
        let widths: Vec<usize> = lines
            .iter()
            .filter(|line| line.starts_with('│'))
            .map(|line| line.chars().count())
            .collect();
        assert!(widths.len() >= 3);
        assert!(
            widths.iter().all(|w| *w == widths[0]),
            "rows are not aligned: {widths:?}"
        );
    }

    #[test]
    fn the_table_header_reaches_the_output() {
        // Regression: the header parsed as an empty row, silently producing a
        // blank heading line.
        let rendered = strip(&render_block(
            "| region | total |\n|---|---|\n| eu | 4 |",
            &plain(),
        ));

        assert!(
            rendered.contains("region"),
            "header text missing: {rendered}"
        );
        assert!(
            rendered.contains("total"),
            "header text missing: {rendered}"
        );
    }

    #[test]
    fn table_numbers_align_right() {
        let markdown = "| n |\n|--:|\n| 1 |\n| 100 |";
        let rendered = strip(&render_block(markdown, &plain()));
        let numbers: Vec<&str> = rendered
            .lines()
            .filter(|line| line.contains('1'))
            .map(|line| line.trim_end())
            .collect();
        assert_eq!(numbers.len(), 2);
        assert!(
            numbers[0].ends_with("1 │"),
            "not right-aligned: {:?}",
            numbers[0]
        );
        assert!(
            numbers[1].ends_with("100 │"),
            "not right-aligned: {:?}",
            numbers[1]
        );
    }

    #[test]
    fn a_table_too_wide_for_the_terminal_degrades_to_plain_lines() {
        let markdown = "| a very long header cell here | another long one |\n|---|---|\n| x | y |";
        let rendered = render_block(markdown, &Style::rendered(false, 20));
        assert!(
            !rendered.contains('┌'),
            "an over-wide table should not draw borders"
        );
    }

    #[test]
    fn lists_render_markers() {
        let rendered = strip(&render_block("- one\n- two\n", &plain()));
        assert!(rendered.contains("• one"));
        assert!(rendered.contains("• two"));
    }

    #[test]
    fn ordered_lists_number_from_the_start() {
        let rendered = strip(&render_block("3. three\n4. four\n", &plain()));
        assert!(rendered.contains("3. three"), "was: {rendered}");
        assert!(rendered.contains("4. four"));
    }

    #[test]
    fn code_blocks_keep_their_contents_verbatim() {
        let rendered = strip(&render_block(
            "```\n  let x = 1;\n\n  let y = 2;\n```",
            &plain(),
        ));
        assert!(rendered.contains("  let x = 1;"));
        assert!(
            rendered.contains("  let y = 2;"),
            "blank line inside a fence was lost"
        );
    }

    #[test]
    fn long_paragraphs_wrap_at_the_width() {
        let text = "word ".repeat(60);
        let rendered = render_block(text.trim(), &Style::rendered(false, 30));
        assert!(
            rendered.lines().all(|line| line.chars().count() <= 30),
            "a line exceeded the width: {:?}",
            rendered.lines().max_by_key(|l| l.chars().count())
        );
    }

    #[test]
    fn a_list_item_stays_on_one_line_through_its_inline_marks() {
        // Regression: each inline run became its own paragraph, so this rendered
        // as four bullets and the code span and strikethrough were dropped.
        let markdown = "- The `parser` now handles nested tables\n- ~~Legacy mode~~ is gone\n";
        let rendered = strip(&render_block(markdown, &plain()));
        let lines: Vec<&str> = rendered.lines().collect();

        assert_eq!(
            lines.len(),
            2,
            "one line per item expected, got {rendered:?}"
        );
        assert_eq!(lines[0], "• The parser now handles nested tables");
        assert_eq!(lines[1], "• Legacy mode is gone");
    }

    #[test]
    fn strikethrough_inside_a_list_is_styled() {
        let rendered = render_block("- ~~old~~ is gone\n", &coloured());
        assert!(
            rendered.contains("\u{1b}[9m"),
            "strikethrough missing: {rendered:?}"
        );
    }

    #[test]
    fn plain_words_are_not_wrapped_in_pointless_escapes() {
        // Regression: every unstyled word got its own `ESC[0m`, which doubled
        // the width of ordinary prose for no visual effect.
        let rendered = render_block("one two three", &coloured());
        assert_eq!(
            rendered.trim_end(),
            "one two three",
            "plain prose gained escapes"
        );
    }

    #[test]
    fn bold_inside_a_wrapped_paragraph_is_styled_per_word() {
        // The wrapping path styles word by word; the style must still be closed.
        let text = format!("{} **bold words here**", "filler ".repeat(30));
        let rendered = render_block(text.trim(), &Style::rendered(true, 40));
        assert!(
            rendered.contains("\u{1b}[1mbold\u{1b}[0m"),
            "bold lost: {rendered:?}"
        );
    }

    #[test]
    fn every_escape_is_balanced() {
        let markdown =
            "# T\n\nSome **bold**, *it*, ~~old~~, `code`, and a [link](http://x).\n\n- a\n- b";
        let rendered = render_block(markdown, &coloured());
        let resets = rendered.matches("\u{1b}[0m").count();
        let opens = rendered.matches("\u{1b}[").count() - resets;
        assert!(
            resets > 0,
            "the sample should contain styling: {rendered:?}"
        );
        assert_eq!(
            opens, resets,
            "unbalanced escapes, style would leak: {rendered:?}"
        );
    }

    #[test]
    fn link_text_survives_without_its_syntax() {
        let rendered = strip(&render_block(
            "see [the docs](http://example.com) now",
            &plain(),
        ));
        assert_eq!(rendered.trim_end(), "see the docs now");
    }

    #[test]
    fn blockquotes_are_marked() {
        let rendered = strip(&render_block("> quoted text", &plain()));
        assert!(rendered.contains("▎ quoted text"), "was: {rendered:?}");
    }

    // ------------------------------------------------------------- streaming

    #[test]
    fn a_paragraph_is_emitted_only_once_it_is_complete() {
        let mut stream = Stream::new(plain());
        assert_eq!(stream.push("hel"), "", "partial text must not be emitted");
        assert_eq!(
            stream.push("lo"),
            "",
            "still not complete without a blank line"
        );
        let out = stream.push("\n");
        assert!(out.is_empty(), "the first newline does not end a paragraph");

        let out = stream.push("\n");
        assert_eq!(
            out.trim_end(),
            "hello",
            "the blank line completed the block"
        );
    }

    #[test]
    fn consecutive_streamed_blocks_are_separated_by_a_blank_line() {
        // Regression: each block is rendered on its own, so the blank line that
        // `render_block` puts between blocks was lost and a heading landed hard
        // against the paragraph above it.
        let mut stream = Stream::new(plain());
        let mut all = String::new();
        all.push_str(&stream.push("Intro paragraph.\n\n"));
        all.push_str(&stream.push("## Numbers\n\n"));
        all.push_str(&stream.push("Body text.\n\n"));

        let text = all.trim_end();
        assert!(
            text.contains("Intro paragraph.\n\nNumbers"),
            "no air before the heading: {text:?}"
        );
        assert!(
            text.contains("Numbers\n\nBody text."),
            "no air after the heading: {text:?}"
        );
    }

    #[test]
    fn the_first_streamed_block_gets_no_leading_blank_line() {
        let mut stream = Stream::new(plain());
        let out = stream.push("First block.\n\n");
        assert!(out.starts_with("First"), "leading blank line: {out:?}");
    }

    #[test]
    fn each_paragraph_is_emitted_as_it_lands() {
        let mut stream = Stream::new(plain());
        let mut all = String::new();
        all.push_str(&stream.push("first para\n\n"));
        all.push_str(&stream.push("second para\n\n"));
        assert!(all.contains("first para"));
        assert!(all.contains("second para"));
    }

    #[test]
    fn a_blank_line_inside_a_fence_does_not_split_the_block() {
        let mut stream = Stream::new(plain());
        let mut all = String::new();
        all.push_str(&stream.push("```rust\n"));
        all.push_str(&stream.push("let a = 1;\n"));
        all.push_str(&stream.push("\n"));
        all.push_str(&stream.push("let b = 2;\n"));
        all.push_str(&stream.push("```\n"));
        all.push_str(&stream.flush());
        assert!(all.contains("let a = 1;"), "was: {all:?}");
        assert!(all.contains("let b = 2;"), "code block was split: {all:?}");
    }

    #[test]
    fn flush_emits_an_unterminated_paragraph() {
        let mut stream = Stream::new(plain());
        stream.push("no trailing blank line");
        assert_eq!(stream.flush().trim_end(), "no trailing blank line");
    }

    #[test]
    fn flushing_twice_emits_nothing_the_second_time() {
        let mut stream = Stream::new(plain());
        stream.push("text\n\n");
        assert!(stream.flush().is_empty(), "the block was already emitted");
    }

    #[test]
    fn a_streamed_table_is_emitted_once_its_last_row_arrives() {
        let mut stream = Stream::new(plain());
        let mut all = String::new();
        all.push_str(&stream.push("| a | b |\n"));
        all.push_str(&stream.push("|---|---|\n"));
        all.push_str(&stream.push("| 1 | 2 |\n\n"));

        assert!(
            all.contains('┌'),
            "the table should be drawn once complete: {all:?}"
        );
        assert!(all.contains("│ 1 │ 2 │"), "row content missing: {all:?}");
    }

    #[test]
    fn deltas_arriving_one_character_at_a_time_still_render() {
        let markdown = "# Title\n\nA **bold** word.\n\n| x | y |\n|---|---|\n| 1 | 2 |\n";
        let mut stream = Stream::new(plain());
        let mut all = String::new();
        for c in markdown.chars() {
            all.push_str(&stream.push(&c.to_string()));
        }
        all.push_str(&stream.flush());

        assert!(all.contains("Title"));
        assert!(all.contains("bold"));
        assert!(all.contains('┌'), "the table was not drawn: {all:?}");
    }
}
