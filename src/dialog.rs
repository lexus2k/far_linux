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
        layout: TextLayout,
    },
    /// A line across the dialog between groups, as Far draws them.
    Separator,
    /// Radio buttons on one row after a label: `Label: (•) a ( ) b`. Left and
    /// Right choose.
    Radio {
        label: String,
        options: Vec<String>,
        chosen: usize,
    },
    Check {
        label: String,
        on: bool,
    },
    /// A row of checkboxes after a label, as in Far's attributes dialog. A box
    /// can be undecided, `None`, shown as `[?]`, when the items it describes
    /// differ. Left and Right move between the boxes.
    Boxes {
        label: String,
        names: Vec<String>,
        states: Vec<Option<bool>>,
        three: Vec<bool>,
        col: usize,
    },
}

/// Where a text box goes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TextLayout {
    /// The label on its own row, the box under it.
    #[default]
    Below,
    /// The label and the box on one row.
    Row,
    /// Label and box on the right half of the row of the field before it,
    /// such as a size beside its checkbox.
    Beside,
}

/// Width of the label column of `Field::Boxes` rows.
const BOXES_LABEL: usize = 8;

/// Width given to each checkbox name in a `Field::Boxes` row, so that the
/// boxes of rows below each other line up.
const BOX_NAME: usize = 7;

/// `text` cut or padded with spaces to `width` columns.
fn pad(text: &str, width: usize) -> String {
    let shown = fit_left(text, width);
    let fill = width.saturating_sub(str_width(&shown));
    format!("{shown}{}", " ".repeat(fill))
}

/// Next state of a checkbox. One that may be undecided goes through `[?]`.
fn next_state(state: Option<bool>, three: bool) -> Option<bool> {
    match state {
        Some(false) => Some(true),
        Some(true) if three => None,
        Some(true) => Some(false),
        None => Some(false),
    }
}

impl Field {
    /// A labelled text box with prefilled content.
    pub fn text(label: &str, value: &str) -> Field {
        Field::Text {
            label: label.to_string(),
            edit: LineEdit::prefilled(value),
            history: None,
            layout: TextLayout::Below,
        }
    }

    /// A text box with a history list on Ctrl+Up and Ctrl+Down.
    pub fn with_history(label: &str, value: &str, kind: Kind) -> Field {
        Field::Text {
            label: label.to_string(),
            edit: LineEdit::prefilled(value),
            history: Some(kind),
            layout: TextLayout::Below,
        }
    }

    /// A labelled checkbox.
    pub fn check(label: &str, on: bool) -> Field {
        Field::Check {
            label: label.to_string(),
            on,
        }
    }

    /// A row of checkboxes. A box whose state is `None` can always return to
    /// it; with `three`, every box can be made undecided.
    pub fn boxes(label: &str, names: &[&str], states: &[Option<bool>], three: bool) -> Field {
        Field::Boxes {
            label: label.to_string(),
            names: names.iter().map(|n| n.to_string()).collect(),
            states: states.to_vec(),
            three: states.iter().map(|s| three || s.is_none()).collect(),
            col: 0,
        }
    }

    /// Puts a text box on one row with its label, or with `beside`, on the
    /// right half of the row of the field before it.
    pub fn laid_out(mut self, how: TextLayout) -> Field {
        if let Field::Text { layout, .. } = &mut self {
            *layout = how;
        }
        self
    }

    pub fn radio(label: &str, options: &[&str], chosen: usize) -> Field {
        Field::Radio {
            label: label.to_string(),
            options: options.iter().map(|o| o.to_string()).collect(),
            chosen: chosen.min(options.len().saturating_sub(1)),
        }
    }

    fn focusable(&self) -> bool {
        !matches!(self, Field::Label(_) | Field::Separator)
    }

    fn height(&self) -> u16 {
        match self {
            Field::Text { layout, .. } => match layout {
                TextLayout::Below => 2,
                TextLayout::Row => 1,
                TextLayout::Beside => 0,
            },
            _ => 1,
        }
    }

    fn hotkey(&self) -> Option<char> {
        match self {
            Field::Text { label, .. } | Field::Check { label, .. } | Field::Radio { label, .. } => {
                parse_label(label).1
            }
            Field::Label(_) | Field::Boxes { .. } | Field::Separator => None,
        }
    }
}

