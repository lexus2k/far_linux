//! Windows Terminal's win32-input-mode. Terminals normally report nothing when
//! Shift, Ctrl or Alt is pressed alone, so the key bar could not follow them.
//! In this mode each key press and release arrives as
//! `ESC [ Vk ; Sc ; Uc ; Kd ; Cs ; Rc _`, including the modifier keys.
//!
//! crossterm cannot parse these sequences, so this module reads them itself. It
//! is used only after the terminal has confirmed the mode, or on request with
//! `FAR_WIN32_INPUT=1`.

use crate::screen::{normalize_key, Input, Key, ALT, CTRL, NONE, SHIFT};
use crossterm::event::{KeyCode, KeyModifiers};
use std::collections::VecDeque;
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

/// Turns the mode on, with focus reports that clear a held modifier.
pub const ENABLE: &str = "\x1b[?9001h\x1b[?1004h";
/// Turns the mode off.
pub const DISABLE: &str = "\x1b[?1004l\x1b[?9001l";

const RIGHT_ALT: u32 = 0x01;
const LEFT_ALT: u32 = 0x02;
const RIGHT_CTRL: u32 = 0x04;
const LEFT_CTRL: u32 = 0x08;
const SHIFT_PRESSED: u32 = 0x10;

/// Longest answer the probe waits for.
const PROBE_TIME: Duration = Duration::from_millis(300);
/// How often `Reader::read` wakes to notice a resized terminal.
const SLICE: Duration = Duration::from_millis(50);
/// Silence after which input held back is given up on. Windows Terminal sends
/// each event whole, but a slow link can split one.
const GIVE_UP_AFTER: Duration = Duration::from_millis(500);
/// The same inside a paste, and after giving up on one. Ending a paste early
/// turns the rest of its line ends into Enter, which runs commands.
const GIVE_UP_PASTE_AFTER: Duration = Duration::from_secs(5);

/// How a paste arrived, so that one given up on can carry on in the same form.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PasteForm {
    Plain,
    Keys,
}

fn mods_from(state: u32) -> KeyModifiers {
    let mut mods = NONE;
    if state & SHIFT_PRESSED != 0 {
        mods |= SHIFT;
    }
    if state & (LEFT_CTRL | RIGHT_CTRL) != 0 {
        mods |= CTRL;
    }
    if state & (LEFT_ALT | RIGHT_ALT) != 0 {
        mods |= ALT;
    }
    mods
}

fn named_key(vk: u32) -> Option<KeyCode> {
    Some(match vk {
        0x08 => KeyCode::Backspace,
        0x09 => KeyCode::Tab,
        0x0D => KeyCode::Enter,
        0x1B => KeyCode::Esc,
        0x21 => KeyCode::PageUp,
        0x22 => KeyCode::PageDown,
        0x23 => KeyCode::End,
        0x24 => KeyCode::Home,
        0x25 => KeyCode::Left,
        0x26 => KeyCode::Up,
        0x27 => KeyCode::Right,
        0x28 => KeyCode::Down,
        0x2D => KeyCode::Insert,
        0x2E => KeyCode::Delete,
        0x70..=0x87 => KeyCode::F((vk - 0x6F) as u8),
        _ => return None,
    })
}

/// The unshifted character on a key, whatever the keyboard layout. Ctrl
/// commands use this, so they also work with a Cyrillic layout.
fn key_char(vk: u32) -> Option<char> {
    Some(match vk {
        0x20 => ' ',
        0x30..=0x39 | 0x41..=0x5A => char::from_u32(vk)?.to_ascii_lowercase(),
        0x60..=0x69 => char::from_u32(vk - 0x60 + 0x30)?,
        0xBA => ';',
        0xBB => '=',
        0xBC => ',',
        0xBD => '-',
        0xBE => '.',
        0xBF => '/',
        0xC0 => '`',
        0xDB => '[',
        0xDC => '\\',
        0xDD => ']',
        0xDE => '\'',
        _ => return None,
    })
}

fn printable(c: char) -> Option<char> {
    (!c.is_control()).then_some(c)
}

/// Converts one key press to the form the application matches on.
fn translate(vk: u32, ch: Option<char>, state: u32) -> Option<Key> {
    let mods = mods_from(state);
    if let Some(code) = named_key(vk) {
        return Some(normalize_key(code, mods, true));
    }
    let text = ch.and_then(printable);
    // AltGr is reported as right Alt plus left Ctrl, and it types text.
    if state & RIGHT_ALT != 0 && state & LEFT_CTRL != 0 {
        if let Some(c) = text {
            return Some(normalize_key(KeyCode::Char(c), NONE, true));
        }
    }
    // Without Ctrl or Alt, no character means nothing was typed, as with the
    // first press of a dead key.
    let c = if mods.contains(CTRL) {
        key_char(vk).or(text)
    } else if mods.contains(ALT) {
        text.or_else(|| key_char(vk))
    } else {
        text
    }?;
    Some(normalize_key(KeyCode::Char(c), mods, true))
}

/// Decodes the byte stream of the mode.
#[derive(Default)]
pub struct Decoder {
    buf: Vec<u8>,
    paste: Option<Vec<u8>>,
    high: Option<u32>,
    held: u8,
    /// Characters from key events without a key code, which is text sent to us
    /// rather than typed.
    sent_text: String,
    /// A paste between markers that arrived as key events. Windows' ConPTY turns
    /// everything the terminal sends into key events: the markers have no key
    /// code, the text looks typed, and a line feed looks like Ctrl+Enter.
    key_paste: Option<String>,
    /// A paste given up on, whose rest may still come.
    abandoned: Option<PasteForm>,
    /// The paste now open is such a rest, which is waited for only once.
    resumed: bool,
    /// The modifier state of the last key event inside a paste, applied when it
    /// ends, so that a Ctrl released during a paste does not stay held.
    paste_state: Option<u32>,
}

const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";
const PASTE_START_TEXT: &str = "\x1b[200~";
const PASTE_END_TEXT: &str = "\x1b[201~";

