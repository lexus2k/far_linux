//! Single-line text editing used by the command line and dialog fields.

use crossterm::event::{KeyCode, KeyModifiers};

/// A one-line editable text with a cursor.
#[derive(Clone, Default)]
pub struct LineEdit {
    chars: Vec<char>,
    pos: usize,
    /// When set, the next typed character replaces the prefilled text.
    fresh: bool,
}

impl LineEdit {
    /// Creates an empty editor.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates an editor with prefilled text that typing replaces.
    pub fn prefilled(text: &str) -> Self {
        let chars: Vec<char> = text.chars().collect();
        let pos = chars.len();
        LineEdit {
            chars,
            pos,
            fresh: !text.is_empty(),
        }
    }

    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    pub fn cursor(&self) -> usize {
        self.pos
    }

    pub fn set(&mut self, text: &str) {
        self.chars = text.chars().collect();
        self.pos = self.chars.len();
        self.fresh = false;
    }

    pub fn clear(&mut self) {
        self.set("");
    }

    pub fn insert_str(&mut self, text: &str) {
        for c in text.chars().filter(|c| !c.is_control()) {
            self.insert(c);
        }
    }

    pub fn insert(&mut self, c: char) {
        if self.fresh {
            self.chars.clear();
            self.pos = 0;
            self.fresh = false;
        }
        self.chars.insert(self.pos, c);
        self.pos += 1;
    }

    pub fn left(&mut self) {
        self.fresh = false;
        self.pos = self.pos.saturating_sub(1);
    }

    pub fn right(&mut self) {
        self.fresh = false;
        self.pos = (self.pos + 1).min(self.chars.len());
    }

    pub fn home(&mut self) {
        self.fresh = false;
        self.pos = 0;
    }

    pub fn end(&mut self) {
        self.fresh = false;
        self.pos = self.chars.len();
    }

    pub fn word_left(&mut self) {
        self.fresh = false;
        while self.pos > 0 && !self.chars[self.pos - 1].is_alphanumeric() {
            self.pos -= 1;
        }
        while self.pos > 0 && self.chars[self.pos - 1].is_alphanumeric() {
            self.pos -= 1;
        }
    }

    pub fn word_right(&mut self) {
        self.fresh = false;
        let n = self.chars.len();
        while self.pos < n && self.chars[self.pos].is_alphanumeric() {
            self.pos += 1;
        }
        while self.pos < n && !self.chars[self.pos].is_alphanumeric() {
            self.pos += 1;
        }
    }

    pub fn backspace(&mut self) {
        self.fresh = false;
        if self.pos > 0 {
            self.pos -= 1;
            self.chars.remove(self.pos);
        }
    }

    pub fn delete(&mut self) {
        self.fresh = false;
        if self.pos < self.chars.len() {
            self.chars.remove(self.pos);
        }
    }

    pub fn delete_word_left(&mut self) {
        let end = self.pos;
        self.word_left();
        self.chars.drain(self.pos..end);
    }

    /// Applies a standard editing key. Returns true when the key was consumed.
    pub fn handle(&mut self, code: KeyCode, mods: KeyModifiers) -> bool {
        let ctrl = mods.contains(KeyModifiers::CONTROL);
        match code {
            KeyCode::Left if ctrl => self.word_left(),
            KeyCode::Right if ctrl => self.word_right(),
            KeyCode::Left => self.left(),
            KeyCode::Right => self.right(),
            KeyCode::Home => self.home(),
            KeyCode::End => self.end(),
            KeyCode::Backspace if ctrl => self.delete_word_left(),
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete => self.delete(),
            KeyCode::Char('y') if ctrl => self.clear(),
            KeyCode::Char('s') if ctrl => self.left(),
            KeyCode::Char('d') if ctrl => self.right(),
            KeyCode::Char(c) if !ctrl && !mods.contains(KeyModifiers::ALT) => self.insert(c),
            _ => return false,
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typing_replaces_prefilled_text_once() {
        let mut e = LineEdit::prefilled("old");
        e.insert('n');
        e.insert('e');
        assert_eq!(e.text(), "ne");
    }

    #[test]
    fn moving_keeps_prefilled_text() {
        let mut e = LineEdit::prefilled("old");
        e.left();
        e.insert('x');
        assert_eq!(e.text(), "olxd");
    }

    #[test]
    fn word_operations() {
        let mut e = LineEdit::new();
        e.set("one two");
        e.delete_word_left();
        assert_eq!(e.text(), "one ");
        e.home();
        e.word_right();
        assert_eq!(e.cursor(), 4);
    }

    #[test]
    fn multibyte_text_is_edited_by_character() {
        let mut e = LineEdit::new();
        e.set("файл");
        e.backspace();
        assert_eq!(e.text(), "фай");
    }
}
