//! The F4 editor: a text buffer with full undo, stream and vertical blocks,
//! search and replace, and encoding-aware saving that never alters bytes the
//! user did not touch.

use crate::dialog::{self, Field, Form};
use crate::encoding;
use crate::history::{History, Kind};
use crate::screen::{
    fit, fit_left, str_width, theme, Input, Key, KeyBars, Screen, Tui, ALT, ALT_SHIFT, CTRL,
    CTRL_SHIFT, NONE, SHIFT,
};
use crate::search::{is_word_char, Hit, Matcher, SearchOptions};
use crate::util;
use crate::viewer::{cell_width, visible_char};
use crossterm::event::KeyCode;
use std::fs;
use std::io::{self, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::rc::Rc;
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

    /// Finds the next or previous match, searching each line on its own.
    pub fn find_with(&self, m: &Matcher, from: Pos, backward: bool) -> Option<(Pos, Pos, Hit)> {
        let found = |l: usize, hit: Hit| Some((pos(l, hit.start), pos(l, hit.end), hit));
        if backward {
            for l in (0..=from.line.min(self.lines.len() - 1)).rev() {
                let line = &self.lines[l];
                let limit = if l == from.line {
                    from.col.min(line_len(line))
                } else {
                    line_len(line)
                };
                if let Some(hit) = m.find(line, limit, true) {
                    return found(l, hit);
                }
            }
            return None;
        }
        for l in from.line..self.lines.len() {
            let start = if l == from.line { from.col } else { 0 };
            if let Some(hit) = m.find(&self.lines[l], start, false) {
                return found(l, hit);
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
    let mode = existing.as_ref().map_or(0o666, |m| m.mode() & 0o777);
    let opened = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&temp);
    let mut file = match opened {
        Ok(file) => file,
        // A read-only folder can still hold a writable file. Other errors,
        // such as a full disk, would only destroy the file in place as well.
        Err(e) if existing.is_some() && e.kind() == io::ErrorKind::PermissionDenied => {
            return write_in_place(&real, data);
        }
        Err(e) => return Err(e),
    };
    // Renaming must not hand the file to another group. If the group cannot be
    // kept, the only way to preserve ownership is to write in place.
    if let Some(meta) = &existing {
        let group_differs = file.metadata().is_ok_and(|m| m.gid() != meta.gid());
        if group_differs && std::os::unix::fs::fchown(&file, None, Some(meta.gid())).is_err() {
            drop(file);
            let _ = fs::remove_file(&temp);
            return write_in_place(&real, data);
        }
    }
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

/// What in-place saving needs from a file, so that failures can be tested.
trait Rewritable: Write + Seek {
    fn set_length(&mut self, length: u64) -> io::Result<()>;
}

impl Rewritable for fs::File {
    fn set_length(&mut self, length: u64) -> io::Result<()> {
        self.set_len(length)
    }
}

/// Overwrites `file` with `data`. If that fails, the old contents go back, so
/// that a full disk does not cost the user the file. An error says whether the
/// restore worked too.
fn rewrite_keeping_old<F: Rewritable>(file: &mut F, old: &[u8], data: &[u8]) -> io::Result<()> {
    let write = |file: &mut F, bytes: &[u8]| -> io::Result<()> {
        file.seek(SeekFrom::Start(0))?;
        file.write_all(bytes)?;
        file.flush()?;
        file.set_length(bytes.len() as u64)
    };
    let Err(error) = write(file, data) else {
        return Ok(());
    };
    match write(file, old) {
        Ok(()) => Err(error),
        Err(_) => Err(io::Error::new(
            error.kind(),
            format!("{error}. The file on disk may be damaged"),
        )),
    }
}

fn write_in_place(path: &Path, data: &[u8]) -> io::Result<()> {
    let old = fs::read(path)?;
    let mut file = fs::OpenOptions::new().write(true).open(path)?;
    rewrite_keeping_old(&mut file, &old, data)?;
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
    NewFile,
    GoToFile,
    History,
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
    search: SearchOptions,
    last_replace: bool,
    last_hit: Option<(Pos, Pos)>,
    bookmarks: [Option<Pos>; 10],
    locked: bool,
    status_bar: bool,
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
            search: SearchOptions::default(),
            last_replace: false,
            last_hit: None,
            bookmarks: [None; 10],
            locked: false,
            status_bar: true,
        })
    }

    fn line(&self) -> &str {
        &self.buf.lines[self.cursor.line]
    }

    fn sync_vis(&mut self) {
        self.want_vis = vis_of(self.line(), self.cursor.col);
    }

    fn select_for(&mut self, mods: crossterm::event::KeyModifiers) {
        let kind = if mods.contains(ALT) {
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

    /// Rows available for text.
    fn text_rows(&self, screen_h: u16, keybar: bool) -> usize {
        let bars = usize::from(self.status_bar) + usize::from(keybar);
        (screen_h as usize).saturating_sub(bars).max(1)
    }

    fn draw(&mut self, s: &mut Screen, keybar: bool) {
        let w = s.w as usize;
        let rows = self.text_rows(s.h, keybar);
        let first = u16::from(self.status_bar);
        self.scroll_into_view(rows, w.max(1));
        s.fill(0, 0, s.w, s.h, ' ', theme::EDITOR);
        if self.status_bar {
            let info = format!(
                " {}{} {} {} Ln {}/{} Col {} ",
                if self.buf.is_modified() { "* " } else { "" },
                encoding::NAMES[self.format.enc],
                if self.format.crlf { "CRLF" } else { "LF" },
                if self.locked {
                    "RO"
                } else if self.overwrite {
                    "OVR"
                } else {
                    "INS"
                },
                self.cursor.line + 1,
                self.buf.lines.len(),
                vis_of(self.line(), self.cursor.col) + 1,
            );
            let room = w.saturating_sub(str_width(&info));
            let title = fit(&fit_left(&util::tilde(&self.path), room), room);
            s.put(0, 0, &format!("{title}{info}"), theme::STATUS);
        }
        let stream = self.stream_range();
        let block = self.block_range();
        let mut buf = [0u8; 4];
        for y in 0..rows {
            let line_no = self.top + y;
            let Some(line) = self.buf.lines.get(line_no) else {
                break;
            };
            let row = y as u16 + first;
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
                        s.fill(x as u16, row, cells as u16, 1, ' ', style);
                    } else {
                        s.put(x as u16, row, visible_char(c).encode_utf8(&mut buf), style);
                    }
                }
                vis += cells;
            }
        }
        let x = vis_of(self.line(), self.cursor.col).saturating_sub(self.left);
        s.cursor = Some((x as u16, (self.cursor.line - self.top) as u16 + first));
        if keybar {
            s.keybar(
                s.h.saturating_sub(1),
                &KeyBars {
                    plain: [
                        "Help", "Save", "", "Quit", "", "View", "Search", "Encod", "", "Quit", "",
                        "",
                    ],
                    shift: [
                        "", "SaveAs", "", "Edit…", "", "", "Next", "CodePg", "", "SaveQ", "", "",
                    ],
                    alt: ["", "", "", "", "", "", "Prev", "Goto", "", "", "ViewHs", ""],
                    ctrl: ["", "", "", "", "", "", "Replac", "", "", "GoFile", "", ""],
                },
            );
        }
    }

    /// The word under or just before the cursor.
    fn word_at_cursor(&self) -> String {
        let chars: Vec<char> = self.line().chars().collect();
        let mut start = self.cursor.col.min(chars.len());
        let mut end = start;
        while start > 0 && is_word_char(chars[start - 1]) {
            start -= 1;
        }
        while end < chars.len() && is_word_char(chars[end]) {
            end += 1;
        }
        chars[start..end].iter().collect()
    }

    /// Where the next search starts. An empty match must not be found twice.
    fn search_start(&self, backward: bool) -> Option<Pos> {
        let empty_here = self
            .last_hit
            .is_some_and(|(a, b)| a == b && b == self.cursor);
        if !backward {
            return Some(if empty_here {
                pos(self.cursor.line, self.cursor.col + 1)
            } else {
                self.cursor
            });
        }
        if let Some((a, _)) = self.stream_range() {
            return Some(a);
        }
        if !empty_here {
            return Some(self.cursor);
        }
        position_before(&self.buf, self.cursor)
    }

    fn select_hit(&mut self, a: Pos, b: Pos) {
        self.selection = Some(Selection {
            kind: SelKind::Stream,
            anchor: a,
            anchor_vis: 0,
        });
        self.cursor = b;
        self.last_hit = Some((a, b));
        self.sync_vis();
    }

    /// Moves to the next or previous match. Returns false if there is none.
    pub fn find_next(&mut self, m: &Matcher, backward: bool) -> bool {
        let Some(from) = self.search_start(backward) else {
            return false;
        };
        match self.buf.find_with(m, from, backward) {
            Some((a, b, _)) => {
                self.select_hit(a, b);
                true
            }
            None => false,
        }
    }

    /// Lists every match in the file.
    pub fn find_all(&self, m: &Matcher) -> Vec<(Pos, Pos)> {
        let mut hits = Vec::new();
        let mut at = Pos::default();
        while let Some((a, b, _)) = self.buf.find_with(m, at, false) {
            hits.push((a, b));
            at = if a == b { pos(b.line, b.col + 1) } else { b };
        }
        hits
    }

    /// Replaces every match from the cursor without asking. Returns the count.
    pub fn replace_remaining(&mut self, m: &Matcher, backward: bool) -> usize {
        let mut count = 0;
        let Some(mut at) = self.search_start(backward) else {
            return 0;
        };
        self.buf.begin_group();
        while let Some((a, b, hit)) = self.buf.find_with(m, at, backward) {
            match self.replace_hit(m, a, b, &hit, backward) {
                Some(next) => at = next,
                None => {
                    count += 1;
                    break;
                }
            }
            count += 1;
        }
        self.buf.end_group();
        self.selection = None;
        self.cursor = self.buf.clamp(at);
        self.sync_vis();
        count
    }

    /// Replaces one match. Returns where to continue, or `None` at the start
    /// of the file when going backward.
    fn replace_hit(
        &mut self,
        m: &Matcher,
        a: Pos,
        b: Pos,
        hit: &Hit,
        backward: bool,
    ) -> Option<Pos> {
        let end = self.buf.replace(a, b, &m.replacement(hit), a);
        self.cursor = end;
        self.last_hit = None;
        if backward {
            return if a == b {
                position_before(&self.buf, a)
            } else {
                Some(a)
            };
        }
        Some(if a == b {
            pos(end.line, end.col + 1)
        } else {
            end
        })
    }

    /// Lines covered by the selection, or the current line.
    fn selected_lines(&self) -> (usize, usize) {
        if let Some((a, b)) = self.stream_range() {
            // A selection ending at column 0 does not include that line.
            let last = if b.col == 0 && b.line > a.line {
                b.line - 1
            } else {
                b.line
            };
            return (a.line, last);
        }
        if let Some((lo, hi, _, _)) = self.block_range() {
            return (lo, hi);
        }
        (self.cursor.line, self.cursor.line)
    }

    /// Alt+I and Alt+U: shifts lines right or left by one position.
    pub fn shift_lines(&mut self, right: bool) {
        let (lo, hi) = self.selected_lines();
        let before = self.cursor;
        self.buf.begin_group();
        for l in lo..=hi {
            if right {
                self.buf.replace(pos(l, 0), pos(l, 0), " ", before);
            } else if self.buf.lines[l].starts_with([' ', '\t']) {
                self.buf.replace(pos(l, 0), pos(l, 1), "", before);
            }
        }
        self.buf.end_group();
        self.cursor = self.buf.clamp(before);
        self.sync_vis();
    }

    /// Ctrl+C with no selection copies the current line.
    fn copy_or_line(&self) -> Clipboard {
        self.copy_selection().unwrap_or_else(|| Clipboard {
            text: format!("{}\n", self.line()),
            vertical: false,
        })
    }

    fn insert_name(&mut self, name: Option<&str>) {
        if let Some(name) = name {
            self.type_text(name);
        }
    }
}

