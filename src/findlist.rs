//! The Alt+F7 results window, laid out as Far's "Find file" dialog: found
//! files grouped under their folder, a status line, and the buttons New
//! search, Go to, View, Stop and Cancel. The search runs on another thread and
//! the list fills while it runs. Only `draw` touches the screen, so the rest is
//! unit tested.

use crate::screen::{str_width, theme, Key, Screen, ALT, NONE, SHIFT};
use crate::util;
use crossterm::event::KeyCode;

/// One found file or folder.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Found {
    /// The folder as shown in the header line above it.
    pub folder: String,
    pub name: String,
    pub size: u64,
    pub mtime: i64,
    pub mode: u32,
    pub is_dir: bool,
    pub is_link: bool,
}

/// What the user asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    None,
    NewSearch,
    GoTo(usize),
    View(usize),
    Edit(usize),
    Stop,
    Cancel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Button {
    NewSearch,
    GoTo,
    View,
    Stop,
    Cancel,
}

impl Button {
    fn label(self) -> &'static str {
        match self {
            Button::NewSearch => "&New search",
            Button::GoTo => "&Go to",
            Button::View => "&View",
            Button::Stop => "&Stop",
            Button::Cancel => "&Cancel",
        }
    }
}

/// A line of the list: a folder header, or a found item by index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Row {
    Folder(usize),
    Item(usize),
}

/// Where the keyboard focus is: the list, or a button by index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Focus {
    List,
    Button(usize),
}

pub struct FindList {
    pub items: Vec<Found>,
    /// Rows, rebuilt as items arrive. A header comes before each run of items
    /// in one folder, as the search can come back to a folder after a subfolder.
    rows: Vec<Row>,
    /// Index into `rows`, always an item row while there are items.
    cursor: usize,
    top: usize,
    focus: Focus,
    /// Set when the search has finished or was stopped.
    pub done: bool,
    /// Height of the list on the last draw, for paging.
    page: usize,
}

impl Default for FindList {
    fn default() -> Self {
        Self::new()
    }
}

impl FindList {
    pub fn new() -> Self {
        FindList {
            items: Vec::new(),
            rows: Vec::new(),
            cursor: 0,
            top: 0,
            focus: Focus::List,
            done: false,
            page: 10,
        }
    }

    pub fn push(&mut self, found: Found) {
        let index = self.items.len();
        let new_folder = self
            .items
            .last()
            .is_none_or(|last| last.folder != found.folder);
        self.items.push(found);
        if new_folder {
            self.rows.push(Row::Folder(index));
        }
        self.rows.push(Row::Item(index));
        if index == 0 {
            self.cursor = self.rows.len() - 1;
        }
    }

    /// Counts of files and folders found, as on Far's status line.
    pub fn counts(&self) -> (usize, usize) {
        let folders = self.items.iter().filter(|f| f.is_dir).count();
        (self.items.len() - folders, folders)
    }

    /// The item under the cursor.
    pub fn current(&self) -> Option<usize> {
        match self.rows.get(self.cursor) {
            Some(Row::Item(i)) => Some(*i),
            _ => None,
        }
    }

    fn buttons(&self) -> Vec<Button> {
        let mut buttons = vec![Button::NewSearch, Button::GoTo, Button::View];
        if !self.done {
            buttons.push(Button::Stop);
        }
        buttons.push(Button::Cancel);
        buttons
    }

    /// Marks the search finished. The Stop button goes, so a focused button
    /// past it moves back by one.
    pub fn finish(&mut self) {
        if self.done {
            return;
        }
        self.done = true;
        if let Focus::Button(b) = self.focus {
            let count = self.buttons().len();
            self.focus = Focus::Button(if b >= 3 { b.saturating_sub(1) } else { b }.min(count - 1));
        }
    }

    /// Moves the cursor by `delta` item rows, skipping folder headers.
    fn move_by(&mut self, delta: isize) {
        let items: Vec<usize> = (0..self.rows.len())
            .filter(|&r| matches!(self.rows[r], Row::Item(_)))
            .collect();
        if items.is_empty() {
            return;
        }
        let at = items.iter().position(|&r| r >= self.cursor).unwrap_or(0);
        let to = (at as isize + delta).clamp(0, items.len() as isize - 1) as usize;
        self.cursor = items[to];
    }

