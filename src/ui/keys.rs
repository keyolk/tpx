//! Key handling. One place that owns the keymap, so the help overlay and the
//! footer hints cannot drift from what the keys actually do.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::collect::{self, capture};
use crate::model::Origin;
use crate::tree::{Kind, Noise, Scope, Sort};

use super::app::{App, Facet, Modal, PendingAction};

/// What the caller must do outside the app — things that need the real terminal
/// (a `sudo` prompt) cannot happen while the alternate screen is up.
pub enum Effect {
    None,
    Quit,
    /// Leave the TUI, run the action on the real terminal, come back.
    RunOnRealTerminal(PendingAction),
}

pub fn handle(app: &mut App, key: KeyEvent) -> Effect {
    // Filter editing swallows everything except its own control keys, so typing
    // `q` into a search does not quit.
    if app.filter_input.is_some() {
        return filter_key(app, key);
    }
    if app.modal.is_some() {
        return modal_key(app, key);
    }
    tree_key(app, key)
}

fn filter_key(app: &mut App, key: KeyEvent) -> Effect {
    let Some(query) = app.filter_input.as_mut() else {
        return Effect::None;
    };
    match key.code {
        KeyCode::Esc => {
            // Filtering is live, so the committed query has already been
            // overwritten by every keystroke. Cancel therefore clears it — the
            // pre-edit state is not recoverable, and leaving a half-typed query
            // applied after Esc would be worse than clearing.
            app.filter_input = None;
            app.filter.query.clear();
            app.rebuild();
        }
        KeyCode::Enter => {
            app.filter.query = query.clone();
            app.filter_input = None;
            app.rebuild();
        }
        KeyCode::Backspace => {
            query.pop();
            let live = query.clone();
            app.filter.query = live;
            app.rebuild();
        }
        KeyCode::Char(ch) => {
            query.push(ch);
            let live = query.clone();
            app.filter.query = live;
            app.rebuild();
        }
        _ => {}
    }
    Effect::None
}

fn modal_key(app: &mut App, key: KeyEvent) -> Effect {
    match (&app.modal, key.code) {
        (_, KeyCode::Esc | KeyCode::Char('q')) => {
            app.modal = None;
            app.touch();
            Effect::None
        }
        (Some(Modal::Confirm { action, .. }), KeyCode::Char('y') | KeyCode::Enter) => {
            let action = action.clone();
            app.modal = None;
            run_action(app, action)
        }
        // The command menu: `x` opened it, the next key picks a command.
        (Some(Modal::CommandMenu), KeyCode::Char('d')) => {
            app.modal = None;
            start_capture(app)
        }
        (Some(Modal::CommandMenu), KeyCode::Char('s')) => {
            app.modal = None;
            app.stop_capture();
            app.set_status("capture stopped");
            Effect::None
        }
        (Some(Modal::CommandMenu), KeyCode::Char('k')) => {
            app.modal = None;
            request_signal(app, "TERM")
        }
        (Some(Modal::CommandMenu), KeyCode::Char('o')) => {
            app.modal = None;
            switch_to_pane(app)
        }
        (Some(Modal::CommandMenu), KeyCode::Char('c')) => {
            app.modal = None;
            copy_selection(app);
            Effect::None
        }
        (Some(Modal::CommandMenu), KeyCode::Char('t')) => {
            // Sort submenu — choose directly instead of cycling.
            app.modal = Some(Modal::SortMenu);
            app.touch();
            Effect::None
        }
        // Sort menu: number keys select the ordering directly.
        (Some(Modal::SortMenu), KeyCode::Char(ch @ '1'..='5')) => {
            let sort = match ch {
                '1' => Sort::Tree,
                '2' => Sort::Cpu,
                '3' => Sort::Memory,
                '4' => Sort::Age,
                '5' => Sort::Connections,
                _ => unreachable!(),
            };
            app.modal = None;
            app.sort = sort;
            app.set_status(format!(
                "sort: {}{}",
                app.sort.label(),
                if app.sort.is_flat() { " (flat)" } else { "" }
            ));
            app.rebuild();
            Effect::None
        }
        _ => {
            app.touch();
            Effect::None
        }
    }
}

