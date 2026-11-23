//! Far's file associations: commands for Enter, Ctrl+PgDn, F3, Alt+F3, F4 and
//! Alt+F4 on files that match a mask, with Far's special symbols such as `!.!`
//! for the file name. They are kept in a text file that the F9 dialogs write
//! and that can also be edited by hand. No UI code here.

use crate::archive::ScratchDir;
use crate::highlight::MaskList;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Which key an association command is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Slot {
    Enter,
    CtrlPgDn,
    View,
    AltView,
    Edit,
    AltEdit,
}

pub const SLOTS: [Slot; 6] = [
    Slot::Enter,
    Slot::CtrlPgDn,
    Slot::View,
    Slot::AltView,
    Slot::Edit,
    Slot::AltEdit,
];

impl Slot {
    fn index(self) -> usize {
        SLOTS.iter().position(|s| *s == self).unwrap_or(0)
    }

    /// The key in the settings file.
    fn key(self) -> &'static str {
        ["enter", "ctrl_pgdn", "view", "alt_view", "edit", "alt_edit"][self.index()]
    }

    /// The label in Far's dialog, with its Alt hotkey.
    pub fn label(self) -> &'static str {
        [
            "E&xecute command (used for Enter):",
            "Exec&ute command (used for Ctrl+PgDn):",
            "&View command (used for F3):",
            "V&iew command (used for Alt+F3):",
            "&Edit command (used for F4):",
            "Edit comm&and (used for Alt+F4):",
        ][self.index()]
    }
}

/// One association.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Association {
    pub mask: String,
    pub description: String,
    pub commands: [String; 6],
}

impl Association {
    pub fn command(&self, slot: Slot) -> &str {
        &self.commands[slot.index()]
    }

    pub fn set_command(&mut self, slot: Slot, text: &str) {
        self.commands[slot.index()] = text.trim().to_string();
    }

    /// How menus show it: the description, or else the masks.
    pub fn label(&self) -> String {
        if self.description.is_empty() {
            self.mask.clone()
        } else {
            self.description.clone()
        }
    }

    /// Whether the file name `name` matches the masks.
    pub fn matches(&self, name: &str) -> bool {
        MaskList::new(&self.mask, &mut Vec::new()).matches(name)
    }
}

/// The associations in their order, which is the order of the menu Far shows
/// when several match.
#[derive(Clone, Debug, Default)]
pub struct Associations {
    pub list: Vec<Association>,
    /// Mistakes found in the settings file, to report.
    pub problems: Vec<String>,
}

/// The most a settings file may hold. Anything larger is a mistake.
const MOST_FILE_BYTES: u64 = 1024 * 1024;

const HEADER: &str = "\
# File associations for far, as Far Manager's F9, Commands, File associations.
# The dialogs there rewrite this file; it can also be edited by hand.
#
# [association] takes:
#   mask         *.md,*.markdown  with |*.old to exclude, ignoring case
#   description  shown in the menu when several associations match
#   enter, ctrl_pgdn, view, alt_view, edit, alt_edit
#                the command for Enter, Ctrl+PgDn, F3, Alt+F3, F4, Alt+F4
#
# Commands run in bash. Far's special symbols are replaced, quoted for bash:
#   !.!  file name          !   name without extension   !`  extension
#   !\\   current folder, ending in /                     !!  a single !
#   !&   selected files     !@! file listing the selected files
#   !?Title?initial!        asks for text before running
#   !#  !^  ![  !]          what follows refers to the passive, active,
#                           left or right panel
#
# For example:
# [association]
# mask = *.md
# description = Markdown
# view = glow -p !.!
";

/// Where the settings live.
pub fn settings_path() -> Option<PathBuf> {
    crate::highlight::settings_path().map(|p| p.with_file_name("associations.ini"))
}

