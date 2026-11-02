//! Modal dialogs: messages, forms and menus. Each one draws over a snapshot
//! of what was on screen and returns when the user answers.

use crate::lineedit::LineEdit;
use crate::screen::{fit, fit_left, str_width, theme, Input, Screen, Tui, NONE, SHIFT};
use crossterm::event::KeyCode;
use std::io;
use std::time::Duration;

const WAIT: Duration = Duration::from_millis(500);

/// Horizontal position that centres a box of width `w`.
fn centered(screen: &Screen, w: u16, h: u16) -> (u16, u16) {
    (
        screen.w.saturating_sub(w) / 2,
        screen.h.saturating_sub(h) / 2,
    )
}

/// Draws text, button row and frame for a message box.
pub fn draw_message(
    s: &mut Screen,
    title: &str,
    lines: &[String],
    buttons: &[&str],
    focus: usize,
    error: bool,
) {
    let (normal, focused) = if error {
        (theme::ERROR, theme::ERROR_FOCUS)
    } else {
        (theme::DIALOG, theme::DIALOG_FOCUS)
    };
    let row_width: usize = buttons.iter().map(|b| str_width(b) + 5).sum();
    let widest = lines.iter().map(|l| str_width(l)).max().unwrap_or(0);
    let max = s.w.saturating_sub(4) as usize;
    let inner = widest
        .max(row_width)
        .max(str_width(title) + 2)
        .min(max.saturating_sub(4));
    let w = (inner + 6) as u16;
    let h = (lines.len() + 4 + usize::from(!buttons.is_empty())) as u16;
    let (x, y) = centered(s, w, h);
    s.fill(x, y, w, h, ' ', normal);
    s.frame(x + 1, y, w.saturating_sub(2), h, true, normal);
    s.title(x + 1, y, w.saturating_sub(2), title, normal);
    for (i, line) in lines.iter().enumerate() {
        let text = fit_left(line, inner);
        s.put(x + 3, y + 1 + i as u16, &text, normal);
    }
    if !buttons.is_empty() {
        let total = row_width.min(inner) as u16;
        let mut bx = x + (w.saturating_sub(total)) / 2;
        let by = y + h - 2;
        for (i, label) in buttons.iter().enumerate() {
            let style = if i == focus { focused } else { normal };
            bx = s.put(bx, by, &format!("[ {label} ]"), style) + 1;
        }
    }
}

/// Shows a message box. Returns the chosen button, or `None` on Esc.
pub fn message(
    tui: &mut Tui,
    title: &str,
    lines: &[String],
    buttons: &[&str],
    error: bool,
) -> io::Result<Option<usize>> {
    let backdrop = tui.screen.clone();
    let mut focus = 0;
    loop {
        tui.screen = resized(&backdrop, &tui.screen);
        tui.screen.cursor = None;
        draw_message(&mut tui.screen, title, lines, buttons, focus, error);
        tui.draw()?;
        let Some(Input::Key(key)) = tui.read(WAIT)? else {
            continue;
        };
        match key.code {
            KeyCode::Esc | KeyCode::F(10) => return Ok(None),
            KeyCode::Enter => return Ok(Some(focus)),
            KeyCode::Left => focus = (focus + buttons.len() - 1) % buttons.len(),
            KeyCode::Right | KeyCode::Tab => focus = (focus + 1) % buttons.len(),
            KeyCode::Char(c) => {
                let c = c.to_lowercase().next().unwrap_or(c);
                let hits: Vec<usize> = buttons
                    .iter()
                    .enumerate()
                    .filter(|(_, b)| b.to_lowercase().starts_with(c))
                    .map(|(i, _)| i)
                    .collect();
                if hits.len() == 1 {
                    return Ok(Some(hits[0]));
                }
            }
            _ => {}
        }
    }
}

/// Reuses the snapshot, or a blank screen if the terminal was resized.
fn resized(backdrop: &Screen, current: &Screen) -> Screen {
    if backdrop.w == current.w && backdrop.h == current.h {
        backdrop.clone()
    } else {
        Screen::new(current.w, current.h)
    }
}

/// Shows an error with a single OK button.
pub fn alert(tui: &mut Tui, title: &str, text: &str) -> io::Result<()> {
    let lines: Vec<String> = text.lines().map(str::to_owned).collect();
    message(tui, title, &lines, &["OK"], true)?;
    Ok(())
}

/// One row of a form.
pub enum Field {
    Label(String),
    Text { label: String, edit: LineEdit },
    Check { label: String, on: bool },
}

