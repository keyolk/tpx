//! Detail-pane facet bodies. Each facet turns the selected row into lines; the
//! pane chrome, tabs and scrolling live in [`super::render`].
//!
//! Split out of `render.rs` purely for size — that file was past the 500-line
//! limit, and the facets are the part with no coupling to layout.

use ratatui::text::{Line, Span};

use crate::collect::introspect;
use crate::model::{Origin, SocketState, human_age, human_bytes};
use crate::palette::{ERROR, INFO, Palette, SUCCESS, WARN};
use crate::tree::Kind;

use super::app::{App, Facet};
use super::render::{field, shorten_path, spinner};

/// Render the selected row through one facet.
pub fn lines_for(app: &App, facet: Facet, palette: Palette) -> Vec<Line<'static>> {
    match facet {
        Facet::Overview => overview_lines(app, palette),
        Facet::Network => network_lines(app, palette),
        Facet::Files => files_lines(app, palette),
        Facet::Output => output_lines(app, palette),
        Facet::Streams => stream_lines(app, palette),
        Facet::Stack => stack_lines(app, palette),
        Facet::Env => env_lines(app, palette),
        Facet::Packets => packet_lines(app, palette),
    }
}

fn overview_lines(app: &App, palette: Palette) -> Vec<Line<'static>> {
    let Some(row) = app.selected_row() else {
        return vec![Line::styled("nothing selected", palette.dim())];
    };
    let mut lines = Vec::new();

    match &row.kind {
        Kind::Process { proc } => {
            lines.push(field("pid", proc.key.pid.to_string(), palette));
            lines.push(field("ppid", proc.ppid.to_string(), palette));
            match &proc.key.origin {
                Origin::Host => lines.push(field("namespace", "host".into(), palette)),
                Origin::Container(id) => {
                    let name = app
                        .snapshot
                        .container(id)
                        .map(|c| c.name.clone())
                        .unwrap_or_default();
                    lines.push(field("namespace", format!("container {name}"), palette));
                    lines.push(Line::styled(
                        "  (this pid is inside the container — it does not exist on the host)",
                        palette.dim(),
                    ));
                }
            }
            lines.push(field("state", proc.state.clone(), palette));
            lines.push(field("age", human_age(proc.age_secs), palette));
            lines.push(field("cpu", format!("{:.1}%", proc.cpu_pct), palette));
            lines.push(field("rss", human_bytes(proc.rss_bytes), palette));

            let threads = match &proc.key.origin {
                Origin::Host => app.detail.host_threads.get(&proc.key.pid).copied(),
                Origin::Container(_) => app
                    .detail
                    .container_detail
                    .get(&proc.key)
                    .and_then(|detail| detail.threads),
            };
            lines.push(field(
                "threads",
                threads.map_or_else(|| "…".into(), |count| count.to_string()),
                palette,
            ));
            let fds = match &proc.key.origin {
                Origin::Host => app.detail.host_fds.get(&proc.key.pid).copied(),
                Origin::Container(_) => app
                    .detail
                    .container_detail
                    .get(&proc.key)
                    .and_then(|detail| detail.fd_count),
            };
            lines.push(field(
                "fds",
                fds.map_or_else(|| "…".into(), |count| count.to_string()),
                palette,
            ));

            if let Some(counters) = app.snapshot.net_counters.get(&proc.key.pid) {
                lines.push(field(
                    "net total",
                    format!(
                        "{} in / {} out",
                        human_bytes(counters.bytes_in),
                        human_bytes(counters.bytes_out)
                    ),
                    palette,
                ));
            }

            // The full command line is the most useful single piece of context
            // and the longest — wrapped so it fills the pane width rather than
            // being truncated to one line. This is also what fills the lower
            // half of the pane that was previously blank.
            lines.push(Line::default());
            lines.push(Line::styled("command", palette.dim()));
            for wrapped in wrap_text(&proc.command, 80) {
                lines.push(Line::raw(format!("  {wrapped}")));
            }

            // A Claude Code process shows its session id and project cwd —
            // the two things needed to find the session in `ccx` or logs.
            if matches!(proc.key.origin, Origin::Host)
                && crate::collect::claude::is_claude(&proc.command)
                && let Some(pane) = app.selected_pane_target()
                && let Some(p) = app.snapshot.panes.iter().find(|pp| pp.target == pane)
                && let Some(session) = crate::collect::claude::session_for(&p.cwd)
            {
                lines.push(Line::default());
                lines.push(Line::styled("claude", palette.dim()));
                lines.push(Line::raw(format!("  session  {}", session.session_id)));
                lines.push(Line::raw(format!("  cwd      {}", session.cwd)));
            }

            // Where this process sits: the pane it belongs to and its subtree
            // footprint. Cheap to compute, and it answers "is this the whole
            // story or just one branch".
            if let Some(pane) = app.selected_pane_target() {
                lines.push(Line::default());
                lines.push(field("pane", pane, palette));
            }
            if row.rollup.proc_count > 1 {
                lines.push(field(
                    "subtree",
                    format!(
                        "{} procs · {:.1}%cpu · {}",
                        row.rollup.proc_count,
                        row.rollup.cpu_pct,
                        human_bytes(row.rollup.rss_bytes)
                    ),
                    palette,
                ));
            }
        }
        Kind::Pane { pane } => {
            lines.push(field("target", pane.target.clone(), palette));
            lines.push(field("session", pane.session.clone(), palette));
            lines.push(field(
                "window",
                format!("{}:{}", pane.window_index, pane.window_name),
                palette,
            ));
            lines.push(field("cwd", pane.cwd.clone(), palette));
            lines.push(field("command", pane.current_command.clone(), palette));
            lines.push(field("shell pid", pane.pid.to_string(), palette));
            lines.push(Line::default());
            lines.push(field(
                "subtree",
                format!("{} procs", row.rollup.proc_count),
                palette,
            ));
            lines.push(field("cpu", format!("{:.1}%", row.rollup.cpu_pct), palette));
            lines.push(field("rss", human_bytes(row.rollup.rss_bytes), palette));
        }
        Kind::Container { container } => {
            lines.push(field("name", container.name.clone(), palette));
            lines.push(field("id", container.short_id.clone(), palette));
            lines.push(field("image", container.image.clone(), palette));
            lines.push(field("status", container.status.clone(), palette));
            lines.push(field("network", container.network_mode.clone(), palette));
            if !container.ports.is_empty() {
                lines.push(field("ports", container.ports.join(", "), palette));
            }
            if let Some(project) = &container.compose_project {
                lines.push(field("compose", project.clone(), palette));
            }
            match &container.attribution {
                Some(attribution) => {
                    lines.push(field("pane", attribution.pane_target.clone(), palette));
                    lines.push(Line::styled(
                        format!("  linked by: {} (heuristic)", attribution.reason.label()),
                        palette.dim(),
                    ));
                }
                None => lines.push(Line::styled("  not linked to any pane", palette.dim())),
            }
            if let Some(metrics) = &container.metrics {
                lines.push(Line::default());
                lines.push(field("cpu", format!("{:.1}%", metrics.cpu_pct), palette));
                lines.push(field(
                    "mem",
                    format!(
                        "{} / {}",
                        human_bytes(metrics.mem_bytes),
                        human_bytes(metrics.mem_limit_bytes)
                    ),
                    palette,
                ));
                lines.push(field("pids", metrics.pids.to_string(), palette));
                lines.push(field(
                    "net",
                    format!(
                        "{} in / {} out",
                        human_bytes(metrics.net_in_bytes),
                        human_bytes(metrics.net_out_bytes)
                    ),
                    palette,
                ));
                lines.push(field(
                    "block io",
                    format!(
                        "{} read / {} write",
                        human_bytes(metrics.block_read_bytes),
                        human_bytes(metrics.block_write_bytes)
                    ),
                    palette,
                ));
            }
        }
        Kind::Session {
            name,
            window_count,
            attached,
        } => {
            lines.push(field("session", name.clone(), palette));
            lines.push(field("windows", window_count.to_string(), palette));
            lines.push(field("attached", attached.to_string(), palette));
            lines.push(field("procs", row.rollup.proc_count.to_string(), palette));
            lines.push(field("cpu", format!("{:.1}%", row.rollup.cpu_pct), palette));
            lines.push(field("rss", human_bytes(row.rollup.rss_bytes), palette));
        }
        Kind::Window {
            name,
            index,
            pane_count,
            active,
            ..
        } => {
            lines.push(field("window", format!("{index}:{name}"), palette));
            lines.push(field("panes", pane_count.to_string(), palette));
            lines.push(field("active", active.to_string(), palette));
            lines.push(field("procs", row.rollup.proc_count.to_string(), palette));
            lines.push(field("cpu", format!("{:.1}%", row.rollup.cpu_pct), palette));
            lines.push(field("rss", human_bytes(row.rollup.rss_bytes), palette));
        }
    }
    lines
}

