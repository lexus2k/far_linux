//! Terminal handling: a cell buffer with diff-based output, and key input.

use crate::input::{self, Reader};
use crossterm::{
    cursor,
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind,
        KeyModifiers, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
        PushKeyboardEnhancementFlags,
    },
    execute, queue,
    style::{
        Attribute, Color, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
    },
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthChar;

/// Foreground, background and intensity of a cell.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Style {
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
}

impl Style {
    pub const fn new(fg: Color, bg: Color) -> Self {
        Style {
            fg,
            bg,
            bold: false,
        }
    }
}

/// Key bar labels for each state of the Shift, Alt and Ctrl keys.
pub struct KeyBars<'a> {
    pub plain: [&'a str; 12],
    pub shift: [&'a str; 12],
    pub alt: [&'a str; 12],
    pub ctrl: [&'a str; 12],
}

/// The colour numbers a terminal cannot redefine, nearest to Far's 16 colours.
/// Numbers 0 to 15 belong to the terminal's colour scheme: Ubuntu's Windows
/// Terminal profile, for one, can draw bright yellow as brown.
pub fn fixed_color(color: Color) -> Color {
    Color::AnsiValue(match color {
        Color::Black => 16,
        Color::DarkRed => 88,
        Color::DarkGreen => 28,
        Color::DarkYellow => 100,
        Color::DarkBlue => 18,
        Color::DarkMagenta => 90,
        Color::DarkCyan => 30,
        Color::Grey => 250,
        Color::DarkGrey => 244,
        Color::Red => 196,
        Color::Green => 46,
        Color::Yellow => 226,
        Color::Blue => 21,
        Color::Magenta => 201,
        Color::Cyan => 51,
        Color::White => 231,
        other => return other,
    })
}

/// Whether the terminal has the 256 colours `fixed_color` needs.
pub fn has_fixed_colors(term: Option<&str>, colorterm: bool, windows_terminal: bool) -> bool {
    colorterm || windows_terminal || term.is_some_and(|t| t.contains("256color"))
}

/// The classic Far colour scheme.
pub mod theme {
    use super::Style;
    use crossterm::style::Color::*;

    pub const PANEL: Style = Style::new(Cyan, DarkBlue);
    pub const PANEL_DIR: Style = Style::new(White, DarkBlue);
    pub const PANEL_EXEC: Style = Style::new(Green, DarkBlue);
    pub const PANEL_LINK: Style = Style::new(Grey, DarkBlue);
    pub const PANEL_HIDDEN: Style = Style::new(DarkCyan, DarkBlue);
    pub const PANEL_ARCHIVE: Style = Style::new(Magenta, DarkBlue);
    pub const PANEL_TEMP: Style = Style::new(DarkYellow, DarkBlue);
    pub const PANEL_SELECTED: Style = Style::new(Yellow, DarkBlue);
    pub const PANEL_CURSOR: Style = Style::new(Black, DarkCyan);
    pub const PANEL_CURSOR_SELECTED: Style = Style::new(Yellow, DarkCyan);
    pub const PANEL_TITLE: Style = Style::new(Black, DarkCyan);
    pub const PANEL_HEADER: Style = Style::new(Yellow, DarkBlue);
    pub const DIALOG: Style = Style::new(Black, Grey);
    pub const DIALOG_FOCUS: Style = Style::new(Black, DarkCyan);
    pub const INPUT: Style = Style::new(Black, DarkCyan);
    pub const ERROR: Style = Style::new(White, DarkRed);
    pub const ERROR_FOCUS: Style = Style::new(Black, Grey);
    pub const KEY_NUM: Style = Style::new(Grey, Black);
    pub const KEY_LABEL: Style = Style::new(Black, DarkCyan);
    pub const CMDLINE: Style = Style::new(Grey, Black);
    pub const EDITOR: Style = Style::new(Cyan, DarkBlue);
    pub const EDITOR_SELECT: Style = Style::new(White, Black);
    pub const STATUS: Style = Style::new(Black, DarkCyan);
    pub const SEARCH_HIT: Style = Style::new(Black, Yellow);
    pub const BLANK: Style = Style::new(Grey, Black);
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Cell {
    ch: char,
    style: Style,
    /// The right half of a double-width character.
    cont: bool,
}

/// An off-screen character grid.
#[derive(Clone)]
pub struct Screen {
    pub w: u16,
    pub h: u16,
    cells: Vec<Cell>,
    pub cursor: Option<(u16, u16)>,
    /// Modifier keys held down, which choose the key bar labels.
    pub held: KeyModifiers,
}

/// Display width of a character. Controls and zero-width marks count as 0.
pub fn char_width(c: char) -> usize {
    c.width().unwrap_or(0)
}

/// Display width of a string.
pub fn str_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

/// Truncates on the right to `width` columns, then pads with spaces.
pub fn fit(s: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let w = char_width(c);
        if used + w > width {
            break;
        }
        out.push(c);
        used += w;
    }
    out.extend(std::iter::repeat_n(' ', width - used));
    out
}

/// Fits a file name to `width`, marking cut names with Far's `}`.
pub fn fit_name(s: &str, width: usize) -> String {
    if str_width(s) <= width || width == 0 {
        return fit(s, width);
    }
    let mut out = fit(s, width - 1).trim_end_matches(' ').to_string();
    let used = str_width(&out);
    out.extend(std::iter::repeat_n(' ', width - 1 - used));
    out.push('}');
    out
}

/// Keeps the end of a string, which is the useful part of a long path.
pub fn fit_left(s: &str, width: usize) -> String {
    if str_width(s) <= width {
        return s.to_string();
    }
    if width <= 3 {
        return ".".repeat(width);
    }
    let mut kept: Vec<char> = Vec::new();
    let mut used = 0;
    for c in s.chars().rev() {
        let w = char_width(c);
        if used + w > width - 3 {
            break;
        }
        kept.push(c);
        used += w;
    }
    let mut out = String::from("...");
    out.extend(kept.iter().rev());
    out
}

impl Screen {
    pub fn new(w: u16, h: u16) -> Self {
        let blank = Cell {
            ch: ' ',
            style: theme::BLANK,
            cont: false,
        };
        Screen {
            w,
            h,
            cells: vec![blank; w as usize * h as usize],
            cursor: None,
            held: KeyModifiers::NONE,
        }
    }

    fn idx(&self, x: u16, y: u16) -> usize {
        y as usize * self.w as usize + x as usize
    }

    /// Reads the character at a position. Used by tests.
    pub fn char_at(&self, x: u16, y: u16) -> char {
        self.cells[self.idx(x, y)].ch
    }

    /// Returns the text of a row. Used by tests.
    pub fn row_text(&self, y: u16) -> String {
        (0..self.w)
            .filter(|x| !self.cells[self.idx(*x, y)].cont)
            .map(|x| self.char_at(x, y))
            .collect()
    }

    /// Writes text, clipping at the right edge. Terminal control characters
    /// are replaced so that a hostile file name cannot inject escape codes.
    pub fn put(&mut self, x: u16, y: u16, s: &str, style: Style) -> u16 {
        let mut x = x;
        if y >= self.h {
            return x;
        }
        for c in s.chars() {
            let (c, w) = if c.is_control() {
                ('?', 1)
            } else {
                (c, char_width(c))
            };
            if w == 0 {
                continue;
            }
            if x as usize + w > self.w as usize {
                break;
            }
            let i = self.idx(x, y);
            // Overwriting half of a wide character blanks the other half.
            if self.cells[i].cont && x > 0 {
                self.cells[i - 1].ch = ' ';
            }
            if i + 1 < self.cells.len() && self.cells[i + 1].cont && w == 1 {
                self.cells[i + 1].cont = false;
                self.cells[i + 1].ch = ' ';
            }
            self.cells[i] = Cell {
                ch: c,
                style,
                cont: false,
            };
            if w == 2 {
                self.cells[i + 1] = Cell {
                    ch: ' ',
                    style,
                    cont: true,
                };
            }
            x += w as u16;
        }
        x
    }

    /// Fills a rectangle with one character.
    pub fn fill(&mut self, x: u16, y: u16, w: u16, h: u16, ch: char, style: Style) {
        let line: String = std::iter::repeat_n(ch, w as usize).collect();
        for row in y..y.saturating_add(h).min(self.h) {
            self.put(x, row, &line, style);
        }
    }

    /// Draws a box border and fills its interior.
    pub fn frame(&mut self, x: u16, y: u16, w: u16, h: u16, double: bool, style: Style) {
        if w < 2 || h < 2 {
            return;
        }
        let (tl, tr, bl, br, hz, vt) = if double {
            ('╔', '╗', '╚', '╝', '═', '║')
        } else {
            ('┌', '┐', '└', '┘', '─', '│')
        };
        let bar: String = std::iter::repeat_n(hz, w as usize - 2).collect();
        self.put(x, y, &format!("{tl}{bar}{tr}"), style);
        self.put(x, y + h - 1, &format!("{bl}{bar}{br}"), style);
        self.fill(x + 1, y + 1, w - 2, h - 2, ' ', style);
        for row in y + 1..y + h - 1 {
            self.put(x, row, &vt.to_string(), style);
            self.put(x + w - 1, row, &vt.to_string(), style);
        }
    }

    /// Draws a centred title on a border row.
    pub fn title(&mut self, x: u16, y: u16, w: u16, text: &str, style: Style) {
        let text = format!(" {} ", fit_left(text, (w as usize).saturating_sub(6)));
        let width = str_width(&text) as u16;
        self.put(x + (w.saturating_sub(width)) / 2, y, &text, style);
    }

    /// Draws the F1..F12 key bar on row `y`.
    pub fn keybar(&mut self, y: u16, bars: &KeyBars) {
        const NO_LABELS: [&str; 12] = [""; 12];
        let labels = match self.held {
            NONE => &bars.plain,
            SHIFT => &bars.shift,
            ALT => &bars.alt,
            CTRL => &bars.ctrl,
            // Far has further sets for combinations, and none of them are bound here.
            _ => &NO_LABELS,
        };
        let cell = (self.w as usize / 12).max(3);
        let mut x = 0u16;
        for (i, label) in labels.iter().enumerate() {
            if x >= self.w {
                break;
            }
            let num = format!("{}", i + 1);
            x = self.put(x, y, &num, theme::KEY_NUM);
            let room = if i == 11 {
                (self.w - x) as usize
            } else {
                cell.saturating_sub(num.len())
            };
            x = self.put(x, y, &fit(label, room), theme::KEY_LABEL);
        }
    }
}

/// A normalized key press.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Key {
    pub code: KeyCode,
    pub mods: KeyModifiers,
}

/// No modifiers.
pub const NONE: KeyModifiers = KeyModifiers::NONE;
/// Ctrl.
pub const CTRL: KeyModifiers = KeyModifiers::CONTROL;
/// Alt.
pub const ALT: KeyModifiers = KeyModifiers::ALT;
/// Shift.
pub const SHIFT: KeyModifiers = KeyModifiers::SHIFT;
/// Alt+Shift.
pub const ALT_SHIFT: KeyModifiers = KeyModifiers::ALT.union(KeyModifiers::SHIFT);
/// Ctrl+Shift.
pub const CTRL_SHIFT: KeyModifiers = KeyModifiers::CONTROL.union(KeyModifiers::SHIFT);

/// Input delivered to the UI.
#[derive(Debug, PartialEq, Eq)]
pub enum Input {
    Key(Key),
    Paste(String),
    Resize,
    /// The modifier keys held down changed.
    Held(KeyModifiers),
}

/// Converts raw terminal key data to the forms the application matches on.
pub fn normalize_key(code: KeyCode, mods: KeyModifiers, enhanced: bool) -> Key {
    let mut code = code;
    let mut mods = mods & (CTRL | ALT | SHIFT);
    if code == KeyCode::BackTab {
        code = KeyCode::Tab;
        mods |= SHIFT;
    }
    if let KeyCode::Char(c) = code {
        // Control codes are ambiguous in legacy terminals: Ctrl+\ arrives as
        // Ctrl+4 and Ctrl+] as Ctrl+5.
        if !enhanced && mods.contains(CTRL) {
            match c {
                '4' => code = KeyCode::Char('\\'),
                '5' => code = KeyCode::Char(']'),
                _ => {}
            }
        }
        if mods.intersects(CTRL | ALT) {
            if let KeyCode::Char(c) = code {
                let lower = c.to_lowercase().next().unwrap_or(c);
                code = KeyCode::Char(lower);
            }
        } else {
            // Shift is already expressed by the character itself.
            mods = NONE;
        }
    }
    // Many terminals send a line feed for Ctrl+Enter.
    if code == KeyCode::Char('j') && mods == CTRL {
        code = KeyCode::Enter;
    }
    Key { code, mods }
}

/// Owns the terminal: raw mode, the alternate screen and output.
pub struct Tui {
    pub screen: Screen,
    last: Option<Screen>,
    /// Whether keys arrive unambiguously, so `Ctrl+4` is not `Ctrl+\\`.
    pub enhanced: bool,
    /// The kitty keyboard protocol is available.
    kitty: bool,
    /// Windows Terminal's input mode is available. See `input.rs`.
    win32: Option<Reader>,
    /// Use colours the terminal's own scheme cannot change.
    fixed_colors: bool,
    /// Modifier keys held down.
    held: KeyModifiers,
    on_alt: bool,
}

impl Tui {
    /// Takes over the terminal.
    pub fn new() -> io::Result<Self> {
        let (w, h) = terminal::size()?;
        terminal::enable_raw_mode()?;
        let kitty = terminal::supports_keyboard_enhancement().unwrap_or(false);
        let win32 = (!kitty && input::try_enable()).then(Reader::new);
        let fixed_colors = has_fixed_colors(
            std::env::var("TERM").ok().as_deref(),
            std::env::var_os("COLORTERM").is_some(),
            std::env::var_os("WT_SESSION").is_some(),
        );
        let mut tui = Tui {
            screen: Screen::new(w, h),
            last: None,
            enhanced: kitty || win32.is_some(),
            kitty,
            win32,
            fixed_colors,
            held: KeyModifiers::NONE,
            on_alt: false,
        };
        tui.enter()?;
        Ok(tui)
    }

    /// Switches to the panels' alternate screen.
    pub fn enter(&mut self) -> io::Result<()> {
        let mut out = io::stdout();
        terminal::enable_raw_mode()?;
        execute!(
            out,
            EnterAlternateScreen,
            EnableBracketedPaste,
            cursor::Hide
        )?;
        if self.kitty {
            execute!(
                out,
                PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
            )?;
        }
        if self.win32.is_some() {
            execute!(out, Print(input::ENABLE))?;
        }
        self.on_alt = true;
        self.last = None;
        let (w, h) = terminal::size()?;
        self.screen = Screen::new(w, h);
        Ok(())
    }

    /// Returns to the normal screen, keeping raw mode so keys can still be
    /// read. This is what Ctrl+O shows.
    pub fn show_console(&mut self) -> io::Result<()> {
        let mut out = io::stdout();
        if self.kitty {
            execute!(out, PopKeyboardEnhancementFlags)?;
        }
        // Commands and the console read ordinary keys.
        if self.win32.is_some() {
            execute!(out, Print(input::DISABLE))?;
            self.held = KeyModifiers::NONE;
            self.screen.held = KeyModifiers::NONE;
        }
        execute!(
            out,
            ResetColor,
            DisableBracketedPaste,
            LeaveAlternateScreen,
            cursor::Show
        )?;
        self.on_alt = false;
        Ok(())
    }

    /// Shows the console until a key is pressed, for Ctrl+O in the viewer and
    /// editor.
    pub fn user_screen(&mut self) -> io::Result<()> {
        self.show_console()?;
        loop {
            if let Some(Input::Key(_)) = self.read(Duration::from_millis(500))? {
                break;
            }
        }
        self.enter()
    }

    /// Writes the differences between the buffer and the terminal.
    pub fn draw(&mut self) -> io::Result<()> {
        let mut buf: Vec<u8> = Vec::with_capacity(4096);
        queue!(buf, cursor::Hide)?;
        let full = match &self.last {
            Some(prev) => prev.w != self.screen.w || prev.h != self.screen.h,
            None => true,
        };
        if full {
            queue!(buf, ResetColor, terminal::Clear(terminal::ClearType::All))?;
        }
        let mut style: Option<Style> = None;
        for y in 0..self.screen.h {
            let mut pen: Option<u16> = None;
            for x in 0..self.screen.w {
                let i = self.screen.idx(x, y);
                let cell = self.screen.cells[i];
                if cell.cont {
                    continue;
                }
                let next_cont = self.screen.cells.get(i + 1).is_some_and(|c| c.cont);
                let unchanged = !full
                    && self.last.as_ref().is_some_and(|p| {
                        p.cells[i] == cell
                            && p.cells.get(i + 1).is_some_and(|c| c.cont) == next_cont
                    });
                if unchanged {
                    continue;
                }
                if pen != Some(x) {
                    queue!(buf, cursor::MoveTo(x, y))?;
                }
                if style != Some(cell.style) {
                    let (fg, bg) = if self.fixed_colors {
                        (fixed_color(cell.style.fg), fixed_color(cell.style.bg))
                    } else {
                        (cell.style.fg, cell.style.bg)
                    };
                    queue!(
                        buf,
                        SetAttribute(Attribute::Reset),
                        SetForegroundColor(fg),
                        SetBackgroundColor(bg)
                    )?;
                    if cell.style.bold {
                        queue!(buf, SetAttribute(Attribute::Bold))?;
                    }
                    style = Some(cell.style);
                }
                queue!(buf, Print(cell.ch))?;
                pen = Some(x + char_width(cell.ch).max(1) as u16);
            }
        }
        queue!(buf, SetAttribute(Attribute::Reset), ResetColor)?;
        if let Some((x, y)) = self.screen.cursor {
            queue!(buf, cursor::MoveTo(x, y), cursor::Show)?;
        }
        let mut out = io::stdout();
        out.write_all(&buf)?;
        out.flush()?;
        self.last = Some(self.screen.clone());
        Ok(())
    }

    /// Reads input, waiting at most `timeout`.
    pub fn read(&mut self, timeout: Duration) -> io::Result<Option<Input>> {
        if self.on_alt && self.win32.is_some() {
            return self.read_win32(timeout);
        }
        if !event::poll(timeout)? {
            return Ok(None);
        }
        match event::read()? {
            Event::Key(k) if k.kind != KeyEventKind::Release => Ok(Some(Input::Key(
                normalize_key(k.code, k.modifiers, self.kitty),
            ))),
            Event::Paste(text) => Ok(Some(Input::Paste(text))),
            Event::Resize(w, h) => {
                self.screen = Screen::new(w, h);
                self.last = None;
                Ok(Some(Input::Resize))
            }
            _ => Ok(None),
        }
    }

    /// Reads input in Windows Terminal's input mode. This reader has no signal
    /// to wake it for a resize, so it compares the size as it waits.
    fn read_win32(&mut self, timeout: Duration) -> io::Result<Option<Input>> {
        let start = Instant::now();
        loop {
            let left = timeout.saturating_sub(start.elapsed());
            let Some(reader) = &mut self.win32 else {
                return Ok(None);
            };
            let input = reader.read(left)?;
            if let Some(Input::Held(mods)) = input {
                self.screen.held = mods;
                self.held = mods;
            }
            if input.is_some() {
                return Ok(input);
            }
            let (w, h) = terminal::size()?;
            if (w, h) != (self.screen.w, self.screen.h) {
                self.screen = Screen::new(w, h);
                self.last = None;
                self.screen.held = self.held;
                return Ok(Some(Input::Resize));
            }
            if start.elapsed() >= timeout {
                return Ok(None);
            }
        }
    }

    /// Copies the held modifiers to the buffer. A dialog puts back the screen
    /// from before it opened, along with what was held then.
    pub fn sync_held(&mut self) {
        self.screen.held = self.held;
    }

    /// Sends text to the system clipboard using OSC 52, where supported.
    pub fn set_clipboard(&self, text: &str) {
        let seq = format!("\x1b]52;c;{}\x07", crate::util::base64(text.as_bytes()));
        let mut out = io::stdout();
        let _ = out.write_all(seq.as_bytes());
        let _ = out.flush();
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        if self.on_alt {
            let _ = self.show_console();
        }
        let _ = terminal::disable_raw_mode();
    }
}

/// Undoes every mode the program sets on the terminal.
const RESTORE: &str = "\x1b[?1004l\x1b[?9001l\x1b[<1u\x1b[?2004l\x1b[?1049l\x1b[?25h\x1b[0m";

static SAVED_TERMIOS: OnceLock<libc::termios> = OnceLock::new();
static SIGNAL_SEEN: AtomicBool = AtomicBool::new(false);

extern "C" fn restore_and_die(signal: libc::c_int) {
    // Only async-signal-safe calls are allowed here.
    if !SIGNAL_SEEN.swap(true, Ordering::SeqCst) {
        // SAFETY: write, tcsetattr, signal and raise are async-signal-safe. The
        // termios value was stored before the handler was installed.
        unsafe {
            libc::write(1, RESTORE.as_ptr().cast(), RESTORE.len());
            if let Some(saved) = SAVED_TERMIOS.get() {
                libc::tcsetattr(0, libc::TCSANOW, saved);
            }
        }
    }
    // SAFETY: restoring the default action and raising the signal again ends the
    // process with the right status.
    unsafe {
        libc::signal(signal, libc::SIG_DFL);
        libc::raise(signal);
    }
}

/// Restores the terminal if the program is told to end. Left in Windows
/// Terminal's input mode, a shell would read every key press as garbage. Call
/// this before raw mode is turned on.
pub fn install_signal_restore() {
    // SAFETY: termios is plain data and tcgetattr fills it on success. The
    // handler has the signature signal() expects.
    unsafe {
        let mut saved: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(io::stdin().as_raw_fd(), &mut saved) == 0 {
            let _ = SAVED_TERMIOS.set(saved);
        }
        let handler = restore_and_die as extern "C" fn(libc::c_int) as libc::sighandler_t;
        libc::signal(libc::SIGTERM, handler);
        libc::signal(libc::SIGHUP, handler);
    }
}

/// Restores the terminal after a panic so that the message stays readable.
pub fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let mut out = io::stdout();
        let _ = execute!(
            out,
            Print(input::DISABLE),
            PopKeyboardEnhancementFlags,
            DisableBracketedPaste,
            LeaveAlternateScreen,
            cursor::Show,
            ResetColor
        );
        let _ = terminal::disable_raw_mode();
        default(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_characters_cannot_reach_the_terminal() {
        let mut s = Screen::new(10, 1);
        s.put(0, 0, "a\x1b[2Jb", theme::BLANK);
        assert_eq!(s.row_text(0).trim_end(), "a?[2Jb");
    }

    #[test]
    fn wide_characters_take_two_cells_and_clip_cleanly() {
        let mut s = Screen::new(3, 1);
        let end = s.put(0, 0, "日本", theme::BLANK);
        assert_eq!(end, 2);
        assert_eq!(s.char_at(0, 0), '日');
        assert_eq!(s.char_at(2, 0), ' ');
    }

    #[test]
    fn overwriting_half_a_wide_character_clears_the_other_half() {
        let mut s = Screen::new(4, 1);
        s.put(0, 0, "日", theme::BLANK);
        s.put(1, 0, "x", theme::BLANK);
        assert_eq!(s.row_text(0), " x  ");
    }

    #[test]
    fn name_fitting() {
        assert_eq!(fit_name("abc", 5), "abc  ");
        assert_eq!(fit_name("abcdefgh", 5), "abcd}");
        assert_eq!(fit_left("/very/long/path", 8), ".../path");
    }

    #[test]
    fn fixed_colors_avoid_the_terminals_own_scheme() {
        use Color::*;
        assert_eq!(fixed_color(Yellow), AnsiValue(226));
        assert_eq!(fixed_color(Green), AnsiValue(46));
        for color in [
            Black,
            DarkRed,
            DarkGreen,
            DarkYellow,
            DarkBlue,
            DarkMagenta,
            DarkCyan,
            Grey,
            DarkGrey,
            Red,
            Green,
            Yellow,
            Blue,
            Magenta,
            Cyan,
            White,
        ] {
            let AnsiValue(n) = fixed_color(color) else {
                panic!("{color:?} has no number");
            };
            assert!(n >= 16, "{color:?} is {n}, which a scheme can redefine");
        }
        assert!(has_fixed_colors(Some("xterm-256color"), false, false));
        assert!(has_fixed_colors(Some("xterm"), true, false));
        assert!(has_fixed_colors(None, false, true));
        assert!(!has_fixed_colors(Some("linux"), false, false));
    }

    #[test]
    fn key_bar_follows_the_held_modifier() {
        let bars = KeyBars {
            plain: ["Plain"; 12],
            shift: ["Shift"; 12],
            alt: ["Alted"; 12],
            ctrl: ["Ctrld"; 12],
        };
        let mut s = Screen::new(100, 1);
        for (held, text) in [
            (NONE, "Plain"),
            (SHIFT, "Shift"),
            (ALT, "Alted"),
            (CTRL, "Ctrld"),
        ] {
            s.held = held;
            s.keybar(0, &bars);
            assert!(s.row_text(0).contains(text), "{}", s.row_text(0));
        }
        s.held = CTRL_SHIFT;
        s.keybar(0, &bars);
        assert!(!s.row_text(0).contains("Plain"));
    }

    #[test]
    fn key_normalization() {
        let k = normalize_key(KeyCode::Char('A'), SHIFT, false);
        assert_eq!((k.code, k.mods), (KeyCode::Char('A'), NONE));
        let k = normalize_key(KeyCode::Char('O'), CTRL, false);
        assert_eq!(k.code, KeyCode::Char('o'));
        let k = normalize_key(KeyCode::Char('4'), CTRL, false);
        assert_eq!(k.code, KeyCode::Char('\\'));
        let k = normalize_key(KeyCode::Char('4'), CTRL, true);
        assert_eq!(k.code, KeyCode::Char('4'));
        let k = normalize_key(KeyCode::Char('j'), CTRL, false);
        assert_eq!((k.code, k.mods), (KeyCode::Enter, CTRL));
        let k = normalize_key(KeyCode::BackTab, SHIFT, false);
        assert_eq!((k.code, k.mods), (KeyCode::Tab, SHIFT));
    }
}
