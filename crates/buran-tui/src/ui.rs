//! The ratatui chat view: scrollable transcript, input line, permission
//! modal, status line. Pure rendering plus key-to-action mapping; the
//! event loop in `main.rs` owns terminal setup/teardown and the client.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

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

/// Whole UI state: transcript, input buffer, pending permission question,
/// scroll offset (`None` = stick to bottom), turn status, the picker
/// overlay when open, and the command registry + menu state.
#[derive(Default)]
pub struct App {
    pub entries: Vec<Entry>,
    pub input: String,
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
                self.input.push('/');
                self.menu_selected = Some(0);
                Action::None
            }
            (_, KeyCode::Char(ch)) if self.menu_selected.is_some() => {
                self.input.push(ch);
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
                self.input.pop();
                if !self.input.starts_with('/') {
                    self.menu_selected = None;
                }
                Action::None
            }
            (_, KeyCode::Enter) => {
                let line = std::mem::take(&mut self.input);
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
                self.input.pop();
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
                self.input.push(ch);
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
    let lines = transcript_lines(app);
    // Follow mode (no manual scroll): offset = lines - height, so the last
    // line sits at the pane bottom. NOT `u16::MAX` — ratatui 0.30 with
    // `Wrap` walks `next_line()` per offset row and early-returns once past
    // the content, so an oversized offset renders an EMPTY pane (the bug
    // this fixes: the transcript never appeared). Clamping to the real
    // count keeps follow mode correct for any content size.
    let line_count = lines.len().min(u16::MAX as usize) as u16;
    let offset = app.scroll.unwrap_or(line_count.saturating_sub(area.height));
    let paragraph = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((offset, 0));
    frame.render_widget(paragraph, area);
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
    let line = Line::from(vec![
        Span::styled("you: ", Style::default().fg(Color::Cyan)),
        Span::styled(
            if blocked {
                String::new()
            } else {
                app.input.clone()
            },
            style,
        ),
        Span::styled("▏", Style::default().fg(Color::Cyan)),
    ]);
    frame.render_widget(Paragraph::new(line), area);
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
        let age = crate::sessions::relative_age(
            &row.started_at,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        );
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
    frame.render_widget(
        Paragraph::new(body).block(block).wrap(Wrap { trim: true }),
        box_area,
    );
}
