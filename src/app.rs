//! The application: panels, command line, hot keys and file operation dialogs.

use crate::archive::{self, ExtractRequest, Format, ScratchDir, FORMATS};
use crate::dialog::{self, Field, Form};
use crate::editor::{self, Clipboard, EditorContext, EditorExit};
use crate::fsops::{
    self, AttrChange, CopyOptions, ErrorChoice, OpSummary, OpUi, OverwriteChoice, Progress, Totals,
    TrashChoice,
};
use crate::history::{History, Kind};
use crate::lineedit::LineEdit;
use crate::panel::{Panel, Sort, View};
use crate::screen::{
    self, fit, fit_left, str_width, theme, Input, Key, Tui, ALT, CTRL, NONE, SHIFT,
};
use crate::shell::Shell;
use crate::util;
use crate::viewer::{self, ViewerExit};
use crossterm::{
    cursor,
    event::{
        self, Event, KeyCode, KeyEventKind, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
        PushKeyboardEnhancementFlags,
    },
    execute,
    style::Print,
    terminal,
};
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

const HISTORY_LIMIT: usize = 200;
const TICK: Duration = Duration::from_millis(1000);
const OP_TICK: Duration = Duration::from_millis(30);

/// What the inactive panel shows instead of files.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Overlay {
    None,
    Info,
    QuickView,
}

/// Every command reachable from the keyboard or the F9 menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    View,
    Edit,
    NewFile,
    Copy,
    CopyHere,
    Move,
    RenameHere,
    Link,
    MkDir,
    Trash,
    DeletePermanently,
    Attributes,
    SelectGroup,
    UnselectGroup,
    InvertSelection,
    SelectAll,
    ClearSelection,
    FindFiles,
    CommandHistory,
    FolderHistory,
    ViewHistory,
    SwapPanels,
    TogglePanels,
    ToggleLeft,
    ToggleRight,
    ToggleInactive,
    ToggleHidden,
    ToggleKeybar,
    Console,
    Reread,
    Location(usize),
    SetView(View),
    SetSort(Sort),
    SortMenu,
    Info,
    QuickView,
    Help,
    DeleteCurrent,
    ArchiveAdd,
    ArchiveExtract,
    ArchiveCommands,
    Quit,
}

type MenuItem = (&'static str, &'static str, Action);

fn panel_menu() -> Vec<MenuItem> {
    vec![
        ("Brief", "Ctrl+1", Action::SetView(View::Brief)),
        ("Medium", "Ctrl+2", Action::SetView(View::Medium)),
        ("Full", "Ctrl+3", Action::SetView(View::Full)),
        ("Wide", "Ctrl+4", Action::SetView(View::Wide)),
        ("Info panel", "Ctrl+L", Action::Info),
        ("Quick view", "Ctrl+Q", Action::QuickView),
        ("Sort modes", "Ctrl+F12", Action::SortMenu),
        ("Show hidden files", "Ctrl+H", Action::ToggleHidden),
        ("Re-read", "Ctrl+R", Action::Reread),
    ]
}

fn files_menu() -> Vec<MenuItem> {
    vec![
        ("View", "F3", Action::View),
        ("Edit", "F4", Action::Edit),
        ("New file", "Shift+F4", Action::NewFile),
        ("Copy", "F5", Action::Copy),
        ("Copy here", "Shift+F5", Action::CopyHere),
        ("Rename or move", "F6", Action::Move),
        ("Rename", "Shift+F6", Action::RenameHere),
        ("Create link", "Alt+F6", Action::Link),
        ("Make folder", "F7", Action::MkDir),
        ("Delete to trash", "F8", Action::Trash),
        (
            "Delete item under cursor",
            "Shift+F8",
            Action::DeleteCurrent,
        ),
        ("Delete permanently", "Shift+Del", Action::DeletePermanently),
        ("File attributes", "Ctrl+A", Action::Attributes),
        ("Select group", "+", Action::SelectGroup),
        ("Unselect group", "-", Action::UnselectGroup),
        ("Invert selection", "*", Action::InvertSelection),
        ("Select all", "Ctrl+Shift+A", Action::SelectAll),
        ("Clear selection", "Menu", Action::ClearSelection),
        ("Add to archive", "Shift+F1", Action::ArchiveAdd),
        ("Extract from archive", "Shift+F2", Action::ArchiveExtract),
        ("Archive commands", "Shift+F3", Action::ArchiveCommands),
    ]
}

fn commands_menu() -> Vec<MenuItem> {
    vec![
        ("Find files", "Alt+F7", Action::FindFiles),
        ("Command history", "Alt+F8", Action::CommandHistory),
        ("View history", "Alt+F11", Action::ViewHistory),
        ("Folder history", "Alt+F12", Action::FolderHistory),
        ("Swap panels", "Ctrl+U", Action::SwapPanels),
        ("Panels on/off", "Ctrl+O", Action::Console),
        ("Inactive panel on/off", "Ctrl+P", Action::ToggleInactive),
        ("Left panel on/off", "Ctrl+F1", Action::ToggleLeft),
        ("Right panel on/off", "Ctrl+F2", Action::ToggleRight),
        ("Key bar on/off", "Ctrl+B", Action::ToggleKeybar),
        ("Help", "F1", Action::Help),
        ("Quit", "F10", Action::Quit),
    ]
}

const HELP: &[(&str, &str)] = &[
    ("Archives", ""),
    ("Enter, Ctrl+PgDn", "Open the archive under the cursor"),
    ("Ctrl+PgUp", "Go up, leaving the archive at its top"),
    ("Shift+F1", "Add the selection to an archive"),
    ("Shift+F2", "Extract the chosen archives"),
    ("Shift+F3", "Archive commands: test"),
    ("F5", "Extract from, or add to, the open archive"),
    ("F6", "Extract and remove, or add and remove"),
    ("F3, F4", "View or edit a temporary copy of a file"),
    ("F7, F8", "Make a folder, delete, inside the archive"),
    ("", ""),
    ("Panels", ""),
    ("Tab", "Other panel"),
    ("Enter", "Open folder, run program, or run the command line"),
    ("Ins, Shift+arrows", "Select"),
    ("+  -  *", "Select group, unselect group, invert"),
    (
        "Alt+letter",
        "Quick search; Ctrl+Enter finds the next match",
    ),
    ("Ctrl+PgUp, Ctrl+PgDn", "Parent folder, enter folder"),
    ("Ctrl+\\", "Root folder"),
    ("Ctrl+O", "Show or hide panels (console)"),
    ("Ctrl+P", "Hide or show the inactive panel"),
    ("Ctrl+F1, Ctrl+F2", "Hide or show left, right panel"),
    ("Ctrl+U", "Swap panels"),
    ("Ctrl+L, Ctrl+Q", "Info panel, quick view"),
    ("Ctrl+R", "Re-read"),
    ("Ctrl+H", "Show or hide hidden files"),
    ("Ctrl+1..4", "View mode (also in the F9 menu)"),
    (
        "Ctrl+F3..F11",
        "Sort; repeat to reverse. Ctrl+F12: sort menu",
    ),
    ("Alt+F1, Alt+F2", "Change location of left, right panel"),
    ("F9", "Menu"),
    ("", ""),
    ("File operations", ""),
    ("F3, F4", "View, edit"),
    ("Shift+F4", "Edit a new file"),
    ("F5, Shift+F5", "Copy, copy under the cursor only"),
    (
        "F6, Shift+F6",
        "Rename or move, rename under the cursor only",
    ),
    ("Alt+F6", "Create a symbolic or hard link"),
    ("F7", "Make folder"),
    ("F8, Del", "Delete to trash"),
    ("Shift+F8", "Delete the item under the cursor to the trash"),
    ("Shift+Del", "Delete permanently"),
    ("Ctrl+A", "Permissions and owner"),
    ("Alt+F7", "Find files"),
    ("", ""),
    ("Command line", ""),
    ("Ctrl+Enter", "Insert file name"),
    ("Ctrl+F", "Insert full path"),
    ("Ctrl+[, Ctrl+]", "Insert path of left, right panel"),
    ("Ctrl+E, Ctrl+X", "Previous, next command"),
    ("Alt+F8", "Command history"),
    ("Alt+F11, Alt+F12", "View history, folder history"),
    ("Ctrl+Y, Esc", "Clear"),
    ("Ctrl+Ins, Alt+Ins", "Copy names, copy full paths"),
];

enum ToUi {
    Progress(Progress),
    Overwrite(PathBuf, PathBuf),
    Error(PathBuf, String),
    Trash(PathBuf),
    Done(OpSummary),
}

enum Reply {
    Overwrite(OverwriteChoice),
    Error(ErrorChoice),
    Trash(TrashChoice),
}

/// Connects a worker thread to the dialogs on the UI thread.
struct ChannelUi {
    tx: Sender<ToUi>,
    rx: Receiver<Reply>,
    cancel: Arc<AtomicBool>,
    last: Option<Instant>,
}

impl OpUi for ChannelUi {
    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    fn progress(&mut self, progress: &Progress) {
        let due = self.last.is_none_or(|t| t.elapsed() >= OP_TICK);
        if due {
            self.last = Some(Instant::now());
            let _ = self.tx.send(ToUi::Progress(progress.clone()));
        }
    }

    fn overwrite(&mut self, src: &Path, dst: &Path) -> OverwriteChoice {
        let _ = self.tx.send(ToUi::Overwrite(src.into(), dst.into()));
        match self.rx.recv() {
            Ok(Reply::Overwrite(choice)) => choice,
            _ => OverwriteChoice::Cancel,
        }
    }

    fn error(&mut self, path: &Path, message: &str) -> ErrorChoice {
        let _ = self.tx.send(ToUi::Error(path.into(), message.into()));
        match self.rx.recv() {
            Ok(Reply::Error(choice)) => choice,
            _ => ErrorChoice::Cancel,
        }
    }

    fn trash_fallback(&mut self, path: &Path) -> TrashChoice {
        let _ = self.tx.send(ToUi::Trash(path.into()));
        match self.rx.recv() {
            Ok(Reply::Trash(choice)) => choice,
            _ => TrashChoice::Cancel,
        }
    }
}

/// Quotes a string for bash.
pub fn shell_quote(text: &str) -> String {
    let safe = !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./+=:,@%".contains(c));
    if safe {
        text.to_string()
    } else {
        format!("'{}'", text.replace('\'', "'\\''"))
    }
}

/// Lists mount points worth offering in the location menu.
fn mount_points() -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(text) = fs::read_to_string("/proc/mounts") else {
        return found;
    };
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let (Some(device), Some(point), Some(kind)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let real = device.starts_with("/dev/")
            || matches!(
                kind,
                "9p" | "drvfs" | "nfs" | "nfs4" | "cifs" | "fuse.sshfs"
            );
        let point = point.replace("\\040", " ");
        let hidden = point.starts_with("/usr/lib/wsl")
            || point.starts_with("/snap")
            || point.starts_with("/boot")
            || point.starts_with("/proc")
            || point.starts_with("/sys")
            || point.starts_with("/dev")
            || point.starts_with("/run")
            || point.starts_with("/init")
            || point.starts_with("/mnt/wsl");
        if real && !hidden && point != "/" {
            found.push(PathBuf::from(point));
        }
    }
    found.sort();
    found.dedup();
    found
}

struct App {
    tui: Tui,
    panels: [Panel; 2],
    active: usize,
    cmd: LineEdit,
    quick: Option<String>,
    overlay: Overlay,
    keybar: bool,
    history: Vec<String>,
    history_pos: Option<usize>,
    folders: Vec<PathBuf>,
    viewed: Vec<(PathBuf, bool)>,
    clipboard: Clipboard,
    shell: Shell,
    quit: bool,
    masks: String,
    find_mask: String,
    find_text: String,
    db: Rc<History>,
}

