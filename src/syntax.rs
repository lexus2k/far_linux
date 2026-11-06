//! Syntax colours for the editor, which Far gets from its Colorer plugin. The
//! grammars come from the `syntect` crate. No terminal code here.

use crossterm::style::Color;
use fancy_regex::Regex;
use std::path::Path;
use std::str::FromStr;
use std::sync::OnceLock;
use syntect::highlighting::{
    self as hl, FontStyle, HighlightIterator, HighlightState, Highlighter, ScopeSelectors,
    StyleModifier, Theme, ThemeItem, ThemeSettings,
};
use syntect::parsing::{ParseState, ScopeStack, SyntaxReference, SyntaxSet};

/// Parser state is kept at least every this many lines or bytes, so that an edit
/// or a jump starts from nearby instead of from the top of the file. The grammars
/// parse about 11000 short lines a second, but only about 200 KB a second of long
/// lines, so bytes are what count.
const CHECKPOINT_LINES: usize = 64;
const CHECKPOINT_BYTES: usize = 8 * 1024;
/// How far back parsing starts after a jump, if nothing nearer is known. Starting
/// from the top of a large file would freeze the editor. Starting here takes a
/// known time, but a jump into the middle of a long comment colours it as code
/// until the text above it is parsed. Colorer has the same limit, which it calls
/// backparse.
const MOST_BEHIND: usize = 1000;
const MOST_BEHIND_BYTES: usize = 32 * 1024;
/// A longer line is not coloured, and the parser skips it. Minified files have
/// lines of megabytes, and even 1000 characters of dense code take 10 ms.
const MOST_LINE_CHARS: usize = 1000;

/// The colours in use, with the palette number a theme carries in its red value.
/// All are from Far's 16, so terminals with few colours can show them.
const PALETTE: [Color; 9] = [
    crate::screen::theme::EDITOR.fg,
    Color::DarkCyan,
    Color::Yellow,
    Color::Green,
    Color::White,
    Color::Magenta,
    Color::Grey,
    Color::Red,
    Color::DarkYellow,
];

/// Which palette colour each kind of text takes, following the default console
/// scheme of Far's Colorer plugin ("Aqua on blue", `hrd/console/default.hrd`).
/// When several match, Sublime's scope rules choose the most specific.
const RULES: &[(&str, usize)] = &[
    ("comment", 1),
    ("constant.character.escape", 1),
    ("string, constant.character", 2),
    ("constant.numeric", 3),
    ("constant.language, keyword, storage.modifier", 4),
    ("storage.type", 2),
    ("storage.type.function, storage.type.class", 4),
    ("punctuation - comment - string", 4),
    ("meta.preprocessor, keyword.control.import", 3),
    ("entity.name.tag", 6),
    ("entity.other.attribute-name, variable.parameter", 3),
    ("entity.name.label, invalid", 5),
    // Markdown, from Colorer's `misc/markdown.hrc`.
    ("markup.heading, entity.name.section", 7),
    ("punctuation.definition.heading", 6),
    ("markup.bold", 8),
    ("markup.italic", 5),
    ("markup.raw", 2),
    (
        "punctuation.definition.bold, punctuation.definition.italic, punctuation.definition.raw",
        7,
    ),
    (
        "punctuation.definition.raw.code-fence, constant.other.language-name",
        6,
    ),
    (
        "meta.link.inline.description, meta.link.reference.description, string.other.link",
        7,
    ),
    (
        "punctuation.definition.link, punctuation.definition.metadata, markup.underline.link",
        6,
    ),
    ("markup.list meta.paragraph.list", 3),
    (
        "punctuation.definition.list_item, markup.list.numbered.bullet, markup.list.unnumbered.bullet",
        4,
    ),
    ("markup.quote", 4),
    ("punctuation.definition.blockquote", 6),
    ("meta.separator", 4),
];

/// File names of Colorer's "text" type, in `hrc/proto.hrc`, which also covers logs.
const TEXT_NAMES: &str =
    r"(?i)read\.?me|\.txt$|\.rus|\.nfo|\.log|install|changelog|authors|copying|faq|license|todo$";

