//! A file panel: directory contents, sorting, selection and drawing.

use crate::screen::{fit, fit_left, fit_name, str_width, theme, Screen, Style};
use crate::util;
use std::ffi::OsString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// One line of a panel.
#[derive(Clone, Debug)]
pub struct Entry {
    pub name: String,
    pub raw: OsString,
    pub is_dir: bool,
    pub is_link: bool,
    pub is_up: bool,
    pub size: u64,
    pub mtime: i64,
    pub ctime: i64,
    pub atime: i64,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub exec: bool,
    pub selected: bool,
}

/// Panel sort orders, on Ctrl+F3 to Ctrl+F11 as in Far.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sort {
    Name,
    Ext,
    Mtime,
    Size,
    Unsorted,
    Ctime,
    Atime,
    Owner,
}

/// Column layouts, on Ctrl+1 to Ctrl+4 as in Far.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    Brief,
    Medium,
    Full,
    Wide,
}

/// A directory panel.
pub struct Panel {
    pub path: PathBuf,
    pub entries: Vec<Entry>,
    pub cursor: usize,
    pub top: usize,
    pub sort: Sort,
    pub reverse: bool,
    pub show_hidden: bool,
    pub view: View,
    pub visible: bool,
    pub free_bytes: Option<u64>,
    pub error: Option<String>,
    dir_mtime: i64,
    /// Rows and columns from the last draw, used for page movement.
    pub rows: usize,
    pub columns: usize,
}

fn entry_from(path: &Path, raw: OsString) -> Option<Entry> {
    let meta = fs::symlink_metadata(path).ok()?;
    let is_link = meta.file_type().is_symlink();
    // A link to a folder acts as a folder; a broken link acts as a file.
    let target = if is_link {
        fs::metadata(path).ok()
    } else {
        None
    };
    let shown = target.as_ref().unwrap_or(&meta);
    Some(Entry {
        name: raw.to_string_lossy().into_owned(),
        raw,
        is_dir: shown.is_dir(),
        is_link,
        is_up: false,
        size: shown.len(),
        mtime: meta.mtime(),
        ctime: meta.ctime(),
        atime: meta.atime(),
        mode: meta.mode(),
        uid: meta.uid(),
        gid: meta.gid(),
        exec: !shown.is_dir() && shown.is_file() && shown.mode() & 0o111 != 0,
        selected: false,
    })
}

fn extension(name: &str) -> &str {
    match name.rfind('.') {
        Some(0) | None => "",
        Some(i) => &name[i + 1..],
    }
}

fn free_space(path: &Path) -> Option<u64> {
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: statvfs fills the zeroed struct and reads a valid C string.
    unsafe {
        let mut s: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c.as_ptr(), &mut s) == 0 {
            Some(s.f_bavail as u64 * s.f_frsize as u64)
        } else {
            None
        }
    }
}

impl Panel {
    /// Creates a panel showing `path`.
    pub fn new(path: PathBuf) -> Self {
        let mut panel = Panel {
            path,
            entries: Vec::new(),
            cursor: 0,
            top: 0,
            sort: Sort::Name,
            reverse: false,
            show_hidden: true,
            view: View::Medium,
            visible: true,
            free_bytes: None,
            error: None,
            dir_mtime: 0,
            rows: 20,
            columns: 2,
        };
        panel.reload();
        panel
    }

    /// Rereads the directory, keeping the cursor and selection by name.
    pub fn reload(&mut self) {
        let keep = self.current().map(|e| e.raw.clone());
        let selected: Vec<OsString> = self
            .entries
            .iter()
            .filter(|e| e.selected)
            .map(|e| e.raw.clone())
            .collect();
        let index = self.cursor;
        self.read_dir();
        for entry in &mut self.entries {
            entry.selected = selected.contains(&entry.raw);
        }
        match keep.and_then(|raw| self.entries.iter().position(|e| e.raw == raw)) {
            Some(i) => self.cursor = i,
            None => self.cursor = index.min(self.entries.len().saturating_sub(1)),
        }
    }