    fn command_for(&self, button: Button) -> Command {
        let item = |make: fn(usize) -> Command| self.current().map_or(Command::None, make);
        match button {
            Button::NewSearch => Command::NewSearch,
            Button::GoTo => item(Command::GoTo),
            Button::View => item(Command::View),
            Button::Stop => Command::Stop,
            Button::Cancel => Command::Cancel,
        }
    }

    pub fn handle(&mut self, key: Key) -> Command {
        let buttons = self.buttons();
        match (key.code, key.mods) {
            (KeyCode::Esc, _) | (KeyCode::F(10), _) => return Command::Cancel,
            (KeyCode::F(3), _) => return self.current().map_or(Command::None, Command::View),
            (KeyCode::F(4), _) => return self.current().map_or(Command::None, Command::Edit),
            (KeyCode::Char(c), ALT) => {
                let c = c.to_ascii_lowercase();
                if let Some(b) = buttons.iter().find(|b| hotkey(b.label()) == Some(c)) {
                    return self.command_for(*b);
                }
            }
            (KeyCode::Tab, NONE) => {
                self.focus = match self.focus {
                    Focus::List => Focus::Button(0),
                    Focus::Button(b) if b + 1 < buttons.len() => Focus::Button(b + 1),
                    Focus::Button(_) => Focus::List,
                }
            }
            (KeyCode::BackTab, _) | (KeyCode::Tab, SHIFT) => {
                self.focus = match self.focus {
                    Focus::List => Focus::Button(buttons.len() - 1),
                    Focus::Button(0) => Focus::List,
                    Focus::Button(b) => Focus::Button(b - 1),
                }
            }
            (KeyCode::Enter, _) | (KeyCode::Char(' '), NONE) => {
                return match self.focus {
                    // Enter on a file goes to it, as in Far.
                    Focus::List if key.code == KeyCode::Enter => {
                        self.current().map_or(Command::None, Command::GoTo)
                    }
                    Focus::List => Command::None,
                    Focus::Button(b) => self.command_for(buttons[b]),
                };
            }
            (KeyCode::Left, _) => {
                if let Focus::Button(b) = self.focus {
                    self.focus = Focus::Button(b.saturating_sub(1));
                }
            }
            (KeyCode::Right, _) => {
                if let Focus::Button(b) = self.focus {
                    self.focus = Focus::Button((b + 1).min(buttons.len() - 1));
                }
            }
            (KeyCode::Up, _) => self.list_key(-1),
            (KeyCode::Down, _) => self.list_key(1),
            (KeyCode::PageUp, _) => self.list_key(-(self.page.max(1) as isize)),
            (KeyCode::PageDown, _) => self.list_key(self.page.max(1) as isize),
            (KeyCode::Home, _) => self.list_key(isize::MIN / 2),
            (KeyCode::End, _) => self.list_key(isize::MAX / 2),
            _ => {}
        }
        Command::None
    }

    fn list_key(&mut self, delta: isize) {
        self.focus = Focus::List;
        self.move_by(delta);
    }

