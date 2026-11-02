//! The F4 editor: a text buffer with full undo, stream and vertical blocks,
//! search and replace, and encoding-aware saving that never alters bytes the
//! user did not touch.

use crate::dialog::{self, Field, Form};
use crate::encoding;
use crate::screen::{
    fit, fit_left, str_width, theme, Input, Screen, Tui, ALT, ALT_SHIFT, CTRL, CTRL_SHIFT, NONE,
    SHIFT,
};
use crate::util;
use crate::viewer::{cell_width, visible_char};
use crossterm::event::KeyCode;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

const MAX_EDIT_SIZE: u64 = 512 * 1024 * 1024;
const BOM: &[u8] = b"\xef\xbb\xbf";

/// A position: line index and character index within the line.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Pos {
    pub line: usize,
    pub col: usize,
}

/// Shorthand for building a position.
pub fn pos(line: usize, col: usize) -> Pos {
    Pos { line, col }
}

#[derive(Clone, Debug)]
struct Edit {
    start: Pos,
    removed: String,
    inserted: String,
    before: Pos,
    after: Pos,
    group: u64,
    id: u64,
}

/// Lines of text with undo history.
pub struct Buffer {
    pub lines: Vec<String>,
    undo: Vec<Edit>,
    redo: Vec<Edit>,
    next_id: u64,
    next_group: u64,
    open_group: Option<u64>,
    saved_id: u64,
}

fn byte_at(line: &str, col: usize) -> usize {
    line.char_indices().nth(col).map_or(line.len(), |(i, _)| i)
}

/// Line length in characters.
fn line_len(line: &str) -> usize {
    line.chars().count()
}

impl Buffer {
    /// Splits text at line feeds. Joining again restores it exactly.
    pub fn from_text(text: &str) -> Self {
        Buffer {
            lines: text.split('\n').map(str::to_owned).collect(),
            undo: Vec::new(),
            redo: Vec::new(),
            next_id: 1,
            next_group: 1,
            open_group: None,
            saved_id: 0,
        }
    }

    pub fn to_text(&self) -> String {
        self.lines.join("\n")
    }

    pub fn is_modified(&self) -> bool {
        self.undo.last().map_or(0, |e| e.id) != self.saved_id
    }

    pub fn mark_saved(&mut self) {
        self.saved_id = self.undo.last().map_or(0, |e| e.id);
    }

    /// Clamps a position to the text.
    pub fn clamp(&self, p: Pos) -> Pos {
        let line = p.line.min(self.lines.len() - 1);
        Pos {
            line,
            col: p.col.min(line_len(&self.lines[line])),
        }
    }

    /// Returns the text between two positions.
    pub fn text_between(&self, a: Pos, b: Pos) -> String {
        if a.line == b.line {
            let line = &self.lines[a.line];
            return line[byte_at(line, a.col)..byte_at(line, b.col)].to_owned();
        }
        let first = &self.lines[a.line];
        let mut out = first[byte_at(first, a.col)..].to_owned();
        for line in &self.lines[a.line + 1..b.line] {
            out.push('\n');
            out.push_str(line);
        }
        let last = &self.lines[b.line];
        out.push('\n');
        out.push_str(&last[..byte_at(last, b.col)]);
        out
    }

    fn raw_replace(&mut self, start: Pos, end: Pos, text: &str) -> (String, Pos) {
        let removed = self.text_between(start, end);
        let first = &self.lines[start.line];
        let head = first[..byte_at(first, start.col)].to_owned();
        let last = &self.lines[end.line];
        let tail = last[byte_at(last, end.col)..].to_owned();
        let mut parts: Vec<String> = text.split('\n').map(str::to_owned).collect();
        let count = parts.len();
        let last_len = line_len(&parts[count - 1]);
        let end_pos = if count == 1 {
            pos(start.line, start.col + last_len)
        } else {
            pos(start.line + count - 1, last_len)
        };
        parts[0].insert_str(0, &head);
        parts[count - 1].push_str(&tail);
        self.lines.splice(start.line..=end.line, parts);
        (removed, end_pos)
    }

    /// Replaces a range and records it for undo. `cursor` is the position to
    /// restore on undo. Returns the end of the inserted text.
    pub fn replace(&mut self, start: Pos, end: Pos, text: &str, cursor: Pos) -> Pos {
        let (removed, after) = self.raw_replace(start, end, text);
        self.redo.clear();
        let id = self.next_id;
        self.next_id += 1;
        // Runs of single typed characters undo together.
        if let Some(last) = self.undo.last_mut() {
            let typed =
                text.chars().count() == 1 && text != "\n" && text != " " && removed.is_empty();
            if typed
                && self.open_group.is_none()
                && last.removed.is_empty()
                && !last.inserted.contains('\n')
                && !last.inserted.ends_with(' ')
                && last.after == start
            {
                last.inserted.push_str(text);
                last.after = after;
                last.id = id;
                return after;
            }
        }
        let group = self.open_group.unwrap_or_else(|| {
            let g = self.next_group;
            self.next_group += 1;
            g
        });
        self.undo.push(Edit {
            start,
            removed,
            inserted: text.to_owned(),
            before: cursor,
            after,
            group,
            id,
        });
        after
    }

    /// Makes edits until [`Buffer::end_group`] undo as one step.
    pub fn begin_group(&mut self) {
        self.open_group = Some(self.next_group);
        self.next_group += 1;
    }

    pub fn end_group(&mut self) {
        self.open_group = None;
    }

    /// Undoes one step and returns where the cursor belongs.
    pub fn undo(&mut self) -> Option<Pos> {
        let group = self.undo.last()?.group;
        let mut cursor = None;
        while self.undo.last().is_some_and(|e| e.group == group) {
            let edit = self.undo.pop()?;
            self.raw_replace(edit.start, edit.after, &edit.removed);
            cursor = Some(edit.before);
            self.redo.push(edit);
        }
        cursor
    }

    /// Redoes one step and returns where the cursor belongs.
    pub fn redo(&mut self) -> Option<Pos> {
        let group = self.redo.last()?.group;
        let mut cursor = None;
        while self.redo.last().is_some_and(|e| e.group == group) {
            let edit = self.redo.pop()?;
            let end = end_of_text(edit.start, &edit.removed);
            self.raw_replace(edit.start, end, &edit.inserted);
            cursor = Some(edit.after);
            self.undo.push(edit);
        }
        cursor
    }