fn tree_key(app: &mut App, key: KeyEvent) -> Effect {
    // Ctrl+C/Z/\/S/Q are terminal-reserved and must reach the terminal, so any
    // Ctrl-modified key that is not explicitly ours is ignored rather than
    // swallowed.
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return Effect::None;
    }

    match key.code {
        KeyCode::Char('q') => return Effect::Quit,
        KeyCode::Char('?') => {
            app.modal = Some(Modal::Help);
            app.touch();
        }
        KeyCode::Char('!') => {
            app.modal = Some(Modal::Diagnostics);
            app.touch();
        }

        // Navigation.
        KeyCode::Char('j') | KeyCode::Down => move_selection(app, 1),
        KeyCode::Char('k') | KeyCode::Up => move_selection(app, -1),
        KeyCode::Char('g') | KeyCode::Home => {
            app.selected = 0;
            after_move(app);
        }
        KeyCode::Char('G') | KeyCode::End => {
            app.selected = app.rows.len().saturating_sub(1);
            after_move(app);
        }
        KeyCode::PageDown => move_selection(app, 10),
        KeyCode::PageUp => move_selection(app, -10),

        // Expand / collapse. `l`/`h` also move, so a single key both opens a
        // node and steps into it — the ranger/yazi reflex.
        KeyCode::Char(' ') | KeyCode::Enter if enter_expands(app) => app.expansion_toggle(),
        KeyCode::Char('l') | KeyCode::Right => {
            let Some(row) = app.selected_row() else {
                return Effect::None;
            };
            if row.expandable && !row.expanded {
                app.expansion_toggle();
            } else {
                move_selection(app, 1);
            }
        }
        KeyCode::Char('h') | KeyCode::Left => {
            let Some(row) = app.selected_row() else {
                return Effect::None;
            };
            if row.expanded {
                let id = row.id.clone();
                app.mark_expansion_manual();
                app.expansion.collapse(&id);
                app.rebuild();
            } else {
                // Jump to the parent, which is the nearest shallower row above.
                let depth = row.depth;
                if let Some(parent) = app.rows[..app.selected]
                    .iter()
                    .rposition(|other| other.depth < depth)
                {
                    app.selected = parent;
                    after_move(app);
                }
            }
        }
        KeyCode::Char('E') => {
            // Works from the snapshot, not the rendered rows: rows only exist
            // one level below what is already open, so a row-based expand would
            // need one press per level.
            app.mark_expansion_manual();
            app.expansion.expand_everything(&app.snapshot);
            app.rebuild();
        }
        KeyCode::Char('C') => {
            app.mark_expansion_manual();
            app.expansion.collapse_all_procs();
            app.rebuild();
        }

        // Facets.
        KeyCode::Char(']') | KeyCode::Tab => cycle_facet(app, 1),
        KeyCode::Char('[') | KeyCode::BackTab => cycle_facet(app, -1),
        KeyCode::Char('1') => set_facet(app, Facet::Overview),
        KeyCode::Char('2') => set_facet(app, Facet::Network),
        KeyCode::Char('3') => set_facet(app, Facet::Files),
        KeyCode::Char('4') => set_facet(app, Facet::Output),
        KeyCode::Char('5') => set_facet(app, Facet::Streams),
        KeyCode::Char('6') => set_facet(app, Facet::Stack),
        KeyCode::Char('7') => set_facet(app, Facet::Env),
        KeyCode::Char('8') => set_facet(app, Facet::Packets),
        // Sampling is an action, not a view change: it blocks for a second, so it
        // gets a key of its own rather than firing when the facet is opened.
        KeyCode::Char('S') => app.sample_selected(),

        // Jumps. Distinct from h/l, which move within the display: these follow a
        // *relationship*, which may lead anywhere in the tree.
        KeyCode::Char('p') => app.jump_to_parent(),
        KeyCode::Char('P') => app.jump_to_peer(),

        // Filter, scope and noise stay as single keys — they are frequent and
        // need to work fast. Sort moved to `xt` so it can be picked directly
        // rather than cycled through.
        KeyCode::Char('/') => {
            app.filter_input = Some(app.filter.query.clone());
            app.touch();
        }
        KeyCode::Esc => {
            if app.filter.is_active() {
                app.filter.query.clear();
                app.rebuild();
            }
        }
        KeyCode::Char('w') => {
            app.scope = match app.scope {
                Scope::CurrentWindow => Scope::Server,
                Scope::Server => Scope::CurrentWindow,
            };
            let label = match app.scope {
                Scope::CurrentWindow => "this window",
                Scope::Server => "whole server",
            };
            app.reset_expansion_for_scope();
            app.set_status(format!("scope: {label}"));
            app.rebuild();
        }
        KeyCode::Char('a') => {
            app.noise = match app.noise {
                Noise::Hide => Noise::Show,
                Noise::Show => Noise::Hide,
            };
            let label = if app.noise == Noise::Show {
                "all processes"
            } else {
                "interesting only"
            };
            app.set_status(format!("showing {label}"));
            app.rebuild();
        }

        // `x` opens the command menu — a two-keystroke prefix for extended
        // actions (dump, stop, kill, switch, copy, sort). Keeps the main keymap
        // clean while leaving room for more commands without collisions.
        KeyCode::Char('x') => {
            app.modal = Some(Modal::CommandMenu);
            app.touch();
        }

        // Actions that stay as single keys: frequent, or need to work fast.
        KeyCode::Char('r') => {
            app.collector.request();
            // A manual refresh is also the way to re-read a stale pane capture.
            app.refresh_pane_output();
            app.refresh_streams();
            app.set_status("refreshing…");
        }

        _ => {}
    }
    Effect::None
}