    /// Draws the window over the whole screen but a margin, as Far does.
    pub fn draw(&mut self, s: &mut Screen, title: &str, status: &str) {
        let x = 4.min(s.w / 10);
        let y = 1.min(s.h / 10);
        let w = s.w.saturating_sub(2 * x).max(20);
        let h = s.h.saturating_sub(2 * y).max(8);
        s.fill(x, y, w, h, ' ', theme::DIALOG);
        s.frame(x, y, w, h, true, theme::DIALOG);
        s.title(x, y, w, title, theme::DIALOG);
        let inner = w.saturating_sub(4) as usize;
        let list_top = y + 1;
        // Frame, list, separator, status, separator, buttons, frame.
        let list_h = h.saturating_sub(6) as usize;
        self.page = list_h.saturating_sub(1).max(1);
        if self.cursor < self.top {
            // Keep the folder header above the first item of a run in view.
            self.top = self.cursor.saturating_sub(1);
            if !matches!(self.rows.get(self.top), Some(Row::Folder(_))) {
                self.top = self.cursor;
            }
        } else if list_h > 0 && self.cursor >= self.top + list_h {
            self.top = self.cursor + 1 - list_h;
        }
        for line in 0..list_h {
            let row_y = list_top + line as u16;
            let Some(row) = self.rows.get(self.top + line) else {
                break;
            };
            match *row {
                Row::Folder(i) => {
                    let text = format!(" {}", self.items[i].folder);
                    s.put(x + 2, row_y, &fit_left(&text, inner), theme::DIALOG);
                }
                Row::Item(i) => {
                    let here = self.top + line == self.cursor;
                    let style = if here && self.focus == Focus::List {
                        theme::DIALOG_FOCUS
                    } else {
                        theme::DIALOG
                    };
                    let text = format!("  {}", item_line(&self.items[i]));
                    let shown = pad(&fit_left(&text, inner), inner);
                    s.put(x + 2, row_y, &shown, style);
                }
            }
        }
        let sep = list_top + list_h as u16;
        s.put(
            x,
            sep,
            &format!("╠{}╣", "═".repeat(w.saturating_sub(2) as usize)),
            theme::DIALOG,
        );
        s.put(x + 2, sep + 1, &fit_left(status, inner), theme::DIALOG);
        s.put(
            x,
            sep + 2,
            &format!("╟{}╢", "─".repeat(w.saturating_sub(2) as usize)),
            theme::DIALOG,
        );
        let buttons = self.buttons();
        let labels: Vec<String> = buttons
            .iter()
            .map(|b| {
                let text = b.label().replace('&', "");
                // The default button has braces, as in Far.
                if *b == Button::NewSearch {
                    format!("{{ {text} }}")
                } else {
                    format!("[ {text} ]")
                }
            })
            .collect();
        let total: usize = labels.iter().map(|l| str_width(l) + 1).sum();
        let mut bx = x + w.saturating_sub(total as u16) / 2;
        for (i, label) in labels.iter().enumerate() {
            let style = if self.focus == Focus::Button(i) {
                theme::DIALOG_FOCUS
            } else {
                theme::DIALOG
            };
            bx = s.put(bx, sep + 3, label, style) + 1;
        }
        s.cursor = None;
    }
}

/// `date│  size│mode      │name`, Far's columns with the Unix mode in place
/// of the Windows attributes.
pub fn item_line(f: &Found) -> String {
    let date = if f.mtime == 0 {
        " ".repeat(8)
    } else {
        util::format_date(f.mtime)
    };
    let size = if f.is_dir {
        "Folder".to_string()
    } else {
        util::fit_size(f.size, 6)
    };
    let mode = util::mode_string(f.mode, f.is_dir, f.is_link);
    format!("{date}│{size:>6}│{mode}│{}", f.name)
}

fn hotkey(label: &str) -> Option<char> {
    let at = label.find('&')?;
    label[at + 1..]
        .chars()
        .next()
        .map(|c| c.to_ascii_lowercase())
}

fn fit_left(text: &str, width: usize) -> String {
    crate::screen::fit_left(text, width)
}

