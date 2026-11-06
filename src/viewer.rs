//! The F3 viewer. It reads only the part of the file it needs, so size does
//! not matter, and never loads the file into memory.

use crate::dialog::{self, Field, Form};
use crate::editor::Clipboard;
use crate::encoding;
use crate::fsops::find_bytes;
use crate::history::{History, Kind};
use crate::screen::{
    char_width, fit, fit_left, theme, Input, KeyBars, Screen, Tui, ALT, CTRL, CTRL_SHIFT, NONE,
    SHIFT,
};
use crate::search::{parse_hex, Matcher, SearchOptions};
use crate::util;
use crossterm::event::KeyCode;
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

const MAX_LINE: usize = 64 * 1024;
const SEARCH_CHUNK: u64 = 1024 * 1024;
const TAB: usize = 8;
const HEX_WIDTH: u64 = 16;
const WINDOW: usize = 1024 * 1024;

/// What to do after the viewer closes.
#[derive(Debug, PartialEq, Eq)]
pub enum ViewerExit {
    Close,
    Edit,
    NextFile,
    PreviousFile,
    GoToFile,
    History,
}

enum Source {
    File(File),
    Memory(Vec<u8>),
}

/// One screen row of text.
#[derive(Debug)]
pub struct Row {
    pub bytes: Vec<u8>,
    pub next: u64,
}

/// Viewer state, independent of the terminal.
pub struct Viewer {
    pub path: PathBuf,
    source: Source,
    pub size: u64,
    pub top: u64,
    pub left: usize,
    pub wrap: bool,
    pub hex: bool,
    pub enc: usize,
    pub mark: Option<(u64, u64)>,
    pub word_wrap: bool,
    pub dump: bool,
    pub hex_width: u64,
    pub scrollbar: bool,
    pub status_bar: bool,
    bookmarks: [Option<u64>; 10],
    jumps: Vec<u64>,
    table: [char; 256],
}

