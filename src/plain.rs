//! `--plain` output: the whole tree as text, for pipes, scripts, and screen
//! readers. Keeping parity with the TUI is deliberate — a TUI-only tool is
//! unusable in a pipeline and inaccessible.

use std::collections::HashMap;

use anyhow::Result;

use crate::collect;
use crate::model::{CollectError, Snapshot, human_age, human_bytes};
use crate::tree::{self, Expansion, Filter, Kind, Noise, Row, Scope};

/// What `--plain` prints.
pub struct Options {
    /// Show every process, not only the interesting ones.
    pub show_all: bool,
    pub scope: Scope,
    /// Annotate each process with where its stdout/stderr go.
    ///
    /// Off by default: it costs one `lsof` per process, which on a server-wide
    /// tree is hundreds of subprocess spawns. The flag makes that cost the
    /// caller's explicit choice.
    pub show_streams: bool,
}

pub fn print(options: Options) -> Result<()> {
    let Options {
        show_all,
        scope,
        show_streams,
    } = options;
    let snapshot = collect_once();
    let noise = if show_all { Noise::Show } else { Noise::Hide };
    let current_window = collect::tmux::current_window();

    // Plain output has no interaction, so everything is expanded — a collapsed
    // node in a pipe is just missing data.
    let mut expansion = Expansion::default();
    let build = |expansion: &Expansion| {
        tree::build(
            &snapshot,
            expansion,
            noise,
            &Filter::default(),
            &scope,
            current_window.as_ref(),
        )
    };
    expansion.expand_everything(&snapshot);
    let rows = build(&expansion);

    // One bulk lsof for every process, not one per row: the per-process form cost
    // 26 seconds on a server-wide tree, the bulk form 0.23s.
    let streams = if show_streams {
        collect::streams::locate_all().unwrap_or_default()
    } else {
        Default::default()
    };
    // The pane a row belongs to is only knowable while walking the tree in order,
    // so it is tracked here — without it a nested pty (ccproxy -> claude) reports
    // "no tmux pane" even though its output is relayed into one.
    let mut owning_pane: Option<String> = None;
    for row in &rows {
        if let Kind::Pane { pane } = &row.kind {
            owning_pane = Some(pane.target.clone());
        }
        println!("{}", plain_row(row));
        if show_streams {
            print_streams(row, &streams, owning_pane.as_deref());
        }
    }

    let conflicts = snapshot.port_conflicts();
    if !conflicts.is_empty() {
        println!();
        println!("contested listen ports:");
        let mut ports: Vec<_> = conflicts.into_iter().collect();
        ports.sort_by_key(|(port, _)| *port);
        for (port, holders) in ports {
            let names: Vec<String> = holders
                .iter()
                .map(|key| {
                    let name = snapshot
                        .proc(key)
                        .map(|proc| proc.name().to_string())
                        .unwrap_or_else(|| "?".into());
                    format!("{name}({})", key.pid)
                })
                .collect();
            println!("  :{port}  {}", names.join(", "));
        }
    }

    // Failures go to stderr so a pipe consuming stdout is unaffected, but they
    // are never silent.
    for CollectError { source, message } in &snapshot.errors {
        eprintln!("tpx: {source} failed: {message}");
    }
    Ok(())
}

/// One synchronous collection round, without the background worker.
fn collect_once() -> Snapshot {
    let mut snapshot = Snapshot::default();
    match collect::tmux::panes() {
        Ok(panes) => snapshot.panes = panes,
        Err(error) => snapshot.errors.push(CollectError {
            source: "tmux",
            message: error.to_string(),
        }),
    }
    match collect::host::processes() {
        Ok((procs, children)) => {
            snapshot.procs = procs;
            snapshot.children = children;
        }
        Err(error) => snapshot.errors.push(CollectError {
            source: "ps",
            message: error.to_string(),
        }),
    }
    match collect::net::sockets() {
        Ok(sockets) => snapshot.sockets = sockets,
        Err(error) => snapshot.errors.push(CollectError {
            source: "lsof",
            message: error.to_string(),
        }),
    }
    if collect::docker_available() {
        match collect::container::containers() {
            Ok(mut containers) => {
                collect::container::attribute(&mut containers, &snapshot.panes, &snapshot.procs);
                snapshot.containers = containers;
            }
            Err(error) => snapshot.errors.push(CollectError {
                source: "docker",
                message: error.to_string(),
            }),
        }
    }
    snapshot
}

