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
    let c = if mods.contains(CTRL) {
        key_char(vk).or(text)
    } else {
        text.or_else(|| key_char(vk))
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
}

const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

impl Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds bytes and returns what they complete.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Input> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        while self.step(&mut out) {}
        out
    }

    /// A lone Esc byte is held back in case more of a sequence follows. Called
    /// when nothing more arrived.
    pub fn flush(&mut self) -> Vec<Input> {
        if self.paste.is_none() && self.buf == [0x1b] {
            self.buf.clear();
            return vec![Input::Key(normalize_key(KeyCode::Esc, NONE, true))];
        }
        Vec::new()
    }

    /// Handles one item from the front of the buffer. False means more bytes
    /// are needed.
    fn step(&mut self, out: &mut Vec<Input>) -> bool {
        if let Some(text) = &mut self.paste {
            return match find(&self.buf, PASTE_END) {
                Some(end) => {
                    text.extend_from_slice(&self.buf[..end]);
                    let text = String::from_utf8_lossy(text).into_owned();
                    self.paste = None;
                    self.buf.drain(..end + PASTE_END.len());
                    out.push(Input::Paste(text));
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
        let Some(len) = self.buf[2..]
            .iter()
            .position(|b| (0x40..=0x7e).contains(b))
            .map(|i| i + 3)
        else {
            return false;
        };
        if self.buf[..len] == *PASTE_START {
            self.buf.drain(..len);
            self.paste = Some(Vec::new());
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

    fn set_held(&mut self, mods: KeyModifiers, out: &mut Vec<Input>) {
        if mods.bits() != self.held {
            self.held = mods.bits();
            out.push(Input::Held(mods));
        }
    }

    fn key_event(&mut self, params: &str, out: &mut Vec<Input>) {
        let mut fields = [0u32, 0, 0, 0, 0, 1];
        for (slot, text) in fields.iter_mut().zip(params.split(';')) {
            if let Ok(value) = text.parse() {
                *slot = value;
            }
        }
        let [vk, _scan, unit, down, state, repeat] = fields;
        self.set_held(mods_from(state), out);
        if down == 0 {
            return;
        }
        // A character outside the basic plane arrives as two events.
        let ch = match unit {
            0xD800..=0xDBFF => {
                self.high = Some(unit);
                return;
            }
            0xDC00..=0xDFFF => {
                let high = self.high.take().unwrap_or(0xD800);
                char::from_u32(0x10000 + ((high - 0xD800) << 10) + (unit - 0xDC00))
            }
            0 => None,
            _ => char::from_u32(unit),
        };
        if let Some(key) = translate(vk, ch, state) {
            for _ in 0..repeat.clamp(1, 256) {
                out.push(Input::Key(key));
            }
        }
    }
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
        let items = if got > 0 {
            self.decoder.feed(&buf[..got])
        } else {
            self.decoder.flush()
        };
        self.pending.extend(items);
        Ok(self.pending.pop_front())
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
}