    /// Finds a single-line pattern from `from`.
    pub fn find(
        &self,
        needle: &str,
        from: Pos,
        ignore_case: bool,
        backward: bool,
    ) -> Option<(Pos, Pos)> {
        let fold = |text: &str| -> Vec<char> {
            if ignore_case {
                text.chars()
                    .map(|c| c.to_lowercase().next().unwrap_or(c))
                    .collect()
            } else {
                text.chars().collect()
            }
        };
        let pattern = fold(needle);
        if pattern.is_empty() || needle.contains('\n') {
            return None;
        }
        let matches = |line: &[char], at: usize| line[at..].starts_with(&pattern);
        if backward {
            for l in (0..=from.line).rev() {
                let line = fold(&self.lines[l]);
                if line.len() < pattern.len() {
                    continue;
                }
                let limit = if l == from.line { from.col } else { line.len() };
                let highest = limit.min(line.len()).checked_sub(pattern.len());
                if let Some(highest) = highest {
                    for at in (0..=highest).rev() {
                        if matches(&line, at) {
                            return Some((pos(l, at), pos(l, at + pattern.len())));
                        }
                    }
                }
            }
            return None;
        }
        for l in from.line..self.lines.len() {
            let line = fold(&self.lines[l]);
            let begin = if l == from.line { from.col } else { 0 };
            if line.len() < pattern.len() || begin > line.len() - pattern.len() {
                continue;
            }
            for at in begin..=line.len() - pattern.len() {
                if matches(&line, at) {
                    return Some((pos(l, at), pos(l, at + pattern.len())));
                }
            }
        }
        None
    }
}

fn end_of_text(start: Pos, text: &str) -> Pos {
    let breaks = text.matches('\n').count();
    let last = text.rsplit('\n').next().unwrap_or("");
    if breaks == 0 {
        pos(start.line, start.col + line_len(last))
    } else {
        pos(start.line + breaks, line_len(last))
    }
}

/// How a file on disk maps to edited text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileFormat {
    pub enc: usize,
    pub crlf: bool,
    pub bom: bool,
}

/// Decodes file contents. Mixed line endings stay in the text as `\r`, so
/// they survive a save unchanged.
pub fn decode_file(bytes: &[u8]) -> Result<(String, FileFormat), String> {
    if encoding::is_utf16(bytes) {
        return Err("UTF-16 files can be viewed with F3 but not edited yet".into());
    }
    let (body, bom) = match bytes.strip_prefix(BOM) {
        Some(rest) => (rest, true),
        None => (bytes, false),
    };
    let (text, enc) = match encoding::decode(body, 0) {
        Some(text) => (text, 0),
        // Not UTF-8. A single-byte encoding round-trips every byte.
        None => (encoding::decode(body, 1).unwrap_or_default(), 1),
    };
    let lf = text.matches('\n').count();
    let crlf = lf > 0 && text.matches("\r\n").count() == lf;
    let text = if crlf {
        text.replace("\r\n", "\n")
    } else {
        text
    };
    Ok((text, FileFormat { enc, crlf, bom }))
}

/// Encodes text for saving. Fails if a character does not fit the encoding.
pub fn encode_file(text: &str, format: &FileFormat) -> Result<Vec<u8>, String> {
    let converted;
    let text = if format.crlf {
        converted = text.replace('\n', "\r\n");
        &converted
    } else {
        text
    };
    let Some(mut bytes) = encoding::encode(text, format.enc) else {
        return Err(format!(
            "Some characters do not exist in {}. Press F8 to choose another encoding.",
            encoding::NAMES[format.enc]
        ));
    };
    if format.bom {
        let mut out = BOM.to_vec();
        out.append(&mut bytes);
        return Ok(out);
    }
    Ok(bytes)
}