impl Associations {
    /// Reads the settings text.
    pub fn parse(text: &str) -> Associations {
        let mut found = Associations::default();
        let mut inside = false;
        for (index, line) in text.lines().enumerate() {
            let number = index + 1;
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            if line.starts_with('[') {
                inside = line.eq_ignore_ascii_case("[association]");
                if inside {
                    found.list.push(Association::default());
                } else {
                    found
                        .problems
                        .push(format!("line {number}: unknown section {line}"));
                }
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                found
                    .problems
                    .push(format!("line {number}: expected key = value"));
                continue;
            };
            let Some(assoc) = found.list.last_mut().filter(|_| inside) else {
                found
                    .problems
                    .push(format!("line {number}: outside any [association]"));
                continue;
            };
            let (key, value) = (key.trim().to_lowercase(), value.trim());
            match key.as_str() {
                "mask" => assoc.mask = value.to_string(),
                "description" => assoc.description = value.to_string(),
                _ => match SLOTS.iter().find(|s| s.key() == key) {
                    Some(slot) => assoc.set_command(*slot, value),
                    None => found
                        .problems
                        .push(format!("line {number}: unknown key {key}")),
                },
            }
        }
        let before = found.list.len();
        found.list.retain(|a| !a.mask.trim().is_empty());
        if found.list.len() < before {
            found
                .problems
                .push("an association without a mask is ignored".to_string());
        }
        for assoc in &found.list {
            let mut problems = Vec::new();
            MaskList::new(&assoc.mask, &mut problems);
            found
                .problems
                .extend(problems.into_iter().map(|p| format!("{}: {p}", assoc.mask)));
        }
        found
    }

    /// The settings text, as the dialogs save it.
    pub fn to_text(&self) -> String {
        let mut text = HEADER.to_string();
        for assoc in &self.list {
            text.push_str("\n[association]\n");
            text.push_str(&format!("mask = {}\n", one_line(&assoc.mask)));
            if !assoc.description.is_empty() {
                text.push_str(&format!("description = {}\n", one_line(&assoc.description)));
            }
            for slot in SLOTS {
                let command = assoc.command(slot);
                if !command.is_empty() {
                    text.push_str(&format!("{} = {}\n", slot.key(), one_line(command)));
                }
            }
        }
        text
    }

    /// Reads the settings file. A missing file means no associations, and a
    /// broken one must never stop the file manager.
    pub fn load(path: &Path) -> Associations {
        match fs::metadata(path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Associations::default(),
            Ok(m) if !m.is_file() || m.len() > MOST_FILE_BYTES => {
                return Associations {
                    list: Vec::new(),
                    problems: vec![format!("{} is not a usable file", path.display())],
                }
            }
            _ => {}
        }
        match fs::read(path) {
            Ok(bytes) => Associations::parse(&String::from_utf8_lossy(&bytes)),
            Err(e) => Associations {
                list: Vec::new(),
                problems: vec![format!("{}: {e}", path.display())],
            },
        }
    }

    /// Writes the settings file through a temporary file and a rename, so a
    /// failed write leaves the old file as it was.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let temp = path.with_extension("ini.tmp");
        let written = fs::File::create(&temp)
            .and_then(|mut f| f.write_all(self.to_text().as_bytes()).and(f.sync_all()));
        match written.and_then(|()| fs::rename(&temp, path)) {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = fs::remove_file(&temp);
                Err(e)
            }
        }
    }

    /// The associations for `name` that have a command for `slot`, in order.
    pub fn matching(&self, name: &str, slot: Slot) -> Vec<&Association> {
        self.list
            .iter()
            .filter(|a| !a.command(slot).is_empty() && a.matches(name))
            .collect()
    }
}

/// A value fits on one line of the settings file.
fn one_line(text: &str) -> String {
    text.replace(['\n', '\r'], " ")
}

/// A panel as the special symbols see it.
#[derive(Clone, Debug, Default)]
pub struct PanelInfo {
    /// The folder shown.
    pub path: PathBuf,
    /// The name under the cursor, if it is a file or folder.
    pub current: Option<OsString>,
    /// The selected names, or the current one when nothing is selected.
    pub selected: Vec<OsString>,
}

/// What the special symbols refer to.
#[derive(Clone, Debug, Default)]
pub struct MetaContext {
    pub active: PanelInfo,
    pub passive: PanelInfo,
    /// The active panel is the left one.
    pub active_is_left: bool,
}

/// A command ready to run, with any list file it names. The list file is
/// removed when this is dropped, after the command has run.
pub struct Expanded {
    pub command: String,
    pub scratch: Option<ScratchDir>,
}

/// Where in the command's quoting a substitution lands.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Quote {
    None,
    Single,
    Double,
}

/// Text for bash at a point with quoting `quote`. Far adds no quotes, so the
/// user writes them where needed; here a name is also escaped, so that a name
/// such as `$(rm -rf ~)` stays a name, quoted or not.
fn escape(text: &str, quote: Quote) -> String {
    match quote {
        Quote::Single => text.replace('\'', "'\\''"),
        Quote::Double => {
            let mut out = String::with_capacity(text.len());
            for c in text.chars() {
                if matches!(c, '\\' | '$' | '`' | '"') {
                    out.push('\\');
                }
                out.push(c);
            }
            out
        }
        Quote::None => {
            let plain = |c: char| c.is_ascii_alphanumeric() || "_./+=:,@%-".contains(c);
            if text.chars().all(plain) {
                text.to_string()
            } else {
                format!("'{}'", text.replace('\'', "'\\''"))
            }
        }
    }
}