impl Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// How long to wait in silence before `flush`.
    pub fn give_up_after(&self) -> Duration {
        if self.paste.is_some() || self.key_paste.is_some() || self.abandoned.is_some() {
            GIVE_UP_PASTE_AFTER
        } else {
            GIVE_UP_AFTER
        }
    }

    /// Whether `flush` has more to do after another silence.
    pub fn waiting(&self) -> bool {
        self.abandoned.is_some()
    }

    /// Adds bytes and returns what they complete.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Input> {
        // The rest of a paste given up on is still part of it.
        match self.abandoned.take() {
            Some(PasteForm::Plain) => {
                self.paste = Some(Vec::new());
                self.resumed = true;
            }
            Some(PasteForm::Keys) => {
                self.key_paste = Some(String::new());
                self.resumed = true;
            }
            None => {}
        }
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        while self.step(&mut out) {}
        self.finish_sent_text(&mut out);
        out
    }

    /// Text sent as key events arrives together. More than one character is a
    /// paste, which keeps its line ends and does not run commands. One is what an
    /// input method sends. The start of a paste marker waits for the rest of it.
    fn finish_sent_text(&mut self, out: &mut Vec<Input>) {
        if self.sent_text.is_empty() || PASTE_START_TEXT.starts_with(&self.sent_text) {
            return;
        }
        self.send_text(out);
    }

    fn send_text(&mut self, out: &mut Vec<Input>) {
        let text = std::mem::take(&mut self.sent_text);
        let mut chars = text.chars();
        match (chars.next(), chars.next()) {
            (None, _) => {}
            (Some('\x1b'), None) => out.push(Input::Key(normalize_key(KeyCode::Esc, NONE, true))),
            (Some(c), None) if !c.is_control() => {
                out.push(Input::Key(normalize_key(KeyCode::Char(c), NONE, true)));
            }
            _ => push_paste(out, &text),
        }
    }

    /// Ends a paste: its text, then the modifiers held at its end.
    fn end_paste(&mut self, text: &str, out: &mut Vec<Input>) {
        self.resumed = false;
        push_paste(out, text);
        if let Some(state) = self.paste_state.take() {
            self.set_held(mods_from(state), out);
        }
    }

    /// Takes one character of text without a key code, which may finish a marker.
    fn push_sent_char(&mut self, c: char, out: &mut Vec<Input>) {
        self.sent_text.push(c);
        if let Some(before) = self.sent_text.strip_suffix(PASTE_START_TEXT) {
            let before = before.len();
            self.sent_text.truncate(before);
            self.send_text(out);
            self.key_paste = Some(String::new());
        }
    }

    /// Called when nothing more arrived for a while. A lone Esc byte was held
    /// back in case more of a sequence followed. A paste whose end marker never
    /// came must not swallow every later key, so it ends here. What follows soon
    /// after is still taken as the rest of it, and a second silence ends that.
    pub fn flush(&mut self) -> Vec<Input> {
        let mut out = Vec::new();
        if let Some(text) = self.key_paste.take() {
            let again = !self.resumed;
            let text = clean_paste(&text);
            self.end_paste(&text, &mut out);
            self.abandoned = again.then_some(PasteForm::Keys);
            return out;
        }
        if let Some(mut text) = self.paste.take() {
            text.append(&mut self.buf);
            let again = !self.resumed;
            let text = self.paste_text(&text);
            self.end_paste(&text, &mut out);
            self.abandoned = again.then_some(PasteForm::Plain);
            return out;
        }
        self.abandoned = None;
        self.send_text(&mut out);
        if self.buf == [0x1b] {
            self.buf.clear();
            out.push(Input::Key(normalize_key(KeyCode::Esc, NONE, true)));
        }
        out
    }

    /// Handles one item from the front of the buffer. False means more bytes
    /// are needed.
    fn step(&mut self, out: &mut Vec<Input>) -> bool {
        if let Some(text) = &mut self.paste {
            return match find(&self.buf, PASTE_END) {
                Some(end) => {
                    text.extend_from_slice(&self.buf[..end]);
                    let text = std::mem::take(text);
                    let text = self.paste_text(&text);
                    self.paste = None;
                    self.buf.drain(..end + PASTE_END.len());
                    self.end_paste(&text, out);
                    true
                }
                None => {
                    // Keep what could be the start of the end marker.
                    let keep = self.buf.len().min(PASTE_END.len() - 1);
                    let moved = self.buf.len() - keep;
                    text.extend_from_slice(&self.buf[..moved]);
                    self.buf.drain(..moved);
                    false
                }
            };
        }
        let Some(&first) = self.buf.first() else {
            return false;
        };
        if first == 0x1b {
            return self.escape(out);
        }
        self.plain(first, out)
    }

    /// A sequence starting at the front of the buffer.
    fn escape(&mut self, out: &mut Vec<Input>) -> bool {
        match self.buf.get(1) {
            None => return false,
            Some(b'[') => {}
            Some(_) => {
                self.buf.remove(0);
                out.push(Input::Key(normalize_key(KeyCode::Esc, NONE, true)));
                return true;
            }
        }
        // Parameter bytes end at a final byte. Anything else means this was not
        // a sequence at all, and must not swallow the real one that follows.
        let Some(at) = self.buf[2..]
            .iter()
            .position(|b| !(0x20..=0x3f).contains(b))
        else {
            return false;
        };
        if !(0x40..=0x7e).contains(&self.buf[2 + at]) {
            self.buf.drain(..2 + at);
            return true;
        }
        let len = at + 3;
        if self.buf[..len] == *PASTE_START {
            self.buf.drain(..len);
            self.paste = Some(Vec::new());
            return true;
        }
        if self.buf[..len] == *PASTE_END {
            // A paste that started as key events may still end with a plain marker.
            self.buf.drain(..len);
            if let Some(text) = self.key_paste.take() {
                let text = clean_paste(&text);
                self.end_paste(&text, out);
            }
            return true;
        }
        let final_byte = self.buf[len - 1];
        let params = String::from_utf8_lossy(&self.buf[2..len - 1]).into_owned();
        self.buf.drain(..len);
        match final_byte {
            b'_' => self.key_event(&params, out),
            // Focus moved: the release of a held modifier may never arrive.
            b'I' | b'O' => self.set_held(NONE, out),
            // Anything else, such as a late probe answer, is not input.
            _ => {}
        }
        true
    }

    /// A byte outside any sequence: text sent to us directly.
    fn plain(&mut self, first: u8, out: &mut Vec<Input>) -> bool {
        // Text sent as key events came first.
        self.send_text(out);
        let code = match first {
            b'\r' | b'\n' => Some(KeyCode::Enter),
            b'\t' => Some(KeyCode::Tab),
            0x7f => Some(KeyCode::Backspace),
            _ => None,
        };
        if let Some(code) = code {
            self.buf.remove(0);
            out.push(Input::Key(normalize_key(code, NONE, true)));
            return true;
        }
        if first < 0x20 {
            self.buf.remove(0);
            let c = char::from(first + 0x60);
            out.push(Input::Key(normalize_key(KeyCode::Char(c), CTRL, true)));
            return true;
        }
        let need = match first {
            0xC0..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF7 => 4,
            _ => 1,
        };
        // A broken character must not eat the sequence after it.
        let broken = self.buf[1..]
            .iter()
            .take(need - 1)
            .any(|b| b & 0xC0 != 0x80);
        if broken {
            self.buf.remove(0);
            return true;
        }
        if self.buf.len() < need {
            return false;
        }
        let bytes: Vec<u8> = self.buf.drain(..need).collect();
        if let Ok(text) = std::str::from_utf8(&bytes) {
            for c in text.chars() {
                out.push(Input::Key(normalize_key(KeyCode::Char(c), NONE, true)));
            }
        }
        true
    }

    /// The text of a paste between plain markers. Windows Terminal sends the text
    /// as it is but its line ends as key events, so `one ESC[13;28;13;1;0;1_` is
    /// a line. A marker sent as key events can be in here too.
    fn paste_text(&mut self, bytes: &[u8]) -> String {
        let mut text = Vec::with_capacity(bytes.len());
        let mut at = 0;
        while at < bytes.len() {
            let Some((fields, len)) = key_event_at(&bytes[at..]) else {
                text.push(bytes[at]);
                at += 1;
                continue;
            };
            at += len;
            let [_vk, _scan, unit, down, state, repeat] = fields;
            self.paste_state = Some(state);
            if down == 0 {
                continue;
            }
            if let Some(c) = self.unit_char(unit) {
                let mut buf = [0u8; 4];
                for _ in 0..repeat.clamp(1, 256) {
                    text.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                }
            }
        }
        clean_paste(&String::from_utf8_lossy(&text))
    }

    /// The character of an event. One outside the basic plane takes two events.
    fn unit_char(&mut self, unit: u32) -> Option<char> {
        match unit {
            0xD800..=0xDBFF => {
                self.high = Some(unit);
                None
            }
            0xDC00..=0xDFFF => {
                let high = self.high.take();
                high.and_then(|h| char::from_u32(0x10000 + ((h - 0xD800) << 10) + (unit - 0xDC00)))
            }
            0 => None,
            _ => {
                // A high surrogate must be followed by its low one at once.
                self.high = None;
                char::from_u32(unit)
            }
        }
    }

    fn push_key_paste(&mut self, c: char, out: &mut Vec<Input>) {
        let Some(text) = &mut self.key_paste else {
            return;
        };
        text.push(c);
        if let Some(body) = text.strip_suffix(PASTE_END_TEXT) {
            let body = clean_paste(body);
            self.key_paste = None;
            self.end_paste(&body, out);
        }
    }

    fn set_held(&mut self, mods: KeyModifiers, out: &mut Vec<Input>) {
        if mods.bits() != self.held {
            self.held = mods.bits();
            out.push(Input::Held(mods));
        }
    }

    fn key_event(&mut self, params: &str, out: &mut Vec<Input>) {
        let [vk, _scan, unit, down, state, repeat] = key_fields(params);
        if self.key_paste.is_some() {
            // Ctrl and other keys that the text seems to press mean nothing here.
            self.paste_state = Some(state);
            if down != 0 {
                if let Some(c) = self.unit_char(unit) {
                    for _ in 0..repeat.clamp(1, 256) {
                        self.push_key_paste(c, out);
                    }
                }
            }
            return;
        }
        if vk != 0 {
            self.finish_sent_text(out);
            // Whatever was held back was not a marker after all.
            self.send_text(out);
        }
        self.set_held(mods_from(state), out);
        if down == 0 {
            return;
        }
        let ch = self.unit_char(unit);
        // Text sent to us, unless Ctrl or Alt makes it a key.
        if vk == 0 && !mods_from(state).intersects(CTRL | ALT) {
            if let Some(c) = ch {
                for _ in 0..repeat.clamp(1, 256) {
                    self.push_sent_char(c, out);
                }
            }
            return;
        }
        if let Some(key) = translate(vk, ch, state) {
            for _ in 0..repeat.clamp(1, 256) {
                out.push(Input::Key(key));
            }
        }
    }
}