    fn read_dir(&mut self) {
        self.entries.clear();
        self.error = None;
        if self.path.parent().is_some() {
            self.entries.push(Entry {
                name: "..".into(),
                raw: "..".into(),
                is_dir: true,
                is_link: false,
                is_up: true,
                size: 0,
                mtime: 0,
                ctime: 0,
                atime: 0,
                mode: 0,
                uid: 0,
                gid: 0,
                exec: false,
                selected: false,
            });
        }
        self.dir_mtime = fs::metadata(&self.path).map(|m| m.mtime()).unwrap_or(0);
        self.free_bytes = free_space(&self.path);
        match fs::read_dir(&self.path) {
            Ok(items) => {
                for item in items.flatten() {
                    let raw = item.file_name();
                    if !self.show_hidden && raw.as_bytes().first() == Some(&b'.') {
                        continue;
                    }
                    if let Some(entry) = entry_from(&item.path(), raw) {
                        self.entries.push(entry);
                    }
                }
            }
            Err(e) => self.error = Some(e.to_string()),
        }
        self.apply_sort();
    }

    /// True when the directory changed on disk since it was read.
    pub fn changed_on_disk(&self) -> bool {
        fs::metadata(&self.path)
            .map(|m| m.mtime() != self.dir_mtime)
            .unwrap_or(false)
    }

    /// Sorts with `..` first, then folders, then files.
    pub fn apply_sort(&mut self) {
        let keep = self.current().map(|e| e.raw.clone());
        let (sort, reverse) = (self.sort, self.reverse);
        if sort != Sort::Unsorted {
            self.entries.sort_by(|a, b| {
                let rank = |e: &Entry| (!e.is_up, !e.is_dir);
                rank(a).cmp(&rank(b)).then_with(|| {
                    let by_name = || {
                        a.name
                            .to_lowercase()
                            .cmp(&b.name.to_lowercase())
                            .then_with(|| a.raw.cmp(&b.raw))
                    };
                    let order = match sort {
                        Sort::Name | Sort::Unsorted => by_name(),
                        Sort::Ext => extension(&a.name)
                            .to_lowercase()
                            .cmp(&extension(&b.name).to_lowercase())
                            .then_with(by_name),
                        // Newest and largest first, as in Far.
                        Sort::Mtime => b.mtime.cmp(&a.mtime).then_with(by_name),
                        Sort::Size => b.size.cmp(&a.size).then_with(by_name),
                        Sort::Ctime => b.ctime.cmp(&a.ctime).then_with(by_name),
                        Sort::Atime => b.atime.cmp(&a.atime).then_with(by_name),
                        Sort::Owner => a.uid.cmp(&b.uid).then_with(by_name),
                    };
                    if reverse {
                        order.reverse()
                    } else {
                        order
                    }
                })
            });
        }
        if let Some(raw) = keep {
            if let Some(i) = self.entries.iter().position(|e| e.raw == raw) {
                self.cursor = i;
            }
        }
    }

    /// Selects a sort order. Choosing the current one reverses it.
    pub fn set_sort(&mut self, sort: Sort) {
        if self.sort == sort {
            self.reverse = !self.reverse;
        } else {
            self.sort = sort;
            self.reverse = false;
        }
        if sort == Sort::Unsorted {
            self.reload();
        } else {
            self.apply_sort();
        }
    }

    pub fn current(&self) -> Option<&Entry> {
        self.entries.get(self.cursor)
    }

    /// Full path of an entry.
    pub fn entry_path(&self, entry: &Entry) -> PathBuf {
        self.path.join(&entry.raw)
    }

    /// Full path under the cursor, except for `..`.
    pub fn current_path(&self) -> Option<PathBuf> {
        self.current()
            .filter(|e| !e.is_up)
            .map(|e| self.entry_path(e))
    }

    /// The selection, or the entry under the cursor if nothing is selected.
    pub fn targets(&self) -> Vec<PathBuf> {
        let selected: Vec<PathBuf> = self
            .entries
            .iter()
            .filter(|e| e.selected)
            .map(|e| self.entry_path(e))
            .collect();
        if selected.is_empty() {
            self.current_path().into_iter().collect()
        } else {
            selected
        }
    }