/// `Enter` expands a group but activates a pane — a pane row's natural action is
/// "take me there", not "show me one more level".
fn enter_expands(app: &App) -> bool {
    !matches!(
        app.selected_row().map(|row| &row.kind),
        Some(Kind::Pane { .. })
    )
}

fn move_selection(app: &mut App, delta: isize) {
    if app.rows.is_empty() {
        return;
    }
    let last = app.rows.len() - 1;
    app.selected = (app.selected as isize + delta).clamp(0, last as isize) as usize;
    after_move(app);
}

/// Navigation invalidates anything bound to the previous row.
fn after_move(app: &mut App) {
    app.stop_capture();
    app.capture_lines.clear();
    app.ensure_facet_data();
    app.touch();
}

fn cycle_facet(app: &mut App, delta: isize) {
    let current = Facet::ALL
        .iter()
        .position(|facet| *facet == app.facet)
        .unwrap_or(0);
    let count = Facet::ALL.len() as isize;
    let next = ((current as isize + delta).rem_euclid(count)) as usize;
    set_facet(app, Facet::ALL[next]);
}

fn set_facet(app: &mut App, facet: Facet) {
    if app.facet == facet {
        return;
    }
    // Leaving the packets facet stops the capture — a tap must not keep running
    // out of sight.
    if app.facet == Facet::Packets {
        app.stop_capture();
    }
    app.facet = facet;
    app.ensure_facet_data();
    app.touch();
}

/// Ask to start a capture. Nothing runs until the modal is confirmed, and the
/// modal shows the exact command line.
fn start_capture(app: &mut App) -> Effect {
    set_facet(app, Facet::Packets);

    if let Some(container) = app.selected_container().cloned() {
        if !container.running {
            app.set_error("container is not running");
            return Effect::None;
        }
        let mut command = capture::container_command_line(&container.id);
        if !collect::container::sidecar_image_present() {
            command.push_str("\n\n(will pull ");
            command.push_str(collect::container::SIDECAR_IMAGE);
            command.push_str(" first — this takes a moment)");
        }
        app.modal = Some(Modal::Confirm {
            title: format!("capture packets in container {}", container.name),
            command,
            action: PendingAction::CaptureContainer {
                id: container.id,
                name: container.name,
            },
        });
        app.touch();
        return Effect::None;
    }

    let Some(proc) = app.selected_proc().cloned() else {
        app.set_error("select a process or container to capture");
        return Effect::None;
    };
    // Container processes capture through their container's namespace, not the
    // host's — the host has no route to them.
    if let Origin::Container(id) = &proc.key.origin {
        let name = app
            .snapshot
            .container(id)
            .map(|c| c.name.clone())
            .unwrap_or_default();
        app.modal = Some(Modal::Confirm {
            title: format!("capture packets in container {name}"),
            command: capture::container_command_line(id),
            action: PendingAction::CaptureContainer {
                id: id.clone(),
                name,
            },
        });
        app.touch();
        return Effect::None;
    }

    let sockets = app.selected_sockets().to_vec();
    match capture::check_host_capture(&sockets) {
        Ok(filter) => {
            let interface = capture::host_interface(&sockets);
            let mut command = capture::host_command_line(proc.key.pid, &interface, &filter);
            if capture::host_capture_needs_sudo() {
                command.push_str("\n\nmacOS restricts /dev/bpf* to the access_bpf group,");
                command.push_str("\nso this needs sudo. You will be prompted in the terminal.");
            }
            app.modal = Some(Modal::Confirm {
                title: format!("capture packets for {} (pid {})", proc.name(), proc.key.pid),
                command,
                action: PendingAction::CaptureHost {
                    pid: proc.key.pid,
                    interface,
                    filter,
                },
            });
            app.touch();
        }
        Err(error) => app.set_error(error.to_string()),
    }
    Effect::None
}

