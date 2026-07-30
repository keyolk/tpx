use anyhow::Result;
use clap::Parser;

use tpx::tree::Scope;

/// tmux-aware process explorer.
///
/// Shows the window it is running in by default; `--server` widens to every
/// session.
#[derive(Parser)]
#[command(name = "tpx", version, about, long_about = None)]
struct Cli {
    /// Show every session on the tmux server, not just the current window.
    /// In the TUI this is also toggleable with `w`.
    #[arg(long, short = 's')]
    server: bool,

    /// Print a plain-text tree and exit. The scripting and accessibility
    /// surface — a TUI is unreadable to a screen reader and unusable in a pipe.
    #[arg(long)]
    plain: bool,

    /// With --plain, show every process instead of only interesting ones.
    #[arg(long)]
    all: bool,

    /// With --plain, annotate each process with where its stdout/stderr go.
    /// Costs one `lsof` per process, so it is opt-in.
    #[arg(long)]
    streams: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let scope = if cli.server {
        Scope::Server
    } else {
        Scope::CurrentWindow
    };
    if cli.plain {
        return tpx::plain::print(tpx::plain::Options {
            show_all: cli.all,
            scope,
            show_streams: cli.streams,
        });
    }
    tpx::ui::run(scope)
}