impl Field {
    /// A labelled text box with prefilled content.
    pub fn text(label: &str, value: &str) -> Field {
        Field::Text {
            label: label.to_string(),
            edit: LineEdit::prefilled(value),
        }
    }

    /// A labelled checkbox.
    pub fn check(label: &str, on: bool) -> Field {
        Field::Check {
            label: label.to_string(),
            on,
        }
    }

    fn focusable(&self) -> bool {
        !matches!(self, Field::Label(_))
    }

    fn height(&self) -> u16 {
        match self {
            Field::Text { .. } => 2,
            _ => 1,
        }
    }
}

/// A dialog made of text boxes and checkboxes.
pub struct Form {
    pub fields: Vec<Field>,
}

impl Form {
    pub fn new(fields: Vec<Field>) -> Self {
        Form { fields }
    }

    /// Text of the field at `index`.
    pub fn text(&self, index: usize) -> String {
        match &self.fields[index] {
            Field::Text { edit, .. } => edit.text(),
            _ => String::new(),
        }
    }

    /// State of the checkbox at `index`.
    pub fn checked(&self, index: usize) -> bool {
        matches!(&self.fields[index], Field::Check { on: true, .. })
    }

    /// Runs the form. Returns true on Enter and false on Esc.
    pub fn run(&mut self, tui: &mut Tui, title: &str) -> io::Result<bool> {
        let backdrop = tui.screen.clone();
        let mut focus = self.fields.iter().position(Field::focusable).unwrap_or(0);
        loop {
            tui.screen = resized(&backdrop, &tui.screen);
            self.draw(&mut tui.screen, title, focus);
            tui.draw()?;
            let input = tui.read(WAIT)?;
            let key = match input {
                Some(Input::Key(key)) => key,
                Some(Input::Paste(text)) => {
                    if let Field::Text { edit, .. } = &mut self.fields[focus] {
                        edit.insert_str(&text);
                    }
                    continue;
                }
                _ => continue,
            };
            match (key.code, key.mods) {
                (KeyCode::Esc, _) | (KeyCode::F(10), _) => {
                    tui.screen.cursor = None;
                    return Ok(false);
                }
                (KeyCode::Enter, _) => {
                    tui.screen.cursor = None;
                    return Ok(true);
                }
                (KeyCode::Down, _) | (KeyCode::Tab, NONE) => focus = self.step(focus, 1),
                (KeyCode::Up, _) | (KeyCode::Tab, SHIFT) => focus = self.step(focus, -1),
                _ => match &mut self.fields[focus] {
                    Field::Text { edit, .. } => {
                        edit.handle(key.code, key.mods);
                    }
                    Field::Check { on, .. } => {
                        if key.code == KeyCode::Char(' ') {
                            *on = !*on;
                        }
                    }
                    Field::Label(_) => {}
                },
            }
        }
    }

    fn step(&self, focus: usize, dir: isize) -> usize {
        let n = self.fields.len() as isize;
        let mut i = focus as isize;
        for _ in 0..n {
            i = (i + dir).rem_euclid(n);
            if self.fields[i as usize].focusable() {
                break;
            }
        }
        i as usize
    }

    fn draw(&self, s: &mut Screen, title: &str, focus: usize) {
        let w = s.w.saturating_sub(8).clamp(20, 72);
        let body: u16 = self.fields.iter().map(Field::height).sum();
        let h = body + 3;
        let (x, y) = centered(s, w, h);
        s.fill(x, y, w, h, ' ', theme::DIALOG);
        s.frame(x + 1, y, w.saturating_sub(2), h, true, theme::DIALOG);
        s.title(x + 1, y, w.saturating_sub(2), title, theme::DIALOG);
        let inner = w.saturating_sub(6) as usize;
        let mut row = y + 1;
        s.cursor = None;
        for (i, field) in self.fields.iter().enumerate() {
            match field {
                Field::Label(text) => {
                    s.put(x + 3, row, &fit_left(text, inner), theme::DIALOG);
                    row += 1;
                }
                Field::Text { label, edit } => {
                    s.put(x + 3, row, &fit_left(label, inner), theme::DIALOG);
                    row += 1;
                    let (shown, cursor) = visible_text(edit, inner);
                    s.put(x + 3, row, &fit(&shown, inner), theme::INPUT);
                    if i == focus {
                        s.cursor = Some((x + 3 + cursor as u16, row));
                    }
                    row += 1;
                }
                Field::Check { label, on } => {
                    let mark = if *on { 'x' } else { ' ' };
                    let style = if i == focus {
                        theme::DIALOG_FOCUS
                    } else {
                        theme::DIALOG
                    };
                    s.put(x + 3, row, &format!("[{mark}] {label}"), style);
                    if i == focus {
                        s.cursor = Some((x + 4, row));
                    }
                    row += 1;
                }
            }
        }
        let buttons = "[ OK: Enter ]  [ Cancel: Esc ]";
        let bx = x + w.saturating_sub(str_width(buttons) as u16) / 2;
        s.put(bx, row, buttons, theme::DIALOG);
    }
}