/// The position just before `p`, or `None` at the start of the file.
fn position_before(buf: &Buffer, p: Pos) -> Option<Pos> {
    if p.col > 0 {
        Some(pos(p.line, p.col - 1))
    } else if p.line > 0 {
        Some(pos(p.line - 1, line_len(&buf.lines[p.line - 1])))
    } else {
        None
    }
}

/// Parses one value of the go-to dialog into a zero-based index: absolute,
/// `+n` or `-n` relative, `n%`, and `0xN`, `$N`, `Nh` or `Nm` number forms.
pub fn parse_goto_value(text: &str, current: usize, total: usize, hex: bool) -> Option<usize> {
    let text = text.trim();
    let (sign, body) = match text.chars().next()? {
        '+' => (1, &text[1..]),
        '-' => (-1, &text[1..]),
        _ => (0, text),
    };
    let lower = body.to_lowercase();
    if let Some(percent) = lower.strip_suffix('%') {
        let value: usize = percent.parse().ok()?;
        return Some((total.saturating_mul(value.min(100)) / 100).min(total.saturating_sub(1)));
    }
    let number = if let Some(digits) = lower.strip_prefix("0x").or(lower.strip_prefix('$')) {
        usize::from_str_radix(digits, 16).ok()?
    } else if let Some(digits) = lower.strip_suffix('h') {
        usize::from_str_radix(digits, 16).ok()?
    } else if let Some(digits) = lower.strip_suffix('m') {
        digits.parse().ok()?
    } else if hex {
        usize::from_str_radix(&lower, 16).ok()?
    } else {
        lower.parse().ok()?
    };
    Some(match sign {
        1 => current.saturating_add(number),
        -1 => current.saturating_sub(number),
        _ => number.saturating_sub(1),
    })
}

