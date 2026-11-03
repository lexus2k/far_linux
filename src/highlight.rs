//! Far's file highlighting groups: masks and attributes that choose the colour
//! of a name. The groups live in a text file the user can edit. The first group
//! that matches wins, as in Far. No UI code here.

use crossterm::style::Color;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

/// The default groups, and what a new settings file starts with. Far's own come
/// first, with the colours of its default settings. Executables follow archives
/// and temporary files, because Windows drives mark every file executable.
pub const DEFAULT_TEXT: &str = r#"# File highlighting for far. The first group that matches a file wins.
# Delete this file to get the defaults back.
#
# [masks] names sets of masks, used in a group as <name>.
#
# [group] takes:
#   name             shown in messages about this file
#   mask             *.txt,<name>,*.[ch]  with |*.old to exclude, ignoring case
#   attr             folder, hidden, link, exec, readonly, or !attr for "not"
#   normal           colour of the name
#   cursor           colour under the cursor, else the normal colour
#   selected         colour when marked, else yellow
#   selected_cursor  colour when marked and under the cursor, else yellow
#
# Colours: black darkblue darkgreen darkcyan darkred darkmagenta brown grey
# darkgrey blue green cyan red magenta yellow white, or 0-255, or #rrggbb.

[masks]
arc = *.zip,*.rar,*.[7bgxl]z,*.[bg]zip,*.tar,*.t[agbxl]z,*.z,*.ar[cj],*.r[0-9][0-9],*.a[0-9][0-9],*.bz2,*.cab,*.jar,*.lha,*.lzh,*.ha,*.ac[bei],*.pa[ck],*.rk,*.cpio,*.rpm,*.zoo,*.hqx,*.sit,*.ice,*.uc2,*.ain,*.imp,*.777,*.ufa,*.boa,*.bs[2a],*.sea,*.[ah]pk,*.ddi,*.x2,*.rkv,*.[lw]sz,*.h[ay]p,*.lim,*.sqz,*.chz,*.aa[br],*.msi,*.deb,*.zst,*.tzst,*.lz4,*.lzma
temp = *.bak,*.tmp,*~
image = *.png,*.jpg,*.jpeg,*.gif,*.bmp,*.svg,*.webp,*.tif,*.tiff,*.ico,*.heic,*.psd,*.xcf
media = *.mp3,*.flac,*.ogg,*.opus,*.wav,*.m4a,*.aac,*.wma,*.mp4,*.mkv,*.avi,*.mov,*.webm,*.wmv,*.flv,*.mpg,*.mpeg,*.m4v
doc = *.txt,*.md,*.rst,*.pdf,*.doc,*.docx,*.odt,*.rtf,*.xls,*.xlsx,*.ods,*.csv,*.ppt,*.pptx,*.odp,*.tex,*.epub,*.log,*.htm,*.html
source = *.c,*.h,*.cc,*.cpp,*.cxx,*.hh,*.hpp,*.rs,*.py,*.sh,*.bash,*.js,*.ts,*.go,*.java,*.cs,*.rb,*.pl,*.php,*.lua,*.swift,*.kt,*.sql,*.json,*.xml,*.yaml,*.yml,*.toml,*.ini,*.css,*.mk,*.cmake

[group]
name = Hidden
attr = hidden
normal = darkcyan
cursor = darkgrey

[group]
name = Folder
attr = folder
normal = white

[group]
name = Link
attr = link
normal = grey

[group]
name = Archive
mask = <arc>
normal = magenta

[group]
name = Temporary
mask = <temp>
normal = brown

[group]
name = Image
mask = <image>
normal = 175

[group]
name = Media
mask = <media>
normal = 141

[group]
name = Document
mask = <doc>
normal = 187

[group]
name = Executable
attr = exec
normal = green

[group]
name = Source
mask = <source>
normal = 215
"#;

/// Group references may nest, but not without end.
const MOST_NESTING: u32 = 8;
const MOST_FILE_BYTES: u64 = 1024 * 1024;