/// Colorer's `misc/text.hrc` and the schemes it inherits, in its order. The first
/// alternative wins at the leftmost position, as in Colorer.
const TEXT_SCHEME: &str = concat!(
    r"(?i)(?P<url>\b(?:(?:about|ftp|git|gopher|https?|javascript|news|nntp|wais)://|(?:www|ftp)\.)",
    r"[\[\]@%:+\w./~?\-*=_#&;]+\b/?)",
    r"|(?P<email>[a-z\d][\w.~\-#]*@[\w~\-#]+?\.[?=\w.~\-#]+)",
    r"|(?P<apost>\w'[avdlstm])",
    r"|(?P<word>\p{L}[\p{L}\s]+(?:[\s;=+*&|^()\[\],.\-!~%{}?_]|$))",
    r"|(?P<date>\b[0-3]?\d[/.\-][0-1]?\d[/.\-]\d\d(?:\d\d)?\b|\b\d{4}-(?:0\d|1[012])-(?:[012]\d|3[01])\b)",
    r"|(?P<time>\b[0-2]?\d:[0-5]\d(?::[0-5]\d)?(?:\s?(?:am|pm))?\b)",
    r"|(?P<number>\b\d{1,10}(?:[,.]\d{0,10})?(?:e[\-+]?\d+)?\b|\b[,.]\d{1,10}(?:e[\-+]?\d+)?\b)",
    r"|(?P<quoted>``.*?'')",
    r#"|(?P<string>".*?"|'.*?')"#,
    r"|(?P<label>[┬┼┴┌└┐┘├┤│─╦╬╩╔╚╗╝╠╣║═╡╢╖╕╜╛╞╟╧╨╤╥╙╘╒╓╫╪])",
    r"|(?P<symbol>[;=+\-/*&|^()\[\],.:!~<>%{}?_\\])",
);

/// Android's logcat formats: `threadtime`, `time`, `brief` and `tag`. Far has no
/// log type, so this is our addition on top of the text colours.
const LOGCAT: &str = concat!(
    r"^(?:(?P<date>(?:\d{4}-)?\d\d-\d\d)\s+\d\d:\d\d:\d\d(?:[.,]\d+)?\s+)?",
    r"(?:(?:\w+\s+)?\d+\s+\d+\s+(?P<level1>[VDIWEFA])\s+(?P<tag1>[^:]*?)\s*:",
    r"|(?P<level2>[VDIWEFA])/(?P<tag2>[^(:]*))",
);

fn compiled(cell: &'static OnceLock<Option<Regex>>, pattern: &str) -> Option<&'static Regex> {
    cell.get_or_init(|| Regex::new(pattern).ok()).as_ref()
}

fn text_names() -> Option<&'static Regex> {
    static RE: OnceLock<Option<Regex>> = OnceLock::new();
    compiled(&RE, TEXT_NAMES)
}

fn text_scheme() -> Option<&'static Regex> {
    static RE: OnceLock<Option<Regex>> = OnceLock::new();
    compiled(&RE, TEXT_SCHEME)
}

fn logcat() -> Option<&'static Regex> {
    static RE: OnceLock<Option<Regex>> = OnceLock::new();
    compiled(&RE, LOGCAT)
}

/// Whether a file uses the text colours: by name as in Colorer, or a log without
/// a helpful name, judged by its first line.
fn is_text_file(path: &Path, first_line: &str) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let by_name = text_names().is_some_and(|re| re.is_match(name).unwrap_or(false));
    by_name
        || first_line.starts_with("--------- beginning of")
        || logcat().is_some_and(|re| re.is_match(first_line).unwrap_or(false))
}

fn level_color(level: &str) -> Color {
    match level {
        "E" | "F" | "A" => Color::Red,
        "W" => Color::Yellow,
        "I" => Color::Green,
        "V" => Color::Grey,
        _ => Color::White,
    }
}