/// Parses `line[,column]`. Either part may be left out to keep its value.
pub fn parse_goto(text: &str, cursor: Pos, buf: &Buffer, hex: bool) -> Option<Pos> {
    let mut parts = text.splitn(2, |c: char| " ,.;:".contains(c));
    let line_text = parts.next().unwrap_or("").trim();
    let col_text = parts.next().unwrap_or("").trim();
    if line_text.is_empty() && col_text.is_empty() {
        return None;
    }
    let line = if line_text.is_empty() {
        cursor.line
    } else {
        parse_goto_value(line_text, cursor.line, buf.lines.len(), hex)?
    };
    let line = line.min(buf.lines.len() - 1);
    let col = if col_text.is_empty() {
        cursor.col
    } else {
        parse_goto_value(col_text, cursor.col, line_len(&buf.lines[line]), hex)?
    };
    Some(buf.clamp(pos(line, col)))
}

/// The character Ctrl+Q inserts for the next key.
pub fn literal_char(key: Key) -> Option<char> {
    match key.code {
        KeyCode::Char(c) if key.mods.contains(CTRL) && c.is_ascii() => {
            Some(((c.to_ascii_uppercase() as u8) & 0x1f) as char)
        }
        KeyCode::Char(c) => Some(c),
        KeyCode::Tab => Some('\t'),
        KeyCode::Enter => Some('\r'),
        KeyCode::Esc => Some('\x1b'),
        KeyCode::Backspace => Some('\x08'),
        _ => None,
    }
}

/// What the editor needs from the rest of the program.
pub struct EditorContext<'a> {
    pub clipboard: &'a mut Clipboard,
    pub keybar: &'a mut bool,
    pub db: &'a Rc<History>,
    /// Name under the cursor on the active panel, for Shift+Enter.
    pub active_name: Option<String>,
    /// Name under the cursor on the passive panel, for Ctrl+Shift+Enter.
    pub passive_name: Option<String>,
}

const HELP: &[(&str, &str)] = &[
    ("Movement", ""),
    ("Ctrl+Left, Ctrl+Right", "Word left, word right"),
    ("Ctrl+S", "Left, staying on the line"),
    ("Ctrl+Up, Ctrl+Down", "Scroll the screen"),
    (
        "Ctrl+Home, Ctrl+End",
        "Start, end of file (also Ctrl+PgUp, Ctrl+PgDn)",
    ),
    ("Ctrl+N, Ctrl+E", "Start, end of screen"),
    ("Alt+F8", "Go to line and column"),
    ("Ctrl+Shift+0..9, Ctrl+0..9", "Set, go to bookmark"),
    ("", ""),
    ("Deleting", ""),
    ("Ctrl+Y, Ctrl+K", "Delete line, delete to end of line"),
    ("Ctrl+BS", "Delete word left"),
    ("Ctrl+T, Ctrl+Del", "Delete word right"),
    ("Ctrl+D", "Delete block"),
    ("", ""),
    ("Blocks", ""),
    ("Shift+arrows, Ctrl+Shift+arrows", "Select"),
    ("Alt+arrows, Alt+Shift+arrows", "Select vertical block"),
    ("Ctrl+A, Ctrl+U", "Select all, deselect"),
    (
        "Ctrl+C, Ctrl+Ins",
        "Copy, or the current line if nothing is selected",
    ),
    ("Ctrl+X, Shift+Del", "Cut"),
    ("Ctrl+V, Shift+Ins", "Paste"),
    ("Ctrl++", "Append to clipboard"),
    ("Alt+I, Alt+U", "Shift right, shift left"),
    ("", ""),
    ("Search", ""),
    ("F7, Ctrl+F7", "Search, replace"),
    ("Shift+F7, Alt+F7", "Continue forward, backward"),
    ("", ""),
    ("File", ""),
    ("F2, Shift+F2", "Save, save as"),
    ("F4, F10, Esc", "Quit"),
    ("Shift+F10", "Save and quit"),
    ("Shift+F4", "Edit a new file"),
    ("F6", "Viewer"),
    ("F8, Shift+F8", "Next encoding, choose encoding"),
    ("Ctrl+F10", "Go to this file in the panel"),
    ("Alt+F11", "File view and edit history"),
    ("", ""),
    ("Other", ""),
    ("Ctrl+Z, Alt+BS", "Undo"),
    ("Ctrl+Shift+Z", "Redo"),
    ("Ins", "Insert or overwrite"),
    ("Ctrl+L", "Lock against changes"),
    ("Ctrl+O", "User screen"),
    ("Ctrl+Q", "Insert the next key as a character"),
    ("Shift+Enter", "Insert file name from the active panel"),
    (
        "Ctrl+Shift+Enter",
        "Insert file name from the passive panel",
    ),
    ("Ctrl+F", "Insert the full name of this file"),
    ("Ctrl+B, Ctrl+Shift+B", "Key bar, status line"),
];