fn network_lines(app: &App, palette: Palette) -> Vec<Line<'static>> {
    let sockets = app.selected_sockets();
    if sockets.is_empty() {
        if let Some(container) = app.selected_container() {
            let mut lines = vec![Line::styled("published ports", palette.dim())];
            if container.ports.is_empty() {
                lines.push(Line::styled("  none", palette.dim()));
            }
            for mapping in &container.ports {
                lines.push(Line::raw(format!("  {mapping}")));
            }
            lines.push(Line::default());
            lines.push(Line::styled(
                "expand the container to see per-process sockets inside it",
                palette.dim(),
            ));
            return lines;
        }
        return vec![Line::styled("no sockets held by this row", palette.dim())];
    }

    let conflicts = app.snapshot.port_conflicts();
    let mut lines = Vec::new();
    let listening: Vec<_> = sockets
        .iter()
        .filter(|socket| socket.state == SocketState::Listen)
        .collect();
    let established: Vec<_> = sockets
        .iter()
        .filter(|socket| socket.state == SocketState::Established)
        .collect();

    lines.push(Line::styled(
        format!("listening ({})", listening.len()),
        palette.dim(),
    ));
    for socket in listening {
        let contested = socket
            .local_port()
            .is_some_and(|port| conflicts.contains_key(&port));
        let mut spans = vec![
            Span::styled(
                format!("  {:<4} ", proto_label(socket.proto)),
                palette.dim(),
            ),
            Span::styled(socket.local.clone(), palette.fg(SUCCESS)),
        ];
        if contested {
            // The word matters as much as the color: this is the actionable case.
            spans.push(Span::styled(
                "  ! also held by another process",
                palette.fg(ERROR),
            ));
        }
        lines.push(Line::from(spans));
    }

    lines.push(Line::default());
    lines.push(Line::styled(
        format!("established ({})", established.len()),
        palette.dim(),
    ));
    if app.peers.is_none()
        && established.iter().any(|socket| {
            socket
                .peer
                .as_deref()
                .is_some_and(crate::collect::peers::PeerMap::is_local)
        })
    {
        lines.push(Line::styled(
            "  (P resolves and jumps to the peer process)",
            palette.dim(),
        ));
    }
    for socket in established.iter().take(40) {
        let remote = socket.peer.clone().unwrap_or_default();
        let mut spans = vec![
            Span::styled(
                format!("  {:<4} ", proto_label(socket.proto)),
                palette.dim(),
            ),
            Span::raw(socket.local.clone()),
            Span::styled(" → ", palette.dim()),
            Span::styled(remote.clone(), palette.fg(INFO)),
        ];
        // A loopback address names a port, not a service. Resolving it to the
        // process on the other end is what turns a socket list into a map of who
        // talks to whom — and `P` jumps there.
        if let Some(peers) = &app.peers
            && let Some(proc) = app.selected_proc()
            && let Some(peer) = peers.peer_of(&remote, proc.key.pid)
        {
            spans.push(Span::styled(
                format!("  {} ({})", peer.name, peer.pid),
                palette.fg(SUCCESS),
            ));
        }
        lines.push(Line::from(spans));
    }
    if established.len() > 40 {
        lines.push(Line::styled(
            format!("  … {} more", established.len() - 40),
            palette.dim(),
        ));
    }

    if let Some(proc) = app.selected_proc()
        && let Some(counters) = app.snapshot.net_counters.get(&proc.key.pid)
    {
        lines.push(Line::default());
        lines.push(field(
            "total",
            format!(
                "{} in / {} out (since process start)",
                human_bytes(counters.bytes_in),
                human_bytes(counters.bytes_out)
            ),
            palette,
        ));
    }
    lines
}