impl Viewer {
    /// Opens a file. Files reporting size zero, as in /proc, are read whole.
    pub fn open(path: &Path) -> io::Result<Self> {
        let meta = fs::metadata(path)?;
        if !meta.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "only regular files can be viewed",
            ));
        }
        let mut file = File::open(path)?;
        let mut size = meta.len();
        let source = if size == 0 {
            let mut data = Vec::new();
            (&mut file).take(64 * 1024 * 1024).read_to_end(&mut data)?;
            size = data.len() as u64;
            Source::Memory(data)
        } else {
            Source::File(file)
        };
        Ok(Viewer {
            path: path.to_path_buf(),
            source,
            size,
            top: 0,
            left: 0,
            wrap: false,
            hex: false,
            enc: 0,
            mark: None,
            word_wrap: false,
            dump: false,
            hex_width: HEX_WIDTH,
            scrollbar: false,
            status_bar: true,
            bookmarks: [None; 10],
            jumps: Vec::new(),
            table: ['?'; 256],
        })
    }

    /// Picks up growth of a file being written while it is viewed.
    fn refresh_size(&mut self) {
        if let Source::File(file) = &self.source {
            if let Ok(meta) = file.metadata() {
                self.size = meta.len();
            }
        }
    }

    /// Selects an encoding by index in [`encoding::NAMES`].
    pub fn set_encoding(&mut self, index: usize) {
        self.enc = index;
        if index != 0 {
            self.table = encoding::table(index);
        }
    }

    fn read_at(&self, offset: u64, len: usize) -> Vec<u8> {
        if offset >= self.size {
            return Vec::new();
        }
        match &self.source {
            Source::Memory(data) => {
                let start = offset as usize;
                let end = (start + len).min(data.len());
                data[start..end].to_vec()
            }
            Source::File(file) => {
                let mut buf = vec![0u8; len];
                let mut filled = 0;
                while filled < len {
                    match file.read_at(&mut buf[filled..], offset + filled as u64) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => filled += n,
                    }
                }
                buf.truncate(filled);
                buf
            }
        }
    }

    /// Splits bytes into characters and the number of bytes each one uses.
    pub fn decode(&self, bytes: &[u8]) -> Vec<(char, usize)> {
        if self.enc != 0 {
            return bytes.iter().map(|b| (self.table[*b as usize], 1)).collect();
        }
        let mut out = Vec::with_capacity(bytes.len());
        let mut rest = bytes;
        while !rest.is_empty() {
            match std::str::from_utf8(rest) {
                Ok(text) => {
                    out.extend(text.chars().map(|c| (c, c.len_utf8())));
                    break;
                }
                Err(e) => {
                    let (good, bad) = rest.split_at(e.valid_up_to());
                    // SAFETY: from_utf8 just validated this prefix.
                    let text = unsafe { std::str::from_utf8_unchecked(good) };
                    out.extend(text.chars().map(|c| (c, c.len_utf8())));
                    out.push(('\u{fffd}', 1));
                    rest = &bad[1..];
                }
            }
        }
        out
    }

    /// Returns the row starting at `offset` for a screen `width`.
    pub fn row_at(&self, offset: u64, width: usize) -> Row {
        if self.dump {
            // Dump mode ignores line breaks and fills every row.
            let chunk = self.read_at(offset, width.max(1) * 4);
            let mut used = 0;
            let mut col = 0;
            for (c, len) in self.decode(&chunk) {
                let cells = char_width(c).max(1);
                if col + cells > width.max(1) && used > 0 {
                    break;
                }
                col += cells;
                used += len;
            }
            return Row {
                bytes: chunk[..used].to_vec(),
                next: offset + used as u64,
            };
        }
        let chunk = self.read_at(offset, MAX_LINE);
        let newline = chunk.iter().position(|b| *b == b'\n');
        let mut line_len = newline.unwrap_or(chunk.len());
        let mut next = match newline {
            Some(pos) => offset + pos as u64 + 1,
            None => offset + chunk.len() as u64,
        };
        if newline.is_none() && self.enc == 0 && chunk.len() == MAX_LINE {
            // A forced break must not split a UTF-8 sequence.
            line_len = utf8_safe_cut(&chunk);
            next = offset + line_len as u64;
        }
        if self.wrap {
            let mut col = 0;
            let mut used = 0;
            let mut after_space = 0;
            for (c, len) in self.decode(&chunk[..line_len]) {
                let w = cell_width(c, col);
                if col + w > width.max(1) && used > 0 {
                    if self.word_wrap && after_space > 0 {
                        used = after_space;
                    }
                    return Row {
                        bytes: chunk[..used].to_vec(),
                        next: offset + used as u64,
                    };
                }
                col += w;
                used += len;
                if c == ' ' || c == '\t' {
                    after_space = used;
                }
            }
        }
        let mut bytes = chunk;
        bytes.truncate(line_len);
        Row { bytes, next }
    }

    /// Finds where the logical line containing the byte before `offset` starts.
    fn line_start(&self, offset: u64) -> u64 {
        if offset == 0 {
            return 0;
        }
        let begin = offset.saturating_sub(MAX_LINE as u64);
        let chunk = self.read_at(begin, (offset - begin) as usize);
        let search = &chunk[..chunk.len().saturating_sub(1)];
        match search.iter().rposition(|b| *b == b'\n') {
            Some(pos) => begin + pos as u64 + 1,
            None => begin,
        }
    }

    /// Returns the start of the row above `offset`.
    pub fn prev_row(&self, offset: u64, width: usize) -> u64 {
        if self.dump {
            return offset.saturating_sub(width.max(1) as u64);
        }
        let mut at = self.line_start(offset);
        loop {
            let next = self.row_at(at, width).next;
            if next >= offset || next <= at {
                return at;
            }
            at = next;
        }
    }

    /// True when the last byte of the file is already on screen.
    pub fn at_end(&self, rows: usize, width: usize) -> bool {
        if self.hex {
            return self.top + rows as u64 * self.hex_width >= self.size;
        }
        let mut at = self.top;
        for _ in 0..rows {
            if at >= self.size {
                return true;
            }
            at = self.row_at(at, width).next;
        }
        at >= self.size
    }

    pub fn scroll_down(&mut self, count: usize, rows: usize, width: usize) {
        for _ in 0..count {
            if self.at_end(rows, width) {
                return;
            }
            self.top = if self.hex {
                self.top + self.hex_width
            } else {
                self.row_at(self.top, width).next
            };
        }
    }

    pub fn scroll_up(&mut self, count: usize, width: usize) {
        for _ in 0..count {
            if self.top == 0 {
                return;
            }
            self.top = if self.hex {
                self.top.saturating_sub(self.hex_width)
            } else {
                self.prev_row(self.top, width)
            };
        }
    }

    /// Shows the end of the file.
    pub fn go_end(&mut self, rows: usize, width: usize) {
        if self.hex {
            let lines = self.size.div_ceil(self.hex_width);
            self.top = lines.saturating_sub(rows as u64) * self.hex_width;
            return;
        }
        self.top = self.size;
        self.scroll_up(rows, width);
    }

    /// Moves to a byte offset, aligned to the start of its row.
    pub fn go_to(&mut self, offset: u64, width: usize) {
        let offset = offset.min(self.size);
        if self.hex {
            self.top = offset - offset % self.hex_width;
        } else if self.dump {
            self.top = offset;
        } else {
            self.top = self.line_start(offset);
            let mut at = self.top;
            loop {
                let next = self.row_at(at, width).next;
                if next > offset || next <= at || next >= self.size {
                    self.top = at;
                    return;
                }
                at = next;
            }
        }
    }

    /// Searches for bytes from `from`, in either direction.
    pub fn search(
        &self,
        needle: &[u8],
        from: u64,
        ignore_case: bool,
        backward: bool,
    ) -> Option<u64> {
        if needle.is_empty() {
            return None;
        }
        let overlap = needle.len() as u64 - 1;
        if backward {
            let mut end = from.min(self.size);
            while end > 0 {
                let begin = end.saturating_sub(SEARCH_CHUNK);
                let chunk = self.read_at(begin, (end - begin) as usize);
                if let Some(pos) = rfind_bytes(&chunk, needle, ignore_case) {
                    return Some(begin + pos as u64);
                }
                if begin == 0 {
                    return None;
                }
                end = begin + overlap;
            }
            return None;
        }
        let mut at = from;
        while at < self.size {
            let chunk = self.read_at(at, (SEARCH_CHUNK + overlap) as usize);
            if chunk.is_empty() {
                return None;
            }
            if let Some(pos) = find_bytes(&chunk, needle, ignore_case) {
                return Some(at + pos as u64);
            }
            at += SEARCH_CHUNK;
        }
        None
    }

    /// Searches decoded text line by line with the full set of options.
    /// Returns the byte offset and byte length of the match.
    pub fn search_text(&self, m: &Matcher, from: u64, backward: bool) -> Option<(u64, u64)> {
        if backward {
            self.search_text_backward(m, from)
        } else {
            self.search_text_forward(m, from)
        }
    }

    fn match_line(
        &self,
        m: &Matcher,
        line: &[u8],
        base: u64,
        limit: u64,
        backward: bool,
    ) -> Option<(u64, u64)> {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let decoded = self.decode(line);
        let text: String = decoded.iter().map(|(c, _)| *c).collect();
        let mut offsets = Vec::with_capacity(decoded.len() + 1);
        let mut at = base;
        offsets.push(at);
        for (_, len) in &decoded {
            at += *len as u64;
            offsets.push(at);
        }
        let mut col = if backward {
            offsets.iter().rposition(|o| *o <= limit)?
        } else {
            offsets.iter().position(|o| *o >= limit)?
        };
        // Zero-length matches are meaningless here, so look past them.
        loop {
            let hit = m.find(&text, col, backward)?;
            if hit.end > hit.start {
                return Some((offsets[hit.start], offsets[hit.end] - offsets[hit.start]));
            }
            col = if backward {
                hit.end.checked_sub(1)?
            } else {
                hit.end + 1
            };
            if col >= offsets.len() {
                return None;
            }
        }
    }

    fn search_text_forward(&self, m: &Matcher, from: u64) -> Option<(u64, u64)> {
        let mut at = self.line_start(from.saturating_add(1).min(self.size));
        while at < self.size {
            let window = self.read_at(at, WINDOW);
            if window.is_empty() {
                return None;
            }
            let mut start = 0;
            while start < window.len() {
                let newline = window[start..].iter().position(|b| *b == b'\n');
                let Some(length) = newline else {
                    if start > 0 {
                        break; // Reread so that the line is complete.
                    }
                    let base = at + start as u64;
                    let hit = self.match_line(m, &window[start..], base, from, false);
                    if hit.is_some() {
                        return hit;
                    }
                    start = window.len();
                    break;
                };
                let base = at + start as u64;
                let hit = self.match_line(m, &window[start..start + length], base, from, false);
                if hit.is_some() {
                    return hit;
                }
                start += length + 1;
            }
            at += start as u64;
        }
        None
    }

    fn search_text_backward(&self, m: &Matcher, from: u64) -> Option<(u64, u64)> {
        let mut end = from.min(self.size);
        // Include the rest of the line, so a match ending at `from` is seen.
        let mut limit_line_end = end;
        while end > 0 || limit_line_end > 0 {
            let begin = limit_line_end.saturating_sub(WINDOW as u64);
            let window = self.read_at(begin, (limit_line_end - begin) as usize);
            if window.is_empty() {
                return None;
            }
            let mut stop = window.len();
            loop {
                let search = &window[..stop.saturating_sub(usize::from(
                    window.get(stop.wrapping_sub(1)) == Some(&b'\n'),
                ))];
                let newline = search.iter().rposition(|b| *b == b'\n');
                let start = newline.map_or(0, |p| p + 1);
                // A partial first line is reread from an earlier window.
                if newline.is_none() && begin > 0 && stop < window.len() {
                    break;
                }
                let base = begin + start as u64;
                let hit = self.match_line(m, &window[start..search.len()], base, end, true);
                if hit.is_some() {
                    return hit;
                }
                if start == 0 {
                    limit_line_end = begin;
                    stop = 0;
                    break;
                }
                stop = start;
            }
            if stop > 0 {
                limit_line_end = begin + stop as u64;
            }
            end = end.min(limit_line_end);
            if limit_line_end == 0 {
                return None;
            }
        }
        None
    }

    /// The widest row on screen, for Ctrl+Shift+Right.
    pub fn widest_row(&self, rows: usize, width: usize) -> usize {
        let mut offset = self.top;
        let mut widest = 0;
        for _ in 0..rows {
            if offset >= self.size {
                break;
            }
            let row = self.row_at(offset, width);
            let mut col = 0;
            for (c, _) in self.decode(&row.bytes) {
                col += cell_width(c, col);
            }
            widest = widest.max(col);
            if row.next <= offset {
                break;
            }
            offset = row.next;
        }
        widest
    }

    /// Records the position before a jump, for Ctrl+Z.
    fn remember_jump(&mut self) {
        self.jumps.push(self.top);
    }

    /// Parses a goto request: `50%`, decimal bytes, or `0x` hexadecimal bytes.
    pub fn parse_goto(&self, text: &str) -> Option<u64> {
        let text = text.trim();
        if let Some(percent) = text.strip_suffix('%') {
            let p: u64 = percent.trim().parse().ok()?;
            return Some((self.size as u128 * p.min(100) as u128 / 100) as u64);
        }
        if let Some(hex) = text.strip_prefix("0x") {
            return u64::from_str_radix(hex, 16).ok();
        }
        text.parse().ok()
    }

    /// Rows available for the file.
    fn text_rows(&self, screen_h: u16, keybar: bool) -> usize {
        let bars = usize::from(self.status_bar) + usize::from(keybar);
        (screen_h as usize).saturating_sub(bars).max(1)
    }

    fn draw(&self, s: &mut Screen, keybar: bool) {
        let w = s.w as usize;
        s.fill(0, 0, s.w, s.h, ' ', theme::EDITOR);
        let first = u16::from(self.status_bar);
        if self.status_bar {
            let percent = if self.size == 0 {
                100
            } else {
                (self.top as u128 * 100 / self.size as u128) as u64
            };
            let info = format!(
                " {} {} {}% ",
                encoding::NAMES[self.enc],
                util::group_digits(self.size),
                percent
            );
            let room = w.saturating_sub(crate::screen::str_width(&info));
            let line = format!(
                "{}{info}",
                fit(&fit_left(&util::tilde(&self.path), room), room)
            );
            s.put(0, 0, &line, theme::STATUS);
        }
        let rows = self.text_rows(s.h, keybar);
        if self.hex {
            self.draw_hex(s, rows, first);
        } else {
            self.draw_text(s, rows, w, first);
        }
        if self.scrollbar && s.w > 0 {
            let thumb = if self.size == 0 {
                0
            } else {
                (self.top as u128 * rows as u128 / self.size as u128) as usize
            };
            for y in 0..rows {
                let mark = if y == thumb.min(rows - 1) {
                    "█"
                } else {
                    "░"
                };
                s.put(s.w - 1, first + y as u16, mark, theme::EDITOR);
            }
        }
        if keybar {
            s.keybar(
                s.h.saturating_sub(1),
                &KeyBars {
                    plain: [
                        "Help",
                        if self.wrap { "Unwrap" } else { "Wrap" },
                        "Quit",
                        if self.hex { "Text" } else { "Hex" },
                        "",
                        "Edit",
                        "Search",
                        "Encod",
                        "",
                        "Quit",
                        "",
                        "",
                    ],
                    shift: [
                        "", "WWrap", "", "Mode", "", "", "Next", "CodePg", "", "", "", "",
                    ],
                    alt: ["", "", "", "", "", "", "Prev", "Goto", "", "", "ViewHs", ""],
                    ctrl: ["", "", "", "", "", "", "", "", "", "GoFile", "", ""],
                },
            );
        }
    }

    fn draw_text(&self, s: &mut Screen, rows: usize, width: usize, first: u16) {
        let mut offset = self.top;
        for y in 0..rows {
            if offset >= self.size {
                break;
            }
            let row = self.row_at(offset, width);
            let mut col = 0usize;
            let mut at = offset;
            let mut text = [0u8; 4];
            for (c, len) in self.decode(&row.bytes) {
                let cells = if self.dump {
                    char_width(c).max(1)
                } else {
                    cell_width(c, col)
                };
                let style = match self.mark {
                    Some((start, length)) if at < start + length && at + len as u64 > start => {
                        theme::SEARCH_HIT
                    }
                    _ => theme::EDITOR,
                };
                if col + cells > self.left && col >= self.left {
                    let x = col - self.left;
                    if x + cells > width {
                        break;
                    }
                    if c == '\t' && !self.dump {
                        s.fill(x as u16, y as u16 + first, cells as u16, 1, ' ', style);
                    } else {
                        let shown = visible_char(c);
                        s.put(
                            x as u16,
                            y as u16 + first,
                            shown.encode_utf8(&mut text),
                            style,
                        );
                    }
                }
                col += cells;
                at += len as u64;
            }
            offset = if row.next > offset {
                row.next
            } else {
                self.size
            };
        }
    }

    fn draw_hex(&self, s: &mut Screen, rows: usize, first: u16) {
        let per_row = self.hex_width;
        let text_col = 14 + per_row as usize * 3 + 2;
        for y in 0..rows {
            let offset = self.top + y as u64 * per_row;
            if offset >= self.size {
                break;
            }
            let bytes = self.read_at(offset, per_row as usize);
            let row = y as u16 + first;
            s.put(0, row, &format!("{offset:012X}: "), theme::EDITOR);
            for (i, b) in bytes.iter().enumerate() {
                let at = offset + i as u64;
                let style = match self.mark {
                    Some((start, len)) if at >= start && at < start + len => theme::SEARCH_HIT,
                    _ => theme::EDITOR,
                };
                let gap = usize::from(i as u64 >= per_row / 2);
                s.put((14 + i * 3 + gap) as u16, row, &format!("{b:02X}"), style);
                let shown = if b.is_ascii_graphic() || *b == b' ' {
                    *b as char
                } else {
                    '.'
                };
                s.put((text_col + i) as u16, row, &shown.to_string(), style);
            }
        }
    }
}