    /// Changes directory, placing the cursor on `focus` if given.
    pub fn chdir(&mut self, path: &Path, focus: Option<&OsString>) -> Result<(), String> {
        if let Err(e) = fs::read_dir(path) {
            return Err(format!("{}: {e}", path.display()));
        }
        self.path = path.to_path_buf();
        self.read_dir();
        self.top = 0;
        self.cursor = focus
            .and_then(|raw| self.entries.iter().position(|e| &e.raw == raw))
            .unwrap_or(0);
        Ok(())
    }

    /// Goes to the parent directory with the cursor on the folder just left.
    pub fn go_up(&mut self) -> Result<(), String> {
        let Some(parent) = self.path.parent().map(Path::to_path_buf) else {
            return Ok(());
        };
        let name = self.path.file_name().map(|n| n.to_os_string());
        self.chdir(&parent, name.as_ref())
    }

    /// Moves the cursor to a name, if present.
    pub fn goto_name(&mut self, raw: &OsString) {
        if let Some(i) = self.entries.iter().position(|e| &e.raw == raw) {
            self.cursor = i;
        }
    }

    /// Moves the cursor by a signed amount, clamping at the ends.
    pub fn move_by(&mut self, delta: isize) {
        let last = self.entries.len().saturating_sub(1) as isize;
        self.cursor = (self.cursor as isize + delta).clamp(0, last.max(0)) as usize;
    }

    /// Number of entries visible at once.
    pub fn capacity(&self) -> usize {
        (self.rows * self.columns).max(1)
    }

    /// Toggles selection of the entry under the cursor. `..` cannot be selected.
    pub fn toggle_current(&mut self) {
        if let Some(entry) = self.entries.get_mut(self.cursor) {
            if !entry.is_up {
                entry.selected = !entry.selected;
            }
        }
    }

    /// Moves by `delta`, applying one selection state to all entries crossed,
    /// which is how Shift+movement behaves.
    pub fn select_move(&mut self, delta: isize) {
        let state = self.current().is_some_and(|e| !e.selected);
        let last = self.entries.len().saturating_sub(1) as isize;
        let end = (self.cursor as isize + delta).clamp(0, last.max(0));
        let (mut i, step) = (self.cursor as isize, delta.signum());
        while i != end {
            let entry = &mut self.entries[i as usize];
            if !entry.is_up {
                entry.selected = state;
            }
            i += step;
        }
        self.cursor = end as usize;
        // Reaching either end also marks the last entry.
        if end == 0 || end == last {
            let entry = &mut self.entries[end as usize];
            if !entry.is_up {
                entry.selected = state;
            }
        }
    }

    /// Sets the selection of files matching a mask. Folders are left out,
    /// which is Far's default.
    pub fn select_mask(&mut self, mask: &str, state: bool) {
        for entry in &mut self.entries {
            if !entry.is_up && !entry.is_dir && util::wildcard_list(mask, &entry.name) {
                entry.selected = state;
            }
        }
    }

    /// Inverts the selection of files.
    pub fn invert_selection(&mut self) {
        for entry in &mut self.entries {
            if !entry.is_up && !entry.is_dir {
                entry.selected = !entry.selected;
            }
        }
    }

    pub fn clear_selection(&mut self) {
        for entry in &mut self.entries {
            entry.selected = false;
        }
    }

    /// Finds the next name starting with `prefix`, beginning at `from`.
    pub fn find_prefix(&self, prefix: &str, from: usize) -> Option<usize> {
        let prefix = prefix.to_lowercase();
        let n = self.entries.len();
        (0..n).map(|i| (from + i) % n).find(|i| {
            !self.entries[*i].is_up && self.entries[*i].name.to_lowercase().starts_with(&prefix)
        })
    }

    fn name_style(entry: &Entry, on_cursor: bool) -> Style {
        match (on_cursor, entry.selected) {
            (true, true) => theme::PANEL_CURSOR_SELECTED,
            (true, false) => theme::PANEL_CURSOR,
            (false, true) => theme::PANEL_SELECTED,
            (false, false) => {
                if entry.is_dir {
                    theme::PANEL_DIR
                } else if entry.is_link {
                    theme::PANEL_LINK
                } else if entry.exec {
                    theme::PANEL_EXEC
                } else {
                    theme::PANEL
                }
            }
        }
    }