/// What a group can test about a file.
#[derive(Clone, Copy, Debug, Default)]
pub struct Facts<'a> {
    pub name: &'a str,
    pub is_dir: bool,
    pub is_link: bool,
    pub hidden: bool,
    pub exec: bool,
    pub read_only: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Attr {
    Folder,
    Hidden,
    Link,
    Exec,
    ReadOnly,
}

impl Attr {
    fn parse(text: &str) -> Option<Attr> {
        Some(match text {
            "folder" => Attr::Folder,
            "hidden" => Attr::Hidden,
            "link" => Attr::Link,
            "exec" => Attr::Exec,
            "readonly" => Attr::ReadOnly,
            _ => return None,
        })
    }

    fn holds(self, facts: &Facts) -> bool {
        match self {
            Attr::Folder => facts.is_dir,
            Attr::Hidden => facts.hidden,
            Attr::Link => facts.is_link,
            Attr::Exec => facts.exec,
            Attr::ReadOnly => facts.read_only,
        }
    }
}

/// One element of a mask.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Token {
    Any,
    One,
    Char(char),
    Set {
        negated: bool,
        ranges: Vec<(char, char)>,
    },
}

fn parse_pattern(mask: &str) -> Vec<Token> {
    // Far's *.* means every name, with or without a dot.
    let mask = if mask == "*.*" { "*" } else { mask };
    let chars: Vec<char> = mask.chars().flat_map(char::to_lowercase).collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' => {
                if tokens.last() != Some(&Token::Any) {
                    tokens.push(Token::Any);
                }
            }
            '?' => tokens.push(Token::One),
            '[' => {
                if let Some(len) = chars[i + 1..].iter().skip(1).position(|c| *c == ']') {
                    let body = &chars[i + 1..i + 2 + len];
                    tokens.push(parse_set(body));
                    i += len + 2;
                } else {
                    tokens.push(Token::Char('['));
                }
            }
            c => tokens.push(Token::Char(c)),
        }
        i += 1;
    }
    tokens
}

fn parse_set(body: &[char]) -> Token {
    let negated = matches!(body.first(), Some('!' | '^'));
    let body = if negated { &body[1..] } else { body };
    let mut ranges = Vec::new();
    let mut i = 0;
    while i < body.len() {
        if i + 2 < body.len() && body[i + 1] == '-' {
            ranges.push((body[i], body[i + 2]));
            i += 3;
        } else {
            ranges.push((body[i], body[i]));
            i += 1;
        }
    }
    Token::Set { negated, ranges }
}

fn token_matches(token: &Token, c: char) -> bool {
    match token {
        Token::Any | Token::One => true,
        Token::Char(t) => *t == c,
        Token::Set { negated, ranges } => {
            ranges.iter().any(|(a, b)| (*a..=*b).contains(&c)) != *negated
        }
    }
}

/// Greedy matching that returns to only the last `*`, so a hostile file name
/// cannot make it slow.
fn pattern_matches(tokens: &[Token], name: &[char]) -> bool {
    let (mut t, mut n) = (0, 0);
    let mut retry = None;
    while n < name.len() {
        match tokens.get(t) {
            Some(Token::Any) => {
                t += 1;
                retry = Some((t, n));
            }
            Some(token) if token_matches(token, name[n]) => {
                t += 1;
                n += 1;
            }
            _ => match retry {
                Some((after_any, from)) => {
                    retry = Some((after_any, from + 1));
                    t = after_any;
                    n = from + 1;
                }
                None => return false,
            },
        }
    }
    tokens[t..].iter().all(|token| *token == Token::Any)
}

/// A compiled mask list: what to include and what to exclude.
#[derive(Clone, Debug, Default)]
struct Masks {
    include: Vec<Vec<Token>>,
    exclude: Vec<Vec<Token>>,
}

impl Masks {
    fn matches(&self, name: &[char]) -> bool {
        let any = |list: &[Vec<Token>]| list.iter().any(|t| pattern_matches(t, name));
        any(&self.include) && !any(&self.exclude)
    }
}