fn text_of(name: &OsStr) -> Result<&str, String> {
    name.to_str().ok_or_else(|| {
        format!(
            "{} is not valid UTF-8, so it cannot be put in a command",
            name.to_string_lossy()
        )
    })
}

/// The folder as `!\` gives it: with a trailing `/`, so `!\!.!` is a path.
fn folder_text(path: &Path) -> Result<String, String> {
    let text = text_of(path.as_os_str())?;
    Ok(if text.ends_with('/') {
        text.to_string()
    } else {
        format!("{text}/")
    })
}

/// Splits a name at its last dot, as `!` and `!`` do. A leading dot, as in
/// `.bashrc`, is part of the name.
fn split_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(at) if at > 0 => (&name[..at], &name[at + 1..]),
        _ => (name, ""),
    }
}

/// Replaces Far's special symbols in `template`. `ask` is called for each
/// `!?title?initial!` and returns None when the user cancels, which makes the
/// whole result None. An error names what could not be put in the command.
pub fn expand(
    template: &str,
    ctx: &MetaContext,
    ask: &mut dyn FnMut(&str, &str) -> Option<String>,
) -> Result<Option<Expanded>, String> {
    let chars: Vec<char> = template.chars().collect();
    let rest = |i: usize| -> String { chars[i..].iter().collect() };
    let mut out = String::new();
    let mut quote = Quote::None;
    let mut panel = &ctx.active;
    let mut scratch: Option<ScratchDir> = None;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c != '!' {
            match (quote, c) {
                (Quote::None, '\'') => quote = Quote::Single,
                (Quote::None, '"') => quote = Quote::Double,
                (Quote::Single, '\'') | (Quote::Double, '"') => quote = Quote::None,
                (Quote::None | Quote::Double, '\\') if i + 1 < chars.len() => {
                    out.push(c);
                    i += 1;
                    out.push(chars[i]);
                    i += 1;
                    continue;
                }
                _ => {}
            }
            out.push(c);
            i += 1;
            continue;
        }
        let tail = rest(i);
        let current =
            || -> Result<&str, String> { panel.current.as_deref().map_or(Ok(""), text_of) };
        let simple: &[(&str, u8)] = &[
            ("!!", b'!'),
            ("!.!", b'n'),
            ("!-!", b'n'),
            ("!+!", b'n'),
            ("!$!", b'l'),
            ("!=\\", b'r'),
            ("!=/", b'r'),
            ("!`~", b'e'),
            ("!?!", b'd'),
            ("!~", b's'),
            ("!`", b'e'),
            ("!:", b'd'),
            ("!\\", b'p'),
            ("!/", b'p'),
            ("!#", b'#'),
            ("!^", b'^'),
            ("![", b'['),
            ("!]", b']'),
        ];
        if let Some((token, kind)) = simple.iter().find(|(t, _)| tail.starts_with(t)) {
            i += token.chars().count();
            match kind {
                b'!' => out.push('!'),
                b'n' => out.push_str(&escape(current()?, quote)),
                b's' => out.push_str(&escape(split_ext(current()?).0, quote)),
                b'e' => out.push_str(&escape(split_ext(current()?).1, quote)),
                b'd' => {}
                b'p' => out.push_str(&escape(&folder_text(&panel.path)?, quote)),
                b'r' => {
                    let real = fs::canonicalize(&panel.path).unwrap_or(panel.path.clone());
                    out.push_str(&escape(&folder_text(&real)?, quote));
                }
                b'l' => {
                    let file = list_file(panel, "", &mut scratch)?;
                    out.push_str(&escape(&file, quote));
                }
                b'#' => panel = &ctx.passive,
                b'^' => panel = &ctx.active,
                b'[' => {
                    panel = if ctx.active_is_left {
                        &ctx.active
                    } else {
                        &ctx.passive
                    }
                }
                b']' => {
                    panel = if ctx.active_is_left {
                        &ctx.passive
                    } else {
                        &ctx.active
                    }
                }
                _ => {}
            }
            continue;
        }
        // !@modifiers! : a file listing the selected names.
        if let Some(body) = tail.strip_prefix("!@") {
            if let Some(end) = body.find('!') {
                let modifiers = &body[..end];
                if modifiers.chars().all(|m| "QSFAUW".contains(m)) {
                    let file = list_file(panel, modifiers, &mut scratch)?;
                    out.push_str(&escape(&file, quote));
                    i += 2 + modifiers.chars().count() + 1;
                    continue;
                }
            }
        }
        // !& and !&~, with Q or q: the selected names, each quoted.
        if tail.starts_with("!&") {
            i += 2;
            if chars.get(i) == Some(&'~') {
                i += 1;
            }
            if matches!(chars.get(i), Some('Q' | 'q')) {
                i += 1;
            }
            let mut names = Vec::new();
            for name in &panel.selected {
                names.push(escape(text_of(name)?, Quote::None));
            }
            let joined = names.join(" ");
            // Inside quotes the list is one word; bash cannot split it there.
            out.push_str(&match quote {
                Quote::None => joined,
                _ => escape(&joined, quote),
            });
            continue;
        }
        // !?title?initial! : ask the user.
        if let Some(body) = tail.strip_prefix("!?") {
            if let Some(mid) = body.find('?') {
                if let Some(end) = body[mid + 1..].find('!') {
                    let mut title = &body[..mid];
                    // !?$history$title?...! names a history; there is none here.
                    if let Some(after) = title.strip_prefix('$') {
                        if let Some(close) = after.find('$') {
                            title = &after[close + 1..];
                        }
                    }
                    let init = &body[mid + 1..mid + 1 + end];
                    let Some(answer) = ask(title, init) else {
                        return Ok(None);
                    };
                    out.push_str(&escape(&answer, quote));
                    i += 2 + body[..mid + 1 + end].chars().count() + 1;
                    continue;
                }
            }
        }
        // A lone ! is the name without its extension.
        out.push_str(&escape(split_ext(current()?).0, quote));
        i += 1;
    }
    Ok(Some(Expanded {
        command: out,
        scratch,
    }))
}

