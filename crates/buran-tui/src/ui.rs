//! The ratatui chat view: scrollable transcript, input line, permission
//! modal, status line. Pure rendering plus key-to-action mapping; the
//! event loop in `main.rs` owns terminal setup/teardown and the client.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::acp_client::{Answer, PermissionQuestion};

/// How the transcript is split into entries.
pub enum Entry {
    /// One user input line, echoed verbatim.
    User(String),
    /// Assistant text, possibly many chunks concatenated into one entry.
    Assistant(String),
    /// A permission decision the user made on a tool call.
    Permission { title: String, outcome: String },
    /// A tool call the agent made (transparency for allow_all: no modal
    /// asks, so the transcript is the only place the calls are visible).
    ToolCall {
        title: String,
        status: Option<String>,
    },
}

/// What a key press means to the app loop.
pub enum Action {
    /// Append the typed line to the transcript and send it.
    Submit(String),
    /// Quit the app.
    Quit,
    /// Open the session picker (Ctrl-R).
    OpenPicker,
    /// User picked a session in the picker; the loop switches.
    SwitchSession(String),
    /// Nothing (consumed by the view: typing, scrolling, modal answers).
    None,
}

/// The session picker overlay. Opened with Ctrl-R when no turn is in
/// flight and no permission modal is up; `rows`/`hidden` come from
/// [`crate::sessions::catalog`].
#[derive(Default)]
pub struct Picker {
    pub rows: Vec<crate::sessions::SessionRow>,
    pub hidden: usize,
    pub selected: usize,
}

impl Picker {
    /// Moves the selection; clamped, wrap-free (the list is short and a
    /// runaway cursor at either end is clearer than a surprise wrap).
    pub fn move_selection(&mut self, delta: i32) {
        let len = self.rows.len();
        if len == 0 {
            return;
        }
        let current = self.selected as i32;
        self.selected = current.saturating_add(delta).clamp(0, len as i32 - 1) as usize;
    }
}

/// One row of the slash-completion menu: the protocol's AvailableCommand
/// is flattened here so the view never reaches into the SDK types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandRow {
    pub name: String,
    pub description: String,
}

/// Whole UI state: transcript, input buffer + cursor, pending permission
/// question, scroll offset (`None` = stick to bottom), turn status, the
/// picker overlay when open, and the command registry + menu state.
#[derive(Default)]
pub struct App {
    pub entries: Vec<Entry>,
    pub input: String,
    /// Cursor position within `input` as a byte offset. Invariant: always
    /// on a char boundary — every mutation moves it by whole chars, so
    /// slicing `input[..cursor]` is safe.
    pub cursor: usize,
    /// `Some` while the agent awaits a permission answer.
    pub question: Option<PermissionQuestion>,
    /// Manual scroll offset from the bottom; `None` follows the tail.
    pub scroll: Option<u16>,
    pub streaming: bool,
    pub picker: Option<Picker>,
    /// Slash commands from the agent's announcement (empty until one
    /// arrives: an old agent = no menu, '/' just types).
    pub commands: Vec<CommandRow>,
    /// Menu selection while open; `None` = closed.
    pub menu_selected: Option<usize>,
}

impl App {
    /// Rows filtered by the current `/query` prefix — the render path
    /// and the navigation bound. Empty while the menu is closed or the
    /// input is not a `/query`.
    pub fn menu_rows_filtered(&self) -> Vec<CommandRow> {
        if self.menu_selected.is_none() {
            return Vec::new();
        }
        let Some(query) = self.input.strip_prefix('/') else {
            return Vec::new();
        };
        self.commands
            .iter()
            .filter(|c| c.name.starts_with(query))
            .cloned()
            .collect()
    }
}

impl App {
    /// Appends assistant text: joins onto the trailing assistant entry so
    /// streamed chunks read as one message.
    pub fn append_assistant(&mut self, chunk: &str) {
        match self.entries.last_mut() {
            Some(Entry::Assistant(text)) => text.push_str(chunk),
            _ => self.entries.push(Entry::Assistant(chunk.to_string())),
        }
        self.scroll = None;
    }