/// Adds a paste, unless it is empty.
fn push_paste(out: &mut Vec<Input>, text: &str) {
    if !text.is_empty() {
        out.push(Input::Paste(text.to_owned()));
    }
}

/// Pasted text without markers, focus reports and control characters other than
/// line ends and tabs. Esc or Ctrl+C pressed during a paste would be among them.
fn clean_paste(text: &str) -> String {
    text.replace(PASTE_START_TEXT, "")
        .replace(PASTE_END_TEXT, "")
        .replace("\x1b[I", "")
        .replace("\x1b[O", "")
        .chars()
        .filter(|c| !c.is_control() || matches!(c, '\r' | '\n' | '\t'))
        .collect()
}

/// The fields and length of a whole key event at the start of `bytes`.
fn key_event_at(bytes: &[u8]) -> Option<([u32; 6], usize)> {
    let rest = bytes.strip_prefix(b"\x1b[")?;
    let end = rest
        .iter()
        .position(|b| !(b.is_ascii_digit() || *b == b';'))?;
    if rest[end] != b'_' {
        return None;
    }
    let params = std::str::from_utf8(&rest[..end]).ok()?;
    Some((key_fields(params), end + 3))
}

/// The fields of a key event, with the defaults of the protocol.
fn key_fields(params: &str) -> [u32; 6] {
    let mut fields = [0u32, 0, 0, 0, 0, 1];
    for (slot, text) in fields.iter_mut().zip(params.split(';')) {
        if let Ok(value) = text.parse() {
            *slot = value;
        }
    }
    fields
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Waits up to `timeout` for bytes on standard input.
fn read_stdin(buf: &mut [u8], timeout: Duration) -> io::Result<usize> {
    let fd = io::stdin().as_raw_fd();
    let mut poll = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
    // SAFETY: `poll` is one valid pollfd for the duration of the call.
    let ready = unsafe { libc::poll(&mut poll, 1, ms) };
    if ready < 0 {
        let err = io::Error::last_os_error();
        return if err.kind() == io::ErrorKind::Interrupted {
            Ok(0)
        } else {
            Err(err)
        };
    }
    if ready == 0 {
        return Ok(0);
    }
    // SAFETY: `buf` is valid for writes of `buf.len()` bytes.
    let got = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
    match got {
        n if n > 0 => Ok(n as usize),
        0 => Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
        _ => {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                Ok(0)
            } else {
                Err(err)
            }
        }
    }
}