/// Writes the selected names of `panel`, one per line, to a new list file,
/// and returns its path. `F` gives full paths and `Q` quotes each name.
fn list_file(
    panel: &PanelInfo,
    modifiers: &str,
    scratch: &mut Option<ScratchDir>,
) -> Result<String, String> {
    if scratch.is_none() {
        *scratch = Some(ScratchDir::new().map_err(|e| e.to_string())?);
    }
    let dir = scratch
        .as_ref()
        .map(|s| s.path().to_path_buf())
        .unwrap_or_default();
    let count = fs::read_dir(&dir).map(|d| d.count()).unwrap_or(0);
    let file = dir.join(format!("list{count}"));
    let mut text = String::new();
    for name in &panel.selected {
        let name = text_of(name)?;
        let line = if modifiers.contains('F') {
            format!("{}{name}", folder_text(&panel.path)?)
        } else {
            name.to_string()
        };
        if modifiers.contains('Q') {
            text.push_str(&format!("\"{line}\"\n"));
        } else {
            text.push_str(&line);
            text.push('\n');
        }
    }
    fs::write(&file, text).map_err(|e| e.to_string())?;
    text_of(file.as_os_str()).map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> MetaContext {
        MetaContext {
            active: PanelInfo {
                path: PathBuf::from("/home/u/docs"),
                current: Some("report.final.md".into()),
                selected: vec!["a b.txt".into(), "c.txt".into()],
            },
            passive: PanelInfo {
                path: PathBuf::from("/srv"),
                current: Some("other.txt".into()),
                selected: vec!["other.txt".into()],
            },
            active_is_left: true,
        }
    }

    fn run(template: &str, ctx: &MetaContext) -> String {
        expand(template, ctx, &mut |_, _| Some("typed".into()))
            .unwrap()
            .unwrap()
            .command
    }

    #[test]
    fn names_paths_and_extensions_replace_their_symbols() {
        let c = ctx();
        assert_eq!(run("glow !.!", &c), "glow report.final.md");
        assert_eq!(run("echo ! !`", &c), "echo report.final md");
        assert_eq!(
            run("cp !\\!.! /tmp", &c),
            "cp /home/u/docs/report.final.md /tmp"
        );
        assert_eq!(run("echo !!", &c), "echo !");
        assert_eq!(run("echo !:!?!x", &c), "echo x");
        assert_eq!(
            run("diff !.! !#!\\!.!", &c),
            "diff report.final.md /srv/other.txt"
        );
        assert_eq!(
            run("echo !#!.! !^!.!", &c),
            "echo other.txt report.final.md"
        );
        assert_eq!(
            run("echo ![!.! !]!.!", &c),
            "echo report.final.md other.txt"
        );
        assert_eq!(
            run("echo !-! !+!", &c),
            "echo report.final.md report.final.md"
        );
        assert_eq!(split_ext(".bashrc"), (".bashrc", ""));
    }

    #[test]
    fn hostile_names_stay_names_whatever_the_quoting() {
        let mut c = ctx();
        c.active.current = Some("x'$(rm -rf ~)\"`id`.txt".into());
        // Unquoted, single quoted and double quoted symbols.
        assert_eq!(run("cat !.!", &c), "cat 'x'\\''$(rm -rf ~)\"`id`.txt'");
        assert_eq!(run("cat '!.!'", &c), "cat 'x'\\''$(rm -rf ~)\"`id`.txt'");
        assert_eq!(
            run("cat \"!.!\"", &c),
            "cat \"x'\\$(rm -rf ~)\\\"\\`id\\`.txt\""
        );
        // Check with bash itself that each prints the name unchanged.
        for template in ["printf %s !.!", "printf %s '!.!'", "printf %s \"!.!\""] {
            let command = run(template, &c);
            let out = std::process::Command::new("bash")
                .arg("-c")
                .arg(&command)
                .output()
                .unwrap();
            assert_eq!(
                String::from_utf8_lossy(&out.stdout),
                "x'$(rm -rf ~)\"`id`.txt",
                "{command}"
            );
        }
    }

    #[test]
    fn selection_list_file_and_questions() {
        let c = ctx();
        assert_eq!(run("rm !&", &c), "rm 'a b.txt' c.txt");
        assert_eq!(run("rm !&Q", &c), "rm 'a b.txt' c.txt");
        let done = expand("wc -l !@F!", &c, &mut |_, _| None).unwrap().unwrap();
        let file = done.command.strip_prefix("wc -l ").unwrap().to_string();
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            "/home/u/docs/a b.txt\n/home/u/docs/c.txt\n"
        );
        drop(done);
        assert!(
            !Path::new(&file).exists(),
            "the list file outlives the command"
        );
        let mut asked = Vec::new();
        let done = expand("grep !?$hist$Find:?TODO! !.!", &c, &mut |t, i| {
            asked.push((t.to_string(), i.to_string()));
            Some("a b".into())
        })
        .unwrap()
        .unwrap();
        assert_eq!(done.command, "grep 'a b' report.final.md");
        assert_eq!(asked, vec![("Find:".to_string(), "TODO".to_string())]);
        // Cancelling a question cancels the command.
        assert!(expand("x !?Q?!", &c, &mut |_, _| None).unwrap().is_none());
    }

    #[test]
    fn non_utf8_names_are_refused() {
        use std::os::unix::ffi::OsStringExt;
        let mut c = ctx();
        c.active.current = Some(OsString::from_vec(vec![b'a', 0xFF]));
        assert!(expand("cat !.!", &c, &mut |_, _| None).is_err());
    }

    #[test]
    fn settings_round_trip_and_report_mistakes() {
        let mut a = Association {
            mask: "*.md,*.markdown|README.md".into(),
            description: "Markdown".into(),
            ..Association::default()
        };
        a.set_command(Slot::View, "glow -p !.!");
        a.set_command(Slot::AltEdit, "code !.!");
        let all = Associations {
            list: vec![a.clone()],
            problems: Vec::new(),
        };
        let back = Associations::parse(&all.to_text());
        assert_eq!(back.list, vec![a]);
        assert!(back.problems.is_empty(), "{:?}", back.problems);
        assert_eq!(back.matching("notes.MD", Slot::View).len(), 1);
        assert!(back.matching("README.md", Slot::View).is_empty());
        assert!(back.matching("notes.md", Slot::Enter).is_empty());
        let broken =
            Associations::parse("mask = *.x\n[other]\n[association]\nwhat = 1\nview = v\n");
        assert_eq!(broken.problems.len(), 4, "{:?}", broken.problems);
        assert!(broken.list.is_empty());
    }

    #[test]
    fn saving_replaces_the_file_whole() {
        let dir = std::env::temp_dir().join(format!("far_assoc_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("associations.ini");
        let mut all = Associations::default();
        all.list.push(Association {
            mask: "*.pdf".into(),
            ..Association::default()
        });
        all.save(&path).unwrap();
        assert_eq!(Associations::load(&path).list, all.list);
        assert!(!path.with_extension("ini.tmp").exists());
        assert!(Associations::load(&dir.join("missing.ini")).list.is_empty());
        fs::remove_dir_all(dir).unwrap();
    }
}