/// A dialog of text boxes, checkboxes and optional buttons.
/// Called after each key with the fields and the index of the focused one, so
/// that fields can be kept in step with each other.
type OnChange = Box<dyn FnMut(&mut [Field], usize)>;

pub struct Form {
    pub fields: Vec<Field>,
    buttons: Vec<String>,
    db: Option<Rc<History>>,
    on_change: Option<OnChange>,
    /// Far's AutoComplete: earlier entries matching what is typed in a text
    /// box with a history, shown as a list under it.
    suggest: Option<Suggest>,
}

/// The AutoComplete list of the text box `field`.
struct Suggest {
    field: usize,
    items: Vec<String>,
    /// The highlighted item. None until Down is pressed, so that Enter still
    /// confirms the dialog.
    chosen: Option<usize>,
}

/// Most entries the AutoComplete list shows.
const SUGGEST_ROWS: usize = 8;

/// History entries that start with `typed`, ignoring case, newest first, as
/// Far's AutoComplete lists them. Nothing for empty text, or when the only
/// match is the text itself.
pub fn suggestions(history: &[String], typed: &str) -> Vec<String> {
    if typed.is_empty() {
        return Vec::new();
    }
    let lower = typed.to_lowercase();
    let found: Vec<String> = history
        .iter()
        .filter(|h| h.to_lowercase().starts_with(&lower) && h.as_str() != typed)
        .cloned()
        .collect();
    found
}

impl Form {
    pub fn new(fields: Vec<Field>) -> Self {
        Form {
            fields,
            buttons: Vec::new(),
            db: None,
            on_change: None,
            suggest: None,
        }
    }

    /// Updates the AutoComplete list after the text of `focus` may have
    /// changed.
    fn refresh_suggest(&mut self, focus: usize) {
        let Some(db) = &self.db else {
            return;
        };
        let Some(Field::Text {
            edit,
            history: Some(kind),
            ..
        }) = self.fields.get(focus)
        else {
            self.suggest = None;
            return;
        };
        let items = suggestions(&db.list(*kind), &edit.text());
        self.suggest = (!items.is_empty()).then_some(Suggest {
            field: focus,
            items,
            chosen: None,
        });
    }

    /// Keys for an open AutoComplete list. Returns true when the key was used.
    fn suggest_key(&mut self, key: Key) -> bool {
        let Some(list) = &mut self.suggest else {
            return false;
        };
        let last = list.items.len().min(SUGGEST_ROWS).saturating_sub(1);
        match (key.code, key.mods) {
            (KeyCode::Down, NONE) => {
                list.chosen = Some(list.chosen.map_or(0, |c| (c + 1).min(last)));
            }
            (KeyCode::Up, NONE) => list.chosen = list.chosen.and_then(|c| c.checked_sub(1)),
            (KeyCode::Esc, _) => self.suggest = None,
            (KeyCode::Enter, _) => {
                let Some(chosen) = list.chosen else {
                    return false;
                };
                let (field, value) = (list.field, list.items[chosen].clone());
                if let Some(Field::Text { edit, .. }) = self.fields.get_mut(field) {
                    edit.set(&value);
                }
                self.suggest = None;
            }
            (KeyCode::Delete, SHIFT) => {
                let Some(chosen) = list.chosen else {
                    return false;
                };
                let field = list.field;
                let value = list.items[chosen].clone();
                if let (
                    Some(db),
                    Some(Field::Text {
                        history: Some(kind),
                        ..
                    }),
                ) = (&self.db, self.fields.get(field))
                {
                    db.remove(*kind, &value);
                }
                self.refresh_suggest(field);
            }
            _ => return false,
        }
        true
    }

    /// Ctrl+End: completes the text with the first match, as Far does when
    /// AutoComplete is off.
    fn complete_first(&mut self, focus: usize) {
        self.refresh_suggest(focus);
        let first = self.suggest.as_ref().map(|s| s.items[0].clone());
        if let (Some(value), Some(Field::Text { edit, .. })) = (first, self.fields.get_mut(focus)) {
            edit.set(&value);
        }
        self.suggest = None;
    }

    /// Runs `f` after each key, with the fields and the focused index.
    pub fn on_change(mut self, f: impl FnMut(&mut [Field], usize) + 'static) -> Self {
        self.on_change = Some(Box::new(f));
        self
    }

    /// The chosen option of a row of radio buttons.
    pub fn chosen(&self, index: usize) -> usize {
        match &self.fields[index] {
            Field::Radio { chosen, .. } => *chosen,
            _ => 0,
        }
    }