/// Reads input while the mode is on.
#[derive(Default)]
pub struct Reader {
    decoder: Decoder,
    pending: VecDeque<Input>,
    last_bytes: Option<Instant>,
}

impl Reader {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the next input, waiting at most `timeout`. Returns `None` when
    /// the time runs out, or sooner so that the caller can check the size.
    pub fn read(&mut self, timeout: Duration) -> io::Result<Option<Input>> {
        if let Some(input) = self.pending.pop_front() {
            return Ok(Some(input));
        }
        let mut buf = [0u8; 4096];
        let got = read_stdin(&mut buf, timeout.min(SLICE))?;
        if got > 0 {
            self.last_bytes = Some(Instant::now());
            let items = self.decoder.feed(&buf[..got]);
            self.pending.extend(items);
        } else if self
            .last_bytes
            .is_some_and(|t| t.elapsed() >= self.decoder.give_up_after())
        {
            let items = self.decoder.flush();
            self.pending.extend(items);
            // A paste given up on waits for one more silence.
            self.last_bytes = self.decoder.waiting().then(Instant::now);
        }
        Ok(self.pending.pop_front())
    }

    /// Queues input that arrived before this reader took over, ahead of newer
    /// input.
    pub fn queue(&mut self, input: Input) {
        self.pending.push_back(input);
    }
}

/// What the terminal said about the mode, from its reply to the probe.
fn answer(reply: &[u8]) -> Option<bool> {
    const ASK: &[u8] = b"\x1b[?9001;";
    if let Some(at) = find(reply, ASK) {
        let rest = &reply[at + ASK.len()..];
        if let Some(end) = find(rest, b"$y") {
            return Some(matches!(&rest[..end], b"1" | b"3"));
        }
        return None;
    }
    // The reply to the device attributes request comes last, so a terminal
    // that ignored the question has now finished answering.
    let start = find(reply, b"\x1b[?")?;
    reply[start..].contains(&b'c').then_some(false)
}

/// Decides from the environment. `None` means ask the terminal.
fn env_choice(forced: Option<&str>, wt: bool, multiplexer: bool) -> Option<bool> {
    match forced {
        Some("0") => Some(false),
        Some("1") => Some(true),
        _ if !wt || multiplexer => Some(false),
        _ => None,
    }
}

