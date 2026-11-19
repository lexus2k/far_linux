//! Far's AutoComplete for the command line: while a command is typed, a list
//! offers earlier commands that start with it, and completions of its last
//! word from the file system, `PATH` and the environment. The list logic is
//! here, apart from the terminal, so that it can be tested.

use crate::screen::{fit_left, str_width, theme, Key, Screen, NONE, SHIFT};
use crate::shell::{escape_word, file_matches, last_word, path_commands};
use crossterm::event::KeyCode;
use std::ffi::OsString;
use std::path::Path;
use std::sync::{Arc, OnceLock};

/// Most items kept in the list.
const MAX_ITEMS: usize = 200;

/// Most rows the list shows at once. It scrolls through the rest.
pub const ROWS: usize = 10;

/// What the list is made from.
pub struct Sources<'a> {
    /// Earlier commands, newest first.
    pub history: &'a [String],
    /// The folder that file names are relative to.
    pub cwd: &'a Path,
    /// Commands on `PATH` and the builtins, sorted. Empty until they are read.
    pub commands: &'a [String],
    /// Names of environment variables.
    pub env: &'a [String],
}

/// The whole command lines the list offers for `line`: earlier commands that
/// start with it, ignoring case, newest first, then the line with its last
/// word completed. Nothing for a blank line, and never the line itself.
pub fn suggestions(line: &str, src: &Sources) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    if line.trim().is_empty() {
        return found;
    }
    let add = |item: String, found: &mut Vec<String>| {
        if item != line && !item.contains('\n') && !found.contains(&item) {
            found.push(item);
        }
    };
    let lower = line.to_lowercase();
    for old in src.history {
        if old.to_lowercase().starts_with(&lower) && relative_paths_exist(old, src.cwd) {
            add(old.clone(), &mut found);
        }
    }
    let word = last_word(line);
    if !word.value.is_empty() {
        let head: String = line.chars().take(word.start).collect();
        if let Some(name) = word.value.strip_prefix('$').filter(|n| !n.contains('/')) {
            let mut names: Vec<&String> = src.env.iter().filter(|e| e.starts_with(name)).collect();
            names.sort();
            for env in names {
                add(format!("{head}${env}"), &mut found);
            }
        } else if word.is_command && !word.value.contains('/') {
            for command in src.commands {
                if command.starts_with(&word.value) {
                    add(format!("{head}{}", escape_word(command)), &mut found);
                }
            }
        } else {
            let mut files = file_matches(&word.value, src.cwd, word.command == "cd");
            // A path where a command goes, such as `./`, is a program to run,
            // or a folder on the way to one.
            if word.is_command {
                files.retain(|f| f.ends_with('/') || is_program(&src.cwd.join(f)));
            }
            files.sort();
            for file in files {
                add(format!("{head}{}", escape_word(&file)), &mut found);
            }
        }
    }
    found.truncate(MAX_ITEMS);
    found
}

/// A file with an execute bit.
fn is_program(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Whether the words of `command` that start with `./` or `../` name files in
/// `cwd`. Such a command was run in another folder when they do not, as
/// `./build.sh` typed in one project means nothing in the next.
fn relative_paths_exist(command: &str, cwd: &Path) -> bool {
    command
        .split_whitespace()
        .filter(|w| w.starts_with("./") || w.starts_with("../"))
        .all(|w| {
            let w = w.trim_end_matches([';', '&', '|', ')']);
            cwd.join(w).symlink_metadata().is_ok()
        })
}

/// The commands on `PATH`, read once in the background: a `PATH` with Windows
/// folders under WSL takes a large part of a second to read.
#[derive(Clone, Default)]
pub struct Commands(Arc<OnceLock<Vec<String>>>);

impl Commands {
    /// Starts reading `path` on a worker thread.
    pub fn load(path: Option<OsString>) -> Commands {
        let commands = Commands::default();
        let cell = Arc::clone(&commands.0);
        std::thread::spawn(move || {
            let _ = cell.set(path_commands(path.as_deref()));
        });
        commands
    }

    /// The names, or nothing while they are still being read.
    pub fn get(&self) -> &[String] {
        self.0.get().map_or(&[], Vec::as_slice)
    }
}

/// An open list.
#[derive(Debug, PartialEq, Eq)]
pub struct List {
    pub items: Vec<String>,
    /// The highlighted item. None until Down is pressed, so that Enter still
    /// runs the command as typed.
    pub chosen: Option<usize>,
    /// The first item shown.
    pub top: usize,
}

/// What a key did to the list.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The key was for the list, which is still open.
    Moved,
    /// Put this text on the command line and close the list.
    Pick(String),
    /// Close the list.
    Close,
    /// Remove this command from the history.
    Remove(String),
    /// The key is not for the list.
    Ignored,
}