fn request_signal(app: &mut App, signal: &'static str) -> Effect {
    let Some(proc) = app.selected_proc().cloned() else {
        app.set_error("select a process first");
        return Effect::None;
    };
    if let Origin::Container(_) = proc.key.origin {
        app.set_error("signalling container processes is not supported — use docker stop");
        return Effect::None;
    }
    app.modal = Some(Modal::Confirm {
        title: format!("send SIG{signal} to {} (pid {})", proc.name(), proc.key.pid),
        command: format!("kill -{signal} {}\n\n{}", proc.key.pid, proc.command),
        action: PendingAction::Signal {
            pid: proc.key.pid,
            signal,
        },
    });
    app.touch();
    Effect::None
}

fn run_action(app: &mut App, action: PendingAction) -> Effect {
    match action {
        // A sudo password prompt needs the real terminal, so this one goes back
        // to the caller rather than running here.
        PendingAction::CaptureHost { .. } => Effect::RunOnRealTerminal(action),
        PendingAction::CaptureContainer { id, name } => {
            app.capture_lines.clear();
            match capture::start_container(&id, &name) {
                Ok(capture) => {
                    app.capture_lines
                        .push(format!("$ {}", capture.command_line));
                    app.capture = Some(capture);
                    app.set_status("capturing…");
                }
                Err(error) => app.set_error(format!("capture failed: {error}")),
            }
            Effect::None
        }
        PendingAction::Signal { pid, signal } => {
            match collect::cmd::run(
                "kill",
                &[&format!("-{signal}"), &pid.to_string()],
                collect::cmd::FAST,
            ) {
                Ok(_) => {
                    app.set_status(format!("sent SIG{signal} to {pid}"));
                    app.collector.request();
                }
                Err(error) => app.set_error(format!("kill: {error}")),
            }
            Effect::None
        }
    }
}

fn switch_to_pane(app: &mut App) -> Effect {
    let Some(target) = app.selected_pane_target() else {
        app.set_error("no pane for this row");
        return Effect::None;
    };
    if !collect::tmux::inside_tmux() {
        app.set_error("not inside tmux — cannot switch panes");
        return Effect::None;
    }
    match collect::tmux::switch_to(&target) {
        // The tmux client is now showing another pane; tpx keeps running in the
        // pane it started in, which the user can come back to.
        Ok(()) => app.set_status(format!("switched to {target}")),
        Err(error) => app.set_error(format!("tmux: {error}")),
    }
    Effect::None
}

/// Copy the selected row's most useful identifier — pid, container id, or pane
/// target — via the tmux buffer, which works over SSH where pbcopy does not.
fn copy_selection(app: &mut App) {
    let Some(row) = app.selected_row() else {
        return;
    };
    let value = match &row.kind {
        Kind::Process { proc } => proc.key.pid.to_string(),
        Kind::Container { container } => container.short_id.clone(),
        Kind::Pane { pane } => pane.target.clone(),
        _ => row.label(),
    };
    match collect::cmd::run("tmux", &["set-buffer", "--", &value], collect::cmd::FAST) {
        Ok(_) => app.set_status(format!("copied {value}")),
        Err(error) => app.set_error(format!("tmux set-buffer: {error}")),
    }
}