/// Length of `bytes` without a trailing incomplete UTF-8 sequence.
fn utf8_safe_cut(bytes: &[u8]) -> usize {
    let len = bytes.len();
    for back in 1..=3.min(len) {
        let b = bytes[len - back];
        if b & 0xc0 == 0x80 {
            continue;
        }
        let needed = match b {
            0xc0..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf7 => 4,
            _ => 1,
        };
        return if needed > back { len - back } else { len };
    }
    len
}

fn rfind_bytes(hay: &[u8], needle: &[u8], ignore_case: bool) -> Option<usize> {
    if hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).rposition(|w| {
        if ignore_case {
            w.eq_ignore_ascii_case(needle)
        } else {
            w == needle
        }
    })
}

/// Columns used by a character at a given column. Tabs reach the next stop.
pub fn cell_width(c: char, col: usize) -> usize {
    if c == '\t' {
        TAB - col % TAB
    } else if c.is_control() {
        1
    } else {
        char_width(c)
    }
}

/// How a character is drawn: controls become a neutral dot.
pub fn visible_char(c: char) -> char {
    if c.is_control() {
        '·'
    } else {
        c
    }
}

#[derive(Clone, Default)]
struct SearchState {
    opts: SearchOptions,
    hex: bool,
}

const HELP: &[(&str, &str)] = &[
    ("Movement", ""),
    ("Left, Right", "One column"),
    (
        "Ctrl+Left, Ctrl+Right",
        "20 columns; one byte in hex and dump",
    ),
    ("Ctrl+Shift+Left", "Leftmost column"),
    ("Ctrl+Shift+Right", "Rightmost column on screen"),
    ("Alt+Left, Alt+Right", "Hex: bytes per row, by 1"),
    ("Ctrl+Alt+Left, Ctrl+Alt+Right", "Hex: bytes per row, by 16"),
    ("Alt+F8", "Go to offset or percent"),
    ("Ctrl+Z, Alt+BS", "Back to the previous position"),
    ("Ctrl+Shift+0..9, Ctrl+0..9", "Set, go to bookmark"),
    ("", ""),
    ("Modes", ""),
    ("F2, Shift+F2", "Wrap, wrap at words"),
    ("F4, Shift+F4", "Hex, choose text, hex or dump"),
    ("F8, Shift+F8", "Next encoding, choose encoding"),
    ("", ""),
    ("Search", ""),
    ("F7", "Search"),
    ("Shift+F7, Space", "Continue forward"),
    ("Alt+F7", "Continue backward"),
    ("Ctrl+C, Ctrl+Ins", "Copy the found text"),
    ("Ctrl+U", "Clear the found text"),
    ("", ""),
    ("Other", ""),
    ("F3, F10, Esc", "Quit"),
    ("F6", "Editor"),
    ("+, -", "Next, previous file in the panel"),
    ("Ctrl+F10", "Go to this file in the panel"),
    ("Alt+F11", "File view and edit history"),
    ("Ctrl+O", "User screen"),
    ("Ctrl+B, Ctrl+Shift+B", "Key bar, status line"),
    ("Ctrl+S", "Scroll bar"),
];