    fn size_text(entry: &Entry, width: usize) -> String {
        let text = if entry.is_up {
            "Up".to_string()
        } else if entry.is_dir {
            if entry.is_link { "SymLink" } else { "Folder" }.to_string()
        } else {
            util::fit_size(entry.size, width)
        };
        format!("{text:>width$}")
    }

    /// Draws the panel into the given rectangle.
    pub fn draw(&mut self, s: &mut Screen, x: u16, y: u16, w: u16, h: u16, active: bool) {
        if w < 12 || h < 7 {
            return;
        }
        s.frame(x, y, w, h, true, theme::PANEL);
        let inner = w as usize - 2;
        let rows = h as usize - 5;
        let footer = y + h - 3;
        let columns = match self.view {
            View::Brief => 3,
            View::Medium => 2,
            View::Full | View::Wide => 1,
        };
        self.rows = rows.max(1);
        self.columns = columns;
        let cap = self.capacity();
        if self.cursor < self.top {
            self.top = self.cursor;
        }
        if self.cursor >= self.top + cap {
            self.top = self.cursor + 1 - cap;
        }
        if self.top + cap > self.entries.len() {
            self.top = self.entries.len().saturating_sub(cap);
        }

        // Column boundaries.
        let mut edges = Vec::new();
        match self.view {
            View::Brief | View::Medium => {
                let dividers = inner.saturating_sub(columns - 1);
                let mut at = 0;
                for c in 0..columns {
                    let width = dividers / columns + usize::from(c < dividers % columns);
                    edges.push((at, width));
                    at += width + 1;
                }
            }
            View::Full | View::Wide => edges.push((0, inner)),
        }
        // Dividers between name columns.
        for (start, width) in edges.iter().take(edges.len() - 1) {
            let cx = x + 1 + (*start + *width) as u16;
            for row in y + 1..footer {
                s.put(cx, row, "│", theme::PANEL);
            }
        }

        // Full shows name, size, date and time; Wide drops the date and time.
        let (size_w, date_w) = match self.view {
            View::Full if inner >= 40 => (10, 14),
            View::Full | View::Wide if inner >= 24 => (10, 0),
            _ => (0, 0),
        };
        let extra =
            if size_w > 0 { size_w + 1 } else { 0 } + if date_w > 0 { date_w + 1 } else { 0 };
        for (start, width) in &edges {
            let name_width = width - extra;
            let header = format!("{:^name_width$}", "Name");
            s.put(x + 1 + *start as u16, y + 1, &header, theme::PANEL_HEADER);
            if size_w > 0 {
                s.put(
                    x + 1 + (*start + name_width) as u16,
                    y + 1,
                    "│",
                    theme::PANEL,
                );
                s.put(
                    x + 2 + (*start + name_width) as u16,
                    y + 1,
                    &format!("{:^size_w$}", "Size"),
                    theme::PANEL_HEADER,
                );
            }
            if date_w > 0 {
                s.put(
                    x + 2 + (*start + name_width + size_w) as u16,
                    y + 1,
                    "│",
                    theme::PANEL,
                );
                s.put(
                    x + 3 + (*start + name_width + size_w) as u16,
                    y + 1,
                    &format!("{:^date_w$}", "Date  Time"),
                    theme::PANEL_HEADER,
                );
            }
        }

        // Entries, filling columns top to bottom.
        for i in 0..cap {
            let Some(entry) = self.entries.get(self.top + i) else {
                break;
            };
            let column = i / self.rows;
            let row = i % self.rows;
            let (start, width) = edges[column];
            let on_cursor = active && self.top + i == self.cursor;
            let style = Self::name_style(entry, on_cursor);
            let cx = x + 1 + start as u16;
            let cy = y + 2 + row as u16;
            let name_width = width - extra;
            s.put(cx, cy, &fit_name(&entry.name, name_width), style);
            if size_w > 0 {
                let plain = if on_cursor {
                    style
                } else {
                    Self::meta_style(entry)
                };
                s.put(
                    cx + name_width as u16,
                    cy,
                    "│",
                    if on_cursor { style } else { theme::PANEL },
                );
                s.put(
                    cx + name_width as u16 + 1,
                    cy,
                    &Self::size_text(entry, size_w),
                    plain,
                );
                if date_w > 0 {
                    let stamp = if entry.is_up {
                        String::new()
                    } else {
                        format!(
                            "{} {}",
                            util::format_date(entry.mtime),
                            util::format_time(entry.mtime)
                        )
                    };
                    let bar = cx + (name_width + size_w + 1) as u16;
                    s.put(bar, cy, "│", if on_cursor { style } else { theme::PANEL });
                    s.put(bar + 1, cy, &fit(&stamp, date_w), plain);
                }
            }
        }
        if let Some(err) = &self.error {
            s.put(
                x + 2,
                y + 2,
                &fit_left(err, inner.saturating_sub(2)),
                theme::ERROR,
            );
        }

        // Footer: separator, details of the current entry, totals.
        let bar: String = "─".repeat(inner);
        s.put(x, footer, &format!("╟{bar}╢"), theme::PANEL);
        let mut joints: Vec<u16> = edges
            .iter()
            .take(edges.len() - 1)
            .map(|(a, b)| x + 1 + (*a + *b) as u16)
            .collect();
        if size_w > 0 {
            let name_width = edges[0].1 - extra;
            joints.push(x + 1 + name_width as u16);
            if date_w > 0 {
                joints.push(x + 2 + (name_width + size_w) as u16);
            }
        }
        for j in joints {
            s.put(j, footer, "┴", theme::PANEL);
        }

        if let Some(entry) = self.current().cloned() {
            let detail = if entry.is_up {
                String::from("Up")
            } else if entry.is_dir {
                util::format_date(entry.mtime) + " " + &util::format_time(entry.mtime)
            } else {
                format!(
                    "{} {} {}",
                    util::group_digits(entry.size),
                    util::format_date(entry.mtime),
                    util::format_time(entry.mtime)
                )
            };
            let detail_w = str_width(&detail).min(inner);
            let name_room = inner.saturating_sub(detail_w + 1);
            let line = format!("{} {detail}", fit_name(&entry.name, name_room));
            s.put(x + 1, footer + 1, &line, theme::PANEL);
            if entry.is_link {
                if let Ok(target) = fs::read_link(self.entry_path(&entry)) {
                    let text = format!(" -> {}", target.display());
                    s.put(
                        x + 1,
                        footer + 1,
                        &fit_left(&text, inner),
                        theme::PANEL_LINK,
                    );
                }
            }
        }

        // Selection summary sits on the separator, as in Far.
        let selected: Vec<&Entry> = self.entries.iter().filter(|e| e.selected).collect();
        if !selected.is_empty() {
            let bytes: u64 = selected.iter().filter(|e| !e.is_dir).map(|e| e.size).sum();
            let text = format!(
                " {} bytes in {} items ",
                util::group_digits(bytes),
                selected.len()
            );
            s.title(x, footer, w, &text, theme::PANEL_SELECTED);
        }
        let (mut files, mut dirs, mut bytes) = (0u64, 0u64, 0u64);
        for entry in self.entries.iter().filter(|e| !e.is_up) {
            if entry.is_dir {
                dirs += 1;
            } else {
                files += 1;
                bytes += entry.size;
            }
        }
        let mut totals = format!(
            "{} bytes, {files} files, {dirs} folders",
            util::group_digits(bytes)
        );
        if let Some(free) = self.free_bytes {
            totals.push_str(&format!(", {} free", util::fit_size(free, 6)));
        }
        s.title(x, y + h - 1, w, &totals, theme::PANEL);

        let title_style = if active {
            theme::PANEL_TITLE
        } else {
            theme::PANEL
        };
        s.title(x, y, w, &util::tilde(&self.path), title_style);
        let mode = match (self.sort, self.reverse) {
            (Sort::Name, false) => 'n',
            (Sort::Name, true) => 'N',
            (Sort::Ext, false) => 'x',
            (Sort::Ext, true) => 'X',
            (Sort::Mtime, false) => 'm',
            (Sort::Mtime, true) => 'M',
            (Sort::Size, false) => 's',
            (Sort::Size, true) => 'S',
            (Sort::Unsorted, _) => 'u',
            (Sort::Ctime, false) => 'c',
            (Sort::Ctime, true) => 'C',
            (Sort::Atime, false) => 'a',
            (Sort::Atime, true) => 'A',
            (Sort::Owner, false) => 'o',
            (Sort::Owner, true) => 'O',
        };
        s.put(x + 1, y, &mode.to_string(), theme::PANEL_HEADER);
    }