/// Runs the file manager until the user quits.
pub fn run(left: Option<OsString>, right: Option<OsString>) -> io::Result<()> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    // Prefer the logical path, which keeps symbolic link names in the title.
    let start = std::env::var_os("PWD")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute() && same_dir(p, &cwd))
        .unwrap_or(cwd);
    let resolve = |arg: Option<OsString>| -> PathBuf {
        arg.map_or_else(
            || start.clone(),
            |raw| match raw.to_str() {
                Some(text) => util::normalize(&util::resolve_path(&start, text)),
                None => util::normalize(&start.join(&raw)),
            },
        )
    };
    let (left, right) = (resolve(left), resolve(right));
    screen::install_panic_hook();
    let shell = Shell::new()?;
    let tui = Tui::new()?;
    let db = Rc::new(History::open_default());
    let oldest_first = |kind| -> Vec<String> {
        let mut items = db.list(kind);
        items.truncate(HISTORY_LIMIT);
        items.reverse();
        items
    };
    let history = oldest_first(Kind::Command);
    let mut folders: Vec<PathBuf> = oldest_first(Kind::Folder)
        .into_iter()
        .map(PathBuf::from)
        .collect();
    folders.retain(|p| p != &left);
    folders.push(left.clone());
    let viewed: Vec<(PathBuf, bool)> = oldest_first(Kind::View)
        .iter()
        .filter_map(|item| {
            let (tag, path) = item.split_once(':')?;
            Some((PathBuf::from(path), tag == "e"))
        })
        .collect();
    let mut app = App {
        tui,
        panels: [Panel::new(left.clone()), Panel::new(right.clone())],
        active: 0,
        cmd: LineEdit::new(),
        quick: None,
        overlay: Overlay::None,
        keybar: true,
        history,
        history_pos: None,
        folders,
        viewed,
        clipboard: Clipboard::default(),
        shell,
        quit: false,
        masks: db.last(Kind::Mask).unwrap_or_else(|| "*".into()),
        find_mask: db.last(Kind::FindMask).unwrap_or_else(|| "*".into()),
        find_text: db.last(Kind::FindText).unwrap_or_default(),
        db,
    };
    app.main_loop()
}

/// `far --keys`: prints each key as the program receives it, to find out which
/// combinations a terminal delivers.
pub fn show_keys() -> io::Result<()> {
    let enhanced = terminal::supports_keyboard_enhancement().unwrap_or(false);
    terminal::enable_raw_mode()?;
    let mut out = io::stdout();
    if enhanced {
        execute!(
            out,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )?;
    }
    let protocol = if enhanced { "yes" } else { "no" };
    execute!(
        out,
        Print(format!(
            "Keyboard protocol: {protocol}. Press keys; Ctrl+C quits.\r\n"
        ))
    )?;
    let result = (|| -> io::Result<()> {
        loop {
            let Event::Key(raw) = event::read()? else {
                continue;
            };
            if raw.kind == KeyEventKind::Release {
                continue;
            }
            let key = screen::normalize_key(raw.code, raw.modifiers, enhanced);
            execute!(out, Print(format!("{:?} {}\r\n", key.code, key.mods)))?;
            if key.mods == CTRL && key.code == KeyCode::Char('c') {
                return Ok(());
            }
        }
    })();
    if enhanced {
        execute!(out, PopKeyboardEnhancementFlags)?;
    }
    terminal::disable_raw_mode()?;
    result
}