fn pad(text: &str, width: usize) -> String {
    let fill = width.saturating_sub(str_width(text));
    format!("{text}{}", " ".repeat(fill))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key(code: KeyCode) -> Key {
        Key { code, mods: NONE }
    }

    fn found(folder: &str, name: &str) -> Found {
        Found {
            folder: folder.to_string(),
            name: name.to_string(),
            size: 3000 * 1024,
            mode: 0o644,
            ..Default::default()
        }
    }

    #[test]
    fn items_are_grouped_under_their_folder() {
        let mut list = FindList::new();
        list.push(found("/a", "one"));
        list.push(found("/a", "two"));
        list.push(found("/a/sub", "three"));
        list.push(found("/a", "four"));
        assert_eq!(
            list.rows,
            [
                Row::Folder(0),
                Row::Item(0),
                Row::Item(1),
                Row::Folder(2),
                Row::Item(2),
                Row::Folder(3),
                Row::Item(3)
            ]
        );
        assert_eq!(list.current(), Some(0));
    }

    #[test]
    fn the_cursor_skips_folder_lines() {
        let mut list = FindList::new();
        list.push(found("/a", "one"));
        list.push(found("/b", "two"));
        list.handle(key(KeyCode::Down));
        assert_eq!(list.current(), Some(1));
        list.handle(key(KeyCode::Down));
        assert_eq!(list.current(), Some(1));
        list.handle(key(KeyCode::Home));
        assert_eq!(list.current(), Some(0));
        list.handle(key(KeyCode::End));
        assert_eq!(list.current(), Some(1));
        list.handle(key(KeyCode::Up));
        assert_eq!(list.current(), Some(0));
    }

    #[test]
    fn keys_and_buttons_give_commands() {
        let mut list = FindList::new();
        assert_eq!(list.handle(key(KeyCode::Enter)), Command::None);
        list.push(found("/a", "one"));
        assert_eq!(list.handle(key(KeyCode::Enter)), Command::GoTo(0));
        assert_eq!(list.handle(key(KeyCode::F(3))), Command::View(0));
        assert_eq!(list.handle(key(KeyCode::F(4))), Command::Edit(0));
        let alt = |c| Key {
            code: KeyCode::Char(c),
            mods: ALT,
        };
        assert_eq!(list.handle(alt('n')), Command::NewSearch);
        assert_eq!(list.handle(alt('s')), Command::Stop);
        list.handle(key(KeyCode::Tab));
        list.handle(key(KeyCode::Right));
        assert_eq!(list.handle(key(KeyCode::Enter)), Command::GoTo(0));
        // Stop goes when the search ends, and Cancel takes its place.
        list.handle(key(KeyCode::Right));
        list.handle(key(KeyCode::Right));
        list.handle(key(KeyCode::Right));
        assert_eq!(list.focus, Focus::Button(4));
        list.finish();
        assert_eq!(list.handle(alt('s')), Command::None);
        assert_eq!(list.handle(key(KeyCode::Enter)), Command::Cancel);
        assert_eq!(list.handle(key(KeyCode::Esc)), Command::Cancel);
    }

    #[test]
    fn a_line_shows_far_columns() {
        let mut f = found("/a", "adb_camera-app.log");
        f.mtime = 1_790_000_000;
        let line = item_line(&f);
        assert!(
            line.ends_with("│3000 K│-rw-r--r--│adb_camera-app.log"),
            "{line}"
        );
        f.is_dir = true;
        assert!(item_line(&f).contains("│Folder│d"), "{}", item_line(&f));
    }

    #[test]
    fn draw_shows_folders_status_and_buttons() {
        let mut list = FindList::new();
        list.push(found("~/logs/x.tar:/", "adb_camera-app.log"));
        let mut s = Screen::new(100, 20);
        list.draw(&mut s, "Find file: *", "Searching...");
        let text: String = (0..s.h)
            .map(|y| (0..s.w).map(|x| s.char_at(x, y)).collect::<String>() + "\n")
            .collect();
        assert!(text.contains("Find file: *"), "{text}");
        assert!(text.contains("~/logs/x.tar:/"), "{text}");
        assert!(text.contains("adb_camera-app.log"), "{text}");
        assert!(
            text.contains("{ New search } [ Go to ] [ View ] [ Stop ] [ Cancel ]"),
            "{text}"
        );
        list.finish();
        list.draw(
            &mut s,
            "Find file: *",
            "Search done. Found files: 1, folders: 0",
        );
        let text: String = (0..s.h)
            .map(|y| (0..s.w).map(|x| s.char_at(x, y)).collect::<String>() + "\n")
            .collect();
        assert!(
            text.contains("{ New search } [ Go to ] [ View ] [ Cancel ]"),
            "{text}"
        );
        assert!(
            text.contains("Search done. Found files: 1, folders: 0"),
            "{text}"
        );
    }
}
