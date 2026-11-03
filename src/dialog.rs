//! Modal dialogs: messages, forms and menus. Each one draws over a snapshot
//! of what was on screen and returns when the user answers.

use crate::history::{History, Kind};
use crate::lineedit::LineEdit;
use crate::screen::{
    fit, fit_left, str_width, theme, Input, Key, Screen, Tui, ALT, CTRL, NONE, SHIFT,
};
use crossterm::event::KeyCode;
use std::io;
use std::rc::Rc;
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
    let h = (lines.len() + 2 + usize::from(!buttons.is_empty())) as u16;
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
            _ => {
                if let Some(index) = button_for_key(buttons, key) {
                    return Ok(Some(index));
                }
            }
        }
    }
}

/// The button a key chooses by its first letter. Only the plain letter or Alt
/// with it counts: `Ctrl+A` at a delete prompt must not mean All.
fn button_for_key(buttons: &[&str], key: Key) -> Option<usize> {
    let KeyCode::Char(c) = key.code else {
        return None;
    };
    if key.mods != NONE && key.mods != ALT {
        return None;
    }
    let c = c.to_lowercase().next().unwrap_or(c);
    let mut hits = buttons
        .iter()
        .enumerate()
        .filter(|(_, b)| b.to_lowercase().starts_with(c))
        .map(|(i, _)| i);
    match (hits.next(), hits.next()) {
        (Some(index), None) => Some(index),
        _ => None,
    }
}