fn same_dir(a: &Path, b: &Path) -> bool {
    match (fs::metadata(a), fs::metadata(b)) {
        (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
        _ => false,
    }
}

impl App {
    fn main_loop(&mut self) -> io::Result<()> {
        while !self.quit {
            self.draw_main();
            self.tui.draw()?;
            match self.tui.read(TICK)? {
                Some(Input::Key(key)) => self.on_key(key)?,
                Some(Input::Paste(text)) => self.cmd.insert_str(&text),
                Some(Input::Resize) => {}
                None => self.refresh_changed(),
            }
        }
        Ok(())
    }

    fn panel(&mut self) -> &mut Panel {
        &mut self.panels[self.active]
    }

    fn refresh_changed(&mut self) {
        for panel in &mut self.panels {
            if panel.changed_on_disk() {
                panel.reload();
            }
        }
    }

    fn reload_all(&mut self) {
        for panel in &mut self.panels {
            panel.reload();
        }
    }

    fn draw_main(&mut self) {
        let (w, h) = (self.tui.screen.w, self.tui.screen.h);
        let s = &mut self.tui.screen;
        s.fill(0, 0, w, h, ' ', theme::BLANK);
        s.cursor = None;
        let height = h.saturating_sub(1 + u16::from(self.keybar));
        let half = w / 2;
        let inactive = 1 - self.active;
        for side in 0..2 {
            let (x, width) = if side == 0 {
                (0, half)
            } else {
                (half, w - half)
            };
            if !self.panels[side].visible {
                continue;
            }
            if side == inactive && self.overlay != Overlay::None {
                let source = &self.panels[self.active];
                match self.overlay {
                    Overlay::Info => draw_info(s, source, x, width, height),
                    _ => draw_quick_view(s, source, x, width, height),
                }
            } else {
                self.panels[side].draw(s, x, 0, width, height, side == self.active);
            }
        }
        // Command line.
        let y = height;
        let prompt = format!(
            "{}$ ",
            fit_left(
                &util::tilde(&self.panels[self.active].path),
                (w / 2) as usize
            )
        );
        let x = s.put(0, y, &prompt, theme::CMDLINE);
        let room = (w - x) as usize;
        let (shown, column) = dialog::visible_text(&self.cmd, room.max(1));
        s.put(x, y, &fit(&shown, room), theme::CMDLINE);
        s.cursor = Some((x + column as u16, y));
        if self.keybar && h > 1 {
            s.keybar(
                h - 1,
                &[
                    "Help", "", "View", "Edit", "Copy", "RenMov", "MkFold", "Delete", "Menu",
                    "Quit", "", "",
                ],
            );
        }
        // Quick search box under the active panel.
        if let Some(text) = &self.quick {
            let (x, width) = if self.active == 0 {
                (0, half)
            } else {
                (half, w - half)
            };
            let box_w = width.saturating_sub(4).clamp(10, 30);
            let bx = x + (width.saturating_sub(box_w)) / 2;
            let by = height.saturating_sub(3);
            s.frame(bx, by, box_w, 3, false, theme::DIALOG);
            s.title(bx, by, box_w, "Search", theme::DIALOG);
            let inner = box_w as usize - 2;
            s.put(
                bx + 1,
                by + 1,
                &fit(&fit_left(text, inner.saturating_sub(1)), inner),
                theme::INPUT,
            );
            let shown = str_width(text).min(inner - 1) as u16;
            s.cursor = Some((bx + 1 + shown, by + 1));
        }
    }

    fn on_key(&mut self, key: Key) -> io::Result<()> {
        if self.handle_quick(key)? {
            return Ok(());
        }
        let cmd_empty = self.cmd.is_empty();
        match (key.mods, key.code) {
            (ALT, KeyCode::Char(c)) if c.is_alphanumeric() || c == '.' || c == '_' => {
                let mut text = String::new();
                text.push(c);
                self.quick = Some(String::new());
                self.quick_extend(&text);
            }
            (NONE, KeyCode::Tab) => self.switch_panel(),
            (NONE, KeyCode::Enter) => {
                if cmd_empty {
                    self.open_current()?;
                } else {
                    let text = self.cmd.text();
                    self.cmd.clear();
                    self.execute(&text)?;
                }
            }
            (SHIFT, KeyCode::Enter) => {
                if !cmd_empty {
                    let text = self.cmd.text();
                    self.cmd.clear();
                    self.execute(&text)?;
                }
            }
            (CTRL, KeyCode::Enter) => {
                if let Some(entry) = self.panels[self.active].current() {
                    if !entry.is_up {
                        let name = format!("{} ", shell_quote(&entry.name));
                        self.cmd.insert_str(&name);
                    }
                }
            }
            (CTRL, KeyCode::Char('f')) => {
                if let Some(path) = self.panels[self.active].current_path() {
                    let text = format!("{} ", shell_quote(&path.display().to_string()));
                    self.cmd.insert_str(&text);
                }
            }
            (CTRL, KeyCode::Char('[')) | (CTRL, KeyCode::Char(']')) => {
                let side = usize::from(key.code == KeyCode::Char(']'));
                let text = format!(
                    "{} ",
                    shell_quote(&self.panels[side].path.display().to_string())
                );
                self.cmd.insert_str(&text);
            }
            (NONE, KeyCode::Esc) => self.cmd.clear(),
            (CTRL, KeyCode::Char('e')) => self.history_step(-1),
            (CTRL, KeyCode::Char('x')) => self.history_step(1),
            (NONE, KeyCode::Up) => self.panel().move_by(-1),
            (NONE, KeyCode::Down) => self.panel().move_by(1),
            (NONE, KeyCode::Left) if cmd_empty => self.move_column(-1),
            (NONE, KeyCode::Right) if cmd_empty => self.move_column(1),
            (NONE, KeyCode::Home) if cmd_empty => self.panel().cursor = 0,
            (NONE, KeyCode::End) if cmd_empty => {
                let last = self.panels[self.active].entries.len().saturating_sub(1);
                self.panel().cursor = last;
            }
            (NONE, KeyCode::PageUp) => {
                let page = self.panels[self.active].capacity() as isize;
                self.panel().move_by(-page);
            }
            (NONE, KeyCode::PageDown) => {
                let page = self.panels[self.active].capacity() as isize;
                self.panel().move_by(page);
            }
            (CTRL, KeyCode::PageUp) => self.go_up(),
            (CTRL, KeyCode::PageDown) => self.enter_folder(),
            (CTRL, KeyCode::Char('\\')) => {
                self.navigate(Path::new("/"), None)?;
            }
            (NONE, KeyCode::Insert) => {
                self.panel().toggle_current();
                self.panel().move_by(1);
            }
            (SHIFT, KeyCode::Up) => self.panel().select_move(-1),
            (SHIFT, KeyCode::Down) => self.panel().select_move(1),
            (SHIFT, KeyCode::Left) => {
                let rows = self.panels[self.active].rows as isize;
                self.panel().select_move(-rows);
            }
            (SHIFT, KeyCode::Right) => {
                let rows = self.panels[self.active].rows as isize;
                self.panel().select_move(rows);
            }
            (SHIFT, KeyCode::Home) => self.panel().select_move(isize::MIN / 2),
            (SHIFT, KeyCode::End) => self.panel().select_move(isize::MAX / 2),
            (SHIFT, KeyCode::PageUp) => {
                let page = self.panels[self.active].capacity() as isize;
                self.panel().select_move(-page);
            }
            (SHIFT, KeyCode::PageDown) => {
                let page = self.panels[self.active].capacity() as isize;
                self.panel().select_move(page);
            }
            (NONE, KeyCode::Char('+')) if cmd_empty => self.perform(Action::SelectGroup)?,
            (NONE, KeyCode::Char('-')) if cmd_empty => self.perform(Action::UnselectGroup)?,
            (NONE, KeyCode::Char('*')) if cmd_empty => self.perform(Action::InvertSelection)?,
            (NONE, KeyCode::Char(' ')) if cmd_empty => {}
            (NONE, KeyCode::F(9)) => self.top_menu()?,
            (NONE, KeyCode::Delete) if cmd_empty => self.perform(Action::Trash)?,
            (SHIFT, KeyCode::Delete) => self.perform(Action::DeletePermanently)?,
            (CTRL, KeyCode::Insert) => self.copy_names(false),
            (ALT, KeyCode::Insert) => self.copy_names(true),
            _ => {
                if let Some(action) = key_action(key, self.tui.enhanced) {
                    self.perform(action)?;
                } else {
                    self.cmd.handle(key.code, key.mods);
                }
            }
        }
        Ok(())
    }

    /// Handles a key while quick search is open. Returns true if consumed.
    fn handle_quick(&mut self, key: Key) -> io::Result<bool> {
        if self.quick.is_none() {
            return Ok(false);
        }
        match (key.mods, key.code) {
            (NONE, KeyCode::Char(c)) | (ALT, KeyCode::Char(c)) => {
                self.quick_extend(&c.to_string());
                Ok(true)
            }
            (NONE, KeyCode::Backspace) => {
                if let Some(text) = &mut self.quick {
                    text.pop();
                }
                Ok(true)
            }
            (CTRL, KeyCode::Enter) => {
                let text = self.quick.clone().unwrap_or_default();
                let from = self.panels[self.active].cursor + 1;
                let panel = &mut self.panels[self.active];
                if let Some(i) = panel.find_prefix(&text, from) {
                    panel.cursor = i;
                }
                Ok(true)
            }
            (NONE, KeyCode::Esc) => {
                self.quick = None;
                Ok(true)
            }
            _ => {
                self.quick = None;
                Ok(false)
            }
        }
    }

    /// Adds to the search text if some name still matches.
    fn quick_extend(&mut self, more: &str) {
        let candidate = format!("{}{more}", self.quick.as_deref().unwrap_or(""));
        let panel = &mut self.panels[self.active];
        if let Some(i) = panel.find_prefix(&candidate, panel.cursor) {
            panel.cursor = i;
            self.quick = Some(candidate);
        }
    }

    fn switch_panel(&mut self) {
        let other = 1 - self.active;
        if !self.panels[other].visible {
            self.panels[other].visible = true;
        }
        self.active = other;
    }

    fn move_column(&mut self, dir: isize) {
        let panel = self.panel();
        let step = if panel.columns > 1 {
            panel.rows as isize
        } else {
            1
        };
        panel.move_by(dir * step);
    }

    fn go_up(&mut self) {
        let panel = &mut self.panels[self.active];
        if let Err(text) = panel.go_up() {
            let _ = dialog::alert(&mut self.tui, "Error", &text);
        } else {
            let path = self.panels[self.active].path.clone();
            self.remember_folder(path);
        }
    }

    fn enter_folder(&mut self) {
        let Some(entry) = self.panels[self.active].current().cloned() else {
            return;
        };
        if entry.is_up {
            self.go_up();
        } else if self.panels[self.active].archive.is_some() {
            if entry.is_dir {
                self.panel().enter_archive_folder(&entry.name);
            }
        } else if entry.is_dir {
            let path = self.panels[self.active].entry_path(&entry);
            let _ = self.navigate(&path, None);
        } else {
            let path = self.panels[self.active].entry_path(&entry);
            if let Some(format) = archive::detect(&path) {
                let _ = self.enter_archive(&path, format);
            }
        }
    }

    /// Opens an archive in the active panel.
    fn enter_archive(&mut self, path: &Path, format: Format) -> io::Result<()> {
        if !format.is_available() {
            let text = format!(
                "{} is needed to open this archive",
                format.tool().unwrap_or_default()
            );
            return dialog::alert(&mut self.tui, "Archive", &text);
        }
        self.show_wait("Archive", "Reading the archive...")?;
        if let Err(text) = self.panel().open_archive(path, format) {
            dialog::alert(&mut self.tui, "Archive", &text)?;
        }
        Ok(())
    }

    /// Shows a message while the program does something short.
    fn show_wait(&mut self, title: &str, text: &str) -> io::Result<()> {
        self.draw_main();
        let lines = vec![text.to_string()];
        dialog::draw_message(&mut self.tui.screen, title, &lines, &[], 0, false);
        self.tui.draw()
    }

    fn navigate(&mut self, path: &Path, focus: Option<&OsString>) -> io::Result<()> {
        self.navigate_side(self.active, path, focus)
    }

    fn navigate_side(
        &mut self,
        side: usize,
        path: &Path,
        focus: Option<&OsString>,
    ) -> io::Result<()> {
        match self.panels[side].chdir(path, focus) {
            Ok(()) => self.remember_folder(path.to_path_buf()),
            Err(text) => dialog::alert(&mut self.tui, "Error", &text)?,
        }
        Ok(())
    }

    fn remember_folder(&mut self, path: PathBuf) {
        if let Some(text) = path.to_str() {
            self.db.add(Kind::Folder, text);
        }
        self.folders.retain(|p| p != &path);
        self.folders.push(path);
        if self.folders.len() > HISTORY_LIMIT {
            self.folders.remove(0);
        }
    }

    fn remember_viewed(&mut self, path: &Path, edited: bool) {
        if let Some(text) = path.to_str() {
            let tag = if edited { "e" } else { "v" };
            self.db.add(Kind::View, &format!("{tag}:{text}"));
        }
        self.viewed.retain(|(p, e)| !(p == path && *e == edited));
        self.viewed.push((path.to_path_buf(), edited));
        if self.viewed.len() > HISTORY_LIMIT {
            self.viewed.remove(0);
        }
    }

    fn open_current(&mut self) -> io::Result<()> {
        let Some(entry) = self.panels[self.active].current().cloned() else {
            return Ok(());
        };
        if entry.is_up {
            self.go_up();
        } else if self.panels[self.active].archive.is_some() {
            if entry.is_dir {
                self.panel().enter_archive_folder(&entry.name);
            } else {
                self.view_in_archive()?;
            }
        } else if entry.is_dir {
            let path = self.panels[self.active].entry_path(&entry);
            self.navigate(&path, None)?;
        } else if let Some(format) = archive::detect(Path::new(&entry.raw)) {
            let path = self.panels[self.active].entry_path(&entry);
            self.enter_archive(&path, format)?;
        } else if entry.exec {
            self.execute(&format!("./{}", shell_quote(&entry.name)))?;
        } else {
            let path = self.panels[self.active].entry_path(&entry);
            self.view_file(&path)?;
        }
        Ok(())
    }

    fn history_step(&mut self, dir: isize) {
        if self.history.is_empty() {
            return;
        }
        let last = self.history.len() as isize - 1;
        let next = match self.history_pos {
            None if dir < 0 => last,
            None => return,
            Some(i) => i as isize + dir,
        };
        if next > last {
            self.history_pos = None;
            self.cmd.clear();
        } else {
            let i = next.max(0) as usize;
            self.history_pos = Some(i);
            let text = self.history[i].clone();
            self.cmd.set(&text);
        }
    }

    fn remember_command(&mut self, command: &str) {
        self.history_pos = None;
        self.db.add(Kind::Command, command);
        self.history.retain(|c| c != command);
        self.history.push(command.to_string());
        if self.history.len() > HISTORY_LIMIT {
            self.history.remove(0);
        }
    }

    /// Runs a command on the console, then returns to the panels.
    fn execute(&mut self, command: &str) -> io::Result<()> {
        self.tui.show_console()?;
        let result = self.run_on_console(command);
        self.tui.enter()?;
        result
    }

    /// Prints and runs a command. The console must already be showing.
    fn run_on_console(&mut self, command: &str) -> io::Result<()> {
        self.remember_command(command);
        let cwd = self.panels[self.active].path.clone();
        let mut out = io::stdout();
        terminal::disable_raw_mode()?;
        execute!(
            out,
            Print(format!(
                "{}$ {}\r\n",
                util::printable(&util::tilde(&cwd)),
                util::printable(command)
            )),
            cursor::Show
        )?;
        let finished = self.shell.run(command, &cwd);
        terminal::enable_raw_mode()?;
        match finished {
            Ok(done) => {
                if let Some(path) = done.cwd {
                    if path != cwd && path.is_dir() {
                        let _ = self.panels[self.active].chdir(&path, None);
                        self.remember_folder(path);
                    }
                }
            }
            Err(e) => {
                execute!(out, Print(format!("far: cannot start bash: {e}\r\n")))?;
            }
        }
        self.reload_all();
        Ok(())
    }

    /// Ctrl+O: the real terminal screen, with its output and scrollback.
    fn console(&mut self) -> io::Result<()> {
        self.tui.show_console()?;
        let result = self.console_loop();
        self.tui.enter()?;
        result
    }

    fn console_loop(&mut self) -> io::Result<()> {
        let mut out = io::stdout();
        loop {
            let (w, _) = terminal::size()?;
            let prompt = format!(
                "{}$ ",
                util::printable(&util::tilde(&self.panels[self.active].path))
            );
            let prompt = fit_left(&prompt, (w / 2) as usize);
            let room = (w as usize).saturating_sub(str_width(&prompt)).max(1);
            let (shown, column) = dialog::visible_text(&self.cmd, room);
            execute!(
                out,
                Print("\r"),
                terminal::Clear(terminal::ClearType::CurrentLine),
                Print(&prompt),
                Print(fit(&shown, room).trim_end()),
                cursor::MoveToColumn((str_width(&prompt) + column) as u16)
            )?;
            let Some(input) = self.tui.read(TICK)? else {
                continue;
            };
            let key = match input {
                Input::Key(key) => key,
                Input::Paste(text) => {
                    self.cmd.insert_str(&text);
                    continue;
                }
                Input::Resize => continue,
            };
            match (key.mods, key.code) {
                (CTRL, KeyCode::Char('o')) => {
                    execute!(
                        out,
                        Print("\r"),
                        terminal::Clear(terminal::ClearType::CurrentLine)
                    )?;
                    return Ok(());
                }
                (NONE, KeyCode::Enter) | (SHIFT, KeyCode::Enter) => {
                    let text = self.cmd.text();
                    self.cmd.clear();
                    execute!(
                        out,
                        Print("\r"),
                        terminal::Clear(terminal::ClearType::CurrentLine)
                    )?;
                    if text.trim().is_empty() {
                        execute!(out, Print(format!("{prompt}\r\n")))?;
                    } else {
                        self.run_on_console(&text)?;
                    }
                }
                (NONE, KeyCode::Esc) => self.cmd.clear(),
                (NONE, KeyCode::Up) | (CTRL, KeyCode::Char('e')) => self.history_step(-1),
                (NONE, KeyCode::Down) | (CTRL, KeyCode::Char('x')) => self.history_step(1),
                _ => {
                    self.cmd.handle(key.code, key.mods);
                }
            }
        }
    }

    fn copy_names(&mut self, full: bool) {
        let panel = &self.panels[self.active];
        let lines: Vec<String> = panel
            .targets()
            .iter()
            .map(|p| {
                if full {
                    p.display().to_string()
                } else {
                    p.file_name()
                        .map_or(String::new(), |n| n.to_string_lossy().into_owned())
                }
            })
            .collect();
        let text = lines.join("\n");
        if !text.is_empty() {
            self.tui.set_clipboard(&text);
            self.clipboard = Clipboard {
                text,
                vertical: false,
            };
        }
    }

    fn perform(&mut self, action: Action) -> io::Result<()> {
        let inside = self.panels[self.active].archive.is_some();
        if inside
            && matches!(
                action,
                Action::Link | Action::Attributes | Action::CopyHere | Action::RenameHere
            )
        {
            let text = "This command does not work inside an archive";
            return dialog::alert(&mut self.tui, "Archive", text);
        }
        match action {
            Action::DeleteCurrent => self.delete_current()?,
            Action::ArchiveAdd => self.archive_add()?,
            Action::ArchiveExtract => self.archive_extract()?,
            Action::ArchiveCommands => self.archive_commands()?,
            Action::View => self.view_current()?,
            Action::Edit => self.edit_current()?,
            Action::NewFile => self.new_file()?,
            Action::Copy => self.copy_dialog(false)?,
            Action::CopyHere => self.copy_dialog(true)?,
            Action::Move => self.move_dialog(false)?,
            Action::RenameHere => self.move_dialog(true)?,
            Action::Link => self.link_dialog()?,
            Action::MkDir => self.mkdir_dialog()?,
            Action::Trash => self.delete_dialog(false)?,
            Action::DeletePermanently => self.delete_dialog(true)?,
            Action::Attributes => self.attributes_dialog()?,
            Action::SelectGroup => self.group_dialog(true)?,
            Action::UnselectGroup => self.group_dialog(false)?,
            Action::InvertSelection => self.panel().invert_selection(),
            Action::SelectAll => self.panel().select_mask("*", true),
            Action::ClearSelection => self.panel().clear_selection(),
            Action::FindFiles => self.find_dialog()?,
            Action::CommandHistory => self.command_history()?,
            Action::FolderHistory => self.folder_history()?,
            Action::ViewHistory => self.view_history()?,
            Action::SwapPanels => self.panels.swap(0, 1),
            Action::TogglePanels => {
                let on = !(self.panels[0].visible || self.panels[1].visible);
                self.panels[0].visible = on;
                self.panels[1].visible = on;
            }
            Action::ToggleLeft => self.toggle_side(0),
            Action::ToggleRight => self.toggle_side(1),
            Action::ToggleInactive => self.toggle_side(1 - self.active),
            Action::ToggleHidden => {
                let show = !self.panels[self.active].show_hidden;
                for panel in &mut self.panels {
                    panel.show_hidden = show;
                    panel.reload();
                }
            }
            Action::ToggleKeybar => self.keybar = !self.keybar,
            Action::Console => self.console()?,
            Action::Reread => self.reload_all(),
            Action::Location(side) => self.location_menu(side)?,
            Action::SetView(view) => self.panel().view = view,
            Action::SetSort(sort) => self.panel().set_sort(sort),
            Action::SortMenu => self.sort_menu()?,
            Action::Info => self.toggle_overlay(Overlay::Info),
            Action::QuickView => self.toggle_overlay(Overlay::QuickView),
            Action::Help => self.help()?,
            Action::Quit => {
                let lines = vec!["Do you want to quit?".to_string()];
                if dialog::message(&mut self.tui, "Quit", &lines, &["Yes", "No"], false)? == Some(0)
                {
                    self.quit = true;
                }
            }
        }
        Ok(())
    }

    fn toggle_side(&mut self, side: usize) {
        self.panels[side].visible = !self.panels[side].visible;
        if !self.panels[self.active].visible && self.panels[1 - self.active].visible {
            self.active = 1 - self.active;
        }
    }

    fn toggle_overlay(&mut self, overlay: Overlay) {
        self.overlay = if self.overlay == overlay {
            Overlay::None
        } else {
            overlay
        };
        self.panels[1 - self.active].visible = true;
    }

    fn view_current(&mut self) -> io::Result<()> {
        let Some(entry) = self.panels[self.active].current().cloned() else {
            return Ok(());
        };
        if entry.is_up {
            return Ok(());
        }
        if self.panels[self.active].archive.is_some() {
            return if entry.is_dir {
                Ok(())
            } else {
                self.view_in_archive()
            };
        }
        let path = self.panels[self.active].entry_path(&entry);
        if entry.is_dir {
            return self.folder_size(&path);
        }
        self.view_file(&path)
    }

    fn view_file(&mut self, path: &Path) -> io::Result<()> {
        let mut current = path.to_path_buf();
        loop {
            self.remember_viewed(&current, false);
            let db = self.db.clone();
            let exit = viewer::run(
                &mut self.tui,
                &current,
                &mut self.keybar,
                &mut self.clipboard,
                &db,
            );
            self.draw_main();
            match exit {
                Ok(ViewerExit::Close) => {}
                Ok(ViewerExit::Edit) => return self.edit_file(&current),
                Ok(ViewerExit::GoToFile) => return self.jump_to(&current),
                Ok(ViewerExit::History) => return self.view_history(),
                Ok(ViewerExit::NextFile) | Ok(ViewerExit::PreviousFile) => {
                    let forward = matches!(exit, Ok(ViewerExit::NextFile));
                    if let Some(next) = self.adjacent_file(&current, forward) {
                        current = next;
                        continue;
                    }
                }
                Err(e) => {
                    let text = format!("{}\n{e}", current.display());
                    dialog::alert(&mut self.tui, "Viewer", &text)?;
                }
            }
            self.reload_all();
            return Ok(());
        }
    }

    /// The next or previous file in the active panel, moving its cursor too.
    fn adjacent_file(&mut self, current: &Path, forward: bool) -> Option<PathBuf> {
        let panel = &mut self.panels[self.active];
        let name = current.file_name()?;
        let at = panel.entries.iter().position(|e| e.raw == name)?;
        let found = if forward {
            (at + 1..panel.entries.len()).find(|i| !panel.entries[*i].is_dir)
        } else {
            (0..at).rev().find(|i| !panel.entries[*i].is_dir)
        }?;
        panel.cursor = found;
        Some(panel.entry_path(&panel.entries[found]))
    }

    fn edit_current(&mut self) -> io::Result<()> {
        let Some(entry) = self.panels[self.active].current().cloned() else {
            return Ok(());
        };
        if entry.is_up || entry.is_dir {
            return Ok(());
        }
        if self.panels[self.active].archive.is_some() {
            return self.edit_in_archive();
        }
        let path = self.panels[self.active].entry_path(&entry);
        self.edit_file(&path)
    }

    fn edit_file(&mut self, path: &Path) -> io::Result<()> {
        self.remember_viewed(path, true);
        let name_on = |panel: &Panel| panel.current().filter(|e| !e.is_up).map(|e| e.name.clone());
        let db = self.db.clone();
        let mut ctx = EditorContext {
            clipboard: &mut self.clipboard,
            keybar: &mut self.keybar,
            db: &db,
            active_name: name_on(&self.panels[self.active]),
            passive_name: name_on(&self.panels[1 - self.active]),
        };
        let exit = editor::run(&mut self.tui, path, &mut ctx)?;
        self.reload_all();
        if let Some(name) = path.file_name() {
            if path.parent() == Some(self.panels[self.active].path.as_path()) {
                let name = name.to_os_string();
                self.panel().goto_name(&name);
            }
        }
        self.draw_main();
        match exit {
            EditorExit::Close => Ok(()),
            EditorExit::View => self.view_file(path),
            EditorExit::NewFile => self.new_file(),
            EditorExit::GoToFile => self.jump_to(path),
            EditorExit::History => self.view_history(),
        }
    }

    fn new_file(&mut self) -> io::Result<()> {
        let Some(name) = dialog::prompt(&mut self.tui, "Edit", "File name:", "")? else {
            return Ok(());
        };
        if name.is_empty() {
            return Ok(());
        }
        let path = util::resolve_path(&self.panels[self.active].path, &name);
        self.edit_file(&path)
    }

    fn folder_size(&mut self, path: &Path) -> io::Result<()> {
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let flag = cancel.clone();
        let root = path.to_path_buf();
        std::thread::spawn(move || {
            let mut totals = Totals::default();
            let stop = || flag.load(Ordering::Relaxed);
            fsops::scan(&root, &stop, &mut totals);
            let _ = tx.send(totals);
        });
        let backdrop = self.tui.screen.clone();
        loop {
            if let Ok(totals) = rx.try_recv() {
                self.tui.screen = backdrop;
                let lines = vec![
                    path.display().to_string(),
                    format!("Bytes:   {}", util::group_digits(totals.bytes)),
                    format!("Files:   {}", totals.files),
                    format!("Folders: {}", totals.dirs.saturating_sub(1)),
                ];
                dialog::message(&mut self.tui, "Folder size", &lines, &["OK"], false)?;
                return Ok(());
            }
            self.tui.screen = backdrop.clone();
            dialog::draw_message(
                &mut self.tui.screen,
                "Folder size",
                &["Scanning. Esc cancels.".to_string()],
                &[],
                0,
                false,
            );
            self.tui.draw()?;
            if let Some(Input::Key(key)) = self.tui.read(OP_TICK)? {
                if key.code == KeyCode::Esc {
                    cancel.store(true, Ordering::Relaxed);
                    self.tui.screen = backdrop;
                    return Ok(());
                }
            }
        }
    }

    /// Text describing what the operation applies to.
    fn describe(sources: &[PathBuf]) -> String {
        match sources {
            [one] => one
                .file_name()
                .map_or(String::new(), |n| n.to_string_lossy().into_owned()),
            many => format!("{} items", many.len()),
        }
    }

    /// Default target of F5 and F6: the passive panel's archive or folder.
    fn other_path(&self) -> String {
        if let Some(view) = &self.panels[1 - self.active].archive {
            return format!("{}:/{}", view.file.display(), view.folder);
        }
        let mut text = self.panels[1 - self.active].path.display().to_string();
        if !text.ends_with('/') {
            text.push('/');
        }
        text
    }

    fn copy_dialog(&mut self, here: bool) -> io::Result<()> {
        if self.panels[self.active].archive.is_some() {
            return self.extract_from_panel(false);
        }
        if !here && self.panels[1 - self.active].archive.is_some() {
            return self.add_to_passive(false);
        }
        let sources: Vec<PathBuf> = if here {
            self.panels[self.active]
                .current_path()
                .into_iter()
                .collect()
        } else {
            self.panels[self.active].targets()
        };
        if sources.is_empty() {
            return Ok(());
        }
        let default = if here {
            Self::describe(&sources)
        } else {
            self.other_path()
        };
        let label = format!("Copy {} to:", Self::describe(&sources));
        let mut form = Form::new(vec![
            Field::with_history(&label, &default, Kind::Target),
            Field::check("Copy contents of symbolic links", false),
        ])
        .with_db(&self.db);
        if !form.run(&mut self.tui, "Copy")? || form.text(0).is_empty() {
            return Ok(());
        }
        let target = form.text(0);
        if here && target == default && !sources[0].to_str().is_some() {
            // The box shows a lossy name. Unedited, it would name another file.
            return Ok(());
        }
        self.db.add(Kind::Target, &target);
        let options = CopyOptions {
            follow_symlinks: form.checked(1),
        };
        let base = self.panels[self.active].path.clone();
        let items = sources.clone();
        let summary = self.run_op("Copy", move |ui| {
            fsops::copy_items(&items, &target, &base, &options, ui)
        })?;
        self.after_op(&summary);
        Ok(())
    }

    fn move_dialog(&mut self, here: bool) -> io::Result<()> {
        if self.panels[self.active].archive.is_some() {
            return self.extract_from_panel(true);
        }
        if !here && self.panels[1 - self.active].archive.is_some() {
            return self.add_to_passive(true);
        }
        let sources: Vec<PathBuf> = if here {
            self.panels[self.active]
                .current_path()
                .into_iter()
                .collect()
        } else {
            self.panels[self.active].targets()
        };
        if sources.is_empty() {
            return Ok(());
        }
        let default = if here {
            Self::describe(&sources)
        } else {
            self.other_path()
        };
        let label = format!("Rename or move {} to:", Self::describe(&sources));
        let db = self.db.clone();
        let Some(target) = dialog::prompt_history(
            &mut self.tui,
            &db,
            Kind::Target,
            "Rename/Move",
            &label,
            &default,
        )?
        else {
            return Ok(());
        };
        if target.is_empty() {
            return Ok(());
        }
        if here && target == default && !sources[0].to_str().is_some() {
            // The box shows a lossy name. Unedited, it would rename the file.
            return Ok(());
        }
        let base = self.panels[self.active].path.clone();
        let dest_name = (sources.len() == 1)
            .then(|| fsops::resolve_dest(&sources[0], &target, &base, false))
            .filter(|d| d.parent() == Some(base.as_path()))
            .and_then(|d| d.file_name().map(|n| n.to_os_string()));
        let items = sources.clone();
        let summary = self.run_op("Rename/Move", move |ui| {
            fsops::move_items(&items, &target, &base, ui)
        })?;
        self.after_op(&summary);
        if let Some(name) = dest_name {
            self.panel().goto_name(&name);
        }
        Ok(())
    }

    fn link_dialog(&mut self) -> io::Result<()> {
        let Some(source) = self.panels[self.active].current_path() else {
            return Ok(());
        };
        let name = source
            .file_name()
            .map_or(String::new(), |n| n.to_string_lossy().into_owned());
        let default = format!("{}{name}", self.other_path());
        let mut form = Form::new(vec![
            Field::Label(format!("Link to: {}", source.display())),
            Field::text("Link name:", &default),
            Field::check("Symbolic link (off: hard link)", true),
        ]);
        if !form.run(&mut self.tui, "Create link")? || form.text(1).is_empty() {
            return Ok(());
        }
        let link = util::resolve_path(&self.panels[self.active].path, &form.text(1));
        match fsops::make_link(&source, &link, form.checked(2)) {
            Ok(()) => self.reload_all(),
            Err(e) => {
                let text = format!("{}\n{e}", link.display());
                dialog::alert(&mut self.tui, "Create link", &text)?;
            }
        }
        Ok(())
    }

    fn mkdir_dialog(&mut self) -> io::Result<()> {
        let Some(name) = dialog::prompt(&mut self.tui, "Make folder", "Create the folder:", "")?
        else {
            return Ok(());
        };
        if name.is_empty() {
            return Ok(());
        }
        if let Some(view) = &self.panels[self.active].archive {
            let Some(inner) = archive::safe_name(&archive::join_inner(&view.folder, &name)) else {
                return dialog::alert(&mut self.tui, "Make folder", "That name is not allowed");
            };
            let (file, format) = (view.file.clone(), view.format);
            let summary = self.run_op("Make folder", move |ui| {
                archive::make_folder(&file, format, &inner, ui)
            })?;
            self.after_op(&summary);
            return Ok(());
        }
        let path = util::resolve_path(&self.panels[self.active].path, &name);
        match fs::create_dir_all(&path) {
            Ok(()) => {
                self.reload_all();
                if path.parent() == Some(self.panels[self.active].path.as_path()) {
                    if let Some(leaf) = path.file_name() {
                        let leaf = leaf.to_os_string();
                        self.panel().goto_name(&leaf);
                    }
                }
            }
            Err(e) => {
                let text = format!("{}\n{e}", path.display());
                dialog::alert(&mut self.tui, "Make folder", &text)?;
            }
        }
        Ok(())
    }

    /// Shift+F8: the item under the cursor, whatever is selected.
    fn delete_current(&mut self) -> io::Result<()> {
        let selected: Vec<OsString> = self.panels[self.active]
            .entries
            .iter()
            .filter(|e| e.selected)
            .map(|e| e.raw.clone())
            .collect();
        self.panel().clear_selection();
        let result = self.delete_dialog(false);
        for entry in &mut self.panel().entries {
            entry.selected = selected.contains(&entry.raw);
        }
        result
    }

    fn delete_dialog(&mut self, permanent: bool) -> io::Result<()> {
        if self.panels[self.active].archive.is_some() {
            return self.delete_in_archive();
        }
        let sources = self.panels[self.active].targets();
        if sources.is_empty() {
            return Ok(());
        }
        let question = if permanent {
            "Permanently delete, with no way to restore:"
        } else {
            "Move to the trash:"
        };
        let lines = vec![question.to_string(), Self::describe(&sources)];
        let title = if permanent {
            "Delete permanently"
        } else {
            "Delete"
        };
        let answer = dialog::message(
            &mut self.tui,
            title,
            &lines,
            &["Delete", "Cancel"],
            permanent,
        )?;
        if answer != Some(0) {
            return Ok(());
        }
        let items = sources.clone();
        let summary = self.run_op(title, move |ui| fsops::delete_items(&items, permanent, ui))?;
        self.after_op(&summary);
        Ok(())
    }

    fn attributes_dialog(&mut self) -> io::Result<()> {
        let panel = &self.panels[self.active];
        let sources = panel.targets();
        if sources.is_empty() {
            return Ok(());
        }
        let single = (sources.len() == 1)
            .then(|| panel.current().cloned())
            .flatten();
        let (mode, owner, group): (String, String, String) =
            match (&single, fs::symlink_metadata(&sources[0])) {
                (Some(_), Ok(meta)) => (
                    format!("{:04o}", meta.mode() & 0o7777),
                    fsops::user_name(meta.uid()),
                    fsops::group_name(meta.gid()),
                ),
                _ => Default::default(),
            };
        let mut form = Form::new(vec![
            Field::Label(Self::describe(&sources)),
            Field::text("Permissions (octal, empty keeps them):", &mode),
            Field::text("Owner (empty keeps it):", &owner),
            Field::text("Group (empty keeps it):", &group),
            Field::check("Apply to folder contents", false),
        ]);
        if !form.run(&mut self.tui, "Attributes")? {
            return Ok(());
        }
        let mut change = AttrChange {
            recursive: form.checked(4),
            ..Default::default()
        };
        let (mode_text, owner_text, group_text) = (form.text(1), form.text(2), form.text(3));
        // Only what the user changed is applied, recursively or not. Prefilled
        // values describe the first item and must not spread to the others.
        if field_changed(&mode_text, &mode) {
            let Some(value) = fsops::parse_mode(&mode_text) else {
                return dialog::alert(
                    &mut self.tui,
                    "Attributes",
                    "Permissions must be octal, like 644",
                );
            };
            change.mode = Some(value);
        }
        if field_changed(&owner_text, &owner) {
            let Some(value) = fsops::lookup_user(owner_text.trim()) else {
                return dialog::alert(
                    &mut self.tui,
                    "Attributes",
                    &format!("Unknown user: {owner_text}"),
                );
            };
            change.uid = Some(value);
        }
        if field_changed(&group_text, &group) {
            let Some(value) = fsops::lookup_group(group_text.trim()) else {
                return dialog::alert(
                    &mut self.tui,
                    "Attributes",
                    &format!("Unknown group: {group_text}"),
                );
            };
            change.gid = Some(value);
        }
        let items = sources.clone();
        let summary = self.run_op("Attributes", move |ui| {
            fsops::set_attributes(&items, &change, ui)
        })?;
        self.after_op(&summary);
        Ok(())
    }

    /// Extracts the item under the cursor to a private folder.
    fn extract_current_to_scratch(&mut self) -> io::Result<Option<(ScratchDir, PathBuf)>> {
        let panel = &self.panels[self.active];
        let (Some(view), Some(entry)) = (&panel.archive, panel.current()) else {
            return Ok(None);
        };
        let (file, format, folder) = (view.file.clone(), view.format, view.folder.clone());
        let name = entry.name.clone();
        let scratch = match ScratchDir::new() {
            Ok(scratch) => scratch,
            Err(e) => {
                let text = e.to_string();
                dialog::alert(&mut self.tui, "Archive", &text)?;
                return Ok(None);
            }
        };
        let request = ExtractRequest {
            items: vec![archive::join_inner(&folder, &name)],
            base: folder,
            dest: scratch.path().to_path_buf(),
        };
        let summary = self.run_op("Extract", move |ui| {
            archive::extract(&file, format, &request, ui)
        })?;
        let path = scratch.path().join(&name);
        if summary.cancelled || fs::symlink_metadata(&path).is_err() {
            return Ok(None);
        }
        Ok(Some((scratch, path)))
    }

    /// F3 inside an archive: views a temporary copy.
    fn view_in_archive(&mut self) -> io::Result<()> {
        let Some((scratch, path)) = self.extract_current_to_scratch()? else {
            return Ok(());
        };
        let db = self.db.clone();
        let exit = viewer::run(
            &mut self.tui,
            &path,
            &mut self.keybar,
            &mut self.clipboard,
            &db,
        );
        self.draw_main();
        drop(scratch);
        match exit {
            Ok(ViewerExit::Edit) => self.edit_in_archive(),
            Ok(_) => Ok(()),
            Err(e) => dialog::alert(&mut self.tui, "Viewer", &e.to_string()),
        }
    }

    /// F4 inside an archive: edits a temporary copy and offers to put it back.
    fn edit_in_archive(&mut self) -> io::Result<()> {
        let Some((_scratch, path)) = self.extract_current_to_scratch()? else {
            return Ok(());
        };
        let before = fs::read(&path).ok();
        let db = self.db.clone();
        let mut ctx = EditorContext {
            clipboard: &mut self.clipboard,
            keybar: &mut self.keybar,
            db: &db,
            active_name: None,
            passive_name: None,
        };
        editor::run(&mut self.tui, &path, &mut ctx)?;
        self.draw_main();
        if fs::read(&path).ok() == before {
            return Ok(());
        }
        let lines = vec!["The file changed. Update it in the archive?".to_string()];
        let answer = dialog::message(
            &mut self.tui,
            "Archive",
            &lines,
            &["Update", "Discard"],
            false,
        )?;
        if answer != Some(0) {
            return Ok(());
        }
        let Some(view) = &self.panels[self.active].archive else {
            return Ok(());
        };
        let (file, format, folder) = (view.file.clone(), view.format, view.folder.clone());
        let summary = self.run_op("Update archive", move |ui| {
            archive::update_file(&file, format, &folder, &path, ui)
        })?;
        self.after_op(&summary);
        Ok(())
    }

    /// F5 and F6 inside an archive.
    fn extract_from_panel(&mut self, moving: bool) -> io::Result<()> {
        let panel = &self.panels[self.active];
        let Some(view) = &panel.archive else {
            return Ok(());
        };
        let items = panel.archive_targets();
        if items.is_empty() {
            return Ok(());
        }
        let (file, format, folder) = (view.file.clone(), view.format, view.folder.clone());
        let base_dir = panel.path.clone();
        let default = self.other_path();
        let what = match items.as_slice() {
            [one] => one.rsplit('/').next().unwrap_or(one).to_string(),
            many => format!("{} items", many.len()),
        };
        let verb = if moving { "Move" } else { "Extract" };
        let db = self.db.clone();
        let Some(target) = dialog::prompt_history(
            &mut self.tui,
            &db,
            Kind::Target,
            verb,
            &format!("{verb} {what} to:"),
            &default,
        )?
        else {
            return Ok(());
        };
        if target.is_empty() {
            return Ok(());
        }
        let passive = self.panels[1 - self.active]
            .archive
            .as_ref()
            .filter(|_| target == default)
            .map(|v| (v.file.clone(), v.format, v.folder.clone()));
        if passive.as_ref().is_some_and(|(other, _, _)| *other == file) {
            let text = "The source and target are the same archive";
            return dialog::alert(&mut self.tui, "Archive", text);
        }
        let dest = util::resolve_path(&base_dir, &target);
        let summary = self.run_op(verb, move |ui| {
            let mut request = ExtractRequest {
                items: items.clone(),
                base: folder,
                dest,
            };
            let mut scratch = None;
            if passive.is_some() {
                match ScratchDir::new() {
                    Ok(dir) => {
                        request.dest = dir.path().to_path_buf();
                        scratch = Some(dir);
                    }
                    Err(e) => {
                        ui.error(&file, &e.to_string());
                        return OpSummary {
                            skipped: 1,
                            ..Default::default()
                        };
                    }
                }
            }
            let mut summary = archive::extract(&file, format, &request, ui);
            if let (Some((other, other_format, other_folder)), Some(dir), false, 0) =
                (passive, &scratch, summary.cancelled, summary.skipped)
            {
                let sources: Vec<PathBuf> = fs::read_dir(dir.path())
                    .map(|names| names.flatten().map(|e| e.path()).collect())
                    .unwrap_or_default();
                summary = archive::add(&other, other_format, &other_folder, &sources, ui);
            }
            // Only remove what is known to have arrived.
            if moving && !summary.cancelled && summary.skipped == 0 {
                return archive::delete(&file, format, &items, ui);
            }
            summary
        })?;
        self.after_op(&summary);
        Ok(())
    }

    /// F5 and F6 with an archive on the passive panel.
    fn add_to_passive(&mut self, moving: bool) -> io::Result<()> {
        let sources = self.panels[self.active].targets();
        let Some(view) = &self.panels[1 - self.active].archive else {
            return Ok(());
        };
        if sources.is_empty() {
            return Ok(());
        }
        let (file, format, folder) = (view.file.clone(), view.format, view.folder.clone());
        let verb = if moving {
            "Move to archive"
        } else {
            "Add to archive"
        };
        let lines = vec![
            format!("{verb}: {}", Self::describe(&sources)),
            self.other_path(),
        ];
        let answer = dialog::message(&mut self.tui, verb, &lines, &["OK", "Cancel"], false)?;
        if answer != Some(0) {
            return Ok(());
        }
        let summary = self.run_op(verb, move |ui| {
            add_then_remove(&file, format, &folder, &sources, moving, ui)
        })?;
        self.after_op(&summary);
        Ok(())
    }

    fn delete_in_archive(&mut self) -> io::Result<()> {
        let panel = &self.panels[self.active];
        let Some(view) = &panel.archive else {
            return Ok(());
        };
        let items = panel.archive_targets();
        if items.is_empty() {
            return Ok(());
        }
        let (file, format) = (view.file.clone(), view.format);
        let lines = vec![
            "Delete from the archive, with no way to restore:".to_string(),
            match items.as_slice() {
                [one] => one.clone(),
                many => format!("{} items", many.len()),
            },
        ];
        let answer = dialog::message(
            &mut self.tui,
            "Delete from archive",
            &lines,
            &["Delete", "Cancel"],
            true,
        )?;
        if answer != Some(0) {
            return Ok(());
        }
        let summary = self.run_op("Delete from archive", move |ui| {
            archive::delete(&file, format, &items, ui)
        })?;
        self.after_op(&summary);
        Ok(())
    }

    /// Shift+F1: packs the selection into a new or existing archive.
    fn archive_add(&mut self) -> io::Result<()> {
        if self.panels[self.active].archive.is_some() {
            let text = "Leave the archive first, or use F5 from the other panel";
            return dialog::alert(&mut self.tui, "Add to archive", text);
        }
        let sources = self.panels[self.active].targets();
        if sources.is_empty() {
            return Ok(());
        }
        let base = self.panels[self.active].path.clone();
        let stem = match sources.as_slice() {
            [one] => one
                .file_name()
                .map_or(String::new(), |n| n.to_string_lossy().into_owned()),
            _ => base
                .file_name()
                .map_or("archive".to_string(), |n| n.to_string_lossy().into_owned()),
        };
        let mut format = self
            .db
            .setting("archive.format")
            .and_then(|ext| FORMATS.into_iter().find(|f| f.extension() == ext))
            .unwrap_or(Format::Zip);
        let name = format!("{stem}.{}", format.extension());
        let mut form = Form::new(vec![
            Field::with_history("&Archive name", &name, Kind::Target),
            Field::check("&Move files to the archive", false),
        ])
        .with_buttons(&["Add", "&Format", "Cancel"])
        .with_db(&self.db);
        let title = format!("Add {} to archive", Self::describe(&sources));
        loop {
            match form.run_buttons(&mut self.tui, &title)? {
                Some(0) => break,
                Some(1) => {
                    let items: Vec<String> = FORMATS
                        .iter()
                        .map(|f| match f.tool() {
                            Some(tool) if !f.is_available() => {
                                format!("{} (needs {tool})", f.extension())
                            }
                            _ => f.extension().to_string(),
                        })
                        .collect();
                    let at = FORMATS.iter().position(|f| *f == format).unwrap_or(0);
                    if let Some(i) = dialog::menu(&mut self.tui, "Format", &items, at)? {
                        format = FORMATS[i];
                        let text = form.text(0);
                        let renamed = format!("{}.{}", archive::stem(&text), format.extension());
                        form.set_text(0, &renamed);
                    }
                }
                _ => return Ok(()),
            }
        }
        let mut text = form.text(0);
        if text.is_empty() {
            return Ok(());
        }
        match archive::detect(Path::new(&text)) {
            Some(named) => format = named,
            None => text = format!("{text}.{}", format.extension()),
        }
        if !format.is_available() {
            let message = format!("{} is not installed", format.tool().unwrap_or_default());
            return dialog::alert(&mut self.tui, "Add to archive", &message);
        }
        self.db.add(Kind::Target, &text);
        self.db.set_setting("archive.format", format.extension());
        let file = util::resolve_path(&base, &text);
        let moving = form.checked(1);
        let target = file.clone();
        let summary = self.run_op("Add to archive", move |ui| {
            add_then_remove(&target, format, "", &sources, moving, ui)
        })?;
        self.after_op(&summary);
        if file.parent() == Some(base.as_path()) {
            if let Some(leaf) = file.file_name() {
                let leaf = leaf.to_os_string();
                self.panel().goto_name(&leaf);
            }
        }
        Ok(())
    }

    /// Archives chosen on a normal panel.
    fn chosen_archives(&self) -> Vec<(PathBuf, Format)> {
        self.panels[self.active]
            .targets()
            .into_iter()
            .filter_map(|path| archive::detect(&path).map(|format| (path, format)))
            .collect()
    }

    /// Shift+F2: unpacks the chosen archives, or the items in the open one.
    fn archive_extract(&mut self) -> io::Result<()> {
        if self.panels[self.active].archive.is_some() {
            return self.extract_from_panel(false);
        }
        let archives = self.chosen_archives();
        if archives.is_empty() {
            return dialog::alert(&mut self.tui, "Extract", "Choose an archive first");
        }
        let passive = &self.panels[1 - self.active];
        let mut default = passive.path.display().to_string();
        if !default.ends_with('/') {
            default.push('/');
        }
        let mut form = Form::new(vec![
            Field::with_history("Extract &to", &default, Kind::Target),
            Field::check("&Into a folder named after the archive", false),
        ])
        .with_db(&self.db);
        if !form.run(&mut self.tui, "Extract")? || form.text(0).is_empty() {
            return Ok(());
        }
        self.db.add(Kind::Target, &form.text(0));
        let dest = util::resolve_path(&self.panels[self.active].path, &form.text(0));
        let own_folder = form.checked(1);
        let summary = self.run_op("Extract", move |ui| {
            let mut total = OpSummary::default();
            for (file, format) in &archives {
                let mut target = dest.clone();
                if own_folder {
                    let name = file.file_name().unwrap_or_default().to_string_lossy();
                    target.push(archive::stem(&name));
                }
                let request = ExtractRequest {
                    dest: target,
                    ..Default::default()
                };
                let one = archive::extract(file, *format, &request, ui);
                total.done += one.done;
                total.skipped += one.skipped;
                total.cancelled = one.cancelled;
                if one.cancelled {
                    break;
                }
            }
            total
        })?;
        self.after_op(&summary);
        Ok(())
    }

    /// Shift+F3: commands for the current archive.
    fn archive_commands(&mut self) -> io::Result<()> {
        let panel = &self.panels[self.active];
        let target = match &panel.archive {
            Some(view) => Some((view.file.clone(), view.format)),
            None => self.chosen_archives().into_iter().next(),
        };
        let Some((file, format)) = target else {
            return dialog::alert(&mut self.tui, "Archive commands", "Choose an archive first");
        };
        let items = vec!["Test archive".to_string()];
        if dialog::menu(&mut self.tui, "Archive commands", &items, 0)? != Some(0) {
            return Ok(());
        }
        if !format.is_available() {
            let text = format!("{} is not installed", format.tool().unwrap_or_default());
            return dialog::alert(&mut self.tui, "Test archive", &text);
        }
        self.show_wait("Test archive", "Reading the whole archive...")?;
        let text = match archive::test(&file, format) {
            Ok(count) => format!("No errors found in {count} items"),
            Err(e) => format!("The archive is damaged:\n{e}"),
        };
        dialog::alert(&mut self.tui, "Test archive", &text)
    }

    /// Refreshes panels. The selection clears only after complete success, so
    /// a partial failure leaves it in place for another attempt.
    fn after_op(&mut self, summary: &OpSummary) {
        self.reload_all();
        if !summary.cancelled && summary.skipped == 0 {
            self.panel().clear_selection();
        }
    }

    fn group_dialog(&mut self, select: bool) -> io::Result<()> {
        let title = if select { "Select" } else { "Unselect" };
        let masks = self.masks.clone();
        let db = self.db.clone();
        let Some(text) = dialog::prompt_history(
            &mut self.tui,
            &db,
            Kind::Mask,
            title,
            "Mask (several: comma separated):",
            &masks,
        )?
        else {
            return Ok(());
        };
        if !text.is_empty() {
            self.panel().select_mask(&text, select);
            self.masks = text;
        }
        Ok(())
    }

    /// Runs an operation on a worker thread, showing progress and dialogs.
    fn run_op<F>(&mut self, title: &str, work: F) -> io::Result<OpSummary>
    where
        F: FnOnce(&mut ChannelUi) -> OpSummary + Send + 'static,
    {
        let (to_ui, from_worker) = mpsc::channel();
        let (to_worker, from_ui) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut ui = ChannelUi {
            tx: to_ui.clone(),
            rx: from_ui,
            cancel: cancel.clone(),
            last: None,
        };
        let worker = std::thread::spawn(move || {
            let summary = work(&mut ui);
            let _ = to_ui.send(ToUi::Done(summary));
        });
        self.draw_main();
        let backdrop = self.tui.screen.clone();
        let mut progress = Progress::default();
        let summary = loop {
            let mut done = None;
            loop {
                match from_worker.try_recv() {
                    Ok(ToUi::Progress(p)) => progress = p,
                    Ok(ToUi::Done(summary)) => {
                        done = Some(summary);
                        break;
                    }
                    Ok(ToUi::Overwrite(src, dst)) => {
                        self.tui.screen = backdrop.clone();
                        let choice = self.ask_overwrite(&src, &dst)?;
                        let _ = to_worker.send(Reply::Overwrite(choice));
                    }
                    Ok(ToUi::Error(path, text)) => {
                        self.tui.screen = backdrop.clone();
                        let choice = self.ask_error(&path, &text)?;
                        let _ = to_worker.send(Reply::Error(choice));
                    }
                    Ok(ToUi::Trash(path)) => {
                        self.tui.screen = backdrop.clone();
                        let choice = self.ask_trash(&path)?;
                        let _ = to_worker.send(Reply::Trash(choice));
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        done = Some(OpSummary {
                            cancelled: true,
                            ..Default::default()
                        });
                        break;
                    }
                }
            }
            if let Some(summary) = done {
                break summary;
            }
            self.tui.screen = backdrop.clone();
            draw_progress(&mut self.tui.screen, title, &progress);
            self.tui.draw()?;
            if let Some(Input::Key(key)) = self.tui.read(OP_TICK)? {
                if key.code == KeyCode::Esc {
                    cancel.store(true, Ordering::Relaxed);
                }
            }
        };
        let _ = worker.join();
        Ok(summary)
    }

    fn ask_overwrite(&mut self, src: &Path, dst: &Path) -> io::Result<OverwriteChoice> {
        let describe = |path: &Path| match fs::symlink_metadata(path) {
            Ok(meta) => format!(
                "{} bytes, {}",
                util::group_digits(meta.len()),
                util::format_datetime(meta.mtime())
            ),
            Err(_) => String::new(),
        };
        let lines = vec![
            "The destination already exists:".to_string(),
            dst.display().to_string(),
            format!("New:      {}", describe(src)),
            format!("Existing: {}", describe(dst)),
        ];
        let answer = dialog::message(
            &mut self.tui,
            "Warning",
            &lines,
            &["Overwrite", "Skip", "All", "None", "Cancel"],
            true,
        )?;
        Ok(match answer {
            Some(0) => OverwriteChoice::Overwrite,
            Some(1) => OverwriteChoice::Skip,
            Some(2) => OverwriteChoice::OverwriteAll,
            Some(3) => OverwriteChoice::SkipAll,
            _ => OverwriteChoice::Cancel,
        })
    }

    fn ask_error(&mut self, path: &Path, text: &str) -> io::Result<ErrorChoice> {
        let lines = vec![path.display().to_string(), text.to_string()];
        let answer = dialog::message(
            &mut self.tui,
            "Error",
            &lines,
            &["Retry", "Skip", "Ignore all", "Cancel"],
            true,
        )?;
        Ok(match answer {
            Some(0) => ErrorChoice::Retry,
            Some(1) => ErrorChoice::Skip,
            Some(2) => ErrorChoice::SkipAll,
            _ => ErrorChoice::Cancel,
        })
    }

    fn ask_trash(&mut self, path: &Path) -> io::Result<TrashChoice> {
        let lines = vec![
            "This cannot go to the trash:".to_string(),
            path.display().to_string(),
            "Delete it permanently?".to_string(),
        ];
        let answer = dialog::message(
            &mut self.tui,
            "Delete",
            &lines,
            &["Delete", "All", "Skip", "Cancel"],
            true,
        )?;
        Ok(match answer {
            Some(0) => TrashChoice::Delete,
            Some(1) => TrashChoice::DeleteAll,
            Some(2) => TrashChoice::Skip,
            _ => TrashChoice::Cancel,
        })
    }

    fn find_dialog(&mut self) -> io::Result<()> {
        let mut form = Form::new(vec![
            Field::with_history(
                "File mask (several: comma separated):",
                &self.find_mask,
                Kind::FindMask,
            ),
            Field::with_history(
                "Containing text (optional):",
                &self.find_text,
                Kind::FindText,
            ),
            Field::check("Ignore case of text", true),
        ])
        .with_db(&self.db);
        if !form.run(&mut self.tui, "Find files")? {
            return Ok(());
        }
        self.find_mask = form.text(0);
        self.find_text = form.text(1);
        self.db.add(Kind::FindMask, &self.find_mask);
        self.db.add(Kind::FindText, &self.find_text);
        let masks = if self.find_mask.is_empty() {
            "*".to_string()
        } else {
            self.find_mask.clone()
        };
        let text = self.find_text.clone();
        let ignore_case = form.checked(2);
        let root = self.panels[self.active].path.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        let (tx, rx) = mpsc::channel::<Option<PathBuf>>();
        let worker_tx = tx.clone();
        std::thread::spawn(move || {
            let stop = || flag.load(Ordering::Relaxed);
            let needle = (!text.is_empty()).then(|| text.into_bytes());
            let mut sink = |path: PathBuf| {
                let _ = worker_tx.send(Some(path));
            };
            fsops::find_files(
                &root,
                &masks,
                needle.as_deref(),
                ignore_case,
                &stop,
                &mut sink,
                &mut |_| {},
            );
            let _ = worker_tx.send(None);
        });
        drop(tx);
        self.draw_main();
        let backdrop = self.tui.screen.clone();
        let mut hits: Vec<PathBuf> = Vec::new();
        'search: loop {
            loop {
                match rx.try_recv() {
                    Ok(Some(path)) => hits.push(path),
                    Ok(None) | Err(mpsc::TryRecvError::Disconnected) => break 'search,
                    Err(mpsc::TryRecvError::Empty) => break,
                }
            }
            self.tui.screen = backdrop.clone();
            let lines = vec![format!("Found: {}. Esc stops the search.", hits.len())];
            dialog::draw_message(&mut self.tui.screen, "Find files", &lines, &[], 0, false);
            self.tui.draw()?;
            if let Some(Input::Key(key)) = self.tui.read(OP_TICK)? {
                if key.code == KeyCode::Esc {
                    cancel.store(true, Ordering::Relaxed);
                    break 'search;
                }
            }
        }
        self.tui.screen = backdrop;
        if hits.is_empty() {
            let lines = vec!["No files found.".to_string()];
            dialog::message(&mut self.tui, "Find files", &lines, &["OK"], false)?;
            return Ok(());
        }
        let items: Vec<String> = hits.iter().map(|p| util::tilde(p)).collect();
        let title = format!("Found {}. Enter goes to the file", hits.len());
        if let Some(i) = dialog::menu(&mut self.tui, &title, &items, 0)? {
            self.jump_to(&hits[i])?;
        }
        Ok(())
    }

    /// Opens the folder containing a path with the cursor on it.
    fn jump_to(&mut self, path: &Path) -> io::Result<()> {
        let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
            return Ok(());
        };
        let name = name.to_os_string();
        self.navigate(parent, Some(&name))
    }

    fn command_history(&mut self) -> io::Result<()> {
        let items: Vec<String> = self.history.iter().rev().cloned().collect();
        if let Some(i) = dialog::menu(&mut self.tui, "Command history", &items, 0)? {
            self.cmd.set(&items[i]);
        }
        Ok(())
    }

    fn folder_history(&mut self) -> io::Result<()> {
        let paths: Vec<PathBuf> = self.folders.iter().rev().cloned().collect();
        let items: Vec<String> = paths.iter().map(|p| util::tilde(p)).collect();
        if let Some(i) = dialog::menu(&mut self.tui, "Folder history", &items, 0)? {
            self.navigate(&paths[i], None)?;
        }
        Ok(())
    }

    fn view_history(&mut self) -> io::Result<()> {
        let entries: Vec<(PathBuf, bool)> = self.viewed.iter().rev().cloned().collect();
        let items: Vec<String> = entries
            .iter()
            .map(|(p, edited)| {
                format!(
                    "{} {}",
                    if *edited { "Edit:" } else { "View:" },
                    util::tilde(p)
                )
            })
            .collect();
        if let Some(i) = dialog::menu(&mut self.tui, "File view history", &items, 0)? {
            let (path, edited) = entries[i].clone();
            if edited {
                self.edit_file(&path)?;
            } else {
                self.view_file(&path)?;
            }
        }
        Ok(())
    }

    fn location_menu(&mut self, side: usize) -> io::Result<()> {
        let mut paths = vec![PathBuf::from("/")];
        if let Some(home) = util::home_dir() {
            paths.push(home);
        }
        paths.push(self.panels[1 - side].path.clone());
        paths.extend(mount_points());
        let mut unique: Vec<PathBuf> = Vec::new();
        for path in paths {
            if !unique.contains(&path) {
                unique.push(path);
            }
        }
        let items: Vec<String> = unique.iter().map(|p| util::tilde(p)).collect();
        let title = if side == 0 {
            "Left location"
        } else {
            "Right location"
        };
        if let Some(i) = dialog::menu(&mut self.tui, title, &items, 0)? {
            self.panels[side].visible = true;
            self.navigate_side(side, &unique[i], None)?;
            self.active = side;
        }
        Ok(())
    }

    fn sort_menu(&mut self) -> io::Result<()> {
        let orders = [
            ("Name", "Ctrl+F3", Sort::Name),
            ("Extension", "Ctrl+F4", Sort::Ext),
            ("Modified", "Ctrl+F5", Sort::Mtime),
            ("Size", "Ctrl+F6", Sort::Size),
            ("Unsorted", "Ctrl+F7", Sort::Unsorted),
            ("Changed", "Ctrl+F8", Sort::Ctime),
            ("Accessed", "Ctrl+F9", Sort::Atime),
            ("Owner", "Ctrl+F11", Sort::Owner),
        ];
        let items: Vec<String> = orders
            .iter()
            .map(|(n, k, _)| format!("{n:<12}{k}"))
            .collect();
        let current = orders
            .iter()
            .position(|(_, _, s)| *s == self.panels[self.active].sort)
            .unwrap_or(0);
        if let Some(i) = dialog::menu(&mut self.tui, "Sort by", &items, current)? {
            self.panel().set_sort(orders[i].2);
        }
        Ok(())
    }

    fn help(&mut self) -> io::Result<()> {
        dialog::help_list(&mut self.tui, "Help", HELP)
    }

    /// F9: a menu bar with drop-down lists.
    fn top_menu(&mut self) -> io::Result<()> {
        let titles = ["Left", "Files", "Commands", "Right"];
        let backdrop = self.tui.screen.clone();
        let mut bar = usize::from(self.active == 1) * 3;
        let mut item = 0usize;
        let chosen = loop {
            let entries = match bar {
                1 => files_menu(),
                2 => commands_menu(),
                _ => panel_menu(),
            };
            item = item.min(entries.len() - 1);
            self.tui.screen = backdrop.clone();
            self.tui.screen.cursor = None;
            draw_top_menu(&mut self.tui.screen, &titles, bar, &entries, item);
            self.tui.draw()?;
            let Some(Input::Key(key)) = self.tui.read(TICK)? else {
                continue;
            };
            match key.code {
                KeyCode::Esc | KeyCode::F(9) | KeyCode::F(10) => break None,
                KeyCode::Left => {
                    bar = (bar + titles.len() - 1) % titles.len();
                    item = 0;
                }
                KeyCode::Right | KeyCode::Tab => {
                    bar = (bar + 1) % titles.len();
                    item = 0;
                }
                KeyCode::Up => item = (item + entries.len() - 1) % entries.len(),
                KeyCode::Down => item = (item + 1) % entries.len(),
                KeyCode::Home => item = 0,
                KeyCode::End => item = entries.len() - 1,
                KeyCode::Enter => break Some((bar, entries[item].2)),
                _ => {}
            }
        };
        self.tui.screen = backdrop;
        let Some((bar, action)) = chosen else {
            return Ok(());
        };
        // Left and Right apply to that panel, whichever one is active.
        match (bar, action) {
            (0 | 3, Action::SetView(_) | Action::SetSort(_) | Action::SortMenu) => {
                let saved = self.active;
                self.active = usize::from(bar == 3);
                let result = self.perform(action);
                self.active = saved;
                result
            }
            _ => self.perform(action),
        }
    }
}

