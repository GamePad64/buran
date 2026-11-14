//! Picker rendering on ratatui's TestBackend (same offscreen pattern as
//! `transcript_render.rs`): rows carry title/age/dir, selection marker,
//! hidden-count footer, empty-state line. Pure view — no client.

use buran_tui::sessions::SessionRow;
use buran_tui::ui::{App, Picker};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

fn row(id: &str, title: Option<&str>, cwd: &str) -> SessionRow {
    SessionRow {
        id: id.into(),
        title: title.map(str::to_owned),
        started_at: "not a date".into(), // renders verbatim; age math is sessions.rs's tests
        cwd: cwd.into(),
    }
}

fn render_to_string(app: &mut App, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| buran_tui::ui::draw(frame, app))
        .unwrap();
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
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn picker_lists_rows_with_marker_and_hidden_count() {
    let mut app = App {
        picker: Some(Picker {
            rows: vec![
                row("b", Some("newer question"), "/proj"),
                row("a", None, "/proj"),
            ],
            hidden: 2,
            selected: 0,
        }),
        ..App::default()
    };
    let screen = render_to_string(&mut app, 90, 16);
    assert!(screen.contains("newer question"), "{screen}");
    assert!(
        screen.contains("(empty)"),
        "titleless row renders a marker: {screen}"
    );
    assert!(screen.contains("2 more in other directories"), "{screen}");
    assert!(screen.contains("proj"), "cwd tail visible: {screen}");
}

#[test]
fn picker_marks_the_selected_row() {
    let mut app = App {
        picker: Some(Picker {
            rows: vec![
                row("b", Some("first"), "/proj"),
                row("a", Some("second"), "/proj"),
            ],
            hidden: 0,
            selected: 1,
        }),
        ..App::default()
    };
    let screen = render_to_string(&mut app, 90, 16);
    let marked_first = screen.contains(">   ")
        && screen
            .lines()
            .any(|l| l.contains("> ") && l.contains("first"));
    assert!(
        screen.matches(" > ").count() == 1,
        "exactly one selection marker: {screen}"
    );
    let marker_line = screen.lines().find(|l| l.contains(" > ")).unwrap();
    assert!(
        marker_line.contains("second"),
        "marker sits on selected row: {screen}"
    );
    let _ = marked_first;
}

#[test]
fn empty_picker_says_so() {
    let mut app = App {
        picker: Some(Picker {
            rows: Vec::new(),
            hidden: 0,
            selected: 0,
        }),
        ..App::default()
    };
    let screen = render_to_string(&mut app, 90, 16);
    assert!(screen.contains("no sessions in this directory"), "{screen}");
}