/// The part of the text that fits in `width`, with the cursor's column.
pub fn visible_text(edit: &LineEdit, width: usize) -> (String, usize) {
    let chars: Vec<char> = edit.text().chars().collect();
    let cursor = edit.cursor();
    let before: String = chars[..cursor].iter().collect();
    let before_w = str_width(&before);
    if before_w < width {
        return (chars.iter().collect(), before_w);
    }
    // Scroll so that the cursor sits at the right edge.
    let mut used = 1;
    let mut start = cursor;
    while start > 0 {
        let w = crate::screen::char_width(chars[start - 1]);
        if used + w > width {
            break;
        }
        used += w;
        start -= 1;
    }
    (chars[start..].iter().collect(), used - 1)
}

/// Shows a list and returns the chosen index, or `None` on Esc.
pub fn menu(
    tui: &mut Tui,
    title: &str,
    items: &[String],
    start: usize,
) -> io::Result<Option<usize>> {
    if items.is_empty() {
        return Ok(None);
    }
    let backdrop = tui.screen.clone();
    let mut focus = start.min(items.len() - 1);
    let mut top = 0usize;
    loop {
        tui.screen = resized(&backdrop, &tui.screen);
        tui.screen.cursor = None;
        let s = &mut tui.screen;
        let widest = items.iter().map(|i| str_width(i)).max().unwrap_or(0);
        let w = (widest.max(str_width(title) + 2) + 6).min(s.w as usize - 2) as u16;
        let rows = items.len().min(s.h.saturating_sub(6) as usize).max(1);
        let h = rows as u16 + 2;
        let (x, y) = centered(s, w, h);
        if focus < top {
            top = focus;
        }
        if focus >= top + rows {
            top = focus + 1 - rows;
        }
        s.frame(x, y, w, h, true, theme::DIALOG);
        s.title(x, y, w, title, theme::DIALOG);
        for i in 0..rows {
            let item = &items[top + i];
            let style = if top + i == focus {
                theme::DIALOG_FOCUS
            } else {
                theme::DIALOG
            };
            s.put(
                x + 1,
                y + 1 + i as u16,
                &format!(" {} ", fit(item, w as usize - 4)),
                style,
            );
        }
        tui.draw()?;
        let Some(Input::Key(key)) = tui.read(WAIT)? else {
            continue;
        };
        match key.code {
            KeyCode::Esc | KeyCode::F(10) => return Ok(None),
            KeyCode::Enter => return Ok(Some(focus)),
            KeyCode::Up => focus = (focus + items.len() - 1) % items.len(),
            KeyCode::Down => focus = (focus + 1) % items.len(),
            KeyCode::PageUp => focus = focus.saturating_sub(rows),
            KeyCode::PageDown => focus = (focus + rows).min(items.len() - 1),
            KeyCode::Home => focus = 0,
            KeyCode::End => focus = items.len() - 1,
            _ => {}
        }
    }
}

/// Asks for one line of text. Returns `None` on Esc.
pub fn prompt(tui: &mut Tui, title: &str, label: &str, value: &str) -> io::Result<Option<String>> {
    let mut form = Form::new(vec![Field::text(label, value)]);
    Ok(if form.run(tui, title)? {
        Some(form.text(0))
    } else {
        None
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_is_shown_whole() {
        let mut edit = LineEdit::new();
        edit.set("abc");
        assert_eq!(visible_text(&edit, 10), ("abc".to_string(), 3));
    }

    #[test]
    fn long_text_scrolls_to_keep_cursor_visible() {
        let mut edit = LineEdit::new();
        edit.set("abcdefghij");
        let (text, cursor) = visible_text(&edit, 5);
        assert!(cursor < 5);
        assert_eq!(text, "ghij");
    }

    #[test]
    fn message_box_draws_title_text_and_buttons() {
        let mut s = Screen::new(60, 12);
        draw_message(
            &mut s,
            "Delete",
            &["Remove a.txt?".into()],
            &["Yes", "No"],
            0,
            false,
        );
        let all: String = (0..12).map(|y| s.row_text(y)).collect();
        assert!(all.contains("Delete"));
        assert!(all.contains("Remove a.txt?"));
        assert!(all.contains("[ Yes ]"));
        assert!(all.contains("[ No ]"));
    }
}