/// Rows the help overlay and footer are generated from — one list, so they
/// cannot disagree with [`handle`].
pub const KEYMAP: &[(&str, &str)] = &[
    ("j/k ↑↓", "move"),
    ("h/l ←→", "collapse / expand"),
    ("Space", "toggle node"),
    ("g/G", "top / bottom"),
    ("E/C", "expand / collapse all process trees"),
    ("Tab [ ]", "cycle facet"),
    (
        "1..6",
        "overview / net / files / output / streams / packets",
    ),
    ("/", "fuzzy filter (Esc clears)"),
    ("p / P", "jump to parent process / connected peer"),
    (
        "s",
        "cycle sort: tree / cpu / memory / newest / connections",
    ),
    ("w", "widen scope: this window <-> whole server"),
    ("a", "toggle noise (show every process)"),
    ("r", "refresh"),
    (
        "x…",
        "extended commands: xd dump / xs stop / xk kill / xo switch / xc copy / xt sort",
    ),
    ("!", "diagnostics — collector errors, port conflicts"),
    ("?", "this help"),
    ("q", "quit"),
];

/// The 6 hints that stay visible in the footer.
pub const FOOTER_HINTS: &[(&str, &str)] = &[
    ("j/k", "move"),
    ("h/l", "fold"),
    ("Tab", "facet"),
    ("/", "filter"),
    ("x", "cmds"),
    ("?", "help"),
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Pane, Proc, ProcKey, Snapshot};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn app_with_rows() -> App {
        let mut app = App::new(crate::tree::Scope::Server);
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
        for (pid, ppid) in [(100u32, 1u32), (101, 100)] {
            let proc = Proc {
                key: ProcKey::host(pid),
                ppid,
                command: format!("proc-{pid}"),
                age_secs: 1,
                cpu_pct: 5.0,
                cpu_time_secs: 0.0,
                rss_bytes: 1024,
                state: "S".into(),
                threads: None,
                fd_count: None,
            };
            snapshot.children.entry(ppid).or_default().push(pid);
            snapshot.procs.insert(proc.key.clone(), proc);
        }
        app.snapshot = snapshot;
        app.rebuild();
        app
    }

    #[test]
    fn filter_mode_swallows_q_instead_of_quitting() {
        let mut app = app_with_rows();
        handle(&mut app, key(KeyCode::Char('/')));
        let effect = handle(&mut app, key(KeyCode::Char('q')));
        assert!(matches!(effect, Effect::None));
        assert_eq!(app.filter_input.as_deref(), Some("q"));
    }

    #[test]
    fn escape_cancels_filter_editing() {
        let mut app = app_with_rows();
        handle(&mut app, key(KeyCode::Char('/')));
        handle(&mut app, key(KeyCode::Char('z')));
        handle(&mut app, key(KeyCode::Esc));
        assert!(app.filter_input.is_none());
        // Filtering is live, so cancel must also drop the applied query —
        // otherwise the tree stays filtered by an abandoned search.
        assert!(!app.filter.is_active());
    }

    #[test]
    fn enter_commits_the_filter_and_leaves_edit_mode() {
        let mut app = app_with_rows();
        handle(&mut app, key(KeyCode::Char('/')));
        handle(&mut app, key(KeyCode::Char('p')));
        handle(&mut app, key(KeyCode::Enter));
        assert!(app.filter_input.is_none());
        assert_eq!(app.filter.query, "p");
    }

    #[test]
    fn q_quits_from_the_tree() {
        let mut app = app_with_rows();
        assert!(matches!(
            handle(&mut app, key(KeyCode::Char('q'))),
            Effect::Quit
        ));
    }

    #[test]
    fn ctrl_modified_keys_are_left_to_the_terminal() {
        let mut app = app_with_rows();
        for ch in ['c', 'z', 's', 'q', '\\'] {
            let event = KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL);
            assert!(matches!(handle(&mut app, event), Effect::None));
            assert!(!app.should_quit);
        }
    }

    #[test]
    fn navigation_keys_do_not_leave_the_row_range() {
        let mut app = app_with_rows();
        for _ in 0..50 {
            handle(&mut app, key(KeyCode::Char('j')));
        }
        assert_eq!(app.selected, app.rows.len() - 1);
        for _ in 0..50 {
            handle(&mut app, key(KeyCode::Char('k')));
        }
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn facet_cycling_wraps_both_ways() {
        let mut app = app_with_rows();
        assert_eq!(app.facet, Facet::Overview);
        handle(&mut app, key(KeyCode::Char('[')));
        assert_eq!(
            app.facet,
            Facet::Packets,
            "cycling back from the first wraps"
        );
        handle(&mut app, key(KeyCode::Char(']')));
        assert_eq!(app.facet, Facet::Overview);
    }

    #[test]
    fn number_keys_jump_straight_to_a_facet() {
        let mut app = app_with_rows();
        handle(&mut app, key(KeyCode::Char('2')));
        assert_eq!(app.facet, Facet::Network);
        handle(&mut app, key(KeyCode::Char('5')));
        assert_eq!(app.facet, Facet::Streams);
        handle(&mut app, key(KeyCode::Char('6')));
        assert_eq!(app.facet, Facet::Stack);
        handle(&mut app, key(KeyCode::Char('8')));
        assert_eq!(app.facet, Facet::Packets);
    }

    #[test]
    fn noise_toggle_flips_and_reports() {
        let mut app = app_with_rows();
        assert_eq!(app.noise, Noise::Hide);
        handle(&mut app, key(KeyCode::Char('a')));
        assert_eq!(app.noise, Noise::Show);
        assert!(app.status.is_some());
    }

    #[test]
    fn capture_on_a_process_without_sockets_errors_rather_than_tapping_everything() {
        let mut app = app_with_rows();
        // Select the shell process row; it holds no sockets in this fixture.
        handle(&mut app, key(KeyCode::Char('j')));
        handle(&mut app, key(KeyCode::Char('j')));
        handle(&mut app, key(KeyCode::Char('x')));
        handle(&mut app, key(KeyCode::Char('d')));
        assert!(
            app.modal.is_none(),
            "no confirmation for an impossible capture"
        );
        assert!(app.status.as_ref().is_some_and(|status| status.is_error));
    }

    #[test]
    fn signal_requires_confirmation_and_shows_the_command() {
        let mut app = app_with_rows();
        let process_row = app
            .rows
            .iter()
            .position(|row| matches!(row.kind, Kind::Process { .. }))
            .expect("fixture has a process row");
        app.selected = process_row;
        handle(&mut app, key(KeyCode::Char('x')));
        handle(&mut app, key(KeyCode::Char('k')));
        match &app.modal {
            Some(Modal::Confirm { command, .. }) => assert!(command.starts_with("kill -TERM")),
            _ => panic!("expected a confirmation modal"),
        }
    }

    #[test]
    fn escape_closes_a_modal_without_running_it() {
        let mut app = app_with_rows();
        app.selected = app
            .rows
            .iter()
            .position(|row| matches!(row.kind, Kind::Process { .. }))
            .unwrap();
        handle(&mut app, key(KeyCode::Char('x')));
        handle(&mut app, key(KeyCode::Char('k')));
        assert!(app.modal.is_some());
        handle(&mut app, key(KeyCode::Esc));
        assert!(app.modal.is_none());
    }

    #[test]
    fn help_and_footer_are_generated_from_one_keymap() {
        // Every footer hint must name a key the keymap documents, or the footer
        // is advertising something that may not exist.
        for (hint_key, _) in FOOTER_HINTS {
            let first = hint_key.split('/').next().unwrap();
            assert!(
                KEYMAP.iter().any(|(keys, _)| keys.contains(first)),
                "footer hint {hint_key} is not in KEYMAP"
            );
        }
    }

    #[test]
    fn collapse_on_a_leaf_jumps_to_the_parent() {
        let mut app = app_with_rows();
        let deepest = app.rows.len() - 1;
        app.selected = deepest;
        let depth = app.rows[deepest].depth;
        handle(&mut app, key(KeyCode::Char('h')));
        assert!(app.rows[app.selected].depth < depth);
    }
}