/// True when a prefilled attribute field holds a new value.
fn field_changed(text: &str, prefilled: &str) -> bool {
    !text.trim().is_empty() && text.trim() != prefilled.trim()
}

/// Adds files to an archive. With `moving`, the sources are removed only after
/// nothing was skipped and the archive reads back without errors.
fn add_then_remove(
    file: &Path,
    format: Format,
    folder: &str,
    sources: &[PathBuf],
    moving: bool,
    ui: &mut ChannelUi,
) -> OpSummary {
    let summary = archive::add(file, format, folder, sources, ui);
    if !moving || summary.cancelled || summary.skipped > 0 {
        return summary;
    }
    if let Err(e) = archive::test(file, format) {
        ui.error(file, &e.to_string());
        return OpSummary {
            skipped: 1,
            ..summary
        };
    }
    fsops::delete_items(sources, true, ui)
}

/// Maps the remaining hot keys to actions.
fn key_action(key: Key, enhanced: bool) -> Option<Action> {
    let shift_ctrl = CTRL | SHIFT;
    Some(match (key.mods, key.code) {
        (NONE, KeyCode::F(1)) => Action::Help,
        (SHIFT, KeyCode::F(1)) => Action::ArchiveAdd,
        (SHIFT, KeyCode::F(2)) => Action::ArchiveExtract,
        (SHIFT, KeyCode::F(3)) => Action::ArchiveCommands,
        (NONE, KeyCode::F(3)) => Action::View,
        (NONE, KeyCode::F(4)) => Action::Edit,
        (SHIFT, KeyCode::F(4)) => Action::NewFile,
        (NONE, KeyCode::F(5)) => Action::Copy,
        (SHIFT, KeyCode::F(5)) => Action::CopyHere,
        (NONE, KeyCode::F(6)) => Action::Move,
        (SHIFT, KeyCode::F(6)) => Action::RenameHere,
        (ALT, KeyCode::F(6)) => Action::Link,
        (NONE, KeyCode::F(7)) => Action::MkDir,
        (ALT, KeyCode::F(7)) => Action::FindFiles,
        (NONE, KeyCode::F(8)) => Action::Trash,
        (SHIFT, KeyCode::F(8)) => Action::DeleteCurrent,
        (ALT, KeyCode::F(8)) => Action::CommandHistory,
        (NONE, KeyCode::F(10)) => Action::Quit,
        (ALT, KeyCode::F(11)) => Action::ViewHistory,
        (ALT, KeyCode::F(12)) => Action::FolderHistory,
        (ALT, KeyCode::F(1)) => Action::Location(0),
        (ALT, KeyCode::F(2)) => Action::Location(1),
        (CTRL, KeyCode::F(1)) => Action::ToggleLeft,
        (CTRL, KeyCode::F(2)) => Action::ToggleRight,
        (CTRL, KeyCode::F(3)) => Action::SetSort(Sort::Name),
        (CTRL, KeyCode::F(4)) => Action::SetSort(Sort::Ext),
        (CTRL, KeyCode::F(5)) => Action::SetSort(Sort::Mtime),
        (CTRL, KeyCode::F(6)) => Action::SetSort(Sort::Size),
        (CTRL, KeyCode::F(7)) => Action::SetSort(Sort::Unsorted),
        (CTRL, KeyCode::F(8)) => Action::SetSort(Sort::Ctime),
        (CTRL, KeyCode::F(9)) => Action::SetSort(Sort::Atime),
        (CTRL, KeyCode::F(11)) => Action::SetSort(Sort::Owner),
        (CTRL, KeyCode::F(12)) => Action::SortMenu,
        (CTRL, KeyCode::Char('o')) => Action::Console,
        (CTRL, KeyCode::Char('p')) => Action::ToggleInactive,
        (CTRL, KeyCode::Char('u')) => Action::SwapPanels,
        (CTRL, KeyCode::Char('r')) => Action::Reread,
        (CTRL, KeyCode::Char('h')) => Action::ToggleHidden,
        (CTRL, KeyCode::Char('b')) => Action::ToggleKeybar,
        (CTRL, KeyCode::Char('l')) => Action::Info,
        (CTRL, KeyCode::Char('q')) => Action::QuickView,
        (CTRL, KeyCode::Char('a')) => Action::Attributes,
        (m, KeyCode::Char('a')) if m == shift_ctrl => Action::SelectAll,
        (CTRL, KeyCode::Char('1')) => Action::SetView(View::Brief),
        (CTRL, KeyCode::Char('2')) => Action::SetView(View::Medium),
        (CTRL, KeyCode::Char('3')) => Action::SetView(View::Full),
        (CTRL, KeyCode::Char('4')) if enhanced => Action::SetView(View::Wide),
        _ => return None,
    })
}