/// Splits a comma list into patterns, replacing `<name>` references. An
/// excluded part inside a reference is ignored.
fn expand(
    list: &str,
    named: &HashMap<String, String>,
    depth: u32,
    out: &mut Vec<Vec<Token>>,
    problems: &mut Vec<String>,
) {
    for part in list.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let reference = part.strip_prefix('<').and_then(|p| p.strip_suffix('>'));
        let Some(name) = reference else {
            out.push(parse_pattern(part));
            continue;
        };
        match named.get(&name.to_lowercase()) {
            Some(_) if depth >= MOST_NESTING => {
                problems.push(format!("masks nest too deeply at <{name}>"));
            }
            Some(inner) => {
                let inner = inner.split('|').next().unwrap_or("");
                expand(inner, named, depth + 1, out, problems);
            }
            None => problems.push(format!("no masks named <{name}>")),
        }
    }
}

fn compile(text: &str, named: &HashMap<String, String>, problems: &mut Vec<String>) -> Masks {
    let (include, exclude) = text.split_once('|').unwrap_or((text, ""));
    let mut masks = Masks::default();
    expand(include, named, 0, &mut masks.include, problems);
    expand(exclude, named, 0, &mut masks.exclude, problems);
    masks
}

/// Reads a colour name, a number from 0 to 255 or `#rrggbb`.
fn parse_color(text: &str) -> Option<Color> {
    let text = text.to_lowercase();
    if let Some(hex) = text.strip_prefix('#') {
        if hex.len() != 6 || !hex.is_ascii() {
            return None;
        }
        let part = |at: usize| u8::from_str_radix(&hex[at..at + 2], 16).ok();
        return Some(Color::Rgb {
            r: part(0)?,
            g: part(2)?,
            b: part(4)?,
        });
    }
    if let Ok(value) = text.parse::<u8>() {
        return Some(Color::AnsiValue(value));
    }
    Some(match text.as_str() {
        "black" => Color::Black,
        "darkblue" => Color::DarkBlue,
        "darkgreen" => Color::DarkGreen,
        "darkcyan" => Color::DarkCyan,
        "darkred" => Color::DarkRed,
        "darkmagenta" => Color::DarkMagenta,
        "brown" => Color::DarkYellow,
        "grey" | "gray" => Color::Grey,
        "darkgrey" | "darkgray" => Color::DarkGrey,
        "blue" => Color::Blue,
        "green" => Color::Green,
        "cyan" => Color::Cyan,
        "red" => Color::Red,
        "magenta" => Color::Magenta,
        "yellow" => Color::Yellow,
        "white" => Color::White,
        _ => return None,
    })
}

/// A group while it is being read.
#[derive(Default)]
struct RawGroup {
    line: usize,
    name: String,
    mask: Option<String>,
    attrs: Vec<(Attr, bool)>,
    normal: Option<Color>,
    cursor: Option<Color>,
    selected: Option<Color>,
    selected_cursor: Option<Color>,
}

#[derive(Debug)]
struct Group {
    masks: Option<Masks>,
    attrs: Vec<(Attr, bool)>,
    normal: Option<Color>,
    cursor: Option<Color>,
    selected: Option<Color>,
    selected_cursor: Option<Color>,
}

impl Group {
    fn matches(&self, facts: &Facts, name: &[char]) -> bool {
        self.attrs.iter().all(|(a, want)| a.holds(facts) == *want)
            && self.masks.as_ref().is_none_or(|m| m.matches(name))
    }
}

/// The colours a matching group gives one name.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NameColors {
    pub normal: Option<Color>,
    pub cursor: Option<Color>,
    pub selected: Option<Color>,
    pub selected_cursor: Option<Color>,
}

/// All highlighting groups, in order.
#[derive(Debug)]
pub struct Highlight {
    groups: Vec<Group>,
    /// Problems found while reading, each naming its line. The rest still works.
    pub problems: Vec<String>,
}

impl Default for Highlight {
    fn default() -> Self {
        Highlight::parse(DEFAULT_TEXT)
    }
}