    /// Adds/updates a tool-call entry: a fresh entry on start, status
    /// appended to the LAST matching entry on completion.
    pub fn push_tool_call(&mut self, title: &str, status: Option<String>) {
        match status {
            None => {
                self.entries.push(Entry::ToolCall {
                    title: title.to_string(),
                    status: None,
                });
            }
            Some(status) => {
                // Find the last matching open entry (matching by title is
                // enough for v1: parallel same-name calls render as one).
                for entry in self.entries.iter_mut().rev() {
                    if let Entry::ToolCall {
                        title: t,
                        status: s,
                    } = entry
                        && t == title
                        && s.is_none()
                    {
                        *s = Some(status);
                        self.scroll = None;
                        return;
                    }
                }
                // No open entry (out-of-order update): append as resolved.
                self.entries.push(Entry::ToolCall {
                    title: title.to_string(),
                    status: Some(status),
                });
            }
        }
        self.scroll = None;
    }

    pub fn push_user(&mut self, line: &str) {
        self.entries.push(Entry::User(line.to_string()));
        self.scroll = None;
    }

    fn push_permission(&mut self, question: &PermissionQuestion, outcome: &str) {
        self.entries.push(Entry::Permission {
            title: question.title().to_string(),
            outcome: outcome.to_string(),
        });
        self.scroll = None;
    }

    /// Inserts a char at the cursor and moves the cursor after it.
    fn insert_char(&mut self, ch: char) {
        let at = self.cursor.min(self.input.len());
        self.input.insert(at, ch);
        self.cursor = at + ch.len_utf8();
    }

    /// Deletes the char before the cursor (Backspace semantics); no-op at
    /// the start of the line.
    fn delete_before_cursor(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let prev = self.input[..self.cursor]
            .chars()
            .next_back()
            .expect("cursor > 0 means a char exists before it");
        let start = self.cursor - prev.len_utf8();
        self.input.replace_range(start..self.cursor, "");
        self.cursor = start;
    }

    /// Moves the cursor one char left; no-op at the line start.
    fn cursor_left(&mut self) {
        if let Some(prev) = self.input[..self.cursor].chars().next_back() {
            self.cursor -= prev.len_utf8();
        }
    }

    /// Moves the cursor one char right; no-op at the line end.
    fn cursor_right(&mut self) {
        if let Some(next) = self.input[self.cursor..].chars().next() {
            self.cursor += next.len_utf8();
        }
    }

