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
}

/// What a key press means to the app loop.
pub enum Action {
    /// Append the typed line to the transcript and send it.
    Submit(String),
    /// Quit the app.
    Quit,
    /// Nothing (consumed by the view: typing, scrolling, modal answers).
    None,
}

/// Whole UI state: transcript, input buffer, pending permission question,
/// scroll offset (`None` = stick to bottom), and turn status.
#[derive(Default)]
pub struct App {
    pub entries: Vec<Entry>,
    pub input: String,
    /// `Some` while the agent awaits a permission answer.
    pub question: Option<PermissionQuestion>,
    /// Manual scroll offset from the bottom; `None` follows the tail.
    pub scroll: Option<u16>,
    pub streaming: bool,
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

        match (key.modifiers, key.code) {
            (m, KeyCode::Char('c')) if m.contains(KeyModifiers::CONTROL) => Action::Quit,
            (m, KeyCode::Char('q')) if m.contains(KeyModifiers::CONTROL) => Action::Quit,
            (_, KeyCode::Enter) => {
                let line = std::mem::take(&mut self.input);
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
    let [transcript, status, input] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    render_transcript(frame, app, transcript);
    render_status(frame, app, status);
    render_input(frame, app, input);
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
            " streaming… ",
            Style::default().bg(Color::Blue).fg(Color::White),
        )
    } else {
        Span::styled(" ready ", Style::default().fg(Color::DarkGray))
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