/// Runs the viewer until it is closed.
pub fn run(
    tui: &mut Tui,
    path: &Path,
    keybar: &mut bool,
    clipboard: &mut Clipboard,
    db: &Rc<History>,
) -> io::Result<ViewerExit> {
    let mut v = Viewer::open(path)?;
    let mut search = SearchState {
        opts: db.load_search(),
        hex: db.flag("search.hex", false),
    };
    let mut status: Option<String> = None;
    let ctrl_alt = CTRL | ALT;
    loop {
        tui.sync_held();
        v.draw(&mut tui.screen, *keybar);
        tui.screen.cursor = None;
        if let Some(text) = &status {
            let y = tui.screen.h.saturating_sub(1 + u16::from(*keybar));
            let line = fit(text, tui.screen.w as usize);
            tui.screen.put(0, y, &line, theme::ERROR);
        }
        tui.draw()?;
        let input = tui.read(Duration::from_millis(1000))?;
        let Some(input) = input else {
            v.refresh_size();
            continue;
        };
        let Input::Key(key) = input else {
            continue;
        };
        status = None;
        let (w, rows) = (tui.screen.w as usize, v.text_rows(tui.screen.h, *keybar));
        v.refresh_size();
        let text_mode = !v.wrap && !v.hex && !v.dump;
        match (key.mods, key.code) {
            (_, KeyCode::Esc) | (NONE, KeyCode::F(3)) | (NONE, KeyCode::F(10)) => {
                return Ok(ViewerExit::Close);
            }
            (NONE, KeyCode::F(6)) => return Ok(ViewerExit::Edit),
            (NONE, KeyCode::Char('+')) => return Ok(ViewerExit::NextFile),
            (NONE, KeyCode::Char('-')) => return Ok(ViewerExit::PreviousFile),
            (CTRL, KeyCode::F(10)) => return Ok(ViewerExit::GoToFile),
            (ALT, KeyCode::F(11)) => return Ok(ViewerExit::History),
            (NONE, KeyCode::F(1)) => dialog::help_list(tui, "Viewer", HELP)?,
            (NONE, KeyCode::Down) => v.scroll_down(1, rows, w),
            (NONE, KeyCode::Up) => v.scroll_up(1, w),
            (NONE, KeyCode::PageDown) => v.scroll_down(rows.max(1), rows, w),
            (NONE, KeyCode::PageUp) => v.scroll_up(rows.max(1), w),
            (NONE, KeyCode::Home) | (CTRL, KeyCode::Home) => {
                v.remember_jump();
                v.top = 0;
                v.left = 0;
            }
            (NONE, KeyCode::End) | (CTRL, KeyCode::End) => {
                v.remember_jump();
                v.go_end(rows, w);
            }
            (NONE, KeyCode::Right) if text_mode => v.left += 1,
            (NONE, KeyCode::Left) if text_mode => v.left = v.left.saturating_sub(1),
            (CTRL, KeyCode::Right) if text_mode => v.left += 20,
            (CTRL, KeyCode::Left) if text_mode => v.left = v.left.saturating_sub(20),
            (CTRL, KeyCode::Right) if v.hex || v.dump => {
                v.top = (v.top + 1).min(v.size.saturating_sub(1));
            }
            (CTRL, KeyCode::Left) if v.hex || v.dump => v.top = v.top.saturating_sub(1),
            (CTRL_SHIFT, KeyCode::Left) if text_mode => v.left = 0,
            (CTRL_SHIFT, KeyCode::Right) if text_mode => {
                v.left = v.widest_row(rows, w).saturating_sub(w);
            }
            (ALT, KeyCode::Right) if v.hex => v.hex_width = (v.hex_width + 1).min(64),
            (ALT, KeyCode::Left) if v.hex => v.hex_width = v.hex_width.saturating_sub(1).max(1),
            (m, KeyCode::Right) if v.hex && m == ctrl_alt => {
                v.hex_width = (v.hex_width + 16).min(64);
            }
            (m, KeyCode::Left) if v.hex && m == ctrl_alt => {
                v.hex_width = v.hex_width.saturating_sub(16).max(1);
            }
            (NONE, KeyCode::F(2)) => {
                if v.hex || v.dump {
                    // In hex and dump, F2 moves between those two modes.
                    v.hex = !v.hex;
                    v.dump = !v.dump;
                } else {
                    v.wrap = !v.wrap;
                    v.left = 0;
                    let top = v.top;
                    v.go_to(top, w);
                }
            }
            (SHIFT, KeyCode::F(2)) if !v.hex && !v.dump => {
                v.word_wrap = !v.word_wrap;
                v.wrap = true;
                v.left = 0;
            }
            (NONE, KeyCode::F(4)) => {
                let top = v.top;
                v.hex = !v.hex;
                v.dump = false;
                v.go_to(top, w);
            }
            (SHIFT, KeyCode::F(4)) => {
                let items = vec!["Text".to_string(), "Hex".to_string(), "Dump".to_string()];
                let current = if v.hex { 1 } else { usize::from(v.dump) * 2 };
                if let Some(mode) = dialog::menu(tui, "View mode", &items, current)? {
                    let top = v.top;
                    v.hex = mode == 1;
                    v.dump = mode == 2;
                    v.go_to(top, w);
                }
            }
            (NONE, KeyCode::F(8)) => v.set_encoding((v.enc + 1) % encoding::NAMES.len()),
            (SHIFT, KeyCode::F(8)) => {
                let items: Vec<String> = encoding::NAMES.iter().map(|n| n.to_string()).collect();
                if let Some(i) = dialog::menu(tui, "Encoding", &items, v.enc)? {
                    v.set_encoding(i);
                }
            }
            (NONE, KeyCode::F(7)) => {
                if let Some(state) = search_dialog(tui, &search, db)? {
                    let backward = state.1;
                    search = state.0;
                    v.mark = None;
                    status = find_next(&mut v, &search, backward, w);
                }
            }
            (SHIFT, KeyCode::F(7)) | (NONE, KeyCode::Char(' ')) => {
                status = find_next(&mut v, &search, false, w);
            }
            (ALT, KeyCode::F(7)) => status = find_next(&mut v, &search, true, w),
            (ALT, KeyCode::F(8)) => {
                if let Some(text) =
                    dialog::prompt(tui, "Go to", "Offset: bytes, 0x hex or percent%", "")?
                {
                    match v.parse_goto(&text) {
                        Some(offset) => {
                            v.remember_jump();
                            v.go_to(offset, w);
                        }
                        None => status = Some("Invalid position".into()),
                    }
                }
            }
            (CTRL, KeyCode::Char('z')) | (ALT, KeyCode::Backspace) => {
                if let Some(top) = v.jumps.pop() {
                    v.top = top;
                }
            }
            (CTRL_SHIFT, KeyCode::Char(c)) if c.is_ascii_digit() => {
                v.bookmarks[c as usize - '0' as usize] = Some(v.top);
            }
            (CTRL, KeyCode::Char(c)) if c.is_ascii_digit() => {
                if let Some(top) = v.bookmarks[c as usize - '0' as usize] {
                    v.remember_jump();
                    v.top = top;
                }
            }
            (CTRL, KeyCode::Char('c')) | (CTRL, KeyCode::Insert) => {
                if let Some((start, len)) = v.mark {
                    let bytes = v.read_at(start, len as usize);
                    let text: String = v.decode(&bytes).iter().map(|(c, _)| *c).collect();
                    tui.set_clipboard(&text);
                    *clipboard = Clipboard {
                        text,
                        vertical: false,
                    };
                }
            }
            (CTRL, KeyCode::Char('u')) => v.mark = None,
            (CTRL, KeyCode::Char('o')) => tui.user_screen()?,
            (CTRL, KeyCode::Char('b')) => *keybar = !*keybar,
            (CTRL_SHIFT, KeyCode::Char('b')) => v.status_bar = !v.status_bar,
            (CTRL, KeyCode::Char('s')) => v.scrollbar = !v.scrollbar,
            _ => {}
        }
    }
}