    /// Clears the input after submit: buffer and cursor reset together —
    /// a stale cursor past an empty buffer would break the invariant.
    fn take_input(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.input)
    }

    /// Maps a key press to an action. While the permission modal is up it
    /// captures y/n/Esc; otherwise input keys edit the buffer.
    pub fn on_key(&mut self, key: KeyEvent) -> Action {
        if let Some(question) = self.question.as_mut() {
            let outcome = match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    question.answer(Answer::Allow);
                    "allowed"
                }
                KeyCode::Char('n') | KeyCode::Char('N') => {
                    question.answer(Answer::Reject);
                    "rejected"
                }
                KeyCode::Esc => {
                    question.answer(Answer::Cancel);
                    "cancelled"
                }
                _ => return Action::None,
            };
            let question = self.question.take().expect("checked Some above");
            self.push_permission(&question, outcome);
            return Action::None;
        }

        // The picker captures the keyboard while open: navigation and
        // confirm/cancel are its keys; everything else is dropped so a
        // stray character cannot leak into the input buffer of a session
        // the user is about to leave.
        if let Some(picker) = self.picker.as_mut() {
            return match key.code {
                KeyCode::Up => {
                    picker.move_selection(-1);
                    Action::None
                }
                KeyCode::Down => {
                    picker.move_selection(1);
                    Action::None
                }
                KeyCode::Enter => {
                    let picker = self.picker.take().expect("checked Some above");
                    match picker.rows.into_iter().nth(picker.selected) {
                        Some(row) => Action::SwitchSession(row.id),
                        None => Action::None,
                    }
                }
                KeyCode::Esc => {
                    self.picker = None;
                    Action::None
                }
                _ => Action::None,
            };
        }

        match (key.modifiers, key.code) {
            (m, KeyCode::Char('c')) if m.contains(KeyModifiers::CONTROL) => Action::Quit,
            (m, KeyCode::Char('q')) if m.contains(KeyModifiers::CONTROL) => Action::Quit,
            // Picker opens only when idle: a switch mid-turn would race the
            // in-flight prompt (and ACP prompt/stop semantics are not worth
            // untangling for v1 of the picker). The status line says so.
            (m, KeyCode::Char('r')) if m.contains(KeyModifiers::CONTROL) => {
                if self.streaming {
                    return Action::None;
                }
                Action::OpenPicker
            }
            // Slash menu: '/' opens it (empty query = all commands), Tab
            // completes the selected row into the input, Esc closes. The
            // menu only ever shows rows matching the current `/query`, so
            // Enter falls through to submit — a narrowed menu submits the
            // completed command line as-is.
            (_, KeyCode::Char('/'))
                if self.menu_selected.is_none()
                    && self.input.is_empty()
                    && !self.commands.is_empty() =>
            {
                self.insert_char('/');
                self.menu_selected = Some(0);
                Action::None
            }
            (_, KeyCode::Char(ch)) if self.menu_selected.is_some() => {
                self.insert_char(ch);
                // Typing narrows the rows; keep the selection in range.
                let rows = self.menu_rows_filtered().len();
                if let Some(selected) = self.menu_selected.as_mut() {
                    *selected = (*selected).min(rows.saturating_sub(1));
                }
                // A query that matches nothing closes the menu.
                if rows == 0 {
                    self.menu_selected = None;
                }
                Action::None
            }
            (_, KeyCode::Tab) if self.menu_selected.is_some() => {
                let rows = self.menu_rows_filtered();
                if let Some(selected) = self.menu_selected
                    && let Some(row) = rows.get(selected)
                {
                    self.input = format!("/{}", row.name);
                    self.cursor = self.input.len();
                }
                self.menu_selected = None;
                Action::None
            }
            (_, KeyCode::Up) if self.menu_selected.is_some() => {
                if let Some(selected) = self.menu_selected.as_mut() {
                    *selected = selected.saturating_sub(1);
                }
                Action::None
            }
            (_, KeyCode::Down) if self.menu_selected.is_some() => {
                let rows = self.menu_rows_filtered().len();
                if let Some(selected) = self.menu_selected.as_mut() {
                    *selected = (*selected + 1).min(rows.saturating_sub(1));
                }
                Action::None
            }
            (_, KeyCode::Esc) if self.menu_selected.is_some() => {
                self.menu_selected = None;
                Action::None
            }
            (_, KeyCode::Backspace) if self.menu_selected.is_some() => {
                self.delete_before_cursor();
                if !self.input.starts_with('/') {
                    self.menu_selected = None;
                }
                Action::None
            }
            (_, KeyCode::Enter) => {
                let line = self.take_input();
                self.menu_selected = None;
                if line == ":quit" {
                    return Action::Quit;
                }
                if line.is_empty() {
                    return Action::None;
                }
                self.push_user(&line);
                Action::Submit(line)
            }
            (_, KeyCode::Backspace) => {
                self.delete_before_cursor();
                Action::None
            }
            // Arrow keys move the edit cursor within the input line; the
            // slash menu navigates with Up/Down only, so Left/Right stay
            // free for text editing even while the menu is open.
            (_, KeyCode::Left) => {
                self.cursor_left();
                Action::None
            }
            (_, KeyCode::Right) => {
                self.cursor_right();
                Action::None
            }
            // Home/End jump to the line boundaries (readline parity:
            // without them a multi-line typo fix means holding an arrow).
            (_, KeyCode::Home) => {
                self.cursor = 0;
                Action::None
            }
            (_, KeyCode::End) => {
                self.cursor = self.input.len();
                Action::None
            }
            // PageUp/PageDown scroll the transcript away from the tail;
            // newly appended content resets to follow mode.
            (m, KeyCode::PageUp) if m.is_empty() => {
                self.scroll = Some(self.scroll.unwrap_or(0).saturating_add(5));
                Action::None
            }
            (m, KeyCode::PageDown) if m.is_empty() => match self.scroll {
                Some(0) | None => {
                    self.scroll = None;
                    Action::None
                }
                Some(n) => {
                    self.scroll = Some(n.saturating_sub(5));
                    Action::None
                }
            },
            (_, KeyCode::Char(ch)) => {
                self.insert_char(ch);
                Action::None
            }
            _ => Action::None,
        }
    }
}

/// Renders one frame.
pub fn draw(frame: &mut Frame, app: &mut App) {
    // When the slash menu is open, reserve 3 lines for it (title + up to 2
    // rows) between status and input; otherwise the three-row layout is
    // unchanged.
    let menu_height = if app.menu_selected.is_some() { 3 } else { 0 };
    let [transcript, status, menu, input] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(menu_height),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    render_transcript(frame, app, transcript);
    render_status(frame, app, status);
    if menu_height > 0 {
        render_menu(frame, app, menu);
    }
    render_input(frame, app, input);
    if let Some(picker) = &app.picker {
        render_picker(frame, picker, frame.area());
    }
    if let Some(question) = &app.question {
        render_modal(frame, question, frame.area());
    }
}