/// Keys that change the text, which Ctrl+L blocks.
fn changes_text(key: Key) -> bool {
    match (key.mods, key.code) {
        (NONE, KeyCode::Char(_) | KeyCode::Enter | KeyCode::Tab) => true,
        (NONE, KeyCode::Backspace | KeyCode::Delete) => true,
        (SHIFT, KeyCode::Enter | KeyCode::Delete | KeyCode::Insert) => true,
        (CTRL_SHIFT, KeyCode::Enter | KeyCode::Char('z')) => true,
        (CTRL, KeyCode::Backspace | KeyCode::Delete | KeyCode::Insert) => {
            key.code != KeyCode::Insert
        }
        (CTRL, KeyCode::Char(c)) => "htykzxvdqf".contains(c),
        (ALT, KeyCode::Backspace | KeyCode::Char('i') | KeyCode::Char('u')) => true,
        (NONE | CTRL, KeyCode::F(7)) => key.mods == CTRL,
        _ => false,
    }
}

/// Runs the editor until it is closed.
pub fn run(tui: &mut Tui, path: &Path, ctx: &mut EditorContext) -> io::Result<EditorExit> {
    let mut ed = match Editor::open(path) {
        Ok(ed) => ed,
        Err(text) => {
            dialog::alert(tui, "Editor", &text)?;
            return Ok(EditorExit::Close);
        }
    };
    ed.search = ctx.db.load_search();
    let ctrl_alt = CTRL | ALT;
    loop {
        tui.sync_held();
        ed.draw(&mut tui.screen, *ctx.keybar);
        tui.draw()?;
        let input = tui.read(Duration::from_millis(1000))?;
        let key = match input {
            Some(Input::Key(key)) => key,
            Some(Input::Paste(text)) if !ed.locked => {
                let text = text.replace("\r\n", "\n").replace('\r', "\n");
                let start = ed.cursor;
                ed.selection = None;
                ed.cursor = ed.buf.replace(start, start, &text, start);
                ed.sync_vis();
                continue;
            }
            _ => continue,
        };
        if ed.locked && changes_text(key) {
            continue;
        }
        let rows = ed.text_rows(tui.screen.h, *ctx.keybar) as isize;
        let selecting = matches!(key.mods, NONE | SHIFT | ALT | ALT_SHIFT) || key.mods == ctrl_alt;
        match (key.mods, key.code) {
            (_, KeyCode::Up) if selecting => {
                ed.select_for(key.mods);
                ed.move_vertical(-1);
            }
            (_, KeyCode::Down) if selecting => {
                ed.select_for(key.mods);
                ed.move_vertical(1);
            }
            (_, KeyCode::Left) if selecting => {
                ed.select_for(key.mods);
                ed.move_left();
            }
            (_, KeyCode::Right) if selecting => {
                ed.select_for(key.mods);
                ed.move_right();
            }
            (_, KeyCode::PageUp) if selecting => {
                ed.select_for(key.mods);
                ed.move_vertical(-rows);
            }
            (_, KeyCode::PageDown) if selecting => {
                ed.select_for(key.mods);
                ed.move_vertical(rows);
            }
            (_, KeyCode::Home) if selecting => {
                ed.select_for(key.mods);
                ed.cursor.col = 0;
                ed.sync_vis();
            }
            (_, KeyCode::End) if selecting => {
                ed.select_for(key.mods);
                ed.cursor.col = line_len(ed.line());
                ed.sync_vis();
            }
            (CTRL_SHIFT, KeyCode::Up) => {
                ed.select_for(SHIFT);
                ed.move_vertical(-1);
            }
            (CTRL_SHIFT, KeyCode::Down) => {
                ed.select_for(SHIFT);
                ed.move_vertical(1);
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
            (CTRL, KeyCode::Char('s')) => {
                ed.selection = None;
                ed.cursor.col = ed.cursor.col.saturating_sub(1);
                ed.sync_vis();
            }
            (CTRL, KeyCode::Char('n')) => {
                ed.selection = None;
                ed.cursor.line = ed.top;
                ed.move_vertical(0);
            }
            (CTRL, KeyCode::Char('e')) => {
                ed.selection = None;
                ed.cursor.line = ed.top + rows as usize - 1;
                ed.move_vertical(0);
            }
            (CTRL, KeyCode::Up) => ed.top = ed.top.saturating_sub(1),
            (CTRL, KeyCode::Down) => ed.top = (ed.top + 1).min(ed.buf.lines.len() - 1),
            (NONE, KeyCode::Enter) => ed.type_text("\n"),
            (NONE, KeyCode::Tab) => ed.type_text("\t"),
            (SHIFT, KeyCode::Enter) => {
                let name = ctx.active_name.clone();
                ed.insert_name(name.as_deref());
            }
            (CTRL_SHIFT, KeyCode::Enter) => {
                let name = ctx.passive_name.clone();
                ed.insert_name(name.as_deref());
            }
            (CTRL, KeyCode::Char('f')) => {
                let name = ed.path.display().to_string();
                ed.type_text(&name);
            }
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
                let copied = ed.copy_or_line();
                tui.set_clipboard(&copied.text);
                *ctx.clipboard = copied;
            }
            (CTRL, KeyCode::Char('+')) | (CTRL, KeyCode::Char('=')) => {
                if let Some(copied) = ed.copy_selection() {
                    ctx.clipboard.text.push_str(&copied.text);
                    tui.set_clipboard(&ctx.clipboard.text);
                }
            }
            (CTRL, KeyCode::Char('x')) | (SHIFT, KeyCode::Delete) => {
                if let Some(copied) = ed.copy_selection() {
                    tui.set_clipboard(&copied.text);
                    *ctx.clipboard = copied;
                    ed.delete_selection();
                }
            }
            (CTRL, KeyCode::Char('v')) | (SHIFT, KeyCode::Insert) => ed.paste(ctx.clipboard),
            (CTRL, KeyCode::Char('d')) => ed.delete_selection(),
            (ALT, KeyCode::Char('i')) => ed.shift_lines(true),
            (ALT, KeyCode::Char('u')) => ed.shift_lines(false),
            (CTRL, KeyCode::Char('q')) => {
                let next = loop {
                    if let Some(Input::Key(next)) = tui.read(Duration::from_millis(1000))? {
                        break next;
                    }
                };
                if let Some(c) = literal_char(next) {
                    let mut b = [0u8; 4];
                    ed.type_text(c.encode_utf8(&mut b));
                }
            }
            (CTRL_SHIFT, KeyCode::Char(c)) if c.is_ascii_digit() => {
                ed.bookmarks[c as usize - '0' as usize] = Some(ed.cursor);
            }
            (CTRL, KeyCode::Char(c)) if c.is_ascii_digit() => {
                if let Some(p) = ed.bookmarks[c as usize - '0' as usize] {
                    ed.selection = None;
                    ed.cursor = ed.buf.clamp(p);
                    ed.sync_vis();
                }
            }
            (CTRL, KeyCode::Char('l')) => ed.locked = !ed.locked,
            (CTRL, KeyCode::Char('o')) => tui.user_screen()?,
            (CTRL, KeyCode::Char('b')) => *ctx.keybar = !*ctx.keybar,
            (CTRL_SHIFT, KeyCode::Char('b')) => ed.status_bar = !ed.status_bar,
            (NONE, KeyCode::F(1)) => dialog::help_list(tui, "Editor", HELP)?,
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
            (SHIFT, KeyCode::F(4)) => {
                if confirm_leave(tui, &mut ed)? {
                    return Ok(EditorExit::NewFile);
                }
            }
            (CTRL, KeyCode::F(10)) => {
                if confirm_leave(tui, &mut ed)? {
                    return Ok(EditorExit::GoToFile);
                }
            }
            (ALT, KeyCode::F(11)) => {
                if confirm_leave(tui, &mut ed)? {
                    return Ok(EditorExit::History);
                }
            }
            (_, KeyCode::Esc) | (NONE, KeyCode::F(10)) | (NONE, KeyCode::F(4)) => {
                if confirm_leave(tui, &mut ed)? {
                    return Ok(EditorExit::Close);
                }
            }
            (NONE, KeyCode::F(8)) => change_encoding(&mut ed)?,
            (SHIFT, KeyCode::F(8)) => {
                let items: Vec<String> = encoding::NAMES.iter().map(|n| n.to_string()).collect();
                if let Some(i) = dialog::menu(tui, "Encoding", &items, ed.format.enc)? {
                    ed.format.enc = (i + encoding::NAMES.len() - 1) % encoding::NAMES.len();
                    change_encoding(&mut ed)?;
                }
            }
            (NONE, KeyCode::F(7)) => search_dialog(tui, &mut ed, ctx.db, false)?,
            (CTRL, KeyCode::F(7)) => search_dialog(tui, &mut ed, ctx.db, true)?,
            (SHIFT, KeyCode::F(7)) => continue_search(tui, &mut ed, ctx.db, false)?,
            (ALT, KeyCode::F(7)) => continue_search(tui, &mut ed, ctx.db, true)?,
            (ALT, KeyCode::F(8)) => goto_dialog(tui, &mut ed)?,
            _ => {}
        }
    }
}