    /// States of a row of checkboxes.
    pub fn boxes(&self, index: usize) -> Vec<Option<bool>> {
        match &self.fields[index] {
            Field::Boxes { states, .. } => states.clone(),
            _ => Vec::new(),
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
                    self.changed(focus);
                    self.refresh_suggest(focus);
                    continue;
                }
                _ => continue,
            };
            if self.suggest.as_ref().is_some_and(|s| s.field == focus) && self.suggest_key(key) {
                continue;
            }
            if key.code == KeyCode::End && key.mods == CTRL {
                self.complete_first(focus);
                continue;
            }
            let before = self.text(focus.min(self.fields.len().saturating_sub(1)));
            let was = focus;
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
                (KeyCode::Down, _) | (KeyCode::Tab, NONE) => {
                    focus = self.step_keeping_column(focus, 1)
                }
                (KeyCode::Up, _) | (KeyCode::Tab, SHIFT) => {
                    focus = self.step_keeping_column(focus, -1)
                }
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
                    Some(Field::Radio {
                        options, chosen, ..
                    }) => match key.code {
                        KeyCode::Left => *chosen = chosen.saturating_sub(1),
                        KeyCode::Right => {
                            *chosen = (*chosen + 1).min(options.len().saturating_sub(1))
                        }
                        KeyCode::Home => *chosen = 0,
                        KeyCode::End => *chosen = options.len().saturating_sub(1),
                        _ => {}
                    },
                    Some(Field::Boxes {
                        names,
                        states,
                        three,
                        col,
                        ..
                    }) => match key.code {
                        KeyCode::Char(' ') => states[*col] = next_state(states[*col], three[*col]),
                        KeyCode::Left => *col = col.saturating_sub(1),
                        KeyCode::Right => *col = (*col + 1).min(names.len().saturating_sub(1)),
                        KeyCode::Home => *col = 0,
                        KeyCode::End => *col = names.len().saturating_sub(1),
                        _ => {}
                    },
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
            self.changed(focus);
            if focus != was {
                self.suggest = None;
            } else if focus < self.fields.len() && self.text(focus) != before {
                self.refresh_suggest(focus);
            }
        }
    }

    fn changed(&mut self, focus: usize) {
        if let Some(f) = &mut self.on_change {
            f(&mut self.fields, focus);
        }
    }

    /// Moves focus, and from one row of checkboxes to the next keeps the column,
    /// so that Up and Down move through a grid.
    fn step_keeping_column(&mut self, focus: usize, dir: isize) -> usize {
        let to = self.step(focus, dir);
        let from_col = match self.fields.get(focus) {
            Some(Field::Boxes { col, .. }) => Some(*col),
            _ => None,
        };
        if let (Some(c), Some(Field::Boxes { col, names, .. })) =
            (from_col, self.fields.get_mut(to))
        {
            *col = c.min(names.len().saturating_sub(1));
        }
        to
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
        if let (Some((x, y, w)), Some(list)) = (self.draw_form(s, title, focus), &self.suggest) {
            draw_suggest(s, x, y, w, list);
        }
    }

    /// Draws the form, returning where an open AutoComplete list goes.
    fn draw_form(&self, s: &mut Screen, title: &str, focus: usize) -> Option<(u16, u16, u16)> {
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
        let mut list_at = None;
        for (i, field) in self.fields.iter().enumerate() {
            match field {
                Field::Label(text) => {
                    s.put(x + 3, row, &fit_left(text, inner), theme::DIALOG);
                    row += 1;
                }
                Field::Text {
                    label,
                    edit,
                    layout,
                    ..
                } => {
                    let shown = parse_label(label).0;
                    // Where the box goes: under its label, after it on the
                    // same row, or on the right half of the row before.
                    let (box_x, box_row, box_w) = match layout {
                        TextLayout::Below => {
                            s.put(x + 3, row, &fit_left(&shown, inner), theme::DIALOG);
                            row += 2;
                            (x + 3, row - 1, inner)
                        }
                        TextLayout::Row | TextLayout::Beside => {
                            let (lx, ly, room) = if *layout == TextLayout::Row {
                                row += 1;
                                (x + 3, row - 1, inner)
                            } else {
                                let half = inner / 2;
                                (x + 3 + half as u16, row.saturating_sub(1), inner - half)
                            };
                            let label_w = str_width(&shown) + 1;
                            s.put(lx, ly, &fit_left(&shown, room), theme::DIALOG);
                            (lx + label_w as u16, ly, room.saturating_sub(label_w))
                        }
                    };
                    let (visible, cursor) = visible_text(edit, box_w);
                    s.put(box_x, box_row, &fit(&visible, box_w), theme::INPUT);
                    if i == focus {
                        s.cursor = Some((box_x + cursor as u16, box_row));
                    }
                    if self.suggest.as_ref().is_some_and(|l| l.field == i) {
                        list_at = Some((box_x, box_row + 1, box_w as u16));
                    }
                }
                Field::Separator => {
                    let line = format!("╟{}╢", "─".repeat(w.saturating_sub(4) as usize));
                    s.put(x + 1, row, &line, theme::DIALOG);
                    row += 1;
                }
                Field::Radio {
                    label,
                    options,
                    chosen,
                } => {
                    let shown = parse_label(label).0;
                    let mut bx = x + 3;
                    if !shown.is_empty() {
                        bx = s.put(bx, row, &shown, theme::DIALOG) + 1;
                    }
                    for (o, option) in options.iter().enumerate() {
                        let mark = if o == *chosen { '•' } else { ' ' };
                        let here = i == focus && o == *chosen;
                        let style = if here {
                            theme::DIALOG_FOCUS
                        } else {
                            theme::DIALOG
                        };
                        if here {
                            s.cursor = Some((bx + 1, row));
                        }
                        bx = s.put(bx, row, &format!("({mark}) {option}"), style) + 2;
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
                Field::Boxes {
                    label,
                    names,
                    states,
                    col,
                    ..
                } => {
                    let mut bx = s.put(x + 3, row, &pad(label, BOXES_LABEL), theme::DIALOG);
                    for (b, (name, state)) in names.iter().zip(states).enumerate() {
                        let mark = match state {
                            Some(true) => 'x',
                            Some(false) => ' ',
                            None => '?',
                        };
                        let here = i == focus && b == *col;
                        let style = if here {
                            theme::DIALOG_FOCUS
                        } else {
                            theme::DIALOG
                        };
                        if here {
                            s.cursor = Some((bx + 2, row));
                        }
                        let shown = pad(name, BOX_NAME);
                        bx = s.put(bx + 1, row, &format!("[{mark}] {shown}"), style) + 1;
                    }
                    row += 1;
                }
            }
        }
        if self.buttons.is_empty() {
            let hint = "[ OK: Enter ]  [ Cancel: Esc ]";
            let bx = x + w.saturating_sub(str_width(hint) as u16) / 2;
            s.put(bx, row, hint, theme::DIALOG);
            return list_at;
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
        list_at
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

/// Draws the AutoComplete list with its top left at `x`, `y`, as a framed
/// list over whatever is below the text box.
fn draw_suggest(s: &mut Screen, x: u16, y: u16, w: u16, list: &Suggest) {
    let shown = &list.items[..list.items.len().min(SUGGEST_ROWS)];
    let h = shown.len() as u16 + 2;
    // Above the box when there is no room below.
    let y = if y + h > s.h && y > h + 1 {
        y - h - 1
    } else {
        y
    };
    s.fill(x, y, w, h, ' ', theme::DIALOG);
    s.frame(x, y, w, h, false, theme::DIALOG);
    let inner = w.saturating_sub(2) as usize;
    for (i, item) in shown.iter().enumerate() {
        let style = if list.chosen == Some(i) {
            theme::DIALOG_FOCUS
        } else {
            theme::DIALOG
        };
        let text = fit_left(item, inner);
        let fill = inner.saturating_sub(str_width(&text));
        s.put(
            x + 1,
            y + 1 + i as u16,
            &format!("{text}{}", " ".repeat(fill)),
            style,
        );
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

    #[test]
    fn messages_are_safe_at_every_size() {
        let lines = vec!["x".repeat(300), "short".to_string()];
        for w in 1..50 {
            for h in 1..12 {
                let mut s = Screen::new(w, h);
                draw_message(&mut s, "Title", &lines, &["Yes", "No", "Cancel"], 1, true);
                draw_message(&mut s, "", &[], &[], 0, false);
            }
        }
    }

    #[test]
    fn forms_are_safe_at_every_size() {
        let form = Form::new(vec![
            Field::Label("A label".into()),
            Field::text("&Name:", &"x".repeat(200)),
            Field::check("&Option", true),
        ])
        .with_buttons(&["&OK", "&Cancel"]);
        for w in 1..50 {
            for h in 1..14 {
                let mut s = Screen::new(w, h);
                form.draw(&mut s, "Form title", 1);
                form.draw(&mut s, "Form title", 3);
            }
        }
    }

    #[test]
    fn suggestions_match_the_start_ignoring_case_newest_first() {
        let history: Vec<String> = ["*.log", "*.LOG.1", "*.txt", "*.l"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(suggestions(&history, "*.l"), ["*.log", "*.LOG.1"]);
        assert!(suggestions(&history, "").is_empty());
        assert!(suggestions(&history, "*.txt").is_empty(), "the text itself");
        assert!(suggestions(&history, "x").is_empty());
    }

    #[test]
    fn autocomplete_list_follows_typing_and_enter_takes_an_entry() {
        let db = Rc::new(History::in_memory());
        db.add(Kind::FindMask, "*.cpp");
        db.add(Kind::FindMask, "*.log");
        let mut form =
            Form::new(vec![Field::with_history("Mask:", "", Kind::FindMask)]).with_db(&db);
        let key = |code| Key { code, mods: NONE };
        if let Field::Text { edit, .. } = &mut form.fields[0] {
            edit.set("*.");
        }
        form.refresh_suggest(0);
        let list = form.suggest.as_ref().unwrap();
        assert_eq!(list.items, ["*.log", "*.cpp"]);
        // Enter with nothing chosen is left to the dialog, to confirm it.
        assert!(!form.suggest_key(key(KeyCode::Enter)));
        assert!(form.suggest_key(key(KeyCode::Down)));
        assert!(form.suggest_key(key(KeyCode::Down)));
        assert!(form.suggest_key(key(KeyCode::Enter)));
        assert_eq!(form.text(0), "*.cpp");
        assert!(form.suggest.is_none());
        // Shift+Del removes the chosen entry from the history.
        form.fields[0] = Field::with_history("Mask:", "*.", Kind::FindMask);
        form.refresh_suggest(0);
        form.suggest_key(key(KeyCode::Down));
        form.suggest_key(Key {
            code: KeyCode::Delete,
            mods: SHIFT,
        });
        assert_eq!(db.list(Kind::FindMask), ["*.cpp"]);
        assert_eq!(form.suggest.as_ref().unwrap().items, ["*.cpp"]);
        // Esc closes only the list.
        assert!(form.suggest_key(key(KeyCode::Esc)));
        assert!(form.suggest.is_none());
        // Ctrl+End completes from the first match.
        form.complete_first(0);
        assert_eq!(form.text(0), "*.cpp");
    }

    #[test]
    fn the_autocomplete_list_is_drawn_under_its_box() {
        let db = Rc::new(History::in_memory());
        db.add(Kind::FindMask, "*.log");
        let mut form =
            Form::new(vec![Field::with_history("Mask:", "*.", Kind::FindMask)]).with_db(&db);
        form.refresh_suggest(0);
        let mut s = Screen::new(80, 20);
        form.draw(&mut s, "Find", 0);
        let rows: Vec<String> = (0..s.h).map(|y| s.row_text(y)).collect();
        let input = rows
            .iter()
            .position(|r| r.contains("*.") && !r.contains("*.log"))
            .unwrap();
        assert!(rows[input + 2].contains("*.log"), "{rows:#?}");
    }

    #[test]
    fn radio_rows_separators_and_boxes_beside_a_checkbox() {
        let form = Form::new(vec![
            Field::Label("Select archiver:".to_string()),
            Field::radio("", &["zip", "7z"], 1),
            Field::Separator,
            Field::check("Split into volumes", false),
            Field::text("Volume size:", "100M").laid_out(TextLayout::Beside),
        ]);
        assert_eq!(form.chosen(1), 1);
        let mut s = Screen::new(80, 20);
        form.draw(&mut s, "Create archive", 1);
        let rows: Vec<String> = (0..s.h).map(|y| s.row_text(y)).collect();
        assert!(
            rows.iter().any(|r| r.contains("( ) zip  (•) 7z")),
            "{rows:#?}"
        );
        assert!(rows.iter().any(|r| r.contains("╟───")), "{rows:#?}");
        let split = rows
            .iter()
            .find(|r| r.contains("Split into volumes"))
            .unwrap();
        assert!(split.contains("Volume size: 100M"), "{rows:#?}");
    }
}