fn render_transcript(frame: &mut Frame, app: &App, area: Rect) {
    let logical = transcript_lines(app);
    // Hard-wrap BEFORE rendering: the scroll offset counts VISUAL rows, and
    // the widget's own word-wrap would produce more rows than we count —
    // follow mode would stop short of the true bottom and the newest line
    // (the `[EndTurn]` marker, streamed tail) would sit below the pane,
    // invisible. Pre-wrapping here makes the row count exact, so offset
    // `visual_count - height` always lands on the real end. The Paragraph
    // therefore renders without `Wrap` — content is already wrapped.
    let lines = wrap_lines(logical, area.width);
    // Follow mode (no manual scroll): offset = visual_rows - height, so the
    // last row sits at the pane bottom. NOT `u16::MAX` — ratatui 0.30 walks
    // `next_line()` per offset row and early-returns once past the content,
    // so an oversized offset renders an EMPTY pane (an earlier regression).
    // Clamping to the real count keeps follow mode correct for any size.
    let row_count = lines.len().min(u16::MAX as usize) as u16;
    let offset = app.scroll.unwrap_or(row_count.saturating_sub(area.height));
    let paragraph = Paragraph::new(lines).scroll((offset, 0));
    frame.render_widget(paragraph, area);
}

/// Hard-wraps logical lines into visual rows of at most `width` cells
/// (char-count based, no word breaking): splitting is deterministic, so
/// the row count the scroll offset uses is the row count rendered. Wide
/// (CJK) glyphs count as one cell here — the wrap point can then exceed
/// the pane by a cell on such lines, an accepted approximation (the
/// alternative is unicode-width, another dependency for one call site).
fn wrap_lines(lines: Vec<Line<'static>>, width: u16) -> Vec<Line<'static>> {
    if width == 0 {
        return lines;
    }
    let width = width as usize;
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;

    // Closes the row under construction. `current` is taken even when
    // empty: an empty logical line must still occupy one visual row, or
    // paragraph spacing collapses and the offset drifts again.
    fn flush(out: &mut Vec<Line<'static>>, current: &mut Vec<Span<'static>>, used: &mut usize) {
        out.push(Line::from(std::mem::take(current)));
        *used = 0;
    }

    for line in lines {
        for span in line.spans {
            let style = span.style;
            for ch in span.content.chars() {
                if used == width {
                    flush(&mut out, &mut current, &mut used);
                }
                match current.last_mut() {
                    // Adjacent runs of the same style merge back into one
                    // span, keeping the row list small on long paragraphs.
                    // `Cow::to_mut` materializes the borrowed string once;
                    // further pushes then hit the owned buffer.
                    Some(last) if last.style == style => {
                        last.content.to_mut().push(ch);
                    }
                    _ => current.push(Span::styled(ch.to_string(), style)),
                }
                used += 1;
            }
        }
        flush(&mut out, &mut current, &mut used);
    }
    out
}

fn transcript_lines(app: &App) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for entry in &app.entries {
        match entry {
            Entry::User(text) => lines.push(Line::from(Span::styled(
                format!("you > {text}"),
                Style::default().fg(Color::Cyan),
            ))),
            Entry::Assistant(text) => {
                for line in text.lines() {
                    lines.push(Line::from(Span::raw(line.to_string())));
                }
            }
            Entry::ToolCall { title, status } => {
                let suffix = match status {
                    Some(s) => format!(" [{s}]"),
                    None => " [running]".to_string(),
                };
                lines.push(Line::from(Span::styled(
                    format!("tool > {title}{suffix}"),
                    Style::default().fg(Color::DarkGray),
                )));
            }
            Entry::Permission { title, outcome } => {
                let color = match outcome.as_str() {
                    "allowed" => Color::Green,
                    _ => Color::Red,
                };
                lines.push(Line::from(Span::styled(
                    format!("tool > {title} [{outcome}]"),
                    Style::default().fg(color),
                )));
            }
        }
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "buran-tui — type a message, Enter to send; :quit or Ctrl+C exits",
            Style::default().fg(Color::DarkGray),
        )));
    }
    lines
}