fn save_interactive(tui: &mut Tui, ed: &mut Editor) -> io::Result<()> {
    if fs::metadata(&ed.path).is_ok_and(|m| m.mode() & 0o222 == 0) {
        let lines = vec![
            "This file is read-only.".to_string(),
            "Overwrite it anyway?".to_string(),
        ];
        if dialog::message(tui, "Save", &lines, &["Overwrite", "Cancel"], true)? != Some(0) {
            return Ok(());
        }
    }
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
    // The file may be gone by now. The text in the editor is still good.
    let Ok(bytes) = fs::read(&ed.path) else {
        ed.format.enc = next;
        return Ok(());
    };
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

/// F7 and Ctrl+F7.
fn search_dialog(
    tui: &mut Tui,
    ed: &mut Editor,
    db: &Rc<History>,
    replacing: bool,
) -> io::Result<()> {
    let o = ed.search.clone();
    let mut fields = vec![Field::with_history("&Search for", &o.text, Kind::Search)];
    if replacing {
        fields.push(Field::with_history(
            "&Replace with",
            &o.replace,
            Kind::Replace,
        ));
    }
    let first_option = fields.len();
    fields.push(Field::check("&Case sensitive", o.case_sensitive));
    fields.push(Field::check("&Whole words", o.whole_words));
    fields.push(Field::check("Fu&zzy search", o.fuzzy));
    fields.push(Field::check("R&egular expressions", o.regex));
    if replacing {
        fields.push(Field::check("&Preserve style", o.preserve_style));
    }
    let buttons: &[&str] = if replacing {
        &[
            "Replace next",
            "Replace pre&vious",
            "&All",
            "W&ord",
            "Se&lection",
            "Cancel",
        ]
    } else {
        &[
            "Find next",
            "Find pre&vious",
            "&All",
            "W&ord",
            "Se&lection",
            "Cancel",
        ]
    };
    let title = if replacing { "Replace" } else { "Search" };
    let mut form = Form::new(fields).with_buttons(buttons).with_db(db);
    let choice = loop {
        match form.run_buttons(tui, title)? {
            Some(3) => {
                let word = ed.word_at_cursor();
                form.set_text(0, &word);
            }
            Some(4) => {
                if let Some(copied) = ed.copy_selection() {
                    let first = copied.text.split('\n').next().unwrap_or("").to_string();
                    form.set_text(0, &first);
                }
            }
            Some(5) | None => return Ok(()),
            Some(button) => break button,
        }
    };
    if form.text(0).is_empty() {
        return Ok(());
    }
    ed.search = SearchOptions {
        text: form.text(0),
        replace: if replacing { form.text(1) } else { o.replace },
        case_sensitive: form.checked(first_option),
        whole_words: form.checked(first_option + 1),
        fuzzy: form.checked(first_option + 2),
        regex: form.checked(first_option + 3),
        preserve_style: replacing && form.checked(first_option + 4),
    };
    db.save_search(&ed.search, replacing);
    ed.last_replace = replacing;
    match choice {
        2 => show_all_matches(tui, ed),
        button => run_search(tui, ed, button == 1),
    }
}

fn compile(tui: &mut Tui, ed: &Editor) -> io::Result<Option<Matcher>> {
    match Matcher::new(&ed.search) {
        Ok(m) => Ok(Some(m)),
        Err(text) => {
            dialog::alert(tui, "Search", &text)?;
            Ok(None)
        }
    }
}

/// Shift+F7 and Alt+F7 repeat the last search or replace.
fn continue_search(
    tui: &mut Tui,
    ed: &mut Editor,
    db: &Rc<History>,
    backward: bool,
) -> io::Result<()> {
    if ed.search.text.is_empty() {
        return search_dialog(tui, ed, db, false);
    }
    run_search(tui, ed, backward)
}

fn run_search(tui: &mut Tui, ed: &mut Editor, backward: bool) -> io::Result<()> {
    let Some(m) = compile(tui, ed)? else {
        return Ok(());
    };
    if ed.last_replace {
        return replace_interactive(tui, ed, &m, backward);
    }
    if !ed.find_next(&m, backward) {
        let text = format!("Not found: {}", ed.search.text);
        dialog::alert(tui, "Search", &text)?;
    }
    Ok(())
}

fn show_all_matches(tui: &mut Tui, ed: &mut Editor) -> io::Result<()> {
    let Some(m) = compile(tui, ed)? else {
        return Ok(());
    };
    let hits = ed.find_all(&m);
    if hits.is_empty() {
        let text = format!("Not found: {}", ed.search.text);
        return dialog::alert(tui, "Search", &text);
    }
    let items: Vec<String> = hits
        .iter()
        .map(|(a, _)| {
            format!(
                "{}:{}  {}",
                a.line + 1,
                a.col + 1,
                ed.buf.lines[a.line].trim()
            )
        })
        .collect();
    let title = format!("{} matches", hits.len());
    if let Some(i) = dialog::menu(tui, &title, &items, 0)? {
        ed.select_hit(hits[i].0, hits[i].1);
    }
    Ok(())
}

fn replace_interactive(
    tui: &mut Tui,
    ed: &mut Editor,
    m: &Matcher,
    backward: bool,
) -> io::Result<()> {
    let Some(mut at) = ed.search_start(backward) else {
        return Ok(());
    };
    let mut count = 0;
    ed.buf.begin_group();
    while let Some((a, b, hit)) = ed.buf.find_with(m, at, backward) {
        ed.select_hit(a, b);
        ed.draw(&mut tui.screen, true);
        let lines = vec![
            "Replace".to_string(),
            ed.buf.text_between(a, b),
            "with".to_string(),
            m.replacement(&hit),
        ];
        let buttons = ["Replace", "All", "Skip", "Cancel"];
        match dialog::message(tui, "Replace", &lines, &buttons, false)? {
            Some(0) => {
                count += 1;
                match ed.replace_hit(m, a, b, &hit, backward) {
                    Some(next) => at = next,
                    None => break,
                }
            }
            Some(1) => {
                ed.buf.end_group();
                ed.cursor = a;
                ed.last_hit = None;
                ed.selection = None;
                ed.replace_remaining(m, backward);
                return Ok(());
            }
            Some(2) => {
                at = if backward {
                    match position_before(&ed.buf, a) {
                        Some(p) if a == b => p,
                        None if a == b => break,
                        _ => a,
                    }
                } else if a == b {
                    pos(b.line, b.col + 1)
                } else {
                    b
                };
            }
            _ => {
                ed.buf.end_group();
                ed.selection = None;
                return Ok(());
            }
        }
    }
    ed.buf.end_group();
    ed.selection = None;
    ed.cursor = ed.buf.clamp(at);
    ed.sync_vis();
    if count == 0 {
        let text = format!("Not found: {}", ed.search.text);
        dialog::alert(tui, "Replace", &text)?;
    }
    Ok(())
}

/// Alt+F8.
fn goto_dialog(tui: &mut Tui, ed: &mut Editor) -> io::Result<()> {
    let mut form = Form::new(vec![
        Field::text("Line [,column], with +, -, % or 0x if needed:", ""),
        Field::check("&Hex value", false),
    ]);
    if !form.run(tui, "Go to")? {
        return Ok(());
    }
    match parse_goto(&form.text(0), ed.cursor, &ed.buf, form.checked(1)) {
        Some(p) => {
            ed.cursor = p;
            ed.selection = None;
            ed.sync_vis();
        }
        None => dialog::alert(tui, "Go to", "Enter a line number")?,
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
            search: SearchOptions::default(),
            last_replace: false,
            last_hit: None,
            bookmarks: [None; 10],
            locked: false,
            status_bar: true,
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

    fn matcher(o: SearchOptions) -> Matcher {
        Matcher::new(&o).unwrap()
    }

    fn text_opts(text: &str) -> SearchOptions {
        SearchOptions {
            text: text.into(),
            ..Default::default()
        }
    }

    #[test]
    fn search_in_both_directions_and_with_case_folding() {
        let b = Buffer::from_text("Foo bar\nbar foo");
        let spans = |m: &Matcher, from, back| b.find_with(m, from, back).map(|h| (h.0, h.1));
        let bar = matcher(text_opts("bar"));
        assert_eq!(spans(&bar, pos(0, 0), false), Some((pos(0, 4), pos(0, 7))));
        assert_eq!(spans(&bar, pos(0, 5), false), Some((pos(1, 0), pos(1, 3))));
        let foo = matcher(text_opts("foo"));
        assert_eq!(spans(&foo, pos(0, 0), false), Some((pos(0, 0), pos(0, 3))));
        let exact = matcher(SearchOptions {
            case_sensitive: true,
            ..text_opts("foo")
        });
        assert_eq!(spans(&exact, pos(1, 7), true), Some((pos(1, 4), pos(1, 7))));
        assert_eq!(spans(&exact, pos(1, 6), true), None);
        assert_eq!(spans(&matcher(text_opts("zzz")), pos(0, 0), false), None);
    }

    #[test]
    fn shift_f7_style_continuation_visits_each_match_then_stops() {
        let mut ed = editor_with("one foo\ntwo\nthree foo\nfive foo");
        let m = matcher(text_opts("foo"));
        assert!(ed.find_next(&m, false));
        assert_eq!(ed.cursor, pos(0, 7));
        assert!(ed.find_next(&m, false));
        assert_eq!(ed.cursor, pos(2, 9));
        assert!(ed.find_next(&m, false));
        assert_eq!(ed.cursor, pos(3, 8));
        assert!(!ed.find_next(&m, false));
        assert!(ed.find_next(&m, true));
        assert_eq!(ed.cursor, pos(2, 9));
        assert!(ed.find_next(&m, true));
        assert_eq!(ed.cursor, pos(0, 7));
        assert!(!ed.find_next(&m, true));
    }

    #[test]
    fn empty_regex_matches_advance_instead_of_sticking() {
        let mut ed = editor_with("ab\ncd");
        let m = matcher(SearchOptions {
            regex: true,
            ..text_opts("^")
        });
        assert!(ed.find_next(&m, false));
        assert_eq!(ed.cursor, pos(0, 0));
        assert!(ed.find_next(&m, false));
        assert_eq!(ed.cursor, pos(1, 0));
        assert!(!ed.find_next(&m, false));
    }

    #[test]
    fn replace_all_handles_growth_and_undoes_as_one_step() {
        let mut ed = editor_with("a a\na");
        let m = matcher(SearchOptions {
            replace: "aa".into(),
            ..text_opts("a")
        });
        assert_eq!(ed.replace_remaining(&m, false), 3);
        assert_eq!(ed.buf.to_text(), "aa aa\naa");
        ed.buf.undo();
        assert_eq!(ed.buf.to_text(), "a a\na");
    }

    #[test]
    fn replace_backward_works_from_the_end() {
        let mut ed = editor_with("x x x");
        ed.cursor = pos(0, 5);
        let m = matcher(SearchOptions {
            replace: "yy".into(),
            ..text_opts("x")
        });
        assert_eq!(ed.replace_remaining(&m, true), 3);
        assert_eq!(ed.buf.to_text(), "yy yy yy");
    }

    #[test]
    fn regex_replace_uses_groups_and_finishes_on_empty_matches() {
        let mut ed = editor_with("john smith");
        let m = matcher(SearchOptions {
            regex: true,
            replace: "$2 $1".into(),
            ..text_opts(r"(\w+) (\w+)")
        });
        assert_eq!(ed.replace_remaining(&m, false), 1);
        assert_eq!(ed.buf.to_text(), "smith john");

        let mut ed = editor_with("ab\ncd");
        let m = matcher(SearchOptions {
            regex: true,
            replace: "> ".into(),
            ..text_opts("^")
        });
        assert_eq!(ed.replace_remaining(&m, false), 2);
        assert_eq!(ed.buf.to_text(), "> ab\n> cd");

        let mut ed = editor_with("ab");
        let m = matcher(SearchOptions {
            regex: true,
            replace: "-".into(),
            ..text_opts("x*")
        });
        assert_eq!(ed.replace_remaining(&m, false), 3);
        assert_eq!(ed.buf.to_text(), "-a-b-");
    }

    #[test]
    fn find_all_lists_every_match() {
        let ed = editor_with("a a\nb\na");
        let hits = ed.find_all(&matcher(text_opts("a")));
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[2].0, pos(2, 0));
    }

    #[test]
    fn goto_accepts_every_documented_form() {
        let b = Buffer::from_text(&"x\n".repeat(200));
        let at = pos(49, 0);
        let go = |t: &str| parse_goto(t, at, &b, false).map(|p| p.line);
        assert_eq!(go("10"), Some(9));
        assert_eq!(go("+10"), Some(59));
        assert_eq!(go("-10"), Some(39));
        assert_eq!(go("50%"), Some(100));
        assert_eq!(go("0x10"), Some(15));
        assert_eq!(go("$10"), Some(15));
        assert_eq!(go("10h"), Some(15));
        assert_eq!(go("10m"), Some(9));
        assert_eq!(go(""), None);
        assert_eq!(go("x"), None);
        assert_eq!(parse_goto("10", at, &b, true).map(|p| p.line), Some(15));
        assert_eq!(go("999999"), Some(200));
        let wide = Buffer::from_text("abcdef\nxyz");
        assert_eq!(parse_goto("1,3", pos(1, 0), &wide, false), Some(pos(0, 2)));
        assert_eq!(parse_goto("1:3", pos(1, 0), &wide, false), Some(pos(0, 2)));
        assert_eq!(parse_goto(",2", pos(1, 0), &wide, false), Some(pos(1, 1)));
    }

    #[test]
    fn shifting_changes_selected_lines_and_undoes_together() {
        let mut ed = editor_with("a\nb\nc");
        ed.select_for(SHIFT);
        ed.cursor = pos(2, 0);
        ed.shift_lines(true);
        assert_eq!(ed.buf.to_text(), " a\n b\nc");
        ed.shift_lines(false);
        assert_eq!(ed.buf.to_text(), "a\nb\nc");
        ed.selection = None;
        ed.cursor = pos(1, 0);
        ed.shift_lines(false);
        assert_eq!(ed.buf.to_text(), "a\nb\nc");
        ed.shift_lines(true);
        ed.buf.undo();
        assert_eq!(ed.buf.to_text(), "a\nb\nc");
    }

    /// A file that accepts `room` bytes, then fails once.
    struct FullDisk {
        data: Vec<u8>,
        at: usize,
        room: usize,
        failed: bool,
    }

    impl Write for FullDisk {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if !self.failed && self.at + buf.len() > self.room {
                self.failed = true;
                let part = self.room - self.at;
                self.data.splice(
                    self.at..(self.at + part).min(self.data.len()),
                    buf[..part].to_vec(),
                );
                self.at += part;
                return Err(io::Error::other("no space left"));
            }
            let end = (self.at + buf.len()).min(self.data.len());
            self.data.splice(self.at..end, buf.to_vec());
            self.at += buf.len();
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Seek for FullDisk {
        fn seek(&mut self, _: SeekFrom) -> io::Result<u64> {
            self.at = 0;
            Ok(0)
        }
    }

    impl Rewritable for FullDisk {
        fn set_length(&mut self, length: u64) -> io::Result<()> {
            self.data.truncate(length as usize);
            Ok(())
        }
    }

    #[test]
    fn a_failed_in_place_save_puts_the_old_contents_back() {
        let old = b"original contents".to_vec();
        let mut file = FullDisk {
            data: old.clone(),
            at: 0,
            room: 4,
            failed: false,
        };
        let new = vec![b'N'; 1000];
        assert!(rewrite_keeping_old(&mut file, &old, &new).is_err());
        assert_eq!(file.data, old);
    }

    #[test]
    fn a_successful_in_place_save_can_shrink_the_file() {
        let old = b"long original".to_vec();
        let mut file = FullDisk {
            data: old.clone(),
            at: 0,
            room: 1000,
            failed: false,
        };
        rewrite_keeping_old(&mut file, &old, b"new").unwrap();
        assert_eq!(file.data, b"new");
    }

    #[test]
    fn saving_clears_setuid_and_writes_through_hard_links() {
        let dir = std::env::temp_dir().join(format!("far_ed_modes_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("prog");
        fs::write(&file, "old").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o4755)).unwrap();
        write_file(&file, b"new").unwrap();
        assert_eq!(fs::metadata(&file).unwrap().mode() & 0o7777, 0o755);
        let other = dir.join("other");
        fs::hard_link(&file, &other).unwrap();
        write_file(&file, b"shared").unwrap();
        assert_eq!(fs::read_to_string(&other).unwrap(), "shared");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn copy_without_a_block_copies_the_line() {
        let mut ed = editor_with("first\nsecond");
        ed.cursor = pos(1, 2);
        assert_eq!(ed.copy_or_line().text, "second\n");
    }

    #[test]
    fn word_at_cursor_finds_the_word_from_either_side() {
        let mut ed = editor_with("hello big_world");
        ed.cursor = pos(0, 8);
        assert_eq!(ed.word_at_cursor(), "big_world");
        ed.cursor = pos(0, 5);
        assert_eq!(ed.word_at_cursor(), "hello");
    }

    #[test]
    fn literal_keys_become_characters() {
        let key = |code, mods| Key { code, mods };
        assert_eq!(literal_char(key(KeyCode::Char('a'), CTRL)), Some('\u{1}'));
        assert_eq!(literal_char(key(KeyCode::Char('x'), NONE)), Some('x'));
        assert_eq!(literal_char(key(KeyCode::Tab, NONE)), Some('\t'));
        assert_eq!(literal_char(key(KeyCode::F(1), NONE)), None);
    }

    #[test]
    fn the_lock_blocks_edits_but_not_movement_or_saving_keys() {
        let key = |code, mods| Key { code, mods };
        assert!(changes_text(key(KeyCode::Char('a'), NONE)));
        assert!(changes_text(key(KeyCode::Char('y'), CTRL)));
        assert!(changes_text(key(KeyCode::Char('z'), CTRL)));
        assert!(changes_text(key(KeyCode::F(7), CTRL)));
        assert!(!changes_text(key(KeyCode::F(7), NONE)));
        assert!(!changes_text(key(KeyCode::Up, NONE)));
        assert!(!changes_text(key(KeyCode::Char('c'), CTRL)));
        assert!(!changes_text(key(KeyCode::Char('l'), CTRL)));
        assert!(!changes_text(key(KeyCode::Insert, CTRL)));
    }

    #[test]
    fn hiding_the_bars_gives_their_rows_to_the_text() {
        let mut ed = editor_with("line");
        let mut s = Screen::new(40, 10);
        assert_eq!(ed.text_rows(10, true), 8);
        ed.status_bar = false;
        assert_eq!(ed.text_rows(10, false), 10);
        ed.draw(&mut s, false);
        assert_eq!(s.cursor, Some((0, 0)));
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
        ed.draw(&mut s, true);
        assert_eq!(s.cursor, Some((0, 1)));
    }
}