#[cfg(test)]
mod capture_tests {
    use super::*;
    use crate::model::{Pane, Proc, ProcKey, Proto, Snapshot, Socket, SocketState};

    /// A pane whose shell holds a listening socket — the shape `d` must accept.
    fn app_with_socket_holder() -> App {
        let mut app = App::new(crate::tree::Scope::Server);
        let mut snapshot = Snapshot::default();
        snapshot.panes = vec![Pane {
            session: "local".into(),
            window_index: 1,
            window_name: "w".into(),
            pane_index: 1,
            target: "local:1.1".into(),
            cwd: "/src".into(),
            current_command: "server".into(),
            pid: 100,
            active: true,
            window_active: true,
            session_attached: true,
            zoomed: false,
        }];
        let proc = Proc {
            key: ProcKey::host(100),
            ppid: 1,
            command: "node server.js".into(),
            age_secs: 60,
            cpu_pct: 5.0,
            cpu_time_secs: 3.0,
            rss_bytes: 1024,
            state: "S".into(),
            threads: None,
            fd_count: None,
        };
        snapshot.children.insert(1, vec![100]);
        snapshot.procs.insert(proc.key.clone(), proc);
        snapshot.sockets.insert(
            ProcKey::host(100),
            vec![Socket {
                proto: Proto::Tcp,
                local: "*:8080".into(),
                peer: None,
                state: SocketState::Listen,
            }],
        );
        app.snapshot = snapshot;
        app.rebuild();
        app
    }