fn render_status(frame: &mut Frame, app: &App, area: Rect) {
    let state = if app.question.is_some() {
        Span::styled(
            " permission? ",
            Style::default().bg(Color::Yellow).fg(Color::Black),
        )
    } else if app.streaming {
        Span::styled(
            " streaming… (Ctrl-R waits for the turn to end) ",
            Style::default().bg(Color::Blue).fg(Color::White),
        )
    } else {
        Span::styled(
            " ready (Ctrl-R: sessions) ",
            Style::default().fg(Color::DarkGray),
        )
    };
    frame.render_widget(Line::from(vec![state]), area);
}

fn render_input(frame: &mut Frame, app: &App, area: Rect) {
    let blocked = app.question.is_some();
    let style = if blocked {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    };
    let mut spans = vec![Span::styled("you: ", Style::default().fg(Color::Cyan))];
    if blocked {
        // The modal owns the keyboard; no edit cursor is shown.
        spans.push(Span::styled(String::new(), style));
    } else {
        // The cursor is drawn at its true position: reversed highlight on
        // the char under it, or a caret block past the end of the line.
        let cursor = app.cursor.min(app.input.len());
        spans.push(Span::styled(app.input[..cursor].to_string(), style));
        match app.input[cursor..].chars().next() {
            Some(ch) => {
                spans.push(Span::styled(
                    ch.to_string(),
                    style.add_modifier(Modifier::REVERSED),
                ));
                spans.push(Span::styled(
                    app.input[cursor + ch.len_utf8()..].to_string(),
                    style,
                ));
            }
            None => {
                spans.push(Span::styled("▏", Style::default().fg(Color::Cyan)));
            }
        }
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_picker(frame: &mut Frame, picker: &Picker, area: Rect) {
    // 3/4 width, up to 12 rows + footer + 2 border lines, centered.
    let width = (area.width * 3 / 4).max(40).min(area.width);
    let list_height = picker.rows.len().min(12) as u16;
    let height = (list_height + 3).min(area.height);
    let box_area = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" sessions — Ctrl-R, Enter opens, Esc closes ")
        .border_style(Style::default().fg(Color::Cyan));

    // The picker draws its own inner lines so the footer (hidden count)
    // sits below the list inside one bordered box.
    let mut lines: Vec<Line> = Vec::new();
    if picker.rows.is_empty() {
        lines.push(Line::from(Span::styled(
            "no sessions in this directory",
            Style::default().fg(Color::DarkGray),
        )));
    }
    for (i, row) in picker.rows.iter().enumerate() {
        let marker = if i == picker.selected { " > " } else { "   " };
        let age = crate::sessions::relative_age(&row.started_at, jiff::Timestamp::now());
        // cwd shortened to its last component: the picker is filtered to
        // one directory anyway, and the full path eats the title.
        let dir = row
            .cwd
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| row.cwd.display().to_string());
        let title = row.title.clone().unwrap_or_else(|| "(empty)".into());
        let style = if i == picker.selected {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        lines.push(Line::from(Span::styled(
            format!("{marker}{age:>4}  {dir}  {title}"),
            style,
        )));
    }
    if picker.hidden > 0 {
        lines.push(Line::from(Span::styled(
            format!(
                "  {} more in other directories (resume there to see them)",
                picker.hidden
            ),
            Style::default().fg(Color::DarkGray),
        )));
    }
    frame.render_widget(Paragraph::new(lines).block(block), box_area);
}

/// The slash-completion menu: up to two rows (name + description), the
/// selected one highlighted. Drawn in the reserved band between status
/// and input — an inline strip, not an overlay, so the transcript never
/// hides behind it.
fn render_menu(frame: &mut Frame, app: &App, area: Rect) {
    let rows = app.menu_rows_filtered();
    let selected = app
        .menu_selected
        .unwrap_or(0)
        .min(rows.len().saturating_sub(1));
    let mut lines: Vec<Line> = Vec::new();
    for (i, row) in rows.iter().take(2).enumerate() {
        let style = if i == selected {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let marker = if i == selected { " > " } else { "   " };
        lines.push(Line::from(Span::styled(
            format!("{marker}/{}  {}", row.name, row.description),
            style,
        )));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_modal(frame: &mut Frame, question: &PermissionQuestion, area: Rect) {
    // 60% width, centered, at most 7 lines tall.
    let width = (area.width * 3 / 5).max(30).min(area.width);
    let height = 7.min(area.height);
    let box_area = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" permission required ")
        .border_style(Style::default().fg(Color::Yellow));
    let body = format!(
        "{}\n\n[y] allow   [n] reject   [Esc] cancel",
        question.title()
    );
    frame.render_widget(Paragraph::new(body).block(block), box_area);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn left_right_move_cursor_within_input() {
        let mut app = App::default();
        for ch in "abc".chars() {
            app.on_key(key(KeyCode::Char(ch)));
        }
        assert_eq!(app.input, "abc");
        assert_eq!(app.cursor, 3);

        app.on_key(key(KeyCode::Left));
        app.on_key(key(KeyCode::Left));
        assert_eq!(app.cursor, 1);

        app.on_key(key(KeyCode::Char('X')));
        assert_eq!(app.input, "aXbc");
        assert_eq!(app.cursor, 2);

        // Clamped at both ends: no panic, no wraparound.
        app.on_key(key(KeyCode::Left));
        app.on_key(key(KeyCode::Left));
        app.on_key(key(KeyCode::Left));
        assert_eq!(app.cursor, 0);
        app.on_key(key(KeyCode::Right));
        app.on_key(key(KeyCode::Right));
        app.on_key(key(KeyCode::Right));
        app.on_key(key(KeyCode::Right));
        assert_eq!(app.cursor, app.input.len());
    }

    #[test]
    fn backspace_deletes_before_cursor() {
        let mut app = App::default();
        for ch in "ab".chars() {
            app.on_key(key(KeyCode::Char(ch)));
        }
        app.on_key(key(KeyCode::Left));
        app.on_key(key(KeyCode::Backspace));
        assert_eq!(app.input, "b");
        assert_eq!(app.cursor, 0);
        // At the start of the line Backspace is a no-op.
        app.on_key(key(KeyCode::Backspace));
        assert_eq!(app.input, "b");
        assert_eq!(app.cursor, 0);
    }

    #[test]
    fn cursor_is_multibyte_safe() {
        let mut app = App::default();
        for ch in "абв".chars() {
            app.on_key(key(KeyCode::Char(ch)));
        }
        assert_eq!(app.cursor, 6); // 3 chars × 2 bytes
        app.on_key(key(KeyCode::Left));
        assert_eq!(app.cursor, 4);
        app.on_key(key(KeyCode::Backspace));
        assert_eq!(app.input, "ав");
        assert_eq!(app.cursor, 2);
        app.on_key(key(KeyCode::Char('ы')));
        assert_eq!(app.input, "аыв");
        assert_eq!(app.cursor, 4);
    }

    #[test]
    fn submit_resets_cursor_with_buffer() {
        let mut app = App::default();
        for ch in "hi".chars() {
            app.on_key(key(KeyCode::Char(ch)));
        }
        app.on_key(key(KeyCode::Left));
        match app.on_key(key(KeyCode::Enter)) {
            Action::Submit(line) => assert_eq!(line, "hi"),
            _ => panic!("expected Submit"),
        }
        assert_eq!(app.input, "");
        assert_eq!(app.cursor, 0);
    }

    #[test]
    fn home_end_jump_to_boundaries() {
        let mut app = App::default();
        for ch in "xyz".chars() {
            app.on_key(key(KeyCode::Char(ch)));
        }
        app.on_key(key(KeyCode::Home));
        assert_eq!(app.cursor, 0);
        app.on_key(key(KeyCode::Char('a')));
        assert_eq!(app.input, "axyz");
        app.on_key(key(KeyCode::End));
        assert_eq!(app.cursor, app.input.len());
    }

    #[test]
    fn wrap_lines_respects_width_and_preserves_styles() {
        let logical = vec![Line::from(Span::styled(
            "abcdefghij",
            Style::default().fg(Color::Cyan),
        ))];
        let rows = wrap_lines(logical, 4);
        assert_eq!(rows.len(), 3, "10 cells into width 4 wraps to 3 rows");
        let text: String = rows
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert_eq!(text, "abcdefghij");
        // Style survives the split: every fragment keeps the color.
        assert!(
            rows.iter()
                .all(|l| l.spans.iter().all(|s| s.style.fg == Some(Color::Cyan)))
        );
    }

    #[test]
    fn wrap_lines_keeps_empty_lines_as_rows() {
        let logical = vec![Line::from("first"), Line::from(""), Line::from("third")];
        let rows = wrap_lines(logical, 10);
        assert_eq!(rows.len(), 3, "an empty logical line is one visual row");
    }
}
