//! The F3 viewer. It reads only the part of the file it needs, so size does
//! not matter, and never loads the file into memory.

use crate::dialog::{self, Field, Form};
use crate::encoding;
use crate::fsops::find_bytes;
use crate::screen::{char_width, fit, fit_left, theme, Input, Screen, Tui, ALT, CTRL, NONE, SHIFT};
use crate::util;
use crossterm::event::KeyCode;
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

const MAX_LINE: usize = 64 * 1024;
const SEARCH_CHUNK: u64 = 1024 * 1024;
const TAB: usize = 8;
const HEX_WIDTH: u64 = 16;

/// What to do after the viewer closes.
#[derive(Debug, PartialEq, Eq)]
pub enum ViewerExit {
    Close,
    Edit,
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
            for (c, len) in self.decode(&chunk[..line_len]) {
                let w = cell_width(c, col);
                if col + w > width.max(1) && used > 0 {
                    return Row {
                        bytes: chunk[..used].to_vec(),
                        next: offset + used as u64,
                    };
                }
                col += w;
                used += len;
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
            return self.top + rows as u64 * HEX_WIDTH >= self.size;
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
                self.top + HEX_WIDTH
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
                self.top.saturating_sub(HEX_WIDTH)
            } else {
                self.prev_row(self.top, width)
            };
        }
    }

    /// Shows the end of the file.
    pub fn go_end(&mut self, rows: usize, width: usize) {
        if self.hex {
            let lines = self.size.div_ceil(HEX_WIDTH);
            self.top = lines.saturating_sub(rows as u64) * HEX_WIDTH;
            return;
        }
        self.top = self.size;
        self.scroll_up(rows, width);
    }

    /// Moves to a byte offset, aligned to the start of its row.
    pub fn go_to(&mut self, offset: u64, width: usize) {
        let offset = offset.min(self.size);
        if self.hex {
            self.top = offset - offset % HEX_WIDTH;
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

    fn draw(&self, s: &mut Screen) {
        let (w, h) = (s.w as usize, s.h as usize);
        s.fill(0, 0, s.w, s.h, ' ', theme::EDITOR);
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
        let rows = h.saturating_sub(2);
        if self.hex {
            self.draw_hex(s, rows);
        } else {
            self.draw_text(s, rows, w);
        }
        s.keybar(
            s.h - 1,
            &[
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
        );
    }

    fn draw_text(&self, s: &mut Screen, rows: usize, width: usize) {
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
                let cells = cell_width(c, col);
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
                    if c == '\t' {
                        s.fill(x as u16, y as u16 + 1, cells as u16, 1, ' ', style);
                    } else {
                        let shown = visible_char(c);
                        s.put(x as u16, y as u16 + 1, shown.encode_utf8(&mut text), style);
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

    fn draw_hex(&self, s: &mut Screen, rows: usize) {
        for y in 0..rows {
            let offset = self.top + y as u64 * HEX_WIDTH;
            if offset >= self.size {
                break;
            }
            let bytes = self.read_at(offset, HEX_WIDTH as usize);
            let row = y as u16 + 1;
            s.put(0, row, &format!("{offset:012X}: "), theme::EDITOR);
            for (i, b) in bytes.iter().enumerate() {
                let at = offset + i as u64;
                let style = match self.mark {
                    Some((start, len)) if at >= start && at < start + len => theme::SEARCH_HIT,
                    _ => theme::EDITOR,
                };
                let gap = usize::from(i >= 8);
                s.put((14 + i * 3 + gap) as u16, row, &format!("{b:02X}"), style);
                let shown = if b.is_ascii_graphic() || *b == b' ' {
                    *b as char
                } else {
                    '.'
                };
                s.put((64 + i) as u16, row, &shown.to_string(), style);
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

struct SearchState {
    text: String,
    ignore_case: bool,
}

/// Runs the viewer until it is closed.
pub fn run(tui: &mut Tui, path: &Path) -> io::Result<ViewerExit> {
    let mut v = Viewer::open(path)?;
    let mut search: Option<SearchState> = None;
    let mut status: Option<String> = None;
    loop {
        v.draw(&mut tui.screen);
        tui.screen.cursor = None;
        if let Some(text) = &status {
            let y = tui.screen.h.saturating_sub(2);
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
        let (w, rows) = (
            tui.screen.w as usize,
            (tui.screen.h as usize).saturating_sub(2),
        );
        v.refresh_size();
        match (key.mods, key.code) {
            (_, KeyCode::Esc) | (NONE, KeyCode::F(3)) | (NONE, KeyCode::F(10)) => {
                return Ok(ViewerExit::Close);
            }
            (NONE, KeyCode::F(6)) => return Ok(ViewerExit::Edit),
            (NONE, KeyCode::Down) => v.scroll_down(1, rows, w),
            (NONE, KeyCode::Up) => v.scroll_up(1, w),
            (NONE, KeyCode::PageDown) | (NONE, KeyCode::Char(' ')) => {
                v.scroll_down(rows.max(1), rows, w)
            }
            (NONE, KeyCode::PageUp) => v.scroll_up(rows.max(1), w),
            (NONE, KeyCode::Home) | (CTRL, KeyCode::Home) => {
                v.top = 0;
                v.left = 0;
            }
            (NONE, KeyCode::End) | (CTRL, KeyCode::End) => v.go_end(rows, w),
            (NONE, KeyCode::Right) if !v.wrap && !v.hex => v.left += 8,
            (NONE, KeyCode::Left) if !v.wrap && !v.hex => v.left = v.left.saturating_sub(8),
            (NONE, KeyCode::F(2)) => {
                v.wrap = !v.wrap;
                v.left = 0;
                let top = v.top;
                if !v.hex {
                    v.go_to(top, w);
                }
            }
            (NONE, KeyCode::F(4)) => {
                let top = v.top;
                v.hex = !v.hex;
                v.go_to(top, w);
            }
            (NONE, KeyCode::F(8)) => v.set_encoding((v.enc + 1) % encoding::NAMES.len()),
            (NONE, KeyCode::F(7)) => {
                let last = search.as_ref().map_or(String::new(), |s| s.text.clone());
                let ignore = search.as_ref().map_or(true, |s| s.ignore_case);
                let mut form = Form::new(vec![
                    Field::text("Search for:", &last),
                    Field::check("Ignore case (ASCII letters)", ignore),
                ]);
                if form.run(tui, "Search")? && !form.text(0).is_empty() {
                    search = Some(SearchState {
                        text: form.text(0),
                        ignore_case: form.checked(1),
                    });
                    v.mark = None;
                    status = find_next(&mut v, search.as_ref(), false, w);
                }
            }
            (SHIFT, KeyCode::F(7)) => status = find_next(&mut v, search.as_ref(), false, w),
            (ALT, KeyCode::F(7)) => status = find_next(&mut v, search.as_ref(), true, w),
            (ALT, KeyCode::F(8)) => {
                if let Some(text) =
                    dialog::prompt(tui, "Go to", "Offset: bytes, 0x hex or percent%", "")?
                {
                    match v.parse_goto(&text) {
                        Some(offset) => v.go_to(offset, w),
                        None => status = Some("Invalid position".into()),
                    }
                }
            }
            _ => {}
        }
    }
}

fn find_next(
    v: &mut Viewer,
    state: Option<&SearchState>,
    backward: bool,
    width: usize,
) -> Option<String> {
    let state = state?;
    let Some(needle) = encoding::encode(&state.text, v.enc) else {
        return Some(format!(
            "The text cannot be written in {}",
            encoding::NAMES[v.enc]
        ));
    };
    let from = match (v.mark, backward) {
        (Some((start, _)), false) => start + 1,
        (Some((start, len)), true) => start + len - 1,
        (None, false) => v.top,
        (None, true) => v.top,
    };
    match v.search(&needle, from, state.ignore_case, backward) {
        Some(at) => {
            v.mark = Some((at, needle.len() as u64));
            v.go_to(at, width);
            None
        }
        None => Some(format!("Not found: {}", state.text)),
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
        v.draw(&mut s);
        fs::remove_file(p).unwrap();
    }

    #[test]
    fn single_byte_encodings_decode_through_the_table() {
        let (mut v, p) = make("enc", &[0xc6]);
        v.set_encoding(1);
        assert_eq!(v.decode(&[0xc6]), vec![('Ж', 1)]);
        fs::remove_file(p).unwrap();
    }
}