fn draw_top_menu(
    s: &mut screen::Screen,
    titles: &[&str],
    bar: usize,
    entries: &[MenuItem],
    item: usize,
) {
    s.fill(0, 0, s.w, 1, ' ', theme::STATUS);
    let mut x = 2;
    let mut drop_x = 0;
    for (i, title) in titles.iter().enumerate() {
        let text = format!(" {title} ");
        if i == bar {
            drop_x = x;
        }
        let style = if i == bar {
            theme::KEY_NUM
        } else {
            theme::STATUS
        };
        x = s.put(x, 0, &text, style) + 2;
    }
    let name_w = entries.iter().map(|e| str_width(e.0)).max().unwrap_or(0);
    let key_w = entries.iter().map(|e| str_width(e.1)).max().unwrap_or(0);
    let w = (name_w + key_w + 7) as u16;
    let h = entries.len() as u16 + 2;
    let x = drop_x.min(s.w.saturating_sub(w));
    s.frame(x, 1, w, h, true, theme::DIALOG);
    for (i, entry) in entries.iter().enumerate() {
        let style = if i == item {
            theme::DIALOG_FOCUS
        } else {
            theme::DIALOG
        };
        let line = format!(" {}  {} ", fit(entry.0, name_w), fit(entry.1, key_w));
        s.put(x + 1, 2 + i as u16, &fit(&line, w as usize - 2), style);
    }
}