    fn meta_style(entry: &Entry) -> Style {
        if entry.selected {
            theme::PANEL_SELECTED
        } else {
            theme::PANEL
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn panel(tag: &str, files: &[&str], dirs: &[&str]) -> (Panel, PathBuf) {
        let root = std::env::temp_dir().join(format!("far_panel_{}_{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        for f in files {
            fs::write(root.join(f), "x").unwrap();
        }
        for d in dirs {
            fs::create_dir(root.join(d)).unwrap();
        }
        (Panel::new(root.clone()), root)
    }

    fn names(p: &Panel) -> Vec<&str> {
        p.entries.iter().map(|e| e.name.as_str()).collect()
    }

    #[test]
    fn folders_sort_before_files_and_up_is_first() {
        let (p, root) = panel("sort", &["b.txt", "A.txt"], &["zdir", "adir"]);
        assert_eq!(names(&p), ["..", "adir", "zdir", "A.txt", "b.txt"]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn choosing_the_same_sort_reverses_it_and_keeps_the_cursor() {
        let (mut p, root) = panel("rev", &["a", "b", "c"], &[]);
        p.goto_name(&"b".into());
        p.set_sort(Sort::Name);
        assert_eq!(names(&p), ["..", "c", "b", "a"]);
        assert_eq!(p.current().unwrap().name, "b");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn up_entry_is_never_a_target_or_selected() {
        let (mut p, root) = panel("up", &["a"], &[]);
        p.cursor = 0;
        p.toggle_current();
        assert!(!p.entries[0].selected);
        assert!(p.targets().is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn selection_wins_over_cursor_and_survives_reload() {
        let (mut p, root) = panel("sel", &["a", "b", "c"], &[]);
        p.goto_name(&"a".into());
        p.toggle_current();
        p.goto_name(&"c".into());
        assert_eq!(p.targets(), vec![root.join("a")]);
        p.reload();
        assert_eq!(p.targets(), vec![root.join("a")]);
        assert_eq!(p.current().unwrap().name, "c");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn group_selection_skips_folders() {
        let (mut p, root) = panel("group", &["a.txt", "b.log"], &["d.txt"]);
        p.select_mask("*.txt", true);
        assert_eq!(p.targets(), vec![root.join("a.txt")]);
        p.invert_selection();
        assert_eq!(p.targets(), vec![root.join("b.log")]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn going_up_puts_cursor_on_the_folder_left() {
        let (mut p, root) = panel("nav", &[], &["inner"]);
        p.chdir(&root.join("inner"), None).unwrap();
        p.go_up().unwrap();
        assert_eq!(p.current().unwrap().name, "inner");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn hidden_files_follow_the_toggle() {
        let (mut p, root) = panel("hid", &[".hidden", "shown"], &[]);
        assert!(names(&p).contains(&".hidden"));
        p.show_hidden = false;
        p.reload();
        assert!(!names(&p).contains(&".hidden"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn prefix_search_wraps_around() {
        let (p, root) = panel("q", &["alpha", "beta", "apple"], &[]);
        let first = p.find_prefix("a", 0).unwrap();
        let second = p.find_prefix("a", first + 1).unwrap();
        assert_ne!(first, second);
        assert_eq!(p.find_prefix("a", second + 1).unwrap(), first);
        assert_eq!(p.find_prefix("zzz", 0), None);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn long_listing_keeps_cursor_visible() {
        let names_owned: Vec<String> = (0..100).map(|i| format!("f{i:03}")).collect();
        let refs: Vec<&str> = names_owned.iter().map(String::as_str).collect();
        let (mut p, root) = panel("scroll", &refs, &[]);
        p.view = View::Full;
        let mut screen = Screen::new(40, 12);
        p.cursor = 80;
        p.draw(&mut screen, 0, 0, 40, 12, true);
        assert!(p.top <= 80 && 80 < p.top + p.capacity());
        fs::remove_dir_all(root).unwrap();
    }
}