/// The colour of each character of a line of text or log. The result is the
/// same whatever the lines around it, so this needs no parser state.
fn text_line_colors(line: &str) -> Vec<Color> {
    let Some(scheme) = text_scheme() else {
        return Vec::new();
    };
    let starts: Vec<usize> = line.char_indices().map(|(i, _)| i).collect();
    let mut colors = vec![PALETTE[0]; starts.len()];
    let mut paint = |from: usize, to: usize, color: Color| {
        let a = starts.partition_point(|s| *s < from);
        let b = starts.partition_point(|s| *s < to);
        colors[a..b].fill(color);
    };
    let mut at = 0;
    while at < line.len() {
        let Ok(Some(m)) = scheme.captures_from_pos(line, at) else {
            break;
        };
        let Some(all) = m.get(0) else {
            break;
        };
        let group = |name: &str| m.name(name).is_some();
        let (from, to) = (all.start(), all.end());
        if group("url") {
            paint(from, to, Color::Red);
        } else if group("email") || group("date") || group("quoted") {
            paint(from, to, Color::Yellow);
        } else if group("apost") {
            // The letter on each side stays as it is.
            let quote = from + line[from..].find('\'').unwrap_or(0);
            paint(quote, quote + 1, Color::Yellow);
        } else if group("time") || group("number") {
            paint(from, to, Color::Green);
        } else if group("string") {
            paint(from, to, Color::Yellow);
            paint(from, from + 1, Color::DarkYellow);
            paint(to - 1, to, Color::DarkYellow);
        } else if group("label") {
            paint(from, to, Color::Magenta);
        } else if group("symbol") {
            paint(from, to, Color::White);
        }
        // A word's last character is only there to end it, and is looked at again.
        let next = if group("word") {
            line[from..to]
                .char_indices()
                .last()
                .map_or(to, |(i, _)| from + i)
        } else {
            to
        };
        at = if next > at {
            next
        } else {
            at + line[at..].chars().next().map_or(1, char::len_utf8)
        };
    }
    if let Some(Ok(Some(m))) = logcat().map(|re| re.captures(line)) {
        if let Some(date) = m.name("date") {
            paint(date.start(), date.end(), Color::Yellow);
        }
        let pick = |a: &str, b: &str| m.name(a).or_else(|| m.name(b));
        if let (Some(level), Some(tag)) = (pick("level1", "level2"), pick("tag1", "tag2")) {
            let color = level_color(level.as_str());
            paint(level.start(), level.end(), color);
            paint(tag.start(), tag.end(), color);
        }
    }
    colors
}

fn palette_color(index: usize) -> hl::Color {
    hl::Color {
        r: index as u8,
        g: 0,
        b: 0,
        a: 255,
    }
}

fn theme() -> Theme {
    let scopes = RULES
        .iter()
        .filter_map(|(selector, index)| {
            Some(ThemeItem {
                scope: ScopeSelectors::from_str(selector).ok()?,
                style: StyleModifier {
                    foreground: Some(palette_color(*index)),
                    background: None,
                    font_style: Some(FontStyle::empty()),
                },
            })
        })
        .collect();
    Theme {
        settings: ThemeSettings {
            foreground: Some(palette_color(0)),
            ..ThemeSettings::default()
        },
        scopes,
        ..Theme::default()
    }
}

fn syntaxes() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn themed() -> &'static Theme {
    static THEME: OnceLock<Theme> = OnceLock::new();
    THEME.get_or_init(theme)
}

/// Finds the grammar by the file's name, then by its first line, as with `#!`.
fn syntax_for(path: &Path, first_line: &str) -> Option<&'static SyntaxReference> {
    let set = syntaxes();
    let by_name = |text: Option<&std::ffi::OsStr>| {
        text.and_then(|t| t.to_str())
            .and_then(|t| set.find_syntax_by_extension(t))
    };
    by_name(path.extension())
        .or_else(|| by_name(path.file_name()))
        .or_else(|| set.find_syntax_by_first_line(first_line))
        .filter(|s| s.name != "Plain Text")
}

#[derive(Clone)]
struct Checkpoint {
    line: usize,
    /// Parsed from the top of the file, not from a guess after a jump.
    exact: bool,
    parse: ParseState,
    highlight: HighlightState,
}

/// Colours for one open file.
pub struct Colors {
    syntax: Option<&'static SyntaxReference>,
    /// No grammar, but Colorer's text type applies.
    text: bool,
    checkpoints: Vec<Checkpoint>,
    /// Colours of lines near the screen, so that a redraw with nothing changed
    /// costs nothing.
    cache: std::collections::BTreeMap<usize, Vec<Color>>,
    /// Lines parsed so far, for tests of how much work is done.
    parsed: usize,
}

impl Colors {
    /// Prepares colours for a file. Nothing is coloured if no grammar fits.
    pub fn for_file(path: &Path, first_line: &str) -> Colors {
        let syntax = syntax_for(path, first_line);
        Colors {
            syntax,
            text: syntax.is_none() && is_text_file(path, first_line),
            checkpoints: Vec::new(),
            cache: Default::default(),
            parsed: 0,
        }
    }

    fn start_state(&mut self) -> Option<Checkpoint> {
        let syntax = self.syntax?;
        let highlighter = Highlighter::new(themed());
        Some(Checkpoint {
            line: 0,
            exact: true,
            parse: ParseState::new(syntax),
            highlight: HighlightState::new(&highlighter, ScopeStack::new()),
        })
    }