fn draw_progress(s: &mut screen::Screen, title: &str, p: &Progress) {
    let bar_width = (s.w as usize).saturating_sub(16).clamp(10, 60);
    let percent = if p.bytes_total > 0 {
        (p.bytes_done as u128 * 100 / p.bytes_total as u128).min(100) as usize
    } else if p.items_total > 0 {
        (p.items_done as u128 * 100 / p.items_total as u128).min(100) as usize
    } else {
        0
    };
    let filled = percent * bar_width / 100;
    let bar: String = "█".repeat(filled) + &"░".repeat(bar_width - filled);
    let mut lines = vec![p.from.display().to_string()];
    if !p.to.as_os_str().is_empty() {
        lines.push(format!("to {}", p.to.display()));
    }
    lines.push(format!("{bar} {percent:>3}%"));
    lines.push(format!(
        "{} of {} bytes, {} of {} items",
        util::group_digits(p.bytes_done),
        util::group_digits(p.bytes_total),
        p.items_done,
        p.items_total
    ));
    lines.push("Esc cancels".to_string());
    dialog::draw_message(s, title, &lines, &[], 0, false);
}

fn draw_info(s: &mut screen::Screen, source: &Panel, x: u16, w: u16, h: u16) {
    if w < 12 || h < 3 {
        return;
    }
    s.frame(x, 0, w, h, true, theme::PANEL);
    s.title(x, 0, w, "Information", theme::PANEL);
    let mut lines: Vec<(&str, String)> = Vec::new();
    lines.push(("Folder", util::tilde(&source.path)));
    if let Some(free) = source.free_bytes {
        lines.push(("Free", util::group_digits(free)));
    }
    if let Some(entry) = source.current().filter(|e| !e.is_up) {
        let path = source.entry_path(entry);
        lines.push(("Name", entry.name.clone()));
        lines.push((
            "Mode",
            util::mode_string(entry.mode, entry.is_dir, entry.is_link),
        ));
        lines.push((
            "Owner",
            format!(
                "{}:{}",
                fsops::user_name(entry.uid),
                fsops::group_name(entry.gid)
            ),
        ));
        if !entry.is_dir {
            lines.push(("Size", util::group_digits(entry.size)));
        }
        lines.push(("Modified", util::format_datetime(entry.mtime)));
        lines.push(("Changed", util::format_datetime(entry.ctime)));
        lines.push(("Accessed", util::format_datetime(entry.atime)));
        if entry.is_link {
            if let Ok(target) = fs::read_link(&path) {
                lines.push(("Link to", target.display().to_string()));
            }
        }
    }
    let room = (w as usize).saturating_sub(13);
    for (i, (name, value)) in lines.iter().enumerate() {
        let y = 1 + i as u16;
        if y + 1 >= h {
            break;
        }
        s.put(x + 2, y, &fit(name, 9), theme::PANEL_HEADER);
        s.put(x + 11, y, &fit_left(value, room), theme::PANEL);
    }
}

