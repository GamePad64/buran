//! `buran-tui`: a terminal client driving `buran serve` over ACP stdio.
//!
//! The binary owns the terminal (raw mode + alternate screen) and runs a
//! single-threaded event loop. Keyboard events arrive over a tokio
//! channel from a dedicated reader thread (crossterm's own async stream
//! would pull in futures-util for one `.next()` call); protocol events
//! arrive through `AcpClient::next_event`; the in-flight turn is a
//! detached prompt whose stop reason the loop races too. All protocol
//! work lives in `acp_client`; all drawing in `ui`.

mod acp_client;
mod ui;

use anyhow::Context;
use clap::Parser;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{Event, KeyEvent, read};
use ratatui::crossterm::{execute, terminal};

use acp_client::{AcpClient, ClientEvent, TurnHandle};
use ui::App;

#[derive(Parser)]
#[command(name = "buran-tui", about = "TUI client for buran over ACP")]
struct Cli {
    /// Agent command to spawn (defaults to `buran serve` from PATH).
    #[arg(long, default_value = "buran serve")]
    agent: String,
    /// Resume a stored session: an id, or "latest"; bare `--resume`
    /// means "latest". (Attach is completed by the client via
    /// session/loadSession.)
    #[arg(
        long,
        default_missing_value = "latest",
        require_equals = false,
        num_args = 0..=1
    )]
    resume: Option<String>,
    /// Fork a stored session (copy-on-start) and resume the fork.
    /// Conflicts with `--resume`.
    #[arg(long, conflicts_with = "resume")]
    fork: Option<String>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // stderr is the log sink (the ACP stdio channel is untouched); the
    // TUI owns the whole terminal, so logs only surface after restore.
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .try_init();

    let mut terminal = setup_terminal().context("failed to enter TUI mode")?;
    let result = run_app(&mut terminal, &cli.agent, cli.resume, cli.fork).await;
    restore_terminal(&mut terminal)?;

    result
}

/// The loop's outcome: what changed since the last frame.
enum Step {
    /// The user asked to exit.
    Quit,
    /// The user submitted a line; a turn starts.
    Submit(String),
    /// Only view state changed; redraw and loop.
    Continue,
}

async fn run_app(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    agent_command: &str,
    resume: Option<String>,
    fork: Option<String>,
) -> anyhow::Result<()> {
    let client = AcpClient::spawn(agent_command, resume, fork).await?;
    let mut app = App::default();
    let (key_tx, mut key_rx) = tokio::sync::mpsc::channel::<Event>(32);
    spawn_key_reader(key_tx);

    // The in-flight turn, if any.
    let mut turn: Option<TurnHandle> = None;

    loop {
        // Draw before blocking on anything: the first frame, and any
        // state change from the previous loop iteration, hit the screen
        // before the next event.
        terminal
            .draw(|frame| ui::draw(frame, &mut app))
            .context("draw failed")?;

        let step = tokio::select! {
            // Keyboard: quit, edit the input, submit a line, answer the
            // modal.
            event = key_rx.recv() => match event {
                Some(Event::Key(key)) => on_key(&mut app, key),
                // Channel closed (reader died) or non-key event: keep
                // going; non-key events just trigger the redraw above.
                Some(_) => Step::Continue,
                None => Step::Quit,
            },
            // Protocol traffic: chunks append to the transcript; a
            // question parks in the app until answered through `ui`.
            event = client.next_event() => match event {
                Some(ClientEvent::Text(chunk)) => {
                    app.append_assistant(&chunk);
                    Step::Continue
                }
                Some(ClientEvent::Question(question)) => {
                    app.question = Some(question);
                    Step::Continue
                }
                // Agent connection closed: exit cleanly.
                None => Step::Quit,
            },
            // Turn completion, only while a turn is in flight.
            result = async {
                match turn.as_mut() {
                    Some(turn) => turn.stop_reason().await,
                    None => std::future::pending().await,
                }
            } => {
                app.streaming = false;
                match result {
                    Ok(reason) => app.append_assistant(&format!("\n[{reason:?}]")),
                    Err(error) => app.append_assistant(&format!("\n[error: {error}]")),
                }
                turn = None;
                Step::Continue
            }
        };

        match step {
            Step::Quit => break,
            Step::Continue => {}
            Step::Submit(line) => {
                app.streaming = true;
                turn = Some(client.prompt_detached(line));
            }
        }
    }

    client.shutdown();
    Ok(())
}

/// Applies one key press to the app state. Submissions pass through as
/// `Step::Submit` so the loop can start the turn; everything else
/// mutates `app` in place.
fn on_key(app: &mut App, key: KeyEvent) -> Step {
    match app.on_key(key) {
        ui::Action::Submit(line) => Step::Submit(line),
        ui::Action::Quit => Step::Quit,
        ui::Action::None => Step::Continue,
    }
}

/// Reads crossterm events on a dedicated blocking thread and forwards
/// them to the loop. The thread parks in `read()` — cheap for a TUI,
/// and it sidesteps crossterm's async stream (a futures-util pull-in
/// for one method).
fn spawn_key_reader(tx: tokio::sync::mpsc::Sender<Event>) {
    std::thread::spawn(move || {
        while let Ok(event) = read() {
            if tx.blocking_send(event).is_err() {
                break; // loop dropped the receiver: shut down
            }
        }
    });
}

fn setup_terminal() -> anyhow::Result<Terminal<CrosstermBackend<std::io::Stdout>>> {
    terminal::enable_raw_mode().context("enable raw mode")?;
    let mut stdout = std::io::stdout();
    execute!(stdout, terminal::EnterAlternateScreen).context("enter alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    Terminal::new(backend).context("create terminal")
}

fn restore_terminal(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
) -> anyhow::Result<()> {
    terminal.show_cursor().ok();
    execute!(terminal.backend_mut(), terminal::LeaveAlternateScreen).ok();
    terminal::disable_raw_mode().ok();
    Ok(())
}