    /// The colour of each character of the lines from `top`, one list per line.
    /// `changed` is the first line edited since the last call. An empty list
    /// means default colours. A grammar failure turns colours off for the file.
    pub fn line_colors(
        &mut self,
        lines: &[String],
        changed: Option<usize>,
        top: usize,
        count: usize,
    ) -> Vec<Vec<Color>> {
        if self.syntax.is_none() && !self.text {
            return Vec::new();
        }
        if let Some(line) = changed {
            // A checkpoint holds the state before its line, so one at the changed
            // line is still good.
            self.checkpoints.retain(|c| c.line <= line);
            self.cache.retain(|k, _| *k < line);
        }
        let end = top.saturating_add(count).min(lines.len());
        if top >= end {
            return Vec::new();
        }
        let missing: Vec<usize> = (top..end).filter(|l| !self.cache.contains_key(l)).collect();
        if let (Some(&first), Some(&last)) = (missing.first(), missing.last()) {
            if self.text {
                for &l in &missing {
                    let colors = if too_long(&lines[l]) {
                        Vec::new()
                    } else {
                        text_line_colors(&lines[l])
                    };
                    self.cache.insert(l, colors);
                }
            } else if !self.parse(lines, first, last, top, end) {
                return Vec::new();
            }
        }
        // Keep what is near enough to come back by scrolling.
        let keep = top.saturating_sub(count)..end.saturating_add(count);
        self.cache.retain(|k, _| keep.contains(k));
        (top..end)
            .map(|l| self.cache.get(&l).cloned().unwrap_or_default())
            .collect()
    }

    /// Parses up to line `last`, from a checkpoint near `first`, and keeps the
    /// colours of lines from `top` to `end`. False means the grammar failed.
    fn parse(
        &mut self,
        lines: &[String],
        first: usize,
        last: usize,
        top: usize,
        end: usize,
    ) -> bool {
        // How far back a start may be, by lines and by bytes.
        let mut limit = first;
        let mut bytes = 0;
        while limit > 0 && first - limit < MOST_BEHIND && bytes < MOST_BEHIND_BYTES {
            limit -= 1;
            bytes += lines[limit].len() + 1;
        }
        let near = |c: &&Checkpoint| c.line >= limit && c.line <= first;
        let start = self
            .checkpoints
            .iter()
            .rev()
            .find(|c| near(c) && c.exact)
            .or_else(|| self.checkpoints.iter().rev().find(near))
            .cloned();
        let Some(mut state) = start.or_else(|| {
            let mut fresh = self.start_state()?;
            // Nothing near: as if the file began at `limit`.
            fresh.line = limit;
            fresh.exact = limit == 0;
            Some(fresh)
        }) else {
            return false;
        };
        let set = syntaxes();
        let highlighter = Highlighter::new(themed());
        let mut text = String::new();
        let mut since = (0usize, 0usize);
        let mut at = state.line;
        while at <= last {
            let line = &lines[at];
            if at == state.line || since.0 >= CHECKPOINT_LINES || since.1 >= CHECKPOINT_BYTES {
                self.keep_checkpoint(at, &state);
                since = (0, 0);
            }
            since.0 += 1;
            since.1 += line.len() + 1;
            let mut colors = Vec::new();
            if !too_long(line) {
                text.clear();
                text.push_str(line);
                text.push('\n');
                let Ok(ops) = state.parse.parse_line(&text, set) else {
                    self.syntax = None;
                    self.checkpoints.clear();
                    self.cache.clear();
                    return false;
                };
                self.parsed += 1;
                let spans = HighlightIterator::new(&mut state.highlight, &ops, &text, &highlighter);
                if at >= top && at < end {
                    for (style, piece) in spans {
                        let index = style.foreground.r as usize;
                        let color = PALETTE.get(index).copied().unwrap_or(PALETTE[0]);
                        colors.extend(piece.chars().filter(|c| *c != '\n').map(|_| color));
                    }
                } else {
                    // The state still has to follow the text, though nothing is shown.
                    spans.for_each(drop);
                }
            }
            if at >= top && at < end {
                self.cache.insert(at, colors);
            }
            at += 1;
        }
        true
    }

    /// Keeps the state before line `at`, in order. An exact state replaces a
    /// guessed one, so that wrong colours from a jump do not stay.
    fn keep_checkpoint(&mut self, at: usize, state: &Checkpoint) {
        let place = self.checkpoints.partition_point(|c| c.line < at);
        let mut kept = state.clone();
        kept.line = at;
        match self.checkpoints.get_mut(place) {
            Some(c) if c.line == at => {
                if state.exact && !c.exact {
                    *c = kept;
                }
            }
            _ => self.checkpoints.insert(place, kept),
        }
    }
}