fn draw_quick_view(s: &mut screen::Screen, source: &Panel, x: u16, w: u16, h: u16) {
    if w < 12 || h < 3 {
        return;
    }
    s.frame(x, 0, w, h, true, theme::PANEL);
    s.title(x, 0, w, "Quick view", theme::PANEL);
    let Some(entry) = source.current().filter(|e| !e.is_up && !e.is_dir) else {
        return;
    };
    let path = source.entry_path(entry);
    let meta = fs::metadata(&path);
    if !meta.is_ok_and(|m| m.is_file()) {
        return;
    }
    let mut head = vec![0u8; 64 * 1024];
    let filled = fs::File::open(&path)
        .and_then(|mut f| f.read(&mut head))
        .unwrap_or(0);
    head.truncate(filled);
    let text = String::from_utf8_lossy(&head);
    let width = w as usize - 2;
    for (i, line) in text.split('\n').enumerate() {
        let y = 1 + i as u16;
        if y + 1 >= h {
            break;
        }
        let clean: String = line
            .chars()
            .map(|c| {
                if c == '\t' {
                    ' '
                } else {
                    crate::viewer::visible_char(c)
                }
            })
            .collect();
        s.put(x + 1, y, &fit(&clean, width), theme::PANEL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefilled_attribute_values_are_not_changes() {
        assert!(!field_changed("0755", "0755"));
        assert!(!field_changed(" 0755 ", "0755"));
        assert!(!field_changed("", "0755"));
        assert!(field_changed("0700", "0755"));
        assert!(field_changed("0700", ""));
    }

    #[test]
    fn quoting_is_safe_for_bash() {
        assert_eq!(shell_quote("plain.txt"), "plain.txt");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote("$(rm -rf x)"), "'$(rm -rf x)'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn file_operation_keys_match_far() {
        let key = |mods, code| key_action(Key { code, mods }, false);
        assert_eq!(key(NONE, KeyCode::F(5)), Some(Action::Copy));
        assert_eq!(key(SHIFT, KeyCode::F(6)), Some(Action::RenameHere));
        assert_eq!(key(ALT, KeyCode::F(7)), Some(Action::FindFiles));
        assert_eq!(key(CTRL, KeyCode::Char('o')), Some(Action::Console));
        assert_eq!(key(CTRL, KeyCode::Char('p')), Some(Action::ToggleInactive));
        assert_eq!(key(CTRL, KeyCode::F(6)), Some(Action::SetSort(Sort::Size)));
        assert_eq!(key(NONE, KeyCode::Char('x')), None);
    }

    #[test]
    fn every_menu_entry_has_a_label_and_key() {
        for entry in panel_menu()
            .iter()
            .chain(&files_menu())
            .chain(&commands_menu())
        {
            assert!(!entry.0.is_empty());
            assert!(!entry.1.is_empty());
        }
    }

    #[test]
    fn progress_handles_zero_totals() {
        let mut s = screen::Screen::new(80, 24);
        draw_progress(&mut s, "Copy", &Progress::default());
    }
}
