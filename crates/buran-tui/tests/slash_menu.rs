//! Slash-command completion menu: key mapping (open, filter, complete,
//! close) and rendering. Pure view — no client; the registry arrives as
//! `App.commands`, the way the event loop stores the announcement.

use buran_tui::ui::{Action, App, CommandRow};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::empty())
}

fn app_with_registry() -> App {
    App {
        commands: vec![
            CommandRow {
                name: "compact".into(),
                description: "Compact the conversation".into(),
            },
            CommandRow {
                name: "commit".into(),
                description: "Commit changes".into(),
            },
        ],
        ..App::default()
    }
}

#[test]
fn slash_on_empty_input_opens_the_menu() {
    let mut app = app_with_registry();
    let action = app.on_key(key(KeyCode::Char('/')));
    assert!(matches!(action, Action::None));
    assert_eq!(app.input, "/");
    assert_eq!(app.menu_selected, Some(0));
    assert_eq!(app.menu_rows_filtered().len(), 2, "empty query = all rows");
}

#[test]
fn typing_narrows_rows_to_the_prefix() {
    let mut app = app_with_registry();
    app.on_key(key(KeyCode::Char('/')));
    app.on_key(key(KeyCode::Char('c')));
    app.on_key(key(KeyCode::Char('o')));
    let rows = app.menu_rows_filtered();
    assert_eq!(rows.len(), 2, "both commands start with 'co'");
    app.on_key(key(KeyCode::Char('m')));
    // Both "compact" and "commit" start with "com"; narrowing to one
    // needs a fifth char.
    let rows = app.menu_rows_filtered();
    assert_eq!(rows.len(), 2);
    app.on_key(key(KeyCode::Char('p')));
    let rows = app.menu_rows_filtered();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].name, "compact");
}

#[test]
fn query_matching_nothing_closes_the_menu() {
    let mut app = app_with_registry();
    app.on_key(key(KeyCode::Char('/')));
    app.on_key(key(KeyCode::Char('z')));
    assert_eq!(app.menu_selected, None, "no match = no menu");
    assert_eq!(app.input, "/z");
}

#[test]
fn tab_completes_the_selected_command_and_closes() {
    let mut app = app_with_registry();
    app.on_key(key(KeyCode::Char('/')));
    app.on_key(key(KeyCode::Down)); // select "commit"
    let action = app.on_key(key(KeyCode::Tab));
    assert!(matches!(action, Action::None));
    assert_eq!(app.input, "/commit");
    assert_eq!(app.menu_selected, None);
}

#[test]
fn esc_closes_the_menu_without_changing_input() {
    let mut app = app_with_registry();
    app.on_key(key(KeyCode::Char('/')));
    app.on_key(key(KeyCode::Esc));
    assert_eq!(app.menu_selected, None);
    assert_eq!(app.input, "/");
}

#[test]
fn slash_without_a_registry_just_types() {
    let mut app = App::default(); // no commands
    app.on_key(key(KeyCode::Char('/')));
    assert_eq!(
        app.menu_selected, None,
        "old agent: no announcement, no menu"
    );
    assert_eq!(app.input, "/");
}

#[test]
fn menu_renders_rows_with_marker() {
    let mut app = app_with_registry();
    app.on_key(key(KeyCode::Char('/')));
    let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
    terminal
        .draw(|frame| buran_tui::ui::draw(frame, &mut app))
        .unwrap();
    let backend = terminal.backend();
    let screen: String = (0..12)
        .map(|y| {
            (0..80)
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
        .join("\n");
    assert!(screen.contains("/compact"), "{screen}");
    assert!(screen.contains("Compact the conversation"), "{screen}");
    assert!(screen.contains(" > "), "selected row marked: {screen}");
}

#[test]
fn left_right_edit_even_while_menu_is_open() {
    // Arrow Left/Right stay free for text editing when the completion
    // menu is open: only Up/Down/Tab/Esc are menu keys.
    let mut app = app_with_registry();
    app.on_key(key(KeyCode::Char('/')));
    app.on_key(key(KeyCode::Char('c')));
    assert_eq!(app.cursor, 2); // after "/c"

    app.on_key(key(KeyCode::Left));
    assert_eq!(app.cursor, 1, "Left edits the line, not the menu");
    app.on_key(key(KeyCode::Char('x')));
    assert_eq!(app.input, "/xc");
    app.on_key(key(KeyCode::Right));
    assert_eq!(app.cursor, 3);
    // Backspace at end-of-line deletes the char before the cursor —
    // readline parity, no menu interference.
    app.on_key(key(KeyCode::Backspace));
    assert_eq!(app.input, "/x");
}

#[test]
fn cursor_renders_at_its_true_position() {
    let mut app = App::default();
    for ch in "ab".chars() {
        app.on_key(key(KeyCode::Char(ch)));
    }
    app.on_key(key(KeyCode::Left));
    let mut terminal = Terminal::new(TestBackend::new(20, 5)).unwrap();
    terminal
        .draw(|frame| buran_tui::ui::draw(frame, &mut app))
        .unwrap();
    let backend = terminal.backend();
    // The input row is the last one; "you: a" then the highlighted "b".
    let row: String = (0..20)
        .map(|x| {
            backend
                .buffer()
                .cell((x, 4))
                .map(|c| c.symbol())
                .unwrap_or(" ")
        })
        .collect::<String>();
    assert!(row.starts_with("you: ab"), "{row:?}");
    // The char under the cursor ("b") is rendered with REVERSED modifier.
    let b_cell = backend.buffer().cell((6, 4)).unwrap();
    assert_eq!(b_cell.symbol(), "b");
    assert!(
        b_cell
            .style()
            .add_modifier
            .contains(ratatui::style::Modifier::REVERSED),
        "cursor highlights the char under it: {row:?}"
    );
}