    #[test]
    fn d_on_a_socket_holding_process_opens_a_confirmation() {
        let mut app = app_with_socket_holder();
        let process_row = app
            .rows
            .iter()
            .position(|row| matches!(row.kind, Kind::Process { .. }))
            .expect("fixture has a process row");
        app.selected = process_row;

        handle(
            &mut app,
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
        );
        handle(
            &mut app,
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE),
        );

        match &app.modal {
            Some(Modal::Confirm { command, .. }) => {
                assert!(command.contains("tcpdump"), "got: {command}");
                assert!(
                    command.contains("port 8080"),
                    "filter must scope to the process"
                );
            }
            _ => panic!(
                "expected a confirmation modal, status={:?}",
                app.status.as_ref().map(|s| &s.message)
            ),
        }
    }
}

#[cfg(test)]
mod expand_tests {
    use super::*;
    use crate::model::{Pane, Proc, ProcKey, Snapshot};

    /// A pane with a 4-deep chain: shell → a → b → c.
    fn deep_app() -> App {
        let mut app = App::new(crate::tree::Scope::Server);
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
        for (pid, ppid) in [(100u32, 1u32), (101, 100), (102, 101), (103, 102)] {
            let proc = Proc {
                key: ProcKey::host(pid),
                ppid,
                command: format!("level-{pid}"),
                age_secs: 10,
                cpu_pct: 0.0,
                cpu_time_secs: 0.0,
                rss_bytes: 0,
                state: "S".into(),
                threads: None,
                fd_count: None,
            };
            snapshot.children.entry(ppid).or_default().push(pid);
            snapshot.procs.insert(proc.key.clone(), proc);
        }
        app.snapshot = snapshot;
        app.rebuild();
        app
    }

    #[test]
    fn one_press_of_expand_all_opens_every_level() {
        let mut app = deep_app();
        handle(
            &mut app,
            KeyEvent::new(KeyCode::Char('E'), KeyModifiers::NONE),
        );
        // The deepest process must be visible after a single press.
        assert!(
            app.rows.iter().any(|row| row.label() == "level-103"),
            "rows: {:?}",
            app.rows.iter().map(|r| r.label()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn collapse_all_returns_every_process_level() {
        let mut app = deep_app();
        handle(
            &mut app,
            KeyEvent::new(KeyCode::Char('E'), KeyModifiers::NONE),
        );
        handle(
            &mut app,
            KeyEvent::new(KeyCode::Char('C'), KeyModifiers::NONE),
        );
        assert!(!app.rows.iter().any(|row| row.label() == "level-103"));
        // The pane and its shell stay — collapsing processes is not collapsing groups.
        assert!(app.rows.iter().any(|row| row.label() == "level-100"));
    }
}