fn too_long(line: &str) -> bool {
    line.len() > MOST_LINE_CHARS && line.chars().nth(MOST_LINE_CHARS).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn colors_of(name: &str, text: &str) -> Vec<Vec<Color>> {
        let lines: Vec<String> = text.split('\n').map(str::to_owned).collect();
        let mut c = Colors::for_file(Path::new(name), &lines[0]);
        c.line_colors(&lines, Some(0), 0, lines.len())
    }

    /// The colour of the character at `word` in a line.
    fn at(line: &str, colors: &[Color], word: &str) -> Color {
        let byte = line.find(word).unwrap_or_else(|| panic!("no {word}"));
        colors[line[..byte].chars().count()]
    }

    #[test]
    fn every_colour_rule_is_valid() {
        for (selector, index) in RULES {
            assert!(ScopeSelectors::from_str(selector).is_ok(), "{selector}");
            assert!(*index < PALETTE.len(), "{selector}");
        }
    }

    #[test]
    fn a_comment_and_a_string_include_their_marks() {
        let line = "const char* s = \"text\"; // note";
        let c = colors_of("a.c", line);
        assert_eq!(at(line, &c[0], "\"text"), Color::Yellow);
        assert_eq!(at(line, &c[0], "//"), Color::DarkCyan);
        assert_eq!(at(line, &c[0], ";"), Color::White);
    }

    #[test]
    fn cpp_has_comments_keywords_strings_and_numbers() {
        let l0 = "// note";
        let l1 = "#include <vector>";
        let l2 = "int main() { return 42; }";
        let l3 = "const char* s = \"text\";";
        let c = colors_of("a.cpp", &[l0, l1, l2, l3].join("\n"));
        assert_eq!(at(l0, &c[0], "note"), Color::DarkCyan);
        assert_eq!(at(l1, &c[1], "#include"), Color::Green);
        assert_eq!(at(l2, &c[2], "int"), Color::Yellow);
        assert_eq!(at(l2, &c[2], "return"), Color::White);
        assert_eq!(at(l2, &c[2], "42"), Color::Green);
        assert_eq!(at(l3, &c[3], "text"), Color::Yellow);
        assert_eq!(c[0].len(), l0.chars().count());
    }

    #[test]
    fn python_is_coloured() {
        let l0 = "# note";
        let l1 = "def f(x): return 'text' + str(7)";
        let c = colors_of("a.py", &[l0, l1].join("\n"));
        assert_eq!(at(l0, &c[0], "note"), Color::DarkCyan);
        assert_eq!(at(l1, &c[1], "def"), Color::White);
        assert_eq!(at(l1, &c[1], "text"), Color::Yellow);
        assert_eq!(at(l1, &c[1], "7"), Color::Green);
    }

    #[test]
    fn common_languages_have_grammars() {
        for name in [
            "a.c", "a.h", "a.cc", "a.hpp", "a.rs", "a.py", "a.js", "a.java", "a.go", "a.sh",
            "a.html", "a.css", "a.xml", "a.json", "a.yml", "a.sql", "a.md", "a.rb", "a.php",
            "Makefile",
        ] {
            assert!(syntax_for(Path::new(name), "").is_some(), "{name}");
        }
    }

    #[test]
    fn a_first_line_can_name_the_language_and_plain_text_has_none() {
        assert!(syntax_for(Path::new("script"), "#!/bin/bash").is_some());
        assert!(syntax_for(Path::new("notes.txt"), "hello").is_none());
        assert!(syntax_for(Path::new("no_extension"), "hello").is_none());
        assert!(colors_of("no_extension", "hello").is_empty());
    }

    #[test]
    fn a_comment_carries_across_lines() {
        let c = colors_of("a.c", "/* start\nmiddle\nend */\nint x;");
        assert_eq!(c[1][0], Color::DarkCyan);
        assert_eq!(c[3][0], Color::Yellow);
    }

    #[test]
    fn starting_lower_gives_the_same_colours_as_from_the_top() {
        let lines: Vec<String> = (0..1000)
            .map(|i| match i % 3 {
                0 => format!("/* {i}"),
                1 => format!("still {i}"),
                _ => format!("*/ int x{i} = {i};"),
            })
            .collect();
        let mut whole = Colors::for_file(Path::new("a.c"), "");
        let all = whole.line_colors(&lines, Some(0), 0, 1000);
        let mut part = Colors::for_file(Path::new("a.c"), "");
        for top in [700, 3, 998, 256, 512] {
            let got = part.line_colors(&lines, None, top, 2);
            assert_eq!(got, all[top..(top + 2).min(1000)], "from {top}");
        }
    }

    #[test]
    fn an_edit_recolours_the_lines_after_it_but_keeps_earlier_work() {
        let mut lines: Vec<String> = vec!["int a;".into(); 600];
        let mut c = Colors::for_file(Path::new("a.c"), "");
        let before = c.line_colors(&lines, Some(0), 590, 1);
        assert_eq!(before[0][0], Color::Yellow);
        assert!(c.checkpoints.len() >= 3);
        lines[300] = "/* open".into();
        let after = c.line_colors(&lines, Some(300), 590, 1);
        assert_eq!(after[0][0], Color::DarkCyan);
        assert!(c.checkpoints.iter().all(|k| k.line <= 590));
        lines[300] = "int a;".into();
        let again = c.line_colors(&lines, Some(300), 590, 1);
        assert_eq!(again[0][0], Color::Yellow);
    }

    #[test]
    fn very_long_lines_are_left_alone() {
        let long = "x".repeat(MOST_LINE_CHARS + 1);
        let c = colors_of("a.c", &format!("int a;\n{long}\nint b;"));
        assert!(c[1].is_empty());
        assert_eq!(c[2][0], Color::Yellow);
    }

    #[test]
    fn a_jump_parses_a_limited_number_of_lines() {
        let lines = vec!["int a;".to_string(); 100_000];
        let mut c = Colors::for_file(Path::new("a.c"), "");
        let got = c.line_colors(&lines, Some(0), 99_990, 10);
        assert_eq!(got.len(), 10);
        assert_eq!(got[9][0], Color::Yellow);
        let first = c.checkpoints.first().unwrap().line;
        assert!(first >= 99_990 - MOST_BEHIND, "started at {first}");
    }

    #[test]
    fn checkpoints_stay_in_order_after_jumps_in_any_direction() {
        let lines = vec!["int a;".to_string(); 20_000];
        let mut c = Colors::for_file(Path::new("a.c"), "");
        for top in [19_000, 200, 9_000, 8_500, 18_500, 0, 9_100] {
            let got = c.line_colors(&lines, None, top, 5);
            assert_eq!(got[4][0], Color::Yellow, "from {top}");
            assert!(c.checkpoints.windows(2).all(|w| w[0].line < w[1].line));
        }
    }

    #[test]
    fn a_comment_above_a_jump_is_followed_within_the_limit() {
        let mut lines = vec!["int a;".to_string(); 2000];
        lines[1500] = "/* open".into();
        let mut c = Colors::for_file(Path::new("a.c"), "");
        let got = c.line_colors(&lines, None, 1900, 1);
        assert_eq!(got[0][0], Color::DarkCyan);
    }

    #[test]
    fn multibyte_text_lines_up_by_character() {
        let line = "int привет = 1; // 日本語";
        let c = colors_of("a.c", line);
        assert_eq!(c[0].len(), line.chars().count());
        assert_eq!(at(line, &c[0], "日"), Color::DarkCyan);
    }

    #[test]
    fn asking_past_the_end_is_safe() {
        let lines = vec!["int a;".to_string()];
        let mut c = Colors::for_file(Path::new("a.c"), "");
        assert!(c.line_colors(&lines, None, 5, 3).is_empty());
        assert_eq!(c.line_colors(&lines, None, 0, 100).len(), 1);
    }

    #[test]
    fn colorers_text_type_covers_logs_and_its_other_names() {
        for name in [
            "a.log",
            "A.LOG",
            "a.log.1",
            "notes.txt",
            "README",
            "read.me",
            "ChangeLog",
            "LICENSE",
            "install.sh.bak",
            "AUTHORS",
            "my.todo",
            "x.nfo",
        ] {
            assert!(is_text_file(Path::new(name), ""), "{name}");
        }
        for name in ["main.rs", "photo.png", "no_extension", "todo.bak"] {
            assert!(!is_text_file(Path::new(name), "hello"), "{name}");
        }
        // A grammar goes first, so these stay as their languages.
        assert!(!Colors::for_file(Path::new("README.md"), "").text);
        assert!(!Colors::for_file(Path::new("install.py"), "").text);
    }

    #[test]
    fn a_log_with_no_helpful_name_is_found_by_its_first_line() {
        for line in [
            "--------- beginning of main",
            "11-03 22:44:11.802  1234  5678 E Tag: text",
            "E/Tag( 1234): text",
        ] {
            assert!(Colors::for_file(Path::new("logcat"), line).text, "{line}");
        }
    }

    fn text_of(line: &str) -> Vec<Color> {
        colors_of("a.log", line).remove(0)
    }

    #[test]
    fn text_colours_match_colorer() {
        let line =
            "Open \"file\" at 12:30:05 on 2026-11-03 size 42, see https://x.org/a_b or me@x.org.";
        let c = text_of(line);
        assert_eq!(c.len(), line.chars().count());
        assert_eq!(at(line, &c, "Open"), PALETTE[0]);
        assert_eq!(at(line, &c, "\"file"), Color::DarkYellow);
        assert_eq!(at(line, &c, "file"), Color::Yellow);
        assert_eq!(at(line, &c, "12:30"), Color::Green);
        assert_eq!(at(line, &c, "05 on"), Color::Green);
        assert_eq!(at(line, &c, "2026"), Color::Yellow);
        assert_eq!(at(line, &c, "42"), Color::Green);
        assert_eq!(at(line, &c, ","), Color::White);
        assert_eq!(at(line, &c, "https"), Color::Red);
        assert_eq!(at(line, &c, "_b"), Color::Red);
        assert_eq!(at(line, &c, "me@"), Color::Yellow);
    }

    #[test]
    fn digits_inside_names_are_not_numbers_and_an_apostrophe_is_not_a_string() {
        let line = "abc123 don't 0x1f isn't";
        let c = text_of(line);
        assert_eq!(at(line, &c, "123"), PALETTE[0]);
        assert_eq!(at(line, &c, "'t 0"), Color::Yellow);
        assert_eq!(at(line, &c, "1f"), PALETTE[0]);
        assert_eq!(at(line, &c, "isn"), PALETTE[0]);
    }

    #[test]
    fn logcat_threadtime_levels_and_tags_are_coloured() {
        for (level, color) in [
            ('E', Color::Red),
            ('F', Color::Red),
            ('W', Color::Yellow),
            ('I', Color::Green),
            ('D', Color::White),
            ('V', Color::Grey),
        ] {
            let line = format!("11-03 22:44:11.802  1234  5678 {level} ActivityManager: Start 42");
            let c = text_of(&line);
            assert_eq!(at(&line, &c, "11-03"), Color::Yellow, "{level}");
            assert_eq!(at(&line, &c, "22:44"), Color::Green, "{level}");
            assert_eq!(at(&line, &c, ".802"), Color::Green, "{level}");
            assert_eq!(at(&line, &c, "1234"), Color::Green, "{level}");
            assert_eq!(at(&line, &c, "ActivityManager"), color, "{level}");
            assert_eq!(at(&line, &c, "Start"), PALETTE[0], "{level}");
            assert_eq!(at(&line, &c, "42"), Color::Green, "{level}");
            let byte = line.find(&format!(" {level} ")).unwrap() + 1;
            assert_eq!(c[line[..byte].chars().count()], color, "{level}");
        }
    }

    #[test]
    fn logcat_other_formats_are_coloured() {
        for line in [
            "E/Some Tag( 1234): boom",
            "11-03 22:44:11.802 E/Some Tag( 1234): boom",
            "2026-11-03 22:44:11.802 u0_a12 1234 5678 E Some Tag: boom",
            "E/Some Tag: boom",
        ] {
            let c = text_of(line);
            assert_eq!(at(line, &c, "Some Tag"), Color::Red, "{line}");
            assert_eq!(at(line, &c, "boom"), PALETTE[0], "{line}");
        }
    }

    #[test]
    fn a_plain_log_line_is_not_taken_for_logcat() {
        let line = "Error: disk 5 is full";
        let c = text_of(line);
        assert_eq!(at(line, &c, "Error"), PALETTE[0]);
        assert_eq!(at(line, &c, "5"), Color::Green);
    }

    #[test]
    fn text_colours_handle_any_text() {
        for line in [
            "",
            " ",
            "'",
            "\"",
            "``",
            "'' ''",
            "日本語 テキスト 12",
            "привет, мир's 5",
            "╔══╗ ║x║",
            "\t\t\x1b[2J\x00",
            "a@b",
            "https://",
        ] {
            let lines = vec![line.to_string()];
            let mut c = Colors::for_file(Path::new("a.log"), "");
            let got = c.line_colors(&lines, None, 0, 1).remove(0);
            assert_eq!(got.len(), line.chars().count(), "{line:?}");
        }
        let c = text_of("╔═ 日本語 12");
        assert_eq!(c[0], Color::Magenta);
        assert_eq!(c[7], Color::Green);
    }

    #[test]
    fn a_long_log_line_is_left_alone() {
        assert!(text_of(&"x".repeat(MOST_LINE_CHARS + 1)).is_empty());
    }

    #[test]
    fn asking_for_log_lines_past_the_end_is_safe() {
        let lines = vec!["a 1".to_string()];
        let mut c = Colors::for_file(Path::new("a.log"), "");
        assert!(c.line_colors(&lines, None, 5, 3).is_empty());
        assert_eq!(c.line_colors(&lines, None, 0, 100).len(), 1);
    }

    #[test]
    fn markdown_colours_match_colorer() {
        let cases = [
            ("# Head text", "#", Color::Grey),
            ("# Head text", "Head", Color::Red),
            ("para **bold** end", "para", PALETTE[0]),
            ("para **bold** end", "**", Color::Red),
            ("para **bold** end", "bold", Color::DarkYellow),
            ("para *em* end", "*", Color::Red),
            ("para *em* end", "em", Color::Magenta),
            ("para `code` end", "`", Color::Red),
            ("para `code` end", "code", Color::Yellow),
            ("see [link](http://x.y) end", "[", Color::Grey),
            ("see [link](http://x.y) end", "link", Color::Red),
            ("see [link](http://x.y) end", "http", Color::Grey),
            ("see [link](http://x.y) end", ")", Color::Grey),
            ("para \\* esc", "\\*", Color::DarkCyan),
            ("- item text", "-", Color::White),
            ("- item text", "item", Color::Green),
            ("1. item text", "1", Color::White),
            ("1. item text", "item", Color::Green),
            ("> quote text", ">", Color::Grey),
            ("> quote text", "quote", Color::White),
            ("---", "-", Color::White),
            ("```", "`", Color::Grey),
        ];
        for (line, word, color) in cases {
            let c = colors_of("a.md", line);
            assert_eq!(at(line, &c[0], word), color, "{word} in {line}");
        }
    }

    #[test]
    fn a_fenced_markdown_block_is_code() {
        let c = colors_of("a.md", "para\n\n```\nplain code\n```\n");
        assert_eq!(c[3][0], Color::Yellow);
    }

    #[test]
    fn escapes_in_strings_have_their_own_colour() {
        let line = "char* s = \"a\\nb\";";
        let c = colors_of("a.c", line);
        assert_eq!(at(line, &c[0], "\\n"), Color::DarkCyan);
    }

    #[test]
    fn a_redraw_with_nothing_changed_parses_nothing() {
        let lines: Vec<String> = vec!["int a;".into(); 200];
        let mut c = Colors::for_file(Path::new("a.c"), "");
        let first = c.line_colors(&lines, Some(0), 100, 30);
        let work = c.parsed;
        assert!(work > 0);
        assert_eq!(c.line_colors(&lines, None, 100, 30), first);
        assert_eq!(c.parsed, work);
        // One line of scrolling parses from the nearest checkpoint, not the screen.
        let work = c.parsed;
        c.line_colors(&lines, None, 101, 30);
        assert!(c.parsed - work <= CHECKPOINT_LINES, "{}", c.parsed - work);
    }

    #[test]
    fn an_edit_keeps_the_checkpoints_above_it() {
        let lines: Vec<String> = vec!["int a;".into(); 600];
        let mut c = Colors::for_file(Path::new("a.c"), "");
        c.line_colors(&lines, Some(0), 570, 30);
        let work = c.parsed;
        c.line_colors(&lines, Some(500), 570, 30);
        // From a checkpoint near line 500, not from the top.
        assert!(c.parsed - work < 200, "{}", c.parsed - work);
    }

    #[test]
    fn long_lines_limit_how_far_back_a_jump_parses() {
        // Cheap to parse, as only the byte count matters here.
        let long = "x".repeat(MOST_LINE_CHARS - 100);
        let lines: Vec<String> = vec![long; 4000];
        let mut c = Colors::for_file(Path::new("a.c"), "");
        c.line_colors(&lines, Some(0), 3970, 30);
        // 32 KB back at most, which is under 40 of these lines.
        assert!(c.parsed < 80, "{}", c.parsed);
    }

    #[test]
    fn colours_after_a_jump_are_put_right_by_scrolling_from_the_top() {
        let mut lines: Vec<String> = vec!["int a;".into(); 5000];
        lines[0] = "/*".into();
        let mut exact = Colors::for_file(Path::new("a.c"), "");
        let mut jumped = Colors::for_file(Path::new("a.c"), "");
        // The jump guesses: far from the comment start, the lines look like code.
        assert_eq!(
            jumped.line_colors(&lines, None, 3000, 30)[0][0],
            Color::Yellow
        );
        for top in (0..=3000).step_by(30) {
            let want = exact.line_colors(&lines, None, top, 30);
            assert_eq!(jumped.line_colors(&lines, None, top, 30), want, "at {top}");
        }
        assert_eq!(
            jumped.line_colors(&lines, None, 3000, 30)[0][0],
            Color::DarkCyan
        );
    }
}