fn proto_label(proto: crate::model::Proto) -> &'static str {
    match proto {
        crate::model::Proto::Tcp => "tcp",
        crate::model::Proto::Udp => "udp",
    }
}

fn files_lines(app: &App, palette: Palette) -> Vec<Line<'static>> {
    let Some(proc) = app.selected_proc() else {
        return vec![Line::styled("select a process", palette.dim())];
    };
    match &proc.key.origin {
        Origin::Host => {
            let Some(files) = app.detail.host_files.get(&proc.key.pid) else {
                return vec![Line::styled("reading open files…", palette.dim())];
            };
            let mut lines = vec![Line::styled(
                format!("open files ({})", files.len()),
                palette.dim(),
            )];
            for file in files.iter().take(200) {
                lines.push(Line::from(vec![
                    Span::styled(format!("  {:<4} {:<5} ", file.fd, file.kind), palette.dim()),
                    Span::raw(shorten_path(&file.path)),
                ]));
            }
            if files.len() > 200 {
                lines.push(Line::styled(
                    format!("  … {} more", files.len() - 200),
                    palette.dim(),
                ));
            }
            lines
        }
        Origin::Container(_) => {
            let Some(detail) = app.detail.container_detail.get(&proc.key) else {
                return vec![Line::styled("reading /proc via sidecar…", palette.dim())];
            };
            vec![
                field(
                    "cwd",
                    detail.cwd.clone().unwrap_or_else(|| "?".into()),
                    palette,
                ),
                field(
                    "open fds",
                    detail
                        .fd_count
                        .map_or_else(|| "?".into(), |count| count.to_string()),
                    palette,
                ),
                Line::default(),
                Line::styled(
                    "per-fd paths inside a container need another sidecar round-trip",
                    palette.dim(),
                ),
            ]
        }
    }
}