/// F7. Returns the options and whether to search backward.
fn search_dialog(
    tui: &mut Tui,
    last: &SearchState,
    db: &Rc<History>,
) -> io::Result<Option<(SearchState, bool)>> {
    let o = &last.opts;
    let mut form = Form::new(vec![
        Field::with_history("&Search for", &o.text, Kind::Search),
        Field::check("&Hex", last.hex),
        Field::check("&Case sensitive", o.case_sensitive),
        Field::check("&Whole words", o.whole_words),
        Field::check("Fu&zzy search", o.fuzzy),
        Field::check("R&egular expressions", o.regex),
    ])
    .with_buttons(&["Find next", "Find pre&vious", "Cancel"])
    .with_db(db);
    let button = form.run_buttons(tui, "Search")?;
    if !matches!(button, Some(0 | 1)) || form.text(0).is_empty() {
        return Ok(None);
    }
    let state = SearchState {
        opts: SearchOptions {
            text: form.text(0),
            case_sensitive: form.checked(2),
            whole_words: form.checked(3),
            fuzzy: form.checked(4),
            regex: form.checked(5),
            ..Default::default()
        },
        hex: form.checked(1),
    };
    db.save_search(&state.opts, false);
    db.set_flag("search.hex", state.hex);
    Ok(Some((state, button == Some(1))))
}

