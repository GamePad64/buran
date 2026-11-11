//! Transcript rendering tests on ratatui's offscreen TestBackend — no real
//! terminal. These guard the follow-mode scroll contract: the transcript
//! pane must never render empty while it has content.
//!
//! Regression: follow mode used `scroll((u16::MAX, 0))` betting on ratatui
//! clamping the offset. ratatui 0.30 with `Wrap` instead walks `next_line()`
//! once per offset row and early-returns once past the content — an
//! oversized offset rendered an EMPTY transcript (streamed replies were
//! invisible even though the protocol delivered them).

use ratatui::{Terminal, backend::TestBackend};

use buran_tui::ui::{App, Entry};

/// Renders one frame of the app state onto a WxH offscreen terminal and
/// returns the cell text of the transcript area.
fn screen_with(entries: Vec<Entry>, width: u16, height: u16) -> String {
    let mut app = App::default();
    for entry in entries {
        app.entries.push(entry);
    }

    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test backend");
    terminal
        .draw(|frame| buran_tui::ui::draw(frame, &mut app))
        .expect("draw");

    let backend = terminal.backend();
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| {
                    backend
                        .buffer()
                        .cell((x, y))
                        .map(|c| c.symbol())
                        .unwrap_or(" ")
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn transcript_renders_content_at_offset_zero() {
    let screen = screen_with(vec![Entry::Assistant("hello world".into())], 40, 10);
    assert!(
        screen.contains("hello world"),
        "a short transcript must be visible; got:\n{screen}"
    );
}

#[test]
fn empty_transcript_shows_the_hint_line() {
    let screen = screen_with(Vec::new(), 40, 10);
    assert!(
        screen.contains("buran-tui"),
        "the empty-state hint must be visible; got:\n{screen}"
    );
}

#[test]
fn long_transcript_follow_mode_shows_the_last_line() {
    // 50 assistant lines into a 10-row terminal: follow mode must show the
    // TAIL (the newest content), not the head, and never an empty pane.
    let entries = (0..50)
        .map(|i| Entry::Assistant(format!("line {i:02}")))
        .collect();
    let screen = screen_with(entries, 40, 10);
    assert!(
        screen.contains("line 49"),
        "follow mode must show the newest line; got:\n{screen}"
    );
    assert!(
        !screen.contains("line 00"),
        "the oldest line must have scrolled off; got:\n{screen}"
    );
}