impl List {
    /// A list of `items`, or None when there are none.
    pub fn new(items: Vec<String>) -> Option<List> {
        (!items.is_empty()).then_some(List {
            items,
            chosen: None,
            top: 0,
        })
    }

    /// Keys of Far's AutoComplete list: `Down` and `Up` choose an item, `PgDn`
    /// and `PgUp` move a page, `Enter` takes the chosen item, `Esc` closes the
    /// list and `Shift+Del` removes the chosen item from the history.
    pub fn key(&mut self, key: Key) -> Outcome {
        let last = self.items.len() - 1;
        let page = ROWS.min(self.items.len());
        match (key.mods, key.code) {
            (NONE, KeyCode::Down) => {
                self.chosen = Some(self.chosen.map_or(0, |c| (c + 1).min(last)))
            }
            (NONE, KeyCode::Up) => match self.chosen {
                Some(c) => self.chosen = c.checked_sub(1),
                None => return Outcome::Ignored,
            },
            (NONE, KeyCode::PageDown) => {
                self.chosen = Some(self.chosen.map_or(0, |c| (c + page).min(last)));
            }
            (NONE, KeyCode::PageUp) => match self.chosen {
                Some(c) => self.chosen = Some(c.saturating_sub(page)),
                None => return Outcome::Ignored,
            },
            (NONE, KeyCode::Esc) => return Outcome::Close,
            (NONE | SHIFT, KeyCode::Enter) => {
                return match self.chosen {
                    Some(c) => Outcome::Pick(self.items[c].clone()),
                    None => Outcome::Ignored,
                }
            }
            (SHIFT, KeyCode::Delete) => {
                return match self.chosen {
                    Some(c) => Outcome::Remove(self.items[c].clone()),
                    None => Outcome::Ignored,
                }
            }
            _ => return Outcome::Ignored,
        }
        self.scroll_to_chosen();
        Outcome::Moved
    }

    fn scroll_to_chosen(&mut self) {
        let Some(c) = self.chosen else {
            self.top = 0;
            return;
        };
        if c < self.top {
            self.top = c;
        } else if c >= self.top + ROWS {
            self.top = c + 1 - ROWS;
        }
    }

    /// The items on screen, each with whether it is the chosen one.
    pub fn shown(&self, rows: usize) -> Vec<(&str, bool)> {
        let rows = rows.min(ROWS);
        // Keep the chosen item visible when fewer rows fit than usual.
        let top = match self.chosen {
            Some(c) if c >= self.top + rows => c + 1 - rows,
            _ => self.top,
        };
        self.items
            .iter()
            .enumerate()
            .skip(top)
            .take(rows)
            .map(|(i, item)| (item.as_str(), self.chosen == Some(i)))
            .collect()
    }