/// Where the user's settings live.
pub fn settings_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| crate::util::home_dir().map(|h| h.join(".config")))?;
    Some(base.join("far_linux").join("highlight.ini"))
}

impl Highlight {
    /// Reads settings from a file. A file that does not exist means the
    /// defaults. A broken one must never stop the file manager.
    pub fn load(path: &Path) -> Highlight {
        let too_big = fs::metadata(path).is_ok_and(|m| m.len() > MOST_FILE_BYTES);
        if too_big {
            let mut h = Highlight::default();
            h.problems.push(format!("{} is too big", path.display()));
            return h;
        }
        match fs::read(path) {
            Ok(bytes) => Highlight::parse(&String::from_utf8_lossy(&bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Highlight::default(),
            Err(e) => {
                let mut h = Highlight::default();
                h.problems.push(format!("{}: {e}", path.display()));
                h
            }
        }
    }

    /// Reads the settings text.
    pub fn parse(text: &str) -> Highlight {
        enum Section {
            None,
            Masks,
            Group,
        }
        let mut section = Section::None;
        let mut named = HashMap::new();
        let mut raw: Vec<RawGroup> = Vec::new();
        let mut problems = Vec::new();
        for (index, line) in text.lines().enumerate() {
            let number = index + 1;
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            if line.starts_with('[') {
                match line.to_lowercase().as_str() {
                    "[masks]" => section = Section::Masks,
                    "[group]" => {
                        section = Section::Group;
                        raw.push(RawGroup {
                            line: number,
                            ..RawGroup::default()
                        });
                    }
                    _ => {
                        section = Section::None;
                        problems.push(format!("line {number}: unknown section {line}"));
                    }
                }
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                problems.push(format!("line {number}: expected key = value"));
                continue;
            };
            let (key, value) = (key.trim().to_lowercase(), value.trim());
            match section {
                Section::Masks => {
                    named.insert(key, value.to_string());
                }
                Section::Group => {
                    if let Some(group) = raw.last_mut() {
                        read_group_key(group, &key, value, number, &mut problems);
                    }
                }
                Section::None => {
                    problems.push(format!("line {number}: outside any section"));
                }
            }
        }
        let mut groups = Vec::with_capacity(raw.len());
        for group in raw {
            let mut found = Vec::new();
            let masks = group
                .mask
                .as_deref()
                .map(|m| compile(m, &named, &mut found));
            let label = format!("line {} ({})", group.line, group.name);
            problems.extend(found.into_iter().map(|p| format!("{label}: {p}")));
            // A group that tests nothing would colour every file and hide the
            // groups after it, so that is treated as a mistake.
            if group.mask.is_none() && group.attrs.is_empty() {
                problems.push(format!("{label}: needs a mask or attr, so it is ignored"));
                continue;
            }
            groups.push(Group {
                masks,
                attrs: group.attrs,
                normal: group.normal,
                cursor: group.cursor,
                selected: group.selected,
                selected_cursor: group.selected_cursor,
            });
        }
        Highlight { groups, problems }
    }

    /// The colours for a file, from the first group it belongs to.
    pub fn colors(&self, facts: &Facts) -> NameColors {
        let name: Vec<char> = facts.name.chars().flat_map(char::to_lowercase).collect();
        self.groups
            .iter()
            .find(|g| g.matches(facts, &name))
            .map_or_else(NameColors::default, |g| NameColors {
                normal: g.normal,
                cursor: g.cursor.or(g.normal),
                selected: g.selected,
                selected_cursor: g.selected_cursor,
            })
    }
}

fn read_group_key(
    group: &mut RawGroup,
    key: &str,
    value: &str,
    number: usize,
    problems: &mut Vec<String>,
) {
    let mut color = |slot: &mut Option<Color>| match parse_color(value) {
        Some(c) => *slot = Some(c),
        None => problems.push(format!("line {number}: unknown colour {value}")),
    };
    match key {
        "normal" => color(&mut group.normal),
        "cursor" => color(&mut group.cursor),
        "selected" => color(&mut group.selected),
        "selected_cursor" => color(&mut group.selected_cursor),
        "name" => group.name = value.to_string(),
        "mask" => group.mask = Some(value.to_string()),
        "attr" => {
            for word in value.split(',').map(str::trim).filter(|w| !w.is_empty()) {
                let word = word.to_lowercase();
                let (wanted, bare) = match word.strip_prefix('!') {
                    Some(rest) => (false, rest),
                    None => (true, word.as_str()),
                };
                match Attr::parse(bare) {
                    Some(attr) => group.attrs.push((attr, wanted)),
                    None => problems.push(format!("line {number}: unknown attr {word}")),
                }
            }
        }
        _ => problems.push(format!("line {number}: unknown key {key}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str) -> Facts<'_> {
        Facts {
            name,
            ..Facts::default()
        }
    }

    fn mask(mask: &str, name: &str) -> bool {
        let tokens = parse_pattern(mask);
        let chars: Vec<char> = name.chars().flat_map(char::to_lowercase).collect();
        pattern_matches(&tokens, &chars)
    }

    #[test]
    fn defaults_have_no_problems() {
        assert_eq!(Highlight::default().problems, Vec::<String>::new());
    }

    #[test]
    fn far_masks_match() {
        assert!(mask("*.txt", "a.TXT"));
        assert!(mask("*.*", "noext"));
        assert!(mask("*.r[0-9][0-9]", "part.r07"));
        assert!(!mask("*.r[0-9][0-9]", "part.rar"));
        assert!(mask("*.[7bgxl]z", "a.7z"));
        assert!(!mask("*.[7bgxl]z", "a.qz"));
        assert!(mask("*.[!a]z", "a.bz"));
        assert!(!mask("*.[!a]z", "a.az"));
        assert!(mask("a?c", "abc"));
        assert!(!mask("a?c", "ac"));
        assert!(mask("*~", "notes.txt~"));
        assert!(mask("[", "["));
        assert!(mask("файл*", "ФАЙЛ.txt"));
    }

    #[test]
    fn hostile_masks_and_names_stay_fast_and_do_not_panic() {
        let name: String = "a".repeat(5000);
        assert!(!mask("*a*a*a*a*a*a*a*a*a*a*b", &name));
        for broken in ["[", "[]", "[!", "[a-", "*[", "[-]", "[]]"] {
            mask(broken, "name");
        }
    }

    #[test]
    fn far_default_groups_give_far_colours() {
        let h = Highlight::default();
        let normal = |f: Facts| h.colors(&f).normal;
        assert_eq!(normal(file("a.ZIP")), Some(Color::Magenta));
        assert_eq!(normal(file("a.tar.gz")), Some(Color::Magenta));
        assert_eq!(normal(file("old.bak")), Some(Color::DarkYellow));
        assert_eq!(normal(file("x~")), Some(Color::DarkYellow));
        assert_eq!(normal(file("unknown")), None);
        let hidden = Facts {
            name: ".bashrc",
            hidden: true,
            ..Facts::default()
        };
        assert_eq!(normal(hidden), Some(Color::DarkCyan));
        assert_eq!(h.colors(&hidden).cursor, Some(Color::DarkGrey));
        let dir = Facts {
            name: "photos.zip",
            is_dir: true,
            ..Facts::default()
        };
        assert_eq!(normal(dir), Some(Color::White));
    }

    #[test]
    fn added_groups_colour_common_types() {
        let h = Highlight::default();
        for name in ["a.png", "a.mp3", "a.pdf", "a.rs"] {
            assert!(h.colors(&file(name)).normal.is_some(), "{name}");
        }
    }

    #[test]
    fn executables_win_over_source_but_not_over_archives_or_documents() {
        let h = Highlight::default();
        let exec = |name| Facts {
            name,
            exec: true,
            ..Facts::default()
        };
        assert_eq!(h.colors(&exec("run.sh")).normal, Some(Color::Green));
        assert_eq!(h.colors(&exec("tool")).normal, Some(Color::Green));
        // Windows drives under WSL mark everything executable.
        assert_eq!(h.colors(&exec("a.zip")).normal, Some(Color::Magenta));
        assert_ne!(h.colors(&exec("a.pdf")).normal, Some(Color::Green));
    }

    #[test]
    fn first_matching_group_wins_and_order_is_the_files() {
        let h = Highlight::parse(
            "[group]\nmask = *.a\nnormal = red\n[group]\nmask = *\nnormal = blue\n",
        );
        assert_eq!(h.colors(&file("x.a")).normal, Some(Color::Red));
        assert_eq!(h.colors(&file("x.b")).normal, Some(Color::Blue));
    }

    #[test]
    fn exclusion_named_masks_and_nesting() {
        let h = Highlight::parse(
            "[masks]\nall = <one>,*.b\none = *.a\n[group]\nmask = <all>|*.no.a\nnormal = red\n",
        );
        assert!(h.problems.is_empty(), "{:?}", h.problems);
        assert_eq!(h.colors(&file("x.a")).normal, Some(Color::Red));
        assert_eq!(h.colors(&file("x.b")).normal, Some(Color::Red));
        assert_eq!(h.colors(&file("x.no.a")).normal, None);
    }

    #[test]
    fn masks_referring_to_themselves_end_with_a_message() {
        let h = Highlight::parse("[masks]\na = <b>\nb = <a>\n[group]\nmask = <a>\nnormal = red\n");
        assert_eq!(h.problems.len(), 1, "{:?}", h.problems);
        assert_eq!(h.colors(&file("x")).normal, None);
    }

    #[test]
    fn attributes_can_be_required_or_refused() {
        let h = Highlight::parse("[group]\nattr = exec,!link\nnormal = red\n");
        let mut f = Facts {
            name: "x",
            exec: true,
            ..Facts::default()
        };
        assert_eq!(h.colors(&f).normal, Some(Color::Red));
        f.is_link = true;
        assert_eq!(h.colors(&f).normal, None);
    }

    #[test]
    fn colours_accept_names_numbers_and_rgb() {
        assert_eq!(parse_color("Brown"), Some(Color::DarkYellow));
        assert_eq!(parse_color("215"), Some(Color::AnsiValue(215)));
        assert_eq!(
            parse_color("#FF8000"),
            Some(Color::Rgb {
                r: 255,
                g: 128,
                b: 0
            })
        );
        for bad in ["", "#12", "#gggggg", "#ééé", "256", "pink"] {
            assert_eq!(parse_color(bad), None, "{bad}");
        }
    }

    #[test]
    fn mistakes_are_reported_by_line_and_the_rest_still_works() {
        let h = Highlight::parse(
            "nonsense\n[oops]\nx = 1\n[group]\nmask = *.a\nnormal = pink\nweird = 1\nattr = nope\n\
             [group]\nname = Empty\n[group]\nmask = *.b\nnormal = red\nmask2\n",
        );
        for wanted in [
            "line 1", "line 2", "line 3", "line 6", "line 7", "line 8", "Empty", "line 14",
        ] {
            assert!(
                h.problems.iter().any(|p| p.contains(wanted)),
                "{wanted}: {:?}",
                h.problems
            );
        }
        assert_eq!(h.colors(&file("x.b")).normal, Some(Color::Red));
    }

    #[test]
    fn cursor_colour_falls_back_to_normal() {
        let h = Highlight::parse("[group]\nmask = *\nnormal = red\n");
        assert_eq!(h.colors(&file("x")).cursor, Some(Color::Red));
    }

    #[test]
    fn missing_file_means_defaults_and_bad_bytes_do_not_stop_it() {
        let dir = std::env::temp_dir().join(format!("far_hl_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir(&dir).unwrap();
        let missing = Highlight::load(&dir.join("none.ini"));
        assert!(missing.problems.is_empty());
        assert!(missing.colors(&file("a.zip")).normal.is_some());
        let bad = dir.join("bad.ini");
        fs::write(&bad, b"\xff\xfe[group]\n\x00").unwrap();
        assert!(!Highlight::load(&bad).problems.is_empty());
        // A folder cannot be read as a file.
        assert!(!Highlight::load(&dir).problems.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }
}