/// Command-line width in `--plain` output. Matches what `tmux.sh procs` used —
/// wide enough for a real invocation, narrow enough that the indent still reads
/// as a tree.
const PLAIN_COMMAND_WIDTH: usize = 120;

/// Print where a process's output goes, indented under its row.
fn print_streams(
    row: &Row,
    streams: &HashMap<u32, collect::streams::Streams>,
    owning_pane: Option<&str>,
) {
    let Kind::Process { proc } = &row.kind else {
        return;
    };
    // Container fds are invisible to host lsof — saying nothing is better than
    // printing a misleading "unknown".
    if !matches!(proc.key.origin, crate::model::Origin::Host) {
        return;
    }
    let Some(located) = streams.get(&proc.key.pid) else {
        return;
    };
    let mut located = located.clone();
    collect::streams::attach_owning_pane(&mut located, owning_pane);

    let indent = "  ".repeat(row.depth as usize + 1);
    println!("{indent}stdout: {}", located.stdout.summary());
    println!("{indent}stderr: {}", located.stderr.summary());
}

fn plain_row(row: &Row) -> String {
    let indent = "  ".repeat(row.depth as usize);
    let mut line = match &row.kind {
        Kind::Session {
            name,
            attached,
            window_count,
        } => {
            format!(
                "{indent}session {name}  {window_count}w{}",
                if *attached { " attached" } else { "" }
            )
        }
        Kind::Window {
            name,
            index,
            pane_count,
            ..
        } => {
            format!("{indent}window {index}:{name}  {pane_count}p")
        }
        Kind::Pane { pane } => format!(
            "{indent}pane {}  {}  {}",
            pane.target, pane.cwd, pane.current_command
        ),
        Kind::Process { proc } => format!(
            "{indent}{} pid={} {}",
            proc.name(),
            proc.key.pid,
            proc.display_command(PLAIN_COMMAND_WIDTH)
        ),
        Kind::Container { container } => format!(
            "{indent}container {} {} [{}]",
            container.display_name(),
            container.display_image(),
            container.status
        ),
    };

    let mut annotations = Vec::new();
    if row.rollup.cpu_pct >= 0.1 {
        annotations.push(format!("cpu={:.1}%", row.rollup.cpu_pct));
    }
    if row.rollup.rss_bytes > 0 {
        annotations.push(format!("rss={}", human_bytes(row.rollup.rss_bytes)));
    }
    if !row.listen_ports.is_empty() {
        let ports: Vec<String> = row.listen_ports.iter().map(u16::to_string).collect();
        annotations.push(format!(
            "listen={}{}",
            ports.join(","),
            if row.port_conflict { "(conflict)" } else { "" }
        ));
    }
    if row.connections > 0 {
        annotations.push(format!("conns={}", row.connections));
    }
    if let Kind::Process { proc } = &row.kind {
        annotations.push(format!("age={}", human_age(proc.age_secs)));
    }
    if !annotations.is_empty() {
        line.push_str("  [");
        line.push_str(&annotations.join(" "));
        line.push(']');
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Pane, Proc, ProcKey};

    #[test]
    fn plain_rows_are_greppable_and_carry_metrics() {
        let mut snapshot = Snapshot::default();
        snapshot.panes = vec![Pane {
            session: "local".into(),
            window_index: 1,
            window_name: "w".into(),
            pane_index: 1,
            target: "local:1.1".into(),
            cwd: "/src".into(),
            current_command: "fish".into(),
            pid: 100,
            active: true,
            window_active: true,
            session_attached: true,
            zoomed: false,
        }];
        let proc = Proc {
            key: ProcKey::host(100),
            ppid: 1,
            command: "cargo run".into(),
            age_secs: 90,
            cpu_pct: 12.0,
            cpu_time_secs: 0.0,
            rss_bytes: 2 * 1024 * 1024,
            state: "S".into(),
            threads: None,
            fd_count: None,
        };
        snapshot.children.insert(1, vec![100]);
        snapshot.procs.insert(proc.key.clone(), proc);

        let rows = tree::build(
            &snapshot,
            &Expansion::default(),
            Noise::Hide,
            &Filter::default(),
            &Scope::Server,
            None,
        );
        let text: Vec<String> = rows.iter().map(plain_row).collect();
        assert!(text.iter().any(|line| line.starts_with("session local")));
        assert!(text.iter().any(|line| line.contains("pane local:1.1")));
        let proc_line = text.iter().find(|line| line.contains("pid=100")).unwrap();
        assert!(proc_line.contains("cpu=12.0%"));
        assert!(proc_line.contains("rss=2.0M"));
        assert!(proc_line.contains("age=1m"));
    }
}
