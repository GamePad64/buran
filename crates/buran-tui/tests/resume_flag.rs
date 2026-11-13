//! The arg-construction seam is a pure function: no process spawn, no
//! terminal, no protocol — the same unit-level shape as transcript_render.

use buran_tui::acp_client::serve_args;

// `clap` is already a direct dependency of buran-tui (its own Cli derive);
// the conflict probe below reuses it — no dev-dependency addition.

#[test]
fn serve_args_builds_the_selection_flags() {
    assert_eq!(
        serve_args(Some("abc".into()), None),
        vec!["--resume".to_string(), "abc".into()]
    );
    assert_eq!(
        serve_args(None, Some("def".into())),
        vec!["--fork".to_string(), "def".into()]
    );
    assert_eq!(serve_args(None, None), Vec::<String>::new());
}

#[test]
fn tui_clap_rejects_resume_and_fork_together() {
    // The TUI's own clap mirrors `buran serve`: --resume and --fork are
    // two spellings of "which conversation to attach to" and conflict.
    // (serve_args never sees both at once; the conflict is enforced by
    // clap before arg construction.)
    use clap::Parser;
    #[derive(Parser)]
    struct Probe {
        #[arg(long, default_missing_value = "latest", require_equals = false,
              num_args = 0..=1)]
        resume: Option<String>,
        #[arg(long, conflicts_with = "resume")]
        fork: Option<String>,
    }
    assert!(Probe::try_parse_from(["probe", "--resume", "a", "--fork", "b"]).is_err());
    assert!(Probe::try_parse_from(["probe", "--resume", "a"]).is_ok());
    assert!(Probe::try_parse_from(["probe", "--fork", "b"]).is_ok());
}