fn find_next(v: &mut Viewer, state: &SearchState, backward: bool, width: usize) -> Option<String> {
    if state.opts.text.is_empty() {
        return Some("Press F7 to search".into());
    }
    let from = match (v.mark, backward) {
        (Some((start, _)), false) => start + 1,
        (Some((start, _)), true) => start,
        (None, _) => v.top,
    };
    let o = &state.opts;
    let plain_bytes = state.hex || (o.case_sensitive && !o.whole_words && !o.fuzzy && !o.regex);
    let found = if plain_bytes {
        let needle = if state.hex {
            let Some(bytes) = parse_hex(&o.text) else {
                return Some("Hex text needs pairs of digits, like 6C 6F".into());
            };
            bytes
        } else {
            let Some(bytes) = encoding::encode(&o.text, v.enc) else {
                return Some(format!(
                    "The text cannot be written in {}",
                    encoding::NAMES[v.enc]
                ));
            };
            bytes
        };
        let backward_from = from.saturating_add(needle.len() as u64).saturating_sub(1);
        let start = if backward { backward_from } else { from };
        v.search(&needle, start, false, backward)
            .map(|at| (at, needle.len() as u64))
    } else {
        let m = match Matcher::new(o) {
            Ok(m) => m,
            Err(text) => return Some(text),
        };
        v.search_text(&m, from, backward)
    };
    match found {
        Some((at, len)) => {
            v.remember_jump();
            v.mark = Some((at, len));
            v.go_to(at, width);
            None
        }
        None => Some(format!("Not found: {}", o.text)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make(tag: &str, data: &[u8]) -> (Viewer, PathBuf) {
        let path = std::env::temp_dir().join(format!("far_view_{}_{tag}", std::process::id()));
        fs::write(&path, data).unwrap();
        (Viewer::open(&path).unwrap(), path)
    }

    #[test]
    fn rows_follow_lines_and_scrolling_round_trips() {
        let (mut v, p) = make("rows", b"one\ntwo\r\nthree");
        let r = v.row_at(0, 80);
        assert_eq!((r.bytes.as_slice(), r.next), (&b"one"[..], 4));
        v.scroll_down(1, 1, 80);
        assert_eq!(v.top, 4);
        v.scroll_down(1, 1, 80);
        assert_eq!(v.top, 9);
        v.scroll_up(1, 80);
        assert_eq!(v.top, 4);
        v.scroll_up(5, 80);
        assert_eq!(v.top, 0);
        fs::remove_file(p).unwrap();
    }

    #[test]
    fn wrap_breaks_at_width_without_splitting_characters() {
        let (mut v, p) = make("wrap", "жжжжж\nx".as_bytes());
        v.wrap = true;
        let r = v.row_at(0, 3);
        assert_eq!(r.bytes, "жжж".as_bytes());
        assert_eq!(r.next, 6);
        let r2 = v.row_at(r.next, 3);
        assert_eq!(r2.bytes, "жж".as_bytes());
        assert_eq!(v.prev_row(6, 3), 0);
        fs::remove_file(p).unwrap();
    }

    #[test]
    fn end_shows_the_last_page() {
        let data: String = (0..100).map(|i| format!("line{i}\n")).collect();
        let (mut v, p) = make("end", data.as_bytes());
        v.go_end(10, 80);
        let r = v.row_at(v.top, 80);
        assert_eq!(r.bytes, b"line90");
        assert!(v.at_end(10, 80));
        fs::remove_file(p).unwrap();
    }

    #[test]
    fn line_longer_than_the_window_still_advances() {
        let data = vec![b'a'; MAX_LINE * 2 + 10];
        let (v, p) = make("long", &data);
        let r = v.row_at(0, 80);
        assert_eq!(r.next, MAX_LINE as u64);
        fs::remove_file(p).unwrap();
    }

    #[test]
    fn forced_break_does_not_split_a_multibyte_character() {
        let mut data = vec![b'a'; MAX_LINE - 1];
        data.extend_from_slice("ж".as_bytes());
        data.extend_from_slice(b"tail");
        let (v, p) = make("cut", &data);
        let r = v.row_at(0, 80);
        assert_eq!(r.next, MAX_LINE as u64 - 1);
        assert_eq!(v.row_at(r.next, 80).bytes, "жtail".as_bytes());
        assert_eq!(utf8_safe_cut("aж".as_bytes()), 3);
        fs::remove_file(p).unwrap();
    }

    #[test]
    fn invalid_utf8_is_shown_as_replacement_and_keeps_offsets() {
        let (v, p) = make("bad", &[0x41, 0xff, 0x42]);
        let decoded = v.decode(&[0x41, 0xff, 0x42]);
        assert_eq!(decoded, vec![('A', 1), ('\u{fffd}', 1), ('B', 1)]);
        fs::remove_file(p).unwrap();
    }

    #[test]
    fn search_finds_matches_in_both_directions_and_across_chunks() {
        let mut data = vec![b'x'; SEARCH_CHUNK as usize - 2];
        data.extend_from_slice(b"needle");
        data.extend_from_slice(&[b'y'; 100]);
        data.extend_from_slice(b"NEEDLE");
        let (v, p) = make("search", &data);
        let first = SEARCH_CHUNK - 2;
        assert_eq!(v.search(b"needle", 0, false, false), Some(first));
        assert_eq!(v.search(b"needle", first + 1, false, false), None);
        assert_eq!(
            v.search(b"needle", first + 1, true, false),
            Some(first + 106)
        );
        assert_eq!(v.search(b"needle", v.size, true, true), Some(first + 106));
        assert_eq!(v.search(b"needle", first + 105, true, true), Some(first));
        fs::remove_file(p).unwrap();
    }

    fn matcher(text: &str, regex: bool, whole_words: bool) -> Matcher {
        Matcher::new(&SearchOptions {
            text: text.into(),
            regex,
            whole_words,
            ..Default::default()
        })
        .unwrap()
    }

    #[test]
    fn option_search_reports_byte_offsets_in_both_directions() {
        let (v, p) = make("opt", "жж Foo\nbar foo\r\nfoo".as_bytes());
        let m = matcher("foo", false, false);
        assert_eq!(v.search_text(&m, 0, false), Some((5, 3)));
        assert_eq!(v.search_text(&m, 6, false), Some((13, 3)));
        assert_eq!(v.search_text(&m, 14, false), Some((18, 3)));
        assert_eq!(v.search_text(&m, 19, false), None);
        assert_eq!(v.search_text(&m, v.size, true), Some((18, 3)));
        assert_eq!(v.search_text(&m, 18, true), Some((13, 3)));
        assert_eq!(v.search_text(&m, 13, true), Some((5, 3)));
        assert_eq!(v.search_text(&m, 5, true), None);
        fs::remove_file(p).unwrap();
    }

    #[test]
    fn option_search_supports_regex_and_whole_words() {
        let (v, p) = make("rx", b"concat cat\nab12");
        assert_eq!(
            v.search_text(&matcher("cat", false, true), 0, false),
            Some((7, 3))
        );
        assert_eq!(
            v.search_text(&matcher(r"\d+", true, false), 0, false),
            Some((13, 2))
        );
        assert_eq!(v.search_text(&matcher("^", true, false), 0, false), None);
        fs::remove_file(p).unwrap();
    }

    #[test]
    fn option_search_crosses_window_boundaries_in_both_directions() {
        let mut data = vec![];
        while data.len() < WINDOW - 2 {
            data.extend_from_slice(b"filler line\n");
        }
        let at = data.len() as u64;
        data.extend_from_slice(b"target line\n");
        while data.len() < WINDOW * 2 + 100 {
            data.extend_from_slice(b"filler line\n");
        }
        let (v, p) = make("win", &data);
        let m = matcher("target", false, false);
        assert_eq!(v.search_text(&m, 0, false), Some((at, 6)));
        assert_eq!(v.search_text(&m, v.size, true), Some((at, 6)));
        assert_eq!(v.search_text(&m, at + 1, false), None);
        assert_eq!(v.search_text(&m, at, true), None);
        fs::remove_file(p).unwrap();
    }

    #[test]
    fn word_wrap_breaks_after_spaces() {
        let (mut v, p) = make("ww", b"hello world again");
        v.wrap = true;
        assert_eq!(v.row_at(0, 8).bytes, b"hello wo");
        v.word_wrap = true;
        assert_eq!(v.row_at(0, 8).bytes, b"hello ");
        fs::remove_file(p).unwrap();
    }

    #[test]
    fn dump_mode_ignores_line_breaks_and_steps_by_width() {
        let (mut v, p) = make("dump", b"ab\ncd\nef");
        v.dump = true;
        let r = v.row_at(0, 4);
        assert_eq!((r.bytes.as_slice(), r.next), (&b"ab\nc"[..], 4));
        assert_eq!(v.prev_row(4, 4), 0);
        fs::remove_file(p).unwrap();
    }

    #[test]
    fn hex_width_changes_row_size() {
        let (mut v, p) = make("hw", &[0u8; 100]);
        v.hex = true;
        v.hex_width = 10;
        v.scroll_down(1, 2, 80);
        assert_eq!(v.top, 10);
        v.go_to(37, 80);
        assert_eq!(v.top, 30);
        fs::remove_file(p).unwrap();
    }

    #[test]
    fn widest_row_measures_what_is_on_screen() {
        let (v, p) = make("wide", b"ab\nabcdef\nabc");
        assert_eq!(v.widest_row(3, 80), 6);
        assert_eq!(v.widest_row(1, 80), 2);
        fs::remove_file(p).unwrap();
    }

    #[test]
    fn goto_parsing() {
        let (v, p) = make("goto", &[0u8; 1000]);
        assert_eq!(v.parse_goto("50%"), Some(500));
        assert_eq!(v.parse_goto("0x10"), Some(16));
        assert_eq!(v.parse_goto("7"), Some(7));
        assert_eq!(v.parse_goto("x"), None);
        fs::remove_file(p).unwrap();
    }

    #[test]
    fn hex_mode_aligns_and_ends_on_full_rows() {
        let (mut v, p) = make("hex", &[0u8; 1000]);
        v.hex = true;
        v.go_to(35, 80);
        assert_eq!(v.top, 32);
        v.go_end(10, 80);
        assert_eq!(v.top, (63 - 10) * 16);
        fs::remove_file(p).unwrap();
    }

    #[test]
    fn empty_file_is_safe() {
        let (mut v, p) = make("empty", b"");
        v.scroll_down(3, 10, 80);
        v.go_end(10, 80);
        assert_eq!(v.top, 0);
        let mut s = Screen::new(80, 24);
        v.draw(&mut s, true);
        fs::remove_file(p).unwrap();
    }

    #[test]
    fn single_byte_encodings_decode_through_the_table() {
        let (mut v, p) = make("enc", &[0xc6]);
        v.set_encoding(1);
        assert_eq!(v.decode(&[0xc6]), vec![('Ж', 1)]);
        fs::remove_file(p).unwrap();
    }

    #[test]
    fn a_folder_cannot_be_viewed() {
        assert!(Viewer::open(&std::env::temp_dir()).is_err());
    }

    #[test]
    fn growth_of_the_file_is_noticed() {
        let (mut v, p) = make("grow", b"one");
        fs::write(&p, b"one two").unwrap();
        v.refresh_size();
        assert_eq!(v.size, 7);
        fs::remove_file(p).unwrap();
    }

    #[test]
    fn drawing_is_safe_at_every_size_and_in_every_mode() {
        let mut data = b"short\nline\t\xff\xfe\x1b[2J\n".to_vec();
        data.extend(vec![b'x'; 500]);
        data.extend("\nмногобайтный 日本語\n".as_bytes());
        let (mut v, p) = make("sizes", &data);
        for (wrap, hex, dump) in [
            (false, false, false),
            (true, false, false),
            (false, true, false),
            (false, false, true),
        ] {
            v.wrap = wrap;
            v.hex = hex;
            v.dump = dump;
            for w in 1..50u16 {
                for h in 1..8u16 {
                    let mut s = Screen::new(w, h);
                    v.draw(&mut s, true);
                    v.draw(&mut s, false);
                }
            }
        }
        fs::remove_file(p).unwrap();
    }

    #[test]
    fn an_empty_file_draws_and_scrolls() {
        let (mut v, p) = make("empty", b"");
        let mut s = Screen::new(40, 10);
        v.draw(&mut s, true);
        v.scroll_down(5, 5, 40);
        v.scroll_up(5, 40);
        assert_eq!(v.top, 0);
        fs::remove_file(p).unwrap();
    }
}