/// Turns the mode on if this terminal has it, and reports whether it did. Raw
/// mode must already be on. Any other terminal keeps crossterm's key handling.
pub fn try_enable() -> bool {
    let forced = std::env::var("FAR_WIN32_INPUT").ok();
    let wt = std::env::var_os("WT_SESSION").is_some();
    let multiplexer = std::env::var_os("TMUX").is_some() || std::env::var_os("STY").is_some();
    if let Some(choice) = env_choice(forced.as_deref(), wt, multiplexer) {
        return choice;
    }
    let mut out = io::stdout();
    if write!(out, "\x1b[?9001h\x1b[?9001$p\x1b[c")
        .and_then(|()| out.flush())
        .is_err()
    {
        return false;
    }
    let start = Instant::now();
    let mut reply = Vec::new();
    let mut buf = [0u8; 256];
    let mut result = false;
    while let Some(left) = PROBE_TIME.checked_sub(start.elapsed()) {
        match read_stdin(&mut buf, left) {
            Ok(n) => reply.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
        if let Some(on) = answer(&reply) {
            result = on;
            break;
        }
    }
    if !result {
        let _ = write!(out, "\x1b[?9001l");
        let _ = out.flush();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALT_BITS: u32 = LEFT_ALT;

    fn press(vk: u32, unit: u32, state: u32) -> String {
        format!("\x1b[{vk};0;{unit};1;{state};1_")
    }

    fn release(vk: u32, unit: u32, state: u32) -> String {
        format!("\x1b[{vk};0;{unit};0;{state};1_")
    }

    fn keys(input: &str) -> Vec<(KeyCode, KeyModifiers)> {
        Decoder::new()
            .feed(input.as_bytes())
            .into_iter()
            .filter_map(|i| match i {
                Input::Key(k) => Some((k.code, k.mods)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn modifier_keys_report_what_is_held() {
        let mut d = Decoder::new();
        assert_eq!(
            d.feed(press(0x10, 0, SHIFT_PRESSED).as_bytes()),
            vec![Input::Held(SHIFT)]
        );
        // Key repeat must not report the same state again.
        assert_eq!(d.feed(press(0x10, 0, SHIFT_PRESSED).as_bytes()), vec![]);
        assert_eq!(
            d.feed(release(0x10, 0, 0).as_bytes()),
            vec![Input::Held(NONE)]
        );
        assert_eq!(
            d.feed(press(0x11, 0, RIGHT_CTRL).as_bytes()),
            vec![Input::Held(CTRL)]
        );
    }

    #[test]
    fn losing_focus_clears_a_held_modifier() {
        let mut d = Decoder::new();
        d.feed(press(0x12, 0, LEFT_ALT).as_bytes());
        assert_eq!(d.feed(b"\x1b[O"), vec![Input::Held(NONE)]);
        assert_eq!(d.feed(b"\x1b[I"), vec![]);
    }

    #[test]
    fn function_and_navigation_keys_keep_their_modifiers() {
        assert_eq!(
            keys(&press(0x76, 0, SHIFT_PRESSED)),
            vec![(KeyCode::F(7), SHIFT)]
        );
        assert_eq!(
            keys(&press(0x2D, 0, LEFT_CTRL)),
            vec![(KeyCode::Insert, CTRL)]
        );
        assert_eq!(
            keys(&press(0x0D, 13, LEFT_CTRL)),
            vec![(KeyCode::Enter, CTRL)]
        );
        assert_eq!(
            keys(&press(0x09, 9, SHIFT_PRESSED)),
            vec![(KeyCode::Tab, SHIFT)]
        );
    }

    #[test]
    fn releases_type_nothing() {
        assert_eq!(keys(&release(0x41, 97, 0)), vec![]);
    }

    #[test]
    fn text_and_shifted_text() {
        assert_eq!(keys(&press(0x41, 97, 0)), vec![(KeyCode::Char('a'), NONE)]);
        assert_eq!(
            keys(&press(0x41, 65, SHIFT_PRESSED)),
            vec![(KeyCode::Char('A'), NONE)]
        );
        assert_eq!(
            keys(&press(0x44, 0x444, 0)),
            vec![(KeyCode::Char('ф'), NONE)]
        );
    }

    #[test]
    fn ctrl_commands_use_the_key_not_the_layout() {
        // Ctrl+O sends a control code, and with a Cyrillic layout the key is
        // still the O key.
        assert_eq!(
            keys(&press(0x4F, 15, LEFT_CTRL)),
            vec![(KeyCode::Char('o'), CTRL)]
        );
        assert_eq!(
            keys(&press(0xDC, 28, LEFT_CTRL)),
            vec![(KeyCode::Char('\\'), CTRL)]
        );
        assert_eq!(
            keys(&press(0x34, 0, LEFT_CTRL)),
            vec![(KeyCode::Char('4'), CTRL)]
        );
    }

    #[test]
    fn alt_gr_types_text() {
        assert_eq!(
            keys(&press(0x51, 0x40, RIGHT_ALT | LEFT_CTRL)),
            vec![(KeyCode::Char('@'), NONE)]
        );
    }

    #[test]
    fn surrogate_pairs_make_one_character() {
        let input = press(0, 0xD83D, 0) + &press(0, 0xDE00, 0);
        assert_eq!(keys(&input), vec![(KeyCode::Char('😀'), NONE)]);
    }

    #[test]
    fn repeat_count_and_split_reads() {
        assert_eq!(keys("\x1b[65;0;97;1;0;3_").len(), 3);
        let mut d = Decoder::new();
        assert_eq!(d.feed(b"\x1b[65;0;9"), vec![]);
        assert_eq!(d.feed(b"7;1;0;1_").len(), 1);
    }

    #[test]
    fn paste_arrives_whole_even_when_split() {
        let mut d = Decoder::new();
        assert_eq!(d.feed(b"\x1b[200~one\ntw"), vec![]);
        assert_eq!(d.feed(b"o\x1b[20"), vec![]);
        assert_eq!(d.feed(b"1~"), vec![Input::Paste("one\ntwo".to_string())]);
    }

    #[test]
    fn plain_bytes_still_work_and_lone_esc_waits() {
        assert_eq!(
            keys("aя\r"),
            vec![
                (KeyCode::Char('a'), NONE),
                (KeyCode::Char('я'), NONE),
                (KeyCode::Enter, NONE),
            ]
        );
        let mut d = Decoder::new();
        assert_eq!(d.feed(b"\x1b"), vec![]);
        assert_eq!(d.flush().len(), 1);
        assert_eq!(d.flush().len(), 0);
    }

    #[test]
    fn a_broken_start_does_not_swallow_the_key_after_it() {
        let event = press(0x41, 97, 0);
        assert_eq!(
            keys(&format!("\x1b[{event}")),
            vec![(KeyCode::Char('a'), NONE)]
        );
        let mut d = Decoder::new();
        let mut bytes = vec![0xC3];
        bytes.extend_from_slice(event.as_bytes());
        let got = d.feed(&bytes);
        assert_eq!(got.len(), 1, "{got:?}");
    }

    #[test]
    fn a_character_split_across_reads_still_arrives() {
        let mut d = Decoder::new();
        assert_eq!(d.feed(&[0xD1]), vec![]);
        assert_eq!(d.feed(&[0x8F]).len(), 1);
    }

    #[test]
    fn a_paste_without_its_end_marker_ends_when_input_stops() {
        let mut d = Decoder::new();
        assert_eq!(d.feed(b"\x1b[200~text"), vec![]);
        assert_eq!(d.flush(), vec![Input::Paste("text".to_string())]);
        // Soon after, more is the rest of it, given up on after one more silence.
        assert!(d.feed(press(0x41, 97, 0).as_bytes()).is_empty());
        assert_eq!(d.flush(), vec![Input::Paste("a".to_string())]);
        assert!(!d.waiting());
        assert!(matches!(
            d.feed(press(0x41, 97, 0).as_bytes())[..],
            [Input::Key(_)]
        ));
    }

    #[test]
    fn a_paste_sent_as_key_events_keeps_its_line_ends() {
        let mut d = Decoder::new();
        let mut input = String::new();
        for unit in "one\rtwo".encode_utf16() {
            input += &press(0, u32::from(unit), 0);
            input += &release(0, u32::from(unit), 0);
        }
        assert_eq!(
            d.feed(input.as_bytes()),
            vec![Input::Paste("one\rtwo".to_string())]
        );
    }

    #[test]
    fn paste_markers_sent_as_key_events_are_removed() {
        let mut d = Decoder::new();
        let mut input = String::new();
        for unit in "\x1b[200~text\x1b[201~".encode_utf16() {
            input += &press(0, u32::from(unit), 0);
        }
        assert_eq!(
            d.feed(input.as_bytes()),
            vec![Input::Paste("text".to_string())]
        );
    }

    #[test]
    fn one_character_from_an_input_method_is_a_key() {
        assert_eq!(keys(&press(0, 0x444, 0)), vec![(KeyCode::Char('ф'), NONE)]);
    }

    #[test]
    fn a_real_key_ends_sent_text_and_keeps_its_order() {
        let mut d = Decoder::new();
        let input = press(0, 97, 0) + &press(0, 98, 0) + &press(0x0D, 13, 0);
        let got = d.feed(input.as_bytes());
        assert_eq!(got[0], Input::Paste("ab".to_string()));
        assert!(matches!(got[1], Input::Key(k) if k.code == KeyCode::Enter));
    }

    /// Recorded from a headless ConPTY (conhost 10.0.26200) with win32 input and
    /// bracketed paste on, given `ESC[200~one LF two CR three ESC[201~`.
    const CONPTY_PASTE: &str = "\x1b[0;0;27;1;0;1_\x1b[0;0;91;1;0;1_\x1b[0;0;50;1;0;1_\x1b[0;0;48;1;0;1_\x1b[0;0;48;1;0;1_\x1b[0;0;126;1;0;1_\x1b[79;24;111;1;0;1_\x1b[79;24;111;0;0;1_\x1b[78;49;110;1;0;1_\x1b[78;49;110;0;0;1_\x1b[69;18;101;1;0;1_\x1b[69;18;101;0;0;1_\x1b[17;29;0;1;8;1_\x1b[13;28;10;1;8;1_\x1b[13;28;10;0;8;1_\x1b[17;29;0;0;0;1_\x1b[84;20;116;1;0;1_\x1b[84;20;116;0;0;1_\x1b[87;17;119;1;0;1_\x1b[87;17;119;0;0;1_\x1b[79;24;111;1;0;1_\x1b[79;24;111;0;0;1_\x1b[13;28;13;1;0;1_\x1b[13;28;13;0;0;1_\x1b[84;20;116;1;0;1_\x1b[84;20;116;0;0;1_\x1b[72;35;104;1;0;1_\x1b[72;35;104;0;0;1_\x1b[82;19;114;1;0;1_\x1b[82;19;114;0;0;1_\x1b[69;18;101;1;0;1_\x1b[69;18;101;0;0;1_\x1b[69;18;101;1;0;1_\x1b[69;18;101;0;0;1_\x1b[0;0;27;1;0;1_\x1b[0;0;91;1;0;1_\x1b[0;0;50;1;0;1_\x1b[0;0;48;1;0;1_\x1b[0;0;49;1;0;1_\x1b[0;0;126;1;0;1_";

    #[test]
    fn a_paste_through_conpty_keeps_every_line_end() {
        let mut d = Decoder::new();
        assert_eq!(
            d.feed(CONPTY_PASTE.as_bytes()),
            vec![Input::Paste("one\ntwo\rthree".to_string())]
        );
    }

    #[test]
    fn a_paste_through_conpty_may_be_split_anywhere() {
        // Split inside a sequence, inside each marker and inside the text.
        for size in [1, 7, 20, 64, 333] {
            let mut d = Decoder::new();
            let mut got = Vec::new();
            for piece in CONPTY_PASTE.as_bytes().chunks(size) {
                got.extend(d.feed(piece));
            }
            assert_eq!(
                got,
                vec![Input::Paste("one\ntwo\rthree".to_string())],
                "pieces of {size}"
            );
        }
    }

    #[test]
    fn keys_after_a_paste_through_conpty_work() {
        let mut d = Decoder::new();
        let input = CONPTY_PASTE.to_string() + &press(0x71, 0, 0);
        let got = d.feed(input.as_bytes());
        assert_eq!(got.len(), 2);
        assert!(matches!(got[1], Input::Key(k) if k.code == KeyCode::F(2)));
    }

    #[test]
    fn an_end_marker_that_never_comes_gives_the_text_up_later() {
        let mut d = Decoder::new();
        let end = CONPTY_PASTE.len() - 6 * "\x1b[0;0;27;1;0;1_".len();
        // Whole events only: every marker event here has the same length but one.
        let cut = CONPTY_PASTE[..end + 1].rfind("\x1b[").unwrap();
        assert!(d.feed(&CONPTY_PASTE.as_bytes()[..cut]).is_empty());
        assert_eq!(d.give_up_after(), GIVE_UP_PASTE_AFTER);
        assert_eq!(d.flush(), vec![Input::Paste("one\ntwo\rthree".to_string())]);
        // A second silence ends the wait, and keys work again.
        assert!(d.waiting());
        assert!(d.flush().is_empty());
        assert!(!d.waiting());
        assert_eq!(d.give_up_after(), GIVE_UP_AFTER);
        assert!(matches!(
            d.feed(press(0x71, 0, 0).as_bytes())[0],
            Input::Key(k) if k.code == KeyCode::F(2)
        ));
    }

    #[test]
    fn a_marker_start_that_goes_nowhere_is_not_lost() {
        let mut d = Decoder::new();
        assert!(d.feed(press(0, 27, 0).as_bytes()).is_empty());
        let got = d.feed((press(0, 91, 0) + &press(0, 97, 0)).as_bytes());
        assert_eq!(got, vec![Input::Paste("\x1b[a".to_string())]);
        // Esc alone is the key.
        let mut d = Decoder::new();
        assert!(d.feed(press(0, 27, 0).as_bytes()).is_empty());
        assert_eq!(keys_of(d.flush()), vec![(KeyCode::Esc, NONE)]);
    }

    #[test]
    fn a_real_ctrl_enter_is_still_ctrl_enter_outside_a_paste() {
        assert_eq!(
            keys(&press(0x0D, 10, LEFT_CTRL)),
            vec![(KeyCode::Enter, CTRL)]
        );
    }

    /// What the user's Windows Terminal 1.24 sent for one copied line, as seen on
    /// screen: plain markers and text, with the line end as two key events.
    const TERMINAL_LINE_END: &str = "\x1b[13;28;13;1;0;1_\x1b[13;28;13;0;0;1_";

    #[test]
    fn line_ends_sent_as_key_events_inside_a_paste_are_line_ends() {
        let input = format!("\x1b[200~one{TERMINAL_LINE_END}two{TERMINAL_LINE_END}\x1b[201~");
        let mut d = Decoder::new();
        assert_eq!(
            d.feed(input.as_bytes()),
            vec![Input::Paste("one\rtwo\r".to_string())]
        );
    }

    #[test]
    fn a_paste_with_key_events_inside_may_be_split_anywhere() {
        let input = format!("\x1b[200~один{TERMINAL_LINE_END}two\x1b[201~");
        for size in 1..input.len() {
            let mut d = Decoder::new();
            let mut got = Vec::new();
            for piece in input.as_bytes().chunks(size) {
                got.extend(d.feed(piece));
            }
            assert_eq!(got, vec![Input::Paste("один\rtwo".to_string())], "{size}");
        }
    }

    #[test]
    fn a_line_feed_as_ctrl_enter_inside_a_plain_paste_is_a_line_feed() {
        let input = "\x1b[200~a\x1b[17;29;0;1;8;1_\x1b[13;28;10;1;8;1_\x1b[13;28;10;0;8;1_\x1b[17;29;0;0;0;1_b\x1b[201~";
        let mut d = Decoder::new();
        assert_eq!(
            d.feed(input.as_bytes()),
            vec![Input::Paste("a\nb".to_string())]
        );
    }

    #[test]
    fn a_plain_paste_keeps_other_sequences_and_does_not_end_early() {
        let input = "\x1b[200~\x1b[31mred\x1b[13;28_\x1b[201~";
        let mut d = Decoder::new();
        let got = d.feed(input.as_bytes());
        // A short key event is still one, with its fields left at their defaults.
        assert_eq!(got, vec![Input::Paste("[31mred".to_string())]);
    }

    #[test]
    fn mixed_markers_still_end_a_paste() {
        // A plain start with an end sent as key events gives the text up on flush.
        let end: String = "\x1b[201~".chars().map(|c| press(0, c as u32, 0)).collect();
        let mut d = Decoder::new();
        assert!(d.feed(format!("\x1b[200~text{end}").as_bytes()).is_empty());
        assert_eq!(d.flush(), vec![Input::Paste("text".to_string())]);
        // A start sent as key events with a plain end.
        let start: String = "\x1b[200~".chars().map(|c| press(0, c as u32, 0)).collect();
        let mut d = Decoder::new();
        let input = format!("{start}{}\x1b[201~", press(0x41, 97, 0));
        assert_eq!(
            d.feed(input.as_bytes()),
            vec![Input::Paste("a".to_string())]
        );
    }

    #[test]
    fn keys_after_a_plain_paste_with_key_events_work() {
        let input = format!(
            "\x1b[200~a{TERMINAL_LINE_END}\x1b[201~{}",
            press(0x71, 0, 0)
        );
        let mut d = Decoder::new();
        let got = d.feed(input.as_bytes());
        assert_eq!(got[0], Input::Paste("a\r".to_string()));
        assert!(matches!(got[1], Input::Key(k) if k.code == KeyCode::F(2)));
    }

    fn keys_of(inputs: Vec<Input>) -> Vec<(KeyCode, KeyModifiers)> {
        inputs
            .into_iter()
            .filter_map(|i| match i {
                Input::Key(k) => Some((k.code, k.mods)),
                _ => None,
            })
            .collect()
    }

    fn line_end_event() -> &'static str {
        "\x1b[13;28;13;1;0;1_\x1b[13;28;13;0;0;1_"
    }

    #[test]
    fn the_rest_of_a_stalled_plain_paste_is_still_text() {
        let mut d = Decoder::new();
        assert!(d.feed(b"\x1b[200~touch pwned").is_empty());
        assert_eq!(d.flush(), vec![Input::Paste("touch pwned".to_string())]);
        let rest = format!("{}\x1b[201~", line_end_event());
        assert_eq!(
            d.feed(rest.as_bytes()),
            vec![Input::Paste("\r".to_string())]
        );
        assert!(!d.waiting());
    }

    #[test]
    fn the_rest_of_a_stalled_key_paste_is_still_text() {
        let start: String = "\x1b[200~x"
            .chars()
            .map(|c| press(0, c as u32, 0))
            .collect();
        let end: String = "\x1b[201~".chars().map(|c| press(0, c as u32, 0)).collect();
        let mut d = Decoder::new();
        assert!(d.feed(start.as_bytes()).is_empty());
        assert_eq!(d.flush(), vec![Input::Paste("x".to_string())]);
        let rest = press(0x0D, 13, 0) + &end;
        assert_eq!(
            d.feed(rest.as_bytes()),
            vec![Input::Paste("\r".to_string())]
        );
        // A late end marker alone is swallowed, not pasted.
        let mut d = Decoder::new();
        assert!(d.feed(start.as_bytes()).is_empty());
        d.flush();
        assert!(d.feed(end.as_bytes()).is_empty());
        let mut d = Decoder::new();
        assert!(d.feed(b"\x1b[200~x").is_empty());
        d.flush();
        assert!(d.feed(b"\x1b[201~").is_empty());
    }

    #[test]
    fn keys_pressed_during_a_paste_do_not_reach_the_text() {
        let esc = press(0x1B, 27, 0);
        let ctrl_c = press(0x43, 3, LEFT_CTRL);
        let input = format!("\x1b[200~ab{esc}{ctrl_c}cd\x1b[O\x1b[201~");
        assert_eq!(keys_text(&input), "abcd");
        let marker = |m: &str| -> String { m.chars().map(|c| press(0, c as u32, 0)).collect() };
        let input = format!(
            "{}{esc}{ctrl_c}{}",
            marker("\x1b[200~"),
            marker("\x1b[201~")
        );
        assert!(Decoder::new().feed(input.as_bytes()).is_empty());
    }

    fn keys_text(input: &str) -> String {
        match &Decoder::new().feed(input.as_bytes())[..] {
            [Input::Paste(text), ..] => text.clone(),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_ctrl_released_during_a_paste_is_not_left_held() {
        let mut d = Decoder::new();
        assert_eq!(
            d.feed(press(0x11, 0, LEFT_CTRL).as_bytes()),
            vec![Input::Held(CTRL)]
        );
        let input = format!("\x1b[200~text{}\x1b[201~", release(0x11, 0, 0));
        assert_eq!(
            d.feed(input.as_bytes()),
            vec![Input::Paste("text".to_string()), Input::Held(NONE)]
        );
    }

    #[test]
    fn an_empty_paste_is_nothing() {
        assert!(Decoder::new().feed(b"\x1b[200~\x1b[201~").is_empty());
        let mut d = Decoder::new();
        assert!(d.feed(b"\x1b[200~").is_empty());
        assert!(d.flush().is_empty());
    }

    #[test]
    fn sent_characters_keep_alt_and_repeat_counts_and_their_order() {
        assert_eq!(
            keys(&press(0, 97, LEFT_ALT)),
            vec![(KeyCode::Char('a'), ALT)]
        );
        let mut d = Decoder::new();
        assert_eq!(
            d.feed(b"\x1b[0;0;97;1;0;3_"),
            vec![Input::Paste("aaa".to_string())]
        );
        let input = press(0, 97, 0) + &press(0, 98, 0) + "c";
        let got = d.feed(input.as_bytes());
        assert_eq!(got[0], Input::Paste("ab".to_string()));
        assert!(matches!(got[1], Input::Key(k) if k.code == KeyCode::Char('c')));
    }

    #[test]
    fn two_lone_escapes_both_come_out_on_one_flush() {
        let mut d = Decoder::new();
        assert!(d.feed((press(0, 27, 0) + "\x1b").as_bytes()).is_empty());
        assert_eq!(
            keys_of(d.flush()),
            vec![(KeyCode::Esc, NONE), (KeyCode::Esc, NONE)]
        );
    }

    #[test]
    fn a_lone_low_surrogate_types_nothing() {
        assert_eq!(keys(&press(0, 0xDE00, 0)), vec![]);
    }

    #[test]
    fn a_dead_key_types_nothing_until_it_makes_a_character() {
        assert_eq!(keys(&press(0xDE, 0, 0)), vec![]);
        assert_eq!(
            keys(&press(0x41, 0, ALT_BITS)),
            vec![(KeyCode::Char('a'), ALT)]
        );
    }

    #[test]
    fn hostile_input_does_not_panic() {
        let mut d = Decoder::new();
        for input in [
            &b"\x1b[;;;;;;;;;;_"[..],
            b"\x1b[99999999999999;x;y;1;0;99999999_",
            b"\x1b[0;0;56320;1;0;1_",
            b"\xff\xfe\x80",
            b"\x1b[?9001;1$y",
        ] {
            d.feed(input);
        }
    }

    #[test]
    fn probe_answers() {
        assert_eq!(answer(b""), None);
        assert_eq!(answer(b"\x1b[?9001;1$y\x1b[?61c"), Some(true));
        assert_eq!(answer(b"\x1b[?9001;2$y"), Some(false));
        assert_eq!(answer(b"\x1b[?9001;0$y"), Some(false));
        assert_eq!(answer(b"\x1b[?61;4c"), Some(false));
        assert_eq!(answer(b"\x1b[?9001;1"), None);
    }

    #[test]
    fn environment_decides_when_the_mode_is_tried() {
        assert_eq!(env_choice(Some("0"), true, false), Some(false));
        assert_eq!(env_choice(Some("1"), false, true), Some(true));
        assert_eq!(env_choice(None, false, false), Some(false));
        assert_eq!(env_choice(None, true, true), Some(false));
        assert_eq!(env_choice(None, true, false), None);
    }

    #[test]
    fn every_named_key_has_the_right_code() {
        let table = [
            (0x08, KeyCode::Backspace),
            (0x09, KeyCode::Tab),
            (0x0D, KeyCode::Enter),
            (0x1B, KeyCode::Esc),
            (0x21, KeyCode::PageUp),
            (0x22, KeyCode::PageDown),
            (0x23, KeyCode::End),
            (0x24, KeyCode::Home),
            (0x25, KeyCode::Left),
            (0x26, KeyCode::Up),
            (0x27, KeyCode::Right),
            (0x28, KeyCode::Down),
            (0x2D, KeyCode::Insert),
            (0x2E, KeyCode::Delete),
        ];
        for (vk, code) in table {
            assert_eq!(keys(&press(vk, 0, 0)), vec![(code, NONE)], "{vk:#x}");
        }
        for n in 1..=12u8 {
            let vk = 0x6F + u32::from(n);
            assert_eq!(keys(&press(vk, 0, 0)), vec![(KeyCode::F(n), NONE)]);
            assert_eq!(keys(&press(vk, 0, LEFT_ALT)), vec![(KeyCode::F(n), ALT)]);
            assert_eq!(keys(&press(vk, 0, RIGHT_CTRL)), vec![(KeyCode::F(n), CTRL)]);
        }
    }

    #[test]
    fn ctrl_punctuation_uses_the_key() {
        for (vk, c) in [
            (0xBA, ';'),
            (0xBB, '='),
            (0xBC, ','),
            (0xBD, '-'),
            (0xBE, '.'),
            (0xBF, '/'),
            (0xC0, '`'),
            (0xDB, '['),
            (0xDD, ']'),
            (0xDE, '\''),
            (0x20, ' '),
        ] {
            assert_eq!(
                keys(&press(vk, 0, LEFT_CTRL)),
                vec![(KeyCode::Char(c), CTRL)],
                "{vk:#x}"
            );
        }
    }

    #[test]
    fn numpad_digits_with_ctrl_are_digits() {
        assert_eq!(
            keys(&press(0x65, 0, LEFT_CTRL)),
            vec![(KeyCode::Char('5'), CTRL)]
        );
    }

    #[test]
    fn alt_with_a_cyrillic_layout_keeps_the_typed_letter() {
        assert_eq!(
            keys(&press(0x44, 0x432, LEFT_ALT)),
            vec![(KeyCode::Char('в'), ALT)]
        );
    }

    #[test]
    fn modifier_keys_alone_type_nothing() {
        for vk in [0x10, 0x11, 0x12, 0x14, 0x5B] {
            assert_eq!(keys(&press(vk, 0, 0)), vec![], "{vk:#x}");
        }
    }

    #[test]
    fn plain_control_bytes_become_keys() {
        assert_eq!(keys("\t"), vec![(KeyCode::Tab, NONE)]);
        assert_eq!(keys("\x7f"), vec![(KeyCode::Backspace, NONE)]);
        assert_eq!(keys("\x01"), vec![(KeyCode::Char('a'), CTRL)]);
    }

    #[test]
    fn esc_then_other_bytes_is_esc_and_the_rest() {
        assert_eq!(
            keys("\x1bx"),
            vec![(KeyCode::Esc, NONE), (KeyCode::Char('x'), NONE)]
        );
    }

    #[test]
    fn other_sequences_are_not_input() {
        assert_eq!(keys("\x1b[?9001;1$y\x1b[?61c\x1b[1;1R"), vec![]);
    }

    #[test]
    fn a_huge_repeat_count_is_limited() {
        assert_eq!(keys("\x1b[65;0;97;1;0;4000000000_").len(), 256);
        assert_eq!(keys("\x1b[65;0;97;1;0;0_").len(), 1);
    }

    #[test]
    fn a_high_surrogate_is_dropped_by_an_unpaired_one() {
        let input = press(0, 0xD83D, 0) + &press(0x41, 97, 0) + &press(0, 0xDE00, 0);
        assert_eq!(keys(&input), vec![(KeyCode::Char('a'), NONE)]);
    }

    #[test]
    fn modifiers_in_a_key_event_update_what_is_held() {
        let mut d = Decoder::new();
        let got = d.feed(press(0x76, 0, SHIFT_PRESSED).as_bytes());
        assert_eq!(got[0], Input::Held(SHIFT));
        assert!(matches!(got[1], Input::Key(_)));
        assert_eq!(got.len(), 2);
    }
}