/// Writes a file atomically where possible, so a failed save cannot destroy
/// the original. Links keep working: a symbolic link is written through, and
/// a file with several hard links, or owned by someone else, is written in
/// place so that its identity and ownership stay the same.
pub fn write_file(path: &Path, data: &[u8]) -> io::Result<()> {
    let real = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let existing = fs::metadata(&real).ok();
    let in_place = existing
        .as_ref()
        // SAFETY: geteuid has no preconditions.
        .is_some_and(|m| m.nlink() > 1 || m.uid() != unsafe { libc::geteuid() });
    if in_place {
        return write_in_place(&real, data);
    }
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let temp = real.with_file_name(format!(
        ".far_save_{}_{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let mode = existing.as_ref().map_or(0o666, |m| m.mode() & 0o7777);
    let opened = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&temp);
    let mut file = match opened {
        Ok(file) => file,
        // A read-only folder can still hold a writable file.
        Err(_) if existing.is_some() => return write_in_place(&real, data),
        Err(e) => return Err(e),
    };
    let result = (|| {
        file.write_all(data)?;
        file.sync_all()?;
        if existing.is_some() {
            // The umask must not change the mode of an existing file.
            fs::set_permissions(&temp, fs::Permissions::from_mode(mode))?;
        }
        fs::rename(&temp, &real)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn write_in_place(path: &Path, data: &[u8]) -> io::Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)?;
    file.write_all(data)?;
    file.sync_all()
}

/// Text copied in the editor. Vertical blocks paste as columns.
#[derive(Clone, Default)]
pub struct Clipboard {
    pub text: String,
    pub vertical: bool,
}

/// What to do after the editor closes.
#[derive(Debug, PartialEq, Eq)]
pub enum EditorExit {
    Close,
    View,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SelKind {
    Stream,
    Vertical,
}

#[derive(Clone, Copy, Debug)]
struct Selection {
    kind: SelKind,
    anchor: Pos,
    anchor_vis: usize,
}

/// Editor state, independent of the terminal.
pub struct Editor {
    pub path: PathBuf,
    pub buf: Buffer,
    pub format: FileFormat,
    pub cursor: Pos,
    want_vis: usize,
    top: usize,
    left: usize,
    selection: Option<Selection>,
    overwrite: bool,
    mtime: Option<(i64, i64)>,
    search: String,
    replace: String,
    ignore_case: bool,
}

fn stamp(meta: &fs::Metadata) -> (i64, i64) {
    (meta.mtime(), meta.mtime_nsec())
}

/// Columns before a character position, expanding tabs.
fn vis_of(line: &str, col: usize) -> usize {
    let mut vis = 0;
    for c in line.chars().take(col) {
        vis += cell_width(c, vis);
    }
    vis
}

/// The character position at or left of a column.
fn col_of(line: &str, vis: usize) -> usize {
    let mut at = 0;
    for (i, c) in line.chars().enumerate() {
        let w = cell_width(c, at);
        if at + w > vis {
            return i;
        }
        at += w;
    }
    line_len(line)
}

impl Editor {
    /// Opens a file, or starts an empty buffer if it does not exist.
    pub fn open(path: &Path) -> Result<Self, String> {
        let (text, format, mtime) = match fs::metadata(path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => (
                String::new(),
                FileFormat {
                    enc: 0,
                    crlf: false,
                    bom: false,
                },
                None,
            ),
            Err(e) => return Err(e.to_string()),
            Ok(meta) => {
                if !meta.is_file() {
                    return Err("Only regular files can be edited".into());
                }
                if meta.len() > MAX_EDIT_SIZE {
                    return Err("This file is over 512 MB. It can be viewed with F3.".into());
                }
                let bytes = fs::read(path).map_err(|e| e.to_string())?;
                let (text, format) = decode_file(&bytes)?;
                (text, format, Some(stamp(&meta)))
            }
        };
        Ok(Editor {
            path: path.to_path_buf(),
            buf: Buffer::from_text(&text),
            format,
            cursor: Pos::default(),
            want_vis: 0,
            top: 0,
            left: 0,
            selection: None,
            overwrite: false,
            mtime,
            search: String::new(),
            replace: String::new(),
            ignore_case: true,
        })
    }

    fn line(&self) -> &str {
        &self.buf.lines[self.cursor.line]
    }

    fn sync_vis(&mut self) {
        self.want_vis = vis_of(self.line(), self.cursor.col);
    }

    fn select_for(&mut self, mods: crossterm::event::KeyModifiers) {
        let kind = if mods == ALT_SHIFT {
            Some(SelKind::Vertical)
        } else if mods.contains(SHIFT) {
            Some(SelKind::Stream)
        } else {
            None
        };
        match kind {
            None => self.selection = None,
            Some(kind) => {
                let same = self.selection.is_some_and(|s| s.kind == kind);
                if !same {
                    self.selection = Some(Selection {
                        kind,
                        anchor: self.cursor,
                        anchor_vis: vis_of(self.line(), self.cursor.col),
                    });
                }
            }
        }
    }

    fn move_vertical(&mut self, delta: isize) {
        let last = self.buf.lines.len() as isize - 1;
        let line = (self.cursor.line as isize + delta).clamp(0, last) as usize;
        self.cursor.line = line;
        self.cursor.col = col_of(&self.buf.lines[line], self.want_vis);
    }

    fn move_left(&mut self) {
        if self.cursor.col > 0 {
            self.cursor.col -= 1;
        } else if self.cursor.line > 0 {
            self.cursor.line -= 1;
            self.cursor.col = line_len(self.line());
        }
        self.sync_vis();
    }

    fn move_right(&mut self) {
        if self.cursor.col < line_len(self.line()) {
            self.cursor.col += 1;
        } else if self.cursor.line + 1 < self.buf.lines.len() {
            self.cursor.line += 1;
            self.cursor.col = 0;
        }
        self.sync_vis();
    }

    fn word_left(&mut self) {
        let chars: Vec<char> = self.line().chars().collect();
        let mut col = self.cursor.col;
        if col == 0 {
            self.move_left();
            return;
        }
        while col > 0 && !chars[col - 1].is_alphanumeric() {
            col -= 1;
        }
        while col > 0 && chars[col - 1].is_alphanumeric() {
            col -= 1;
        }
        self.cursor.col = col;
        self.sync_vis();
    }

    fn word_right(&mut self) {
        let chars: Vec<char> = self.line().chars().collect();
        let mut col = self.cursor.col;
        if col >= chars.len() {
            self.move_right();
            return;
        }
        while col < chars.len() && chars[col].is_alphanumeric() {
            col += 1;
        }
        while col < chars.len() && !chars[col].is_alphanumeric() {
            col += 1;
        }
        self.cursor.col = col;
        self.sync_vis();
    }

    /// Inserts text at the cursor, honouring overwrite mode for single keys.
    pub fn type_text(&mut self, text: &str) {
        self.selection = None;
        let start = self.cursor;
        let mut end = start;
        if self.overwrite
            && text.chars().count() == 1
            && text != "\n"
            && start.col < line_len(self.line())
        {
            end.col += 1;
        }
        self.cursor = self.buf.replace(start, end, text, start);
        self.sync_vis();
    }

    pub fn backspace(&mut self) {
        self.selection = None;
        let end = self.cursor;
        self.move_left();
        if self.cursor != end {
            self.cursor = self.buf.replace(self.cursor, end, "", end);
        }
        self.sync_vis();
    }

    pub fn delete_forward(&mut self) {
        if self.selection.is_some() {
            self.delete_selection();
            return;
        }
        let start = self.cursor;
        self.move_right();
        let end = self.cursor;
        self.cursor = start;
        if start != end {
            self.buf.replace(start, end, "", start);
        }
        self.sync_vis();
    }

    /// Ordered ends of a stream selection, if it is not empty.
    fn stream_range(&self) -> Option<(Pos, Pos)> {
        let sel = self.selection.filter(|s| s.kind == SelKind::Stream)?;
        let (a, b) = (sel.anchor.min(self.cursor), sel.anchor.max(self.cursor));
        (a != b).then_some((a, b))
    }

    /// Lines and columns of a vertical block, if it is not empty.
    fn block_range(&self) -> Option<(usize, usize, usize, usize)> {
        let sel = self.selection.filter(|s| s.kind == SelKind::Vertical)?;
        let (lo, hi) = (
            sel.anchor.line.min(self.cursor.line),
            sel.anchor.line.max(self.cursor.line),
        );
        let (vis_a, vis_b) = (
            sel.anchor_vis,
            vis_of(self.line(), self.cursor.col).max(self.want_vis),
        );
        let (l, r) = (vis_a.min(vis_b), vis_a.max(vis_b));
        (l != r).then_some((lo, hi, l, r))
    }

    /// Copies the selection, if any.
    pub fn copy_selection(&self) -> Option<Clipboard> {
        if let Some((a, b)) = self.stream_range() {
            return Some(Clipboard {
                text: self.buf.text_between(a, b),
                vertical: false,
            });
        }
        let (lo, hi, l, r) = self.block_range()?;
        let rows: Vec<String> = (lo..=hi)
            .map(|i| {
                let line = &self.buf.lines[i];
                let (from, to) = (col_of(line, l), col_of(line, r));
                line.chars().skip(from).take(to - from).collect()
            })
            .collect();
        Some(Clipboard {
            text: rows.join("\n"),
            vertical: true,
        })
    }

    /// Deletes the selection, if any.
    pub fn delete_selection(&mut self) {
        let before = self.cursor;
        if let Some((a, b)) = self.stream_range() {
            self.cursor = self.buf.replace(a, b, "", before);
        } else if let Some((lo, hi, l, r)) = self.block_range() {
            self.buf.begin_group();
            for i in lo..=hi {
                let line = self.buf.lines[i].clone();
                let (from, to) = (col_of(&line, l), col_of(&line, r));
                if from != to {
                    self.buf.replace(pos(i, from), pos(i, to), "", before);
                }
            }
            self.buf.end_group();
            self.cursor = self.buf.clamp(pos(
                self.cursor.line.min(hi),
                col_of(&self.buf.lines[lo], l),
            ));
        }
        self.selection = None;
        self.sync_vis();
    }

    /// Pastes text. A vertical clipboard goes in as a column.
    pub fn paste(&mut self, clip: &Clipboard) {
        if clip.text.is_empty() {
            return;
        }
        if !clip.vertical {
            self.selection = None;
            let start = self.cursor;
            self.cursor = self.buf.replace(start, start, &clip.text, start);
            self.sync_vis();
            return;
        }
        let before = self.cursor;
        let vis = vis_of(self.line(), before.col);
        self.buf.begin_group();
        for (i, row) in clip.text.split('\n').enumerate() {
            let line_no = before.line + i;
            if line_no >= self.buf.lines.len() {
                let end = self.buf.clamp(pos(usize::MAX, usize::MAX));
                self.buf.replace(end, end, "\n", before);
            }
            let line = self.buf.lines[line_no].clone();
            let width = vis_of(&line, line_len(&line));
            let at = pos(line_no, col_of(&line, vis));
            if width < vis {
                let pad = " ".repeat(vis - width);
                let end = pos(line_no, line_len(&line));
                self.buf.replace(end, end, &format!("{pad}{row}"), before);
            } else {
                self.buf.replace(at, at, row, before);
            }
        }
        self.buf.end_group();
        self.selection = None;
        self.sync_vis();
    }

    fn delete_line(&mut self) {
        self.selection = None;
        let l = self.cursor.line;
        let before = self.cursor;
        if l + 1 < self.buf.lines.len() {
            self.buf.replace(pos(l, 0), pos(l + 1, 0), "", before);
        } else if l > 0 {
            let prev = line_len(&self.buf.lines[l - 1]);
            let end = line_len(&self.buf.lines[l]);
            self.buf.replace(pos(l - 1, prev), pos(l, end), "", before);
            self.cursor.line -= 1;
        } else {
            let end = line_len(&self.buf.lines[0]);
            self.buf.replace(pos(0, 0), pos(0, end), "", before);
        }
        self.cursor = self.buf.clamp(pos(
            self.cursor.line,
            col_of(
                &self.buf.lines[self.cursor.line.min(self.buf.lines.len() - 1)],
                self.want_vis,
            ),
        ));
    }

    /// Saves to the current path.
    pub fn save(&mut self) -> Result<(), String> {
        let bytes = encode_file(&self.buf.to_text(), &self.format)?;
        write_file(&self.path, &bytes).map_err(|e| e.to_string())?;
        self.buf.mark_saved();
        self.mtime = fs::metadata(&self.path).ok().map(|m| stamp(&m));
        Ok(())
    }

    /// True when another program changed the file since it was opened.
    fn changed_on_disk(&self) -> bool {
        match (&self.mtime, fs::metadata(&self.path)) {
            (Some(known), Ok(meta)) => *known != stamp(&meta),
            _ => false,
        }
    }

    fn scroll_into_view(&mut self, rows: usize, width: usize) {
        if self.cursor.line < self.top {
            self.top = self.cursor.line;
        }
        if self.cursor.line >= self.top + rows {
            self.top = self.cursor.line + 1 - rows;
        }
        let vis = vis_of(self.line(), self.cursor.col);
        if vis < self.left {
            self.left = vis.saturating_sub(width / 4);
        }
        if vis >= self.left + width {
            self.left = vis + 1 - width + width / 4;
        }
    }

    fn draw(&mut self, s: &mut Screen) {
        let (w, h) = (s.w as usize, s.h as usize);
        let rows = h.saturating_sub(2).max(1);
        self.scroll_into_view(rows, w.max(1));
        s.fill(0, 0, s.w, s.h, ' ', theme::EDITOR);
        let info = format!(
            " {}{} {} {} Ln {}/{} Col {} ",
            if self.buf.is_modified() { "* " } else { "" },
            encoding::NAMES[self.format.enc],
            if self.format.crlf { "CRLF" } else { "LF" },
            if self.overwrite { "OVR" } else { "INS" },
            self.cursor.line + 1,
            self.buf.lines.len(),
            vis_of(self.line(), self.cursor.col) + 1,
        );
        let room = w.saturating_sub(str_width(&info));
        let title = fit(&fit_left(&util::tilde(&self.path), room), room);
        s.put(0, 0, &format!("{title}{info}"), theme::STATUS);
        let stream = self.stream_range();
        let block = self.block_range();
        let mut buf = [0u8; 4];
        for y in 0..rows {
            let line_no = self.top + y;
            let Some(line) = self.buf.lines.get(line_no) else {
                break;
            };
            let mut vis = 0;
            for (i, c) in line.chars().enumerate() {
                let cells = cell_width(c, vis);
                let here = pos(line_no, i);
                let chosen = stream.is_some_and(|(a, b)| here >= a && here < b)
                    || block.is_some_and(|(lo, hi, l, r)| {
                        line_no >= lo && line_no <= hi && vis >= l && vis < r
                    });
                let style = if chosen {
                    theme::EDITOR_SELECT
                } else {
                    theme::EDITOR
                };
                if vis >= self.left {
                    let x = vis - self.left;
                    if x + cells > w {
                        break;
                    }
                    if c == '\t' {
                        s.fill(x as u16, y as u16 + 1, cells as u16, 1, ' ', style);
                    } else {
                        s.put(
                            x as u16,
                            y as u16 + 1,
                            visible_char(c).encode_utf8(&mut buf),
                            style,
                        );
                    }
                }
                vis += cells;
            }
        }
        let x = vis_of(self.line(), self.cursor.col).saturating_sub(self.left);
        s.cursor = Some((x as u16, (self.cursor.line - self.top) as u16 + 1));
        s.keybar(
            s.h - 1,
            &[
                "Help", "Save", "", "", "", "View", "Search", "Encod", "", "Quit", "", "",
            ],
        );
    }
}

/// Runs the editor until it is closed.
pub fn run(tui: &mut Tui, path: &Path, clip: &mut Clipboard) -> io::Result<EditorExit> {
    let mut ed = match Editor::open(path) {
        Ok(ed) => ed,
        Err(text) => {
            dialog::alert(tui, "Editor", &text)?;
            return Ok(EditorExit::Close);
        }
    };
    loop {
        ed.draw(&mut tui.screen);
        tui.draw()?;
        let input = tui.read(Duration::from_millis(1000))?;
        let key = match input {
            Some(Input::Key(key)) => key,
            Some(Input::Paste(text)) => {
                let text = text.replace("\r\n", "\n").replace('\r', "\n");
                let start = ed.cursor;
                ed.selection = None;
                ed.cursor = ed.buf.replace(start, start, &text, start);
                ed.sync_vis();
                continue;
            }
            _ => continue,
        };
        let rows = (tui.screen.h as usize).saturating_sub(2).max(1) as isize;
        let moving = matches!(key.mods, NONE | SHIFT | ALT_SHIFT);
        match (key.mods, key.code) {
            (_, KeyCode::Up) if moving => {
                ed.select_for(key.mods);
                ed.move_vertical(-1);
            }
            (_, KeyCode::Down) if moving => {
                ed.select_for(key.mods);
                ed.move_vertical(1);
            }
            (_, KeyCode::Left) if moving => {
                ed.select_for(key.mods);
                ed.move_left();
            }
            (_, KeyCode::Right) if moving => {
                ed.select_for(key.mods);
                ed.move_right();
            }
            (_, KeyCode::PageUp) if moving => {
                ed.select_for(key.mods);
                ed.move_vertical(-rows);
            }
            (_, KeyCode::PageDown) if moving => {
                ed.select_for(key.mods);
                ed.move_vertical(rows);
            }
            (_, KeyCode::Home) if moving => {
                ed.select_for(key.mods);
                ed.cursor.col = 0;
                ed.sync_vis();
            }
            (_, KeyCode::End) if moving => {
                ed.select_for(key.mods);
                ed.cursor.col = line_len(ed.line());
                ed.sync_vis();
            }
            (CTRL, KeyCode::Left) | (CTRL_SHIFT, KeyCode::Left) => {
                ed.select_for(key.mods - CTRL);
                ed.word_left();
            }
            (CTRL, KeyCode::Right) | (CTRL_SHIFT, KeyCode::Right) => {
                ed.select_for(key.mods - CTRL);
                ed.word_right();
            }
            (CTRL, KeyCode::Home) | (CTRL_SHIFT, KeyCode::Home) | (CTRL, KeyCode::PageUp) => {
                ed.select_for(key.mods - CTRL);
                ed.cursor = Pos::default();
                ed.sync_vis();
            }
            (CTRL, KeyCode::End) | (CTRL_SHIFT, KeyCode::End) | (CTRL, KeyCode::PageDown) => {
                ed.select_for(key.mods - CTRL);
                ed.cursor = ed.buf.clamp(pos(usize::MAX, usize::MAX));
                ed.sync_vis();
            }
            (CTRL, KeyCode::Up) => ed.top = ed.top.saturating_sub(1),
            (CTRL, KeyCode::Down) => ed.top = (ed.top + 1).min(ed.buf.lines.len() - 1),
            (NONE, KeyCode::Enter) | (SHIFT, KeyCode::Enter) => ed.type_text("\n"),
            (NONE, KeyCode::Tab) => ed.type_text("\t"),
            (NONE, KeyCode::Char(c)) => {
                let mut b = [0u8; 4];
                ed.type_text(c.encode_utf8(&mut b));
            }
            (NONE, KeyCode::Backspace) => ed.backspace(),
            (CTRL, KeyCode::Backspace) | (CTRL, KeyCode::Char('h')) => {
                let end = ed.cursor;
                ed.word_left();
                ed.cursor = ed.buf.replace(ed.cursor, end, "", end);
                ed.selection = None;
                ed.sync_vis();
            }
            (CTRL, KeyCode::Delete) | (CTRL, KeyCode::Char('t')) => {
                let start = ed.cursor;
                ed.word_right();
                let end = ed.cursor;
                ed.cursor = ed.buf.replace(start, end, "", start);
                ed.selection = None;
                ed.sync_vis();
            }
            (NONE, KeyCode::Delete) => ed.delete_forward(),
            (NONE, KeyCode::Insert) => ed.overwrite = !ed.overwrite,
            (CTRL, KeyCode::Char('y')) => ed.delete_line(),
            (CTRL, KeyCode::Char('k')) => {
                ed.selection = None;
                let start = ed.cursor;
                let end = pos(start.line, line_len(ed.line()));
                ed.buf.replace(start, end, "", start);
            }
            (CTRL, KeyCode::Char('z')) | (ALT, KeyCode::Backspace) => {
                ed.selection = None;
                if let Some(p) = ed.buf.undo() {
                    ed.cursor = ed.buf.clamp(p);
                    ed.sync_vis();
                }
            }
            (CTRL_SHIFT, KeyCode::Char('z')) => {
                ed.selection = None;
                if let Some(p) = ed.buf.redo() {
                    ed.cursor = ed.buf.clamp(p);
                    ed.sync_vis();
                }
            }
            (CTRL, KeyCode::Char('a')) => {
                ed.selection = Some(Selection {
                    kind: SelKind::Stream,
                    anchor: Pos::default(),
                    anchor_vis: 0,
                });
                ed.cursor = ed.buf.clamp(pos(usize::MAX, usize::MAX));
                ed.sync_vis();
            }
            (CTRL, KeyCode::Char('u')) => ed.selection = None,
            (CTRL, KeyCode::Char('c')) | (CTRL, KeyCode::Insert) => {
                if let Some(copied) = ed.copy_selection() {
                    tui.set_clipboard(&copied.text);
                    *clip = copied;
                }
            }
            (CTRL, KeyCode::Char('x')) | (SHIFT, KeyCode::Delete) => {
                if let Some(copied) = ed.copy_selection() {
                    tui.set_clipboard(&copied.text);
                    *clip = copied;
                    ed.delete_selection();
                }
            }
            (CTRL, KeyCode::Char('v')) | (SHIFT, KeyCode::Insert) => ed.paste(clip),
            (CTRL, KeyCode::Char('d')) => ed.delete_selection(),
            (NONE, KeyCode::F(2)) => save_interactive(tui, &mut ed)?,
            (SHIFT, KeyCode::F(2)) => save_as(tui, &mut ed)?,
            (SHIFT, KeyCode::F(10)) => {
                save_interactive(tui, &mut ed)?;
                if !ed.buf.is_modified() {
                    return Ok(EditorExit::Close);
                }
            }
            (NONE, KeyCode::F(6)) => {
                if confirm_leave(tui, &mut ed)? {
                    return Ok(EditorExit::View);
                }
            }
            (_, KeyCode::Esc) | (NONE, KeyCode::F(10)) => {
                if confirm_leave(tui, &mut ed)? {
                    return Ok(EditorExit::Close);
                }
            }
            (NONE, KeyCode::F(8)) => change_encoding(&mut ed)?,
            (NONE, KeyCode::F(7)) => {
                let mut form = Form::new(vec![
                    Field::text("Search for:", &ed.search),
                    Field::check("Ignore case", ed.ignore_case),
                ]);
                if form.run(tui, "Search")? {
                    ed.search = form.text(0);
                    ed.ignore_case = form.checked(1);
                    find_step(tui, &mut ed, false)?;
                }
            }
            (SHIFT, KeyCode::F(7)) => find_step(tui, &mut ed, false)?,
            (ALT, KeyCode::F(7)) => find_step(tui, &mut ed, true)?,
            (CTRL, KeyCode::F(7)) => replace_all(tui, &mut ed)?,
            (ALT, KeyCode::F(8)) => {
                if let Some(text) = dialog::prompt(tui, "Go to", "Line [,column]:", "")? {
                    let mut parts = text.split(',').map(|p| p.trim().parse::<usize>());
                    match parts.next() {
                        Some(Ok(line)) => {
                            let col = parts.next().and_then(Result::ok).unwrap_or(1);
                            ed.cursor = ed
                                .buf
                                .clamp(pos(line.saturating_sub(1), col.saturating_sub(1)));
                            ed.selection = None;
                            ed.sync_vis();
                        }
                        _ => dialog::alert(tui, "Go to", "Enter a line number")?,
                    }
                }
            }
            _ => {}
        }
    }
}

fn save_interactive(tui: &mut Tui, ed: &mut Editor) -> io::Result<()> {
    if ed.changed_on_disk() {
        let lines = vec![
            "The file was changed by another program.".to_string(),
            "Overwrite those changes?".to_string(),
        ];
        if dialog::message(tui, "Save", &lines, &["Overwrite", "Cancel"], true)? != Some(0) {
            return Ok(());
        }
    }
    if let Err(text) = ed.save() {
        dialog::alert(tui, "Save failed", &text)?;
    }
    Ok(())
}

fn save_as(tui: &mut Tui, ed: &mut Editor) -> io::Result<()> {
    let current = ed.path.display().to_string();
    if let Some(text) = dialog::prompt(tui, "Save as", "File name:", &current)? {
        if text.is_empty() {
            return Ok(());
        }
        let base = ed.path.parent().unwrap_or(Path::new("/")).to_path_buf();
        let target = util::resolve_path(&base, &text);
        if target != ed.path && target.exists() {
            let lines = vec![
                format!("{} already exists.", target.display()),
                "Replace it?".to_string(),
            ];
            if dialog::message(tui, "Save as", &lines, &["Replace", "Cancel"], true)? != Some(0) {
                return Ok(());
            }
        }
        ed.path = target;
        ed.mtime = None;
        if let Err(text) = ed.save() {
            dialog::alert(tui, "Save failed", &text)?;
        }
    }
    Ok(())
}

/// Returns true when it is safe to leave the editor.
fn confirm_leave(tui: &mut Tui, ed: &mut Editor) -> io::Result<bool> {
    if !ed.buf.is_modified() {
        return Ok(true);
    }
    let lines = vec![
        "The file has been modified.".to_string(),
        ed.path.display().to_string(),
    ];
    match dialog::message(
        tui,
        "Editor",
        &lines,
        &["Save", "Don't save", "Continue"],
        false,
    )? {
        Some(0) => {
            save_interactive(tui, ed)?;
            Ok(!ed.buf.is_modified())
        }
        Some(1) => Ok(true),
        _ => Ok(false),
    }
}

/// F8: reread an unmodified file in the next encoding. With unsaved edits,
/// only the encoding used for saving changes.
fn change_encoding(ed: &mut Editor) -> io::Result<()> {
    let next = (ed.format.enc + 1) % encoding::NAMES.len();
    if ed.buf.is_modified() || ed.mtime.is_none() {
        ed.format.enc = next;
        return Ok(());
    }
    let bytes = fs::read(&ed.path)?;
    let body = if ed.format.bom {
        bytes.strip_prefix(BOM).unwrap_or(&bytes)
    } else {
        &bytes[..]
    };
    match encoding::decode(body, next) {
        Some(text) => {
            let text = if ed.format.crlf {
                text.replace("\r\n", "\n")
            } else {
                text
            };
            ed.buf = Buffer::from_text(&text);
            ed.format.enc = next;
            ed.cursor = ed.buf.clamp(ed.cursor);
            ed.selection = None;
            ed.sync_vis();
        }
        None => {
            // UTF-8 was tried on bytes that are not UTF-8: skip to the next.
            ed.format.enc = next;
            return change_encoding(ed);
        }
    }
    Ok(())
}

fn select_hit(ed: &mut Editor, a: Pos, b: Pos) {
    ed.selection = Some(Selection {
        kind: SelKind::Stream,
        anchor: a,
        anchor_vis: 0,
    });
    ed.cursor = b;
    ed.sync_vis();
}

fn find_step(tui: &mut Tui, ed: &mut Editor, backward: bool) -> io::Result<()> {
    if ed.search.is_empty() {
        return Ok(());
    }
    let from = match (ed.stream_range(), backward) {
        (Some((a, _)), true) => a,
        (_, _) => ed.cursor,
    };
    match ed.buf.find(&ed.search, from, ed.ignore_case, backward) {
        Some((a, b)) => select_hit(ed, a, b),
        None => {
            let text = format!("Not found: {}", ed.search);
            dialog::alert(tui, "Search", &text)?;
        }
    }
    Ok(())
}

fn replace_all(tui: &mut Tui, ed: &mut Editor) -> io::Result<()> {
    let mut form = Form::new(vec![
        Field::text("Search for:", &ed.search),
        Field::text("Replace with:", &ed.replace),
        Field::check("Ignore case", ed.ignore_case),
    ]);
    if !form.run(tui, "Replace")? || form.text(0).is_empty() {
        return Ok(());
    }
    ed.search = form.text(0);
    ed.replace = form.text(1);
    ed.ignore_case = form.checked(2);
    let (needle, with) = (ed.search.clone(), ed.replace.clone());
    let mut at = ed.cursor;
    let mut all = false;
    let mut count = 0;
    ed.buf.begin_group();
    while let Some((a, b)) = ed.buf.find(&needle, at, ed.ignore_case, false) {
        let mut answer = 0;
        if !all {
            select_hit(ed, a, b);
            ed.draw(&mut tui.screen);
            let lines = vec![format!("Replace \"{needle}\" with \"{with}\"?")];
            match dialog::message(
                tui,
                "Replace",
                &lines,
                &["Replace", "All", "Skip", "Cancel"],
                false,
            )? {
                Some(0) => {}
                Some(1) => all = true,
                Some(2) => answer = 2,
                _ => break,
            }
        }
        if answer == 2 {
            at = b;
            continue;
        }
        at = ed.buf.replace(a, b, &with, a);
        count += 1;
    }
    ed.buf.end_group();
    ed.selection = None;
    ed.cursor = ed.buf.clamp(at);
    ed.sync_vis();
    if count == 0 && !all {
        return Ok(());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn editor_with(text: &str) -> Editor {
        Editor {
            path: PathBuf::from("/nonexistent/x"),
            buf: Buffer::from_text(text),
            format: FileFormat {
                enc: 0,
                crlf: false,
                bom: false,
            },
            cursor: Pos::default(),
            want_vis: 0,
            top: 0,
            left: 0,
            selection: None,
            overwrite: false,
            mtime: None,
            search: String::new(),
            replace: String::new(),
            ignore_case: true,
        }
    }

    fn temp(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("far_edit_{}_{tag}", std::process::id()))
    }

    #[test]
    fn replace_across_lines_and_undo_redo_restore_text() {
        let mut b = Buffer::from_text("one\ntwo\nthree");
        let end = b.replace(pos(0, 1), pos(2, 2), "X\nY", pos(0, 0));
        assert_eq!(b.to_text(), "oX\nYree");
        assert_eq!(end, pos(1, 1));
        assert_eq!(b.undo(), Some(pos(0, 0)));
        assert_eq!(b.to_text(), "one\ntwo\nthree");
        b.redo();
        assert_eq!(b.to_text(), "oX\nYree");
    }

    #[test]
    fn typed_characters_undo_as_words() {
        let mut ed = editor_with("");
        for c in ["a", "b", " ", "c", "d"] {
            ed.type_text(c);
        }
        assert_eq!(ed.buf.to_text(), "ab cd");
        ed.buf.undo();
        assert_eq!(ed.buf.to_text(), "ab ");
        ed.buf.undo();
        assert_eq!(ed.buf.to_text(), "ab");
        ed.buf.undo();
        assert_eq!(ed.buf.to_text(), "");
    }

    #[test]
    fn modified_flag_follows_undo_back_to_the_saved_state() {
        let mut b = Buffer::from_text("x");
        assert!(!b.is_modified());
        b.replace(pos(0, 0), pos(0, 0), "y", pos(0, 0));
        assert!(b.is_modified());
        b.undo();
        assert!(!b.is_modified());
        b.replace(pos(0, 0), pos(0, 0), "y", pos(0, 0));
        b.mark_saved();
        assert!(!b.is_modified());
        b.undo();
        assert!(b.is_modified());
    }

    #[test]
    fn editing_uses_character_positions_in_multibyte_text() {
        let mut ed = editor_with("жёлтый");
        ed.cursor = pos(0, 3);
        ed.type_text("x");
        assert_eq!(ed.buf.to_text(), "жёлxтый");
        ed.backspace();
        ed.backspace();
        assert_eq!(ed.buf.to_text(), "жётый");
    }

    #[test]
    fn backspace_and_delete_join_lines() {
        let mut ed = editor_with("ab\ncd");
        ed.cursor = pos(1, 0);
        ed.backspace();
        assert_eq!(ed.buf.to_text(), "abcd");
        assert_eq!(ed.cursor, pos(0, 2));
        let mut ed = editor_with("ab\ncd");
        ed.cursor = pos(0, 2);
        ed.delete_forward();
        assert_eq!(ed.buf.to_text(), "abcd");
        assert_eq!(ed.cursor, pos(0, 2));
    }

    #[test]
    fn overwrite_mode_replaces_a_character_but_not_at_end_of_line() {
        let mut ed = editor_with("ab");
        ed.overwrite = true;
        ed.type_text("x");
        assert_eq!(ed.buf.to_text(), "xb");
        ed.cursor = pos(0, 2);
        ed.type_text("y");
        assert_eq!(ed.buf.to_text(), "xby");
    }

    #[test]
    fn stream_selection_copy_and_delete() {
        let mut ed = editor_with("hello\nworld");
        ed.cursor = pos(0, 2);
        ed.select_for(SHIFT);
        ed.cursor = pos(1, 3);
        let copied = ed.copy_selection().unwrap();
        assert_eq!(copied.text, "llo\nwor");
        assert!(!copied.vertical);
        ed.delete_selection();
        assert_eq!(ed.buf.to_text(), "held");
        ed.buf.undo();
        assert_eq!(ed.buf.to_text(), "hello\nworld");
    }

    #[test]
    fn vertical_block_copy_delete_and_paste() {
        let mut ed = editor_with("abcd\nefgh\nij");
        ed.cursor = pos(0, 1);
        ed.sync_vis();
        ed.select_for(ALT_SHIFT);
        ed.cursor = pos(1, 3);
        ed.sync_vis();
        let copied = ed.copy_selection().unwrap();
        assert_eq!(copied.text, "bc\nfg");
        assert!(copied.vertical);
        ed.delete_selection();
        assert_eq!(ed.buf.to_text(), "ad\neh\nij");
        ed.buf.undo();
        assert_eq!(ed.buf.to_text(), "abcd\nefgh\nij");

        // A column pastes down, padding short lines and adding missing ones.
        let mut ed = editor_with("abc\nx");
        ed.cursor = pos(0, 3);
        ed.sync_vis();
        ed.paste(&Clipboard {
            text: "1\n2\n3".into(),
            vertical: true,
        });
        assert_eq!(ed.buf.to_text(), "abc1\nx  2\n   3");
        ed.buf.undo();
        assert_eq!(ed.buf.to_text(), "abc\nx");
    }

    #[test]
    fn deleting_lines_handles_every_position() {
        let mut ed = editor_with("a\nb\nc");
        ed.cursor = pos(1, 0);
        ed.delete_line();
        assert_eq!(ed.buf.to_text(), "a\nc");
        ed.cursor = pos(1, 0);
        ed.delete_line();
        assert_eq!(ed.buf.to_text(), "a");
        assert_eq!(ed.cursor.line, 0);
        ed.delete_line();
        assert_eq!(ed.buf.to_text(), "");
    }

    #[test]
    fn tabs_map_between_columns_and_characters() {
        assert_eq!(vis_of("\tx", 1), 8);
        assert_eq!(vis_of("ab\tx", 3), 8);
        assert_eq!(col_of("\tx", 8), 1);
        assert_eq!(col_of("\tx", 4), 0);
    }

    #[test]
    fn search_in_both_directions_and_with_case_folding() {
        let b = Buffer::from_text("Foo bar\nbar foo");
        assert_eq!(
            b.find("bar", pos(0, 0), false, false),
            Some((pos(0, 4), pos(0, 7)))
        );
        assert_eq!(
            b.find("bar", pos(0, 5), false, false),
            Some((pos(1, 0), pos(1, 3)))
        );
        assert_eq!(
            b.find("foo", pos(0, 0), true, false),
            Some((pos(0, 0), pos(0, 3)))
        );
        assert_eq!(
            b.find("foo", pos(1, 7), false, true),
            Some((pos(1, 4), pos(1, 7)))
        );
        assert_eq!(b.find("foo", pos(1, 6), false, true), None);
        assert_eq!(b.find("zzz", pos(0, 0), false, false), None);
    }

    #[test]
    fn untouched_files_save_back_byte_for_byte() {
        let samples: Vec<&[u8]> = vec![
            b"no newline",
            b"a\nb\n",
            b"a\r\nb\r\n",
            b"mixed\r\nendings\nhere\r",
            b"\n\n",
            b"",
            b"\xef\xbb\xbfwith bom\n",
            "кириллица\n".as_bytes(),
            b"\xcf\xf0\xe8\xe2\xe5\xf2\n",
            b"\x00\x01binary\xff\xfe\x80\n",
        ];
        for sample in samples {
            let (text, format) = decode_file(sample).unwrap();
            let buffer = Buffer::from_text(&text);
            assert_eq!(
                encode_file(&buffer.to_text(), &format).unwrap(),
                sample,
                "{sample:?}"
            );
        }
    }

    #[test]
    fn file_formats_are_detected() {
        assert!(decode_file(b"a\r\nb\r\n").unwrap().1.crlf);
        assert!(!decode_file(b"a\r\nb\n").unwrap().1.crlf);
        assert_eq!(decode_file(b"\xff\n").unwrap().1.enc, 1);
        assert!(decode_file(b"\xef\xbb\xbfx").unwrap().1.bom);
        assert!(decode_file(b"\xff\xfex\x00").is_err());
    }

    #[test]
    fn saving_refuses_characters_the_encoding_cannot_hold() {
        let format = FileFormat {
            enc: 1,
            crlf: false,
            bom: false,
        };
        assert!(encode_file("日本語", &format).is_err());
    }

    #[test]
    fn saving_keeps_mode_and_writes_through_symlinks() {
        let real = temp("real");
        let link = temp("link");
        let _ = fs::remove_file(&link);
        fs::write(&real, "old").unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o640)).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        write_file(&link, b"new").unwrap();
        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read(&real).unwrap(), b"new");
        assert_eq!(fs::metadata(&real).unwrap().mode() & 0o777, 0o640);
        fs::remove_file(link).unwrap();
        fs::remove_file(real).unwrap();
    }

    #[test]
    fn saving_keeps_hard_links_together() {
        let a = temp("hard_a");
        let b = temp("hard_b");
        let _ = fs::remove_file(&b);
        fs::write(&a, "old").unwrap();
        fs::hard_link(&a, &b).unwrap();
        write_file(&a, b"new").unwrap();
        assert_eq!(fs::read(&b).unwrap(), b"new");
        fs::remove_file(a).unwrap();
        fs::remove_file(b).unwrap();
    }

    #[test]
    fn new_file_is_created_on_first_save() {
        let path = temp("new");
        let _ = fs::remove_file(&path);
        let mut ed = Editor::open(&path).unwrap();
        ed.type_text("hi");
        ed.save().unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"hi");
        assert!(!ed.buf.is_modified());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn external_changes_are_noticed() {
        let path = temp("ext");
        fs::write(&path, "a").unwrap();
        let mut ed = Editor::open(&path).unwrap();
        assert!(!ed.changed_on_disk());
        ed.mtime = Some((1, 1));
        assert!(ed.changed_on_disk());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn drawing_handles_small_and_empty_buffers() {
        let mut ed = editor_with("");
        let mut s = Screen::new(20, 4);
        ed.draw(&mut s);
        assert_eq!(s.cursor, Some((0, 1)));
    }
}