/// Where the selected process's stdout/stderr go, and their tail when reachable.
///
/// Both streams are shown together: a process whose stdout is discarded but whose
/// stderr goes to a log is the common shape, and showing only one would hide the
/// half that has the answer.
fn stream_lines(app: &App, palette: Palette) -> Vec<Line<'static>> {
    let Some(proc) = app.selected_proc() else {
        return vec![Line::styled("select a process", palette.dim())];
    };
    if !matches!(proc.key.origin, Origin::Host) {
        return vec![Line::styled(
            "container process — host lsof cannot see its fds",
            palette.dim(),
        )];
    }
    let Some((located, content)) = app.detail.streams.get(&proc.key.pid) else {
        return vec![Line::styled("locating streams…", palette.dim())];
    };

    let mut lines = Vec::new();
    for (label, sink, body) in [
        ("stdout", &located.stdout, &content.stdout),
        ("stderr", &located.stderr, &content.stderr),
    ] {
        lines.push(Line::from(vec![
            Span::styled(format!("{label:<7}"), palette.bold(INFO)),
            Span::raw(sink.summary()),
        ]));
        match body {
            Some(Ok(text)) if text.trim().is_empty() => {
                lines.push(Line::styled("  (empty)", palette.dim()));
            }
            Some(Ok(text)) => {
                // Tail first: the newest lines are the ones being looked for.
                let tail: Vec<&str> = text.lines().rev().take(STREAM_PREVIEW_LINES).collect();
                for line in tail.into_iter().rev() {
                    lines.push(Line::raw(format!("  {line}")));
                }
            }
            // The reason is the content when there is none — "not readable
            // because SIP blocks pipe reads" is an answer, a blank panel is not.
            Some(Err(reason)) => lines.push(Line::styled(format!("  {reason}"), palette.dim())),
            None => lines.push(Line::styled("  …", palette.dim())),
        }
        lines.push(Line::default());
    }
    lines.push(Line::styled("r to re-read", palette.dim()));
    lines
}

/// Lines of each stream to preview. Both streams share the pane, so neither can
/// take all of it.
const STREAM_PREVIEW_LINES: usize = 40;

/// What the selected process is doing right now, from a stack sample.
///
/// The leaf frame is the answer and leads each thread; the frames above it are
/// context. Waiting threads are marked as such, because a stack parked in
/// `kevent64` means idle and reads identically to a hot loop otherwise.
fn stack_lines(app: &App, palette: Palette) -> Vec<Line<'static>> {
    let Some(proc) = app.selected_proc() else {
        return vec![Line::styled("select a process", palette.dim())];
    };
    if !matches!(proc.key.origin, Origin::Host) {
        return vec![Line::styled(
            "container process — sample cannot reach another pid namespace",
            palette.dim(),
        )];
    }
    let Some(result) = app.detail.stacks.get(&proc.key.pid) else {
        return vec![
            Line::styled("no sample taken", palette.dim()),
            Line::default(),
            Line::raw("press S to sample this process for 1s"),
            Line::default(),
            Line::styled(
                "sampling blocks for a wall-clock second, so it is never automatic",
                palette.dim(),
            ),
        ];
    };
    let sample = match result {
        Ok(sample) => sample,
        Err(reason) => return vec![Line::styled(reason.clone(), palette.fg(ERROR))],
    };

    let busy = sample.threads.iter().filter(|t| !t.is_waiting()).count();
    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                format!("{} threads", sample.threads.len()),
                palette.bold(INFO),
            ),
            Span::raw("  "),
            // The count that matters: a process with 21 threads all parked is idle.
            Span::styled(
                format!("{busy} doing work"),
                if busy > 0 {
                    palette.fg(WARN)
                } else {
                    palette.dim()
                },
            ),
            Span::styled(format!("  ({}ms)", sample.duration_ms), palette.dim()),
        ]),
        Line::default(),
    ];

    for thread in sample.threads.iter().take(STACK_THREADS) {
        let waiting = thread.is_waiting();
        lines.push(Line::from(vec![
            // The letter carries the state, so it survives monochrome.
            Span::styled(
                if waiting { "W " } else { "R " },
                if waiting {
                    palette.dim()
                } else {
                    palette.bold(WARN)
                },
            ),
            Span::styled(format!("{:>5} ", thread.samples), palette.dim()),
            Span::raw(thread.label.clone()),
        ]));
        for (depth, frame) in thread.stack.iter().enumerate() {
            let style = if depth == 0 {
                // The leaf is where the thread actually sat.
                palette.fg(if waiting { INFO } else { WARN })
            } else {
                palette.dim()
            };
            lines.push(Line::styled(format!("      {frame}"), style));
        }
        lines.push(Line::default());
    }
    if sample.threads.len() > STACK_THREADS {
        lines.push(Line::styled(
            format!("… {} more threads", sample.threads.len() - STACK_THREADS),
            palette.dim(),
        ));
    }
    lines.push(Line::styled("S to re-sample", palette.dim()));
    lines
}