    /// Draws the list in a frame whose bottom edge is just above row `above`,
    /// from column `x` to the right edge of the screen.
    pub fn draw(&self, s: &mut Screen, x: u16, above: u16) {
        let room = above.saturating_sub(2) as usize;
        let shown = self.shown(room);
        if shown.is_empty() || s.w < x + 4 {
            return;
        }
        let w = s.w - x;
        let h = shown.len() as u16 + 2;
        let y = above - h;
        s.fill(x, y, w, h, ' ', theme::DIALOG);
        s.frame(x, y, w, h, false, theme::DIALOG);
        let inner = w.saturating_sub(2) as usize;
        for (i, (item, chosen)) in shown.iter().enumerate() {
            let style = if *chosen {
                theme::DIALOG_FOCUS
            } else {
                theme::DIALOG
            };
            let text = fit_left(item, inner);
            let fill = inner.saturating_sub(str_width(&text));
            s.put(
                x + 1,
                y + 1 + i as u16,
                &format!("{text}{}", " ".repeat(fill)),
                style,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn key(mods: crossterm::event::KeyModifiers, code: KeyCode) -> Key {
        Key { code, mods }
    }

    fn folder(tag: &str, files: &[&str]) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("far_autocomplete_{tag}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        for file in files {
            if let Some(dir) = file.strip_suffix('/') {
                fs::create_dir_all(root.join(dir)).unwrap();
            } else {
                fs::create_dir_all(&root).unwrap();
                fs::write(root.join(file), "").unwrap();
            }
        }
        root
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn history_comes_first_newest_first_ignoring_case() {
        let history = strings(&["git status", "Git log", "ls", "git status"]);
        let cwd = folder("history", &[]);
        let commands = strings(&["git", "gitk"]);
        let src = Sources {
            history: &history,
            cwd: &cwd,
            commands: &commands,
            env: &[],
        };
        assert_eq!(
            suggestions("git", &src),
            strings(&["git status", "Git log", "gitk"])
        );
        assert!(suggestions("", &src).is_empty());
        assert!(suggestions("   ", &src).is_empty());
        fs::remove_dir_all(cwd).unwrap();
    }

    #[test]
    fn the_last_word_completes_from_files_commands_and_variables() {
        let cwd = folder("words", &["notes.txt", "new file", "src/", ".hidden"]);
        let commands = strings(&["nano", "nl", "vim"]);
        let env = strings(&["HOME", "HOSTNAME", "PATH"]);
        let src = Sources {
            history: &[],
            cwd: &cwd,
            commands: &commands,
            env: &env,
        };
        // The first word is a command.
        assert_eq!(suggestions("n", &src), strings(&["nano", "nl"]));
        // Later words are files, escaped for bash; hidden ones need the dot.
        assert_eq!(
            suggestions("vim n", &src),
            strings(&["vim new\\ file", "vim notes.txt"])
        );
        assert_eq!(suggestions("ls .h", &src), strings(&["ls .hidden"]));
        // cd offers only folders.
        assert_eq!(suggestions("cd s", &src), strings(&["cd src/"]));
        assert!(suggestions("cd n", &src).is_empty());
        // A word starting with $ is a variable.
        assert_eq!(
            suggestions("echo $HO", &src),
            strings(&["echo $HOME", "echo $HOSTNAME"])
        );
        // A line that ends in a space has no word to complete.
        assert!(suggestions("vim ", &src).is_empty());
        fs::remove_dir_all(cwd).unwrap();
    }

    #[test]
    fn the_typed_line_and_duplicates_are_left_out() {
        let history = strings(&["make", "make test", "make"]);
        let cwd = folder("dups", &[]);
        let commands = strings(&["make"]);
        let src = Sources {
            history: &history,
            cwd: &cwd,
            commands: &commands,
            env: &[],
        };
        assert_eq!(suggestions("make", &src), strings(&["make test"]));
        assert_eq!(suggestions("ma", &src), strings(&["make", "make test"]));
        fs::remove_dir_all(cwd).unwrap();
    }

    #[test]
    fn history_with_paths_from_other_folders_is_left_out() {
        let cwd = folder("relative", &["build.sh", "notes.md", "src/"]);
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(cwd.join("build.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        let history = strings(&[
            "./build.sh --release",
            "./deploy.sh",
            "cat ./src/../build.sh",
            "cat ../elsewhere/x",
            "./build.sh; ./missing",
        ]);
        let src = Sources {
            history: &history,
            cwd: &cwd,
            commands: &[],
            env: &[],
        };
        assert_eq!(
            suggestions("./", &src),
            strings(&["./build.sh --release", "./build.sh", "./src/"])
        );
        assert_eq!(
            suggestions("cat ", &src),
            strings(&["cat ./src/../build.sh"])
        );
        // After a command, a path offers every file.
        assert_eq!(suggestions("cat ./n", &src), strings(&["cat ./notes.md"]));
        fs::remove_dir_all(cwd).unwrap();
    }

    #[test]
    fn keys_choose_take_close_and_remove() {
        let mut list = List::new(strings(&["a", "b", "c"])).unwrap();
        // Enter and Up mean the command line until an item is chosen.
        assert_eq!(list.key(key(NONE, KeyCode::Enter)), Outcome::Ignored);
        assert_eq!(list.key(key(NONE, KeyCode::Up)), Outcome::Ignored);
        assert_eq!(list.key(key(NONE, KeyCode::Down)), Outcome::Moved);
        assert_eq!(list.chosen, Some(0));
        list.key(key(NONE, KeyCode::Down));
        list.key(key(NONE, KeyCode::Down));
        list.key(key(NONE, KeyCode::Down));
        assert_eq!(list.chosen, Some(2));
        assert_eq!(
            list.key(key(SHIFT, KeyCode::Delete)),
            Outcome::Remove("c".into())
        );
        list.key(key(NONE, KeyCode::Up));
        assert_eq!(
            list.key(key(NONE, KeyCode::Enter)),
            Outcome::Pick("b".into())
        );
        // Up from the first item goes back to the command line.
        list.key(key(NONE, KeyCode::Up));
        list.key(key(NONE, KeyCode::Up));
        assert_eq!(list.chosen, None);
        assert_eq!(list.key(key(NONE, KeyCode::Esc)), Outcome::Close);
        assert_eq!(list.key(key(NONE, KeyCode::Char('x'))), Outcome::Ignored);
        assert!(List::new(Vec::new()).is_none());
    }

    #[test]
    fn long_lists_scroll_to_the_chosen_item() {
        let items: Vec<String> = (0..30).map(|i| format!("item{i}")).collect();
        let mut list = List::new(items).unwrap();
        assert_eq!(list.shown(50).len(), ROWS);
        for _ in 0..12 {
            list.key(key(NONE, KeyCode::Down));
        }
        let shown = list.shown(50);
        assert_eq!(shown.first().unwrap().0, "item2");
        assert_eq!(shown.last().unwrap(), &("item11", true));
        list.key(key(NONE, KeyCode::PageDown));
        assert_eq!(list.chosen, Some(21));
        list.key(key(NONE, KeyCode::PageDown));
        list.key(key(NONE, KeyCode::PageDown));
        assert_eq!(list.chosen, Some(29));
        // With room for 3 rows the chosen item still shows.
        assert_eq!(list.shown(3).last().unwrap(), &("item29", true));
        list.key(key(NONE, KeyCode::PageUp));
        assert_eq!(list.chosen, Some(19));
    }

    #[test]
    fn the_list_is_drawn_above_the_command_line_and_fits_any_size() {
        let list = List::new(strings(&["git status", "git log"])).unwrap();
        let mut s = Screen::new(40, 10);
        list.draw(&mut s, 5, 8);
        let row = |y: u16| s.row_text(y);
        assert!(row(6).contains("git log"), "{}", row(6));
        assert!(row(5).contains("git status"));
        for (w, h) in [(0, 0), (1, 1), (5, 2), (8, 3), (40, 3)] {
            let mut s = Screen::new(w, h);
            list.draw(&mut s, 3, h.saturating_sub(1));
        }
    }
}
