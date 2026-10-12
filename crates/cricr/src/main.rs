//! `cricr` — a standalone, cross-platform TUI IRC client with Emacs-style
//! editing and incremental search.

mod app;
mod buffer;
mod config;
mod input;
mod keymap;
mod search;
mod theme;
mod ui;

use std::io::stdout;
use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

/// A standalone TUI IRC client with Emacs-style editing.
#[derive(Debug, Parser)]
#[command(name = "cricr", version, about)]
struct Cli {
    /// Path to the configuration file (defaults to the platform config dir).
    #[arg(short, long)]
    config: Option<PathBuf>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let path = cli.config.unwrap_or_else(config::default_path);
    let cfg = config::Config::load(&path)?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(run(cfg))
}

async fn run(cfg: config::Config) -> Result<()> {
    enable_raw_mode()?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(out);
    let mut terminal = Terminal::new(backend)?;

    let result = run_app(&mut terminal, cfg).await;

    disable_raw_mode().ok();
    execute!(terminal.backend_mut(), LeaveAlternateScreen).ok();
    terminal.show_cursor().ok();
    result
}

async fn run_app(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    cfg: config::Config,
) -> Result<()> {
    let (net_tx, net_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = app::App::new(cfg, net_tx);

    let (term_tx, term_rx) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn(move || {
        while let Ok(event) = crossterm::event::read() {
            if term_tx.send(event).is_err() {
                break;
            }
        }
    });

    // On terminals that still deliver Ctrl+C as SIGINT, swallow it so it can
    // never terminate the app (quit with C-x C-c or /quit instead).
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        if let Ok(mut sigint) = signal(SignalKind::interrupt()) {
            tokio::spawn(async move { while sigint.recv().await.is_some() {} });
        }
    }

    app.run(terminal, net_rx, term_rx).await
}