/// Threads shown in the stack facet. A Node process reports 20+, nearly all
/// parked; the busiest few are the ones that answer the question.
const STACK_THREADS: usize = 5;

/// The environment the selected process was started with.
///
/// Grouped rather than alphabetical, and secret-looking values are masked: a
/// process listing is somewhere tokens leak by accident.
fn env_lines(app: &App, palette: Palette) -> Vec<Line<'static>> {
    let Some(proc) = app.selected_proc() else {
        return vec![Line::styled("select a process", palette.dim())];
    };
    if !matches!(proc.key.origin, Origin::Host) {
        return vec![Line::styled(
            "container process — use `docker inspect` for its env",
            palette.dim(),
        )];
    }
    let Some(result) = app.detail.envs.get(&proc.key.pid) else {
        return vec![Line::styled("reading environment…", palette.dim())];
    };
    let vars = match result {
        Ok(vars) => vars,
        Err(reason) => return vec![Line::styled(reason.clone(), palette.dim())],
    };

    let mut lines = vec![Line::styled(
        format!("{} variables", vars.len()),
        palette.bold(INFO),
    )];
    for (group, entries) in introspect::grouped(vars) {
        lines.push(Line::default());
        lines.push(Line::styled(
            format!("{group} ({})", entries.len()),
            palette.dim(),
        ));
        for (key, value) in entries {
            lines.push(Line::from(vec![
                Span::styled(format!("  {key}="), palette.fg(SUCCESS)),
                Span::raw(introspect::display_value(&key, &value)),
            ]));
        }
    }
    lines
}

fn output_lines(app: &App, palette: Palette) -> Vec<Line<'static>> {
    let Some(target) = app.selected_pane_target() else {
        return vec![Line::styled("no pane for this row", palette.dim())];
    };
    let Some(output) = app.detail.pane_output.get(&target) else {
        return vec![Line::styled("capturing pane output…", palette.dim())];
    };
    let mut lines = vec![
        Line::styled(
            format!("{target} — last lines (r to re-read)"),
            palette.dim(),
        ),
        Line::default(),
    ];
    lines.extend(output.lines().map(|line| Line::raw(line.to_string())));
    lines
}

fn packet_lines(app: &App, palette: Palette) -> Vec<Line<'static>> {
    if app.capture_lines.is_empty() {
        return vec![
            Line::styled("no capture running", palette.dim()),
            Line::default(),
            Line::raw("press d to dump packets for the selected process or container"),
            Line::default(),
            Line::styled(
                "a host capture needs sudo (macOS restricts /dev/bpf*);",
                palette.dim(),
            ),
            Line::styled(
                "a container capture runs in a sidecar and does not.",
                palette.dim(),
            ),
        ];
    }
    let mut lines = Vec::new();
    if app.capture.is_some() {
        lines.push(Line::styled(
            format!("{} capturing…", spinner(app)),
            palette.fg(INFO),
        ));
    }
    // Tail, not head — the newest packets are the interesting ones.
    let visible = app.capture_lines.len().saturating_sub(300);
    lines.extend(
        app.capture_lines[visible..]
            .iter()
            .map(|line| Line::raw(line.clone())),
    );
    lines
}

/// Word-wrap a long string to a column width, for the command display.
fn wrap_text(text: &str, width: usize) -> Vec<String> {
    use unicode_width::UnicodeWidthStr;
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        let candidate = if current.is_empty() {
            word.to_string()
        } else {
            format!("{current} {word}")
        };
        if candidate.width() > width && !current.is_empty() {
            lines.push(current.clone());
            current = word.to_string();
        } else {
            current = candidate;
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}