/// Reuses the snapshot, or a blank screen if the terminal was resized.
pub fn resized(backdrop: &Screen, current: &Screen) -> Screen {
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

/// Splits `&Label` into the shown text and its Alt hotkey.
fn parse_label(label: &str) -> (String, Option<char>) {
    match label.find('&') {
        Some(i) => {
            let shown = format!("{}{}", &label[..i], &label[i + 1..]);
            let hotkey = label[i + 1..]
                .chars()
                .next()
                .and_then(|c| c.to_lowercase().next());
            (shown, hotkey)
        }
        None => (label.to_string(), None),
    }
}

/// One row of a form. A `&` in a label marks its Alt hotkey.
pub enum Field {
    Label(String),
    Text {
        label: String,
        edit: LineEdit,
        history: Option<Kind>,
    },
    Check {
        label: String,
        on: bool,
    },
}

impl Field {
    /// A labelled text box with prefilled content.
    pub fn text(label: &str, value: &str) -> Field {
        Field::Text {
            label: label.to_string(),
            edit: LineEdit::prefilled(value),
            history: None,
        }
    }

    /// A text box with a history list on Ctrl+Up and Ctrl+Down.
    pub fn with_history(label: &str, value: &str, kind: Kind) -> Field {
        Field::Text {
            label: label.to_string(),
            edit: LineEdit::prefilled(value),
            history: Some(kind),
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

    fn hotkey(&self) -> Option<char> {
        match self {
            Field::Text { label, .. } | Field::Check { label, .. } => parse_label(label).1,
            Field::Label(_) => None,
        }
    }
}

/// A dialog of text boxes, checkboxes and optional buttons.
pub struct Form {
    pub fields: Vec<Field>,
    buttons: Vec<String>,
    db: Option<Rc<History>>,
}

impl Form {
    pub fn new(fields: Vec<Field>) -> Self {
        Form {
            fields,
            buttons: Vec::new(),
            db: None,
        }
    }

    /// Adds buttons, which take the place of the OK and Cancel hint.
    pub fn with_buttons(mut self, buttons: &[&str]) -> Self {
        self.buttons = buttons.iter().map(|b| b.to_string()).collect();
        self
    }

    /// Connects the fields made with `Field::with_history` to the database.
    pub fn with_db(mut self, db: &Rc<History>) -> Self {
        self.db = Some(db.clone());
        self
    }

    /// Text of the field at `index`.
    pub fn text(&self, index: usize) -> String {
        match &self.fields[index] {
            Field::Text { edit, .. } => edit.text(),
            _ => String::new(),
        }
    }

    /// Replaces the text of the field at `index`.
    pub fn set_text(&mut self, index: usize, value: &str) {
        if let Field::Text { edit, .. } = &mut self.fields[index] {
            edit.set(value);
        }
    }

    /// State of the checkbox at `index`.
    pub fn checked(&self, index: usize) -> bool {
        matches!(&self.fields[index], Field::Check { on: true, .. })
    }

    /// Runs the form. Returns true on Enter and false on Esc.
    pub fn run(&mut self, tui: &mut Tui, title: &str) -> io::Result<bool> {
        Ok(self.run_buttons(tui, title)?.is_some())
    }

    /// Runs the form. Returns the chosen button, or `None` on Esc. Enter in a
    /// text box or checkbox chooses the first button.
    pub fn run_buttons(&mut self, tui: &mut Tui, title: &str) -> io::Result<Option<usize>> {
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
                    if let Some(Field::Text { edit, .. }) = self.fields.get_mut(focus) {
                        edit.insert_str(&text);
                    }
                    continue;
                }
                _ => continue,
            };
            let on_button = focus.checked_sub(self.fields.len());
            match (key.code, key.mods) {
                (KeyCode::Esc, _) | (KeyCode::F(10), _) => {
                    tui.screen.cursor = None;
                    return Ok(None);
                }
                (KeyCode::Enter, _) => {
                    tui.screen.cursor = None;
                    return Ok(Some(on_button.unwrap_or(0)));
                }
                (KeyCode::Char(c), ALT) => {
                    if let Some(index) = self.fields.iter().position(|f| f.hotkey() == Some(c)) {
                        focus = index;
                        if let Field::Check { on, .. } = &mut self.fields[index] {
                            *on = !*on;
                        }
                    } else if let Some(index) = self
                        .buttons
                        .iter()
                        .position(|b| parse_label(b).1 == Some(c))
                    {
                        tui.screen.cursor = None;
                        return Ok(Some(index));
                    }
                }
                (KeyCode::Up | KeyCode::Down, CTRL) => {
                    let Some(db) = self.db.clone() else {
                        continue;
                    };
                    if let Some(Field::Text {
                        edit,
                        history: Some(kind),
                        ..
                    }) = self.fields.get_mut(focus)
                    {
                        if let Some(value) = history_popup(tui, &db, *kind)? {
                            edit.set(&value);
                        }
                    }
                }
                (KeyCode::Down, _) | (KeyCode::Tab, NONE) => focus = self.step(focus, 1),
                (KeyCode::Up, _) | (KeyCode::Tab, SHIFT) => focus = self.step(focus, -1),
                (KeyCode::Right, _) if on_button.is_some() => focus = self.step(focus, 1),
                (KeyCode::Left, _) if on_button.is_some() => focus = self.step(focus, -1),
                _ => match self.fields.get_mut(focus) {
                    Some(Field::Text { edit, .. }) => {
                        edit.handle(key.code, key.mods);
                    }
                    Some(Field::Check { on, .. }) => {
                        if key.code == KeyCode::Char(' ') {
                            *on = !*on;
                        }
                    }
                    _ => {
                        if key.code == KeyCode::Char(' ') {
                            if let Some(index) = on_button {
                                tui.screen.cursor = None;
                                return Ok(Some(index));
                            }
                        }
                    }
                },
            }
        }
    }

    fn step(&self, focus: usize, dir: isize) -> usize {
        let n = (self.fields.len() + self.buttons.len()) as isize;
        let mut i = focus as isize;
        for _ in 0..n {
            i = (i + dir).rem_euclid(n);
            let usable = self.fields.get(i as usize).is_none_or(Field::focusable);
            if usable {
                break;
            }
        }
        i as usize
    }

    fn draw(&self, s: &mut Screen, title: &str, focus: usize) {
        let w = s.w.saturating_sub(8).clamp(20, 72);
        let inner = w.saturating_sub(6) as usize;
        let button_rows = self.button_rows(inner);
        let body: u16 = self.fields.iter().map(Field::height).sum();
        let h = body + 2 + button_rows.len().max(1) as u16;
        let (x, y) = centered(s, w, h);
        s.fill(x, y, w, h, ' ', theme::DIALOG);
        s.frame(x + 1, y, w.saturating_sub(2), h, true, theme::DIALOG);
        s.title(x + 1, y, w.saturating_sub(2), title, theme::DIALOG);
        let mut row = y + 1;
        s.cursor = None;
        for (i, field) in self.fields.iter().enumerate() {
            match field {
                Field::Label(text) => {
                    s.put(x + 3, row, &fit_left(text, inner), theme::DIALOG);
                    row += 1;
                }
                Field::Text { label, edit, .. } => {
                    let shown = parse_label(label).0;
                    s.put(x + 3, row, &fit_left(&shown, inner), theme::DIALOG);
                    row += 1;
                    let (visible, cursor) = visible_text(edit, inner);
                    s.put(x + 3, row, &fit(&visible, inner), theme::INPUT);
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
                    let shown = parse_label(label).0;
                    s.put(x + 3, row, &format!("[{mark}] {shown}"), style);
                    if i == focus {
                        s.cursor = Some((x + 4, row));
                    }
                    row += 1;
                }
            }
        }
        if self.buttons.is_empty() {
            let hint = "[ OK: Enter ]  [ Cancel: Esc ]";
            let bx = x + w.saturating_sub(str_width(hint) as u16) / 2;
            s.put(bx, row, hint, theme::DIALOG);
            return;
        }
        for indexes in button_rows {
            let total: usize = indexes.iter().map(|i| self.button_width(*i) + 1).sum();
            let mut bx = x + w.saturating_sub(total as u16) / 2;
            for i in indexes {
                let style = if focus == self.fields.len() + i {
                    theme::DIALOG_FOCUS
                } else {
                    theme::DIALOG
                };
                let label = format!("[ {} ]", parse_label(&self.buttons[i]).0);
                bx = s.put(bx, row, &label, style) + 1;
            }
            row += 1;
        }
    }

    fn button_width(&self, index: usize) -> usize {
        str_width(&parse_label(&self.buttons[index]).0) + 4
    }

    /// Splits the buttons into rows that fit the dialog.
    fn button_rows(&self, inner: usize) -> Vec<Vec<usize>> {
        let mut rows: Vec<Vec<usize>> = Vec::new();
        let mut used = 0;
        for i in 0..self.buttons.len() {
            let width = self.button_width(i) + 1;
            match rows.last_mut() {
                Some(row) if used + width <= inner => row.push(i),
                _ => {
                    rows.push(vec![i]);
                    used = 0;
                }
            }
            used += width;
        }
        rows
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
    Ok(match menu_keys(tui, title, items, start, &[])? {
        MenuResult::Chosen(index) => Some(index),
        _ => None,
    })
}

/// How a menu ended.
#[derive(Debug, PartialEq, Eq)]
pub enum MenuResult {
    Chosen(usize),
    /// One of the extra keys, with the item that had the focus.
    Key(Key, usize),
    Closed,
}

/// A menu where the listed keys also close it, for lists with item commands.
pub fn menu_keys(
    tui: &mut Tui,
    title: &str,
    items: &[String],
    start: usize,
    extra: &[Key],
) -> io::Result<MenuResult> {
    if items.is_empty() {
        return Ok(MenuResult::Closed);
    }
    let backdrop = tui.screen.clone();
    let mut focus = start.min(items.len() - 1);
    let mut top = 0usize;
    loop {
        tui.screen = resized(&backdrop, &tui.screen);
        tui.screen.cursor = None;
        let s = &mut tui.screen;
        let widest = items.iter().map(|i| str_width(i)).max().unwrap_or(0);
        let w = (widest.max(str_width(title) + 2) + 6).min((s.w as usize).saturating_sub(2)) as u16;
        if w < 6 || s.h < 3 {
            // Too small to draw. Keys still work, and a resize brings it back.
            let Some(Input::Key(key)) = tui.read(WAIT)? else {
                continue;
            };
            if key.code == KeyCode::Esc {
                return Ok(MenuResult::Closed);
            }
            continue;
        }
        let rows = items.len().min(s.h.saturating_sub(6) as usize).max(1);
        let h = rows as u16 + 2;
        let (x, y) = centered(s, w, h);
        // After the terminal grows, the old top can leave too few items.
        top = top.min(items.len() - rows);
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
        if extra.contains(&key) {
            return Ok(MenuResult::Key(key, focus));
        }
        match key.code {
            KeyCode::Esc | KeyCode::F(10) => return Ok(MenuResult::Closed),
            KeyCode::Enter => return Ok(MenuResult::Chosen(focus)),
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

/// The history of a text box: Enter picks, Ins locks, Shift+Del removes one
/// item and Del clears all that are not locked.
pub fn history_popup(tui: &mut Tui, db: &History, kind: Kind) -> io::Result<Option<String>> {
    let del = Key {
        code: KeyCode::Delete,
        mods: NONE,
    };
    let shift_del = Key {
        code: KeyCode::Delete,
        mods: SHIFT,
    };
    let ins = Key {
        code: KeyCode::Insert,
        mods: NONE,
    };
    let mut focus = 0;
    loop {
        let found = db.items(kind);
        let items: Vec<String> = found
            .iter()
            .map(|(value, locked)| format!("{} {value}", if *locked { "+" } else { " " }))
            .collect();
        match menu_keys(tui, "History", &items, focus, &[del, shift_del, ins])? {
            MenuResult::Chosen(index) => return Ok(Some(found[index].0.clone())),
            MenuResult::Closed => return Ok(None),
            MenuResult::Key(key, index) => {
                focus = index;
                let value = &found[index].0;
                if key == ins {
                    db.toggle_lock(kind, value);
                } else if key == shift_del {
                    db.remove(kind, value);
                } else {
                    db.clear(kind);
                }
            }
        }
    }
}

/// Shows a scrollable list of keys and what they do. An entry with an empty
/// description is a heading.
pub fn help_list(tui: &mut Tui, title: &str, entries: &[(&str, &str)]) -> io::Result<()> {
    let width = entries.iter().map(|(k, _)| str_width(k)).max().unwrap_or(0);
    let items: Vec<String> = entries
        .iter()
        .map(|(key, what)| {
            if what.is_empty() && !key.is_empty() {
                format!("--- {key} ---")
            } else if key.is_empty() {
                String::new()
            } else {
                format!("{}  {what}", fit(key, width))
            }
        })
        .collect();
    menu(tui, title, &items, 0)?;
    Ok(())
}

/// Asks for one line of text, offering and extending a history list.
pub fn prompt_history(
    tui: &mut Tui,
    db: &Rc<History>,
    kind: Kind,
    title: &str,
    label: &str,
    value: &str,
) -> io::Result<Option<String>> {
    let mut form = Form::new(vec![Field::with_history(label, value, kind)]).with_db(db);
    if !form.run(tui, title)? {
        return Ok(None);
    }
    let text = form.text(0);
    db.add(kind, &text);
    Ok(Some(text))
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
    fn button_letters_need_no_modifier_or_alt() {
        let buttons = ["Delete", "All", "Skip", "Skip all", "Cancel"];
        let key = |c, mods| Key {
            code: KeyCode::Char(c),
            mods,
        };
        assert_eq!(button_for_key(&buttons, key('a', NONE)), Some(1));
        assert_eq!(button_for_key(&buttons, key('A', NONE)), Some(1));
        assert_eq!(button_for_key(&buttons, key('a', ALT)), Some(1));
        assert_eq!(button_for_key(&buttons, key('a', CTRL)), None);
        assert_eq!(button_for_key(&buttons, key('d', CTRL)), None);
        // Two buttons share this letter.
        assert_eq!(button_for_key(&buttons, key('s', NONE)), None);
        assert_eq!(button_for_key(&buttons, key('x', NONE)), None);
    }

    #[test]
    fn labels_give_their_hotkey_and_hide_the_marker() {
        assert_eq!(parse_label("R&egular"), ("Regular".to_string(), Some('e')));
        assert_eq!(parse_label("Plain"), ("Plain".to_string(), None));
    }

    #[test]
    fn form_draws_options_and_buttons_inside_small_screens() {
        let form = Form::new(vec![
            Field::text("&Search for", "x"),
            Field::check("&Case sensitive", true),
        ])
        .with_buttons(&["Find next", "&All"]);
        let mut s = Screen::new(80, 20);
        form.draw(&mut s, "Search", 0);
        let all: String = (0..20).map(|y| s.row_text(y)).collect();
        assert!(all.contains("Search for"));
        assert!(all.contains("[x] Case sensitive"));
        assert!(all.contains("[ Find next ]"));
        assert!(all.contains("[ All ]"));
        assert!(!all.contains('&'));
        form.draw(&mut Screen::new(10, 3), "Search", 0);
    }

    #[test]
    fn buttons_that_do_not_fit_wrap_to_another_row() {
        let form = Form::new(vec![]).with_buttons(&["aaaaaa", "bbbbbb", "cccccc"]);
        assert_eq!(form.button_rows(40), vec![vec![0, 1, 2]]);
        assert_eq!(form.button_rows(24), vec![vec![0, 1], vec![2]]);
        assert_eq!(form.button_rows(5), vec![vec![0], vec![1], vec![2]]);
    }

    #[test]
    fn focus_steps_through_fields_and_buttons_and_skips_labels() {
        let form = Form::new(vec![Field::Label("x".into()), Field::text("t", "")])
            .with_buttons(&["A", "B"]);
        assert_eq!(form.step(1, 1), 2);
        assert_eq!(form.step(3, 1), 1);
        assert_eq!(form.step(1, -1), 3);
    }

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
