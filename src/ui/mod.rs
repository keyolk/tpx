//! Terminal setup and the event loop.

pub mod app;
pub mod facets;
pub mod keys;
pub mod render;

use std::io::IsTerminal;
use std::time::Duration;

use anyhow::{Result, bail};
use crossterm::event::{self, Event, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use app::{App, PendingAction};
use keys::Effect;

/// Owns the terminal. `Drop` runs during unwind, so a panic still restores the
/// screen — without this a crash leaves the shell in raw mode.
struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<std::io::Stdout>>,
}

impl TerminalGuard {
    fn enter() -> Result<Self> {
        if !std::io::stdout().is_terminal() {
            bail!("tpx needs an interactive TTY. Run it in a terminal or tmux pane.");
        }
        enable_raw_mode()?;
        let mut stdout = std::io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        Ok(Self {
            terminal: Terminal::new(CrosstermBackend::new(stdout))?,
        })
    }

    /// Hand the real terminal to a child that needs it (a `sudo` password
    /// prompt), then take it back. Without leaving the alternate screen the
    /// prompt is invisible and the app looks hung.
    fn run_external<T>(&mut self, action: impl FnOnce() -> T) -> Result<T> {
        disable_raw_mode()?;
        execute!(self.terminal.backend_mut(), LeaveAlternateScreen)?;
        let result = action();
        enable_raw_mode()?;
        execute!(self.terminal.backend_mut(), EnterAlternateScreen)?;
        // Drop ratatui's back buffer so the next draw repaints every cell —
        // otherwise the old frame reappears piecemeal.
        self.terminal.clear()?;
        Ok(result)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}

/// Poll interval. Long enough that idling costs nothing, short enough that a
/// finished collection or a new packet appears promptly.
const POLL: Duration = Duration::from_millis(120);

pub fn run(scope: crate::tree::Scope) -> Result<()> {
    let mut guard = TerminalGuard::enter()?;
    let mut app = App::new(scope);

    loop {
        // Dirty-flag rendering: the whole widget tree is rebuilt only when
        // something actually changed, so an idle tpx sits at ~0% cpu.
        if app.tick() {
            guard
                .terminal
                .draw(|frame| render::render(frame, &mut app))?;
        }

        if !event::poll(POLL)? {
            continue;
        }
        // Drain the queue after the first event, so a held key repeat renders
        // once at its final position, not once per repeat.
        loop {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    match keys::handle(&mut app, key) {
                        Effect::None => {}
                        Effect::Quit => return Ok(()),
                        Effect::RunOnRealTerminal(action) => {
                            run_privileged(&mut guard, &mut app, action)?;
                        }
                    }
                }
                // Resize needs no handler — layout is recomputed from the frame
                // area every draw — but it does need a repaint.
                Event::Resize(..) => app.touch(),
                _ => {}
            }
            if !event::poll(Duration::ZERO)? {
                break;
            }
        }
    }
}

/// Run an action that needs the real terminal. Only host packet capture does:
/// `sudo` must be able to prompt for a password.
fn run_privileged(guard: &mut TerminalGuard, app: &mut App, action: PendingAction) -> Result<()> {
    let PendingAction::CaptureHost {
        pid,
        interface,
        filter,
    } = action
    else {
        return Ok(());
    };

    app.capture_lines.clear();
    let started = guard.run_external(|| {
        println!("tpx: starting a packet capture for pid {pid}.");
        println!("     sudo may ask for your password.\n");
        crate::collect::capture::start_host(pid, &interface, &filter)
    })?;

    match started {
        Ok(capture) => {
            app.capture_lines
                .push(format!("$ {}", capture.command_line));
            app.capture = Some(capture);
            app.set_status("capturing…");
        }
        Err(error) => app.set_error(format!("capture failed: {error}")),
    }
    app.touch();
    Ok(())
}
