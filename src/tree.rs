//! The navigable tree: session → window → pane → process → container.
//!
//! Rendered as a flat list of [`Row`]s with a depth, because a terminal list
//! widget scrolls and selects over a flat sequence. The tree structure lives in
//! `depth` + the expand/collapse set, not in nested widgets.

use std::collections::{HashMap, HashSet};

mod filter;

pub use filter::Filter;
use filter::retain_matches_with_ancestors;

use crate::model::{Container, Pane, Proc, ProcKey, Rollup, Snapshot, SocketState};

/// Identity of a row, stable across snapshots so selection and expansion
/// survive a refresh. A row index would not — processes come and go.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum NodeId {
    Session(String),
    /// `session:window`
    Window(String, u32),
    /// Pane target string.
    Pane(String),
    Process(ProcKey),
    Container(String),
}

/// What a row represents, with everything the renderer needs.
#[derive(Clone, Debug)]
pub enum Kind {
    Session {
        name: String,
        attached: bool,
        window_count: u32,
    },
    Window {
        name: String,
        index: u32,
        active: bool,
        pane_count: u32,
        zoomed: bool,
    },
    Pane {
        pane: Pane,
    },
    Process {
        proc: Proc,
    },
    Container {
        container: Container,
    },
}

/// One line in the tree view.
#[derive(Clone, Debug)]
pub struct Row {
    pub id: NodeId,
    pub kind: Kind,
    pub depth: u16,
    /// Whether this row has children that expansion would reveal.
    pub expandable: bool,
    pub expanded: bool,
    /// Aggregate usage of this row's whole subtree, for group headers.
    pub rollup: Rollup,
    /// Ports this row's process listens on, and whether any is contested.
    pub listen_ports: Vec<u16>,
    pub port_conflict: bool,
    /// Established connection count, so a busy process reads as busy without
    /// expanding it.
    pub connections: u32,
    /// Which pane this row belongs to, set only in flat orderings where the tree
    /// no longer shows it.
    pub flat_context: Option<String>,
    /// The cwd of the pane this process belongs to — the session root for a
    /// Claude process, available in both tree and flat orderings.
    pub pane_cwd: Option<String>,
}

impl Row {
    /// Short label for the row — the tree's primary column.
    pub fn label(&self) -> String {
        match &self.kind {
            Kind::Session { name, .. } => name.clone(),
            Kind::Window { name, index, .. } => format!("{index}:{name}"),
            Kind::Pane { pane } => format!("{}.{}", pane.window_index, pane.pane_index),
            Kind::Process { proc } => proc.name().to_string(),
            Kind::Container { container } => container.display_name(),
        }
    }

    /// This row's *own* cpu, not its subtree's.
    ///
    /// Sorting by the rollup would rank every ancestor above its own hot child —
    /// a shell whose grandchild burns a core would outrank the grandchild.
    pub fn own_cpu(&self) -> f32 {
        match &self.kind {
            Kind::Process { proc } => proc.cpu_pct,
            Kind::Container { container } => container
                .metrics
                .as_ref()
                .map(|metrics| metrics.cpu_pct)
                .unwrap_or(0.0),
            _ => self.rollup.cpu_pct,
        }
    }

    pub fn own_rss(&self) -> u64 {
        match &self.kind {
            Kind::Process { proc } => proc.rss_bytes,
            Kind::Container { container } => container
                .metrics
                .as_ref()
                .map(|metrics| metrics.mem_bytes)
                .unwrap_or(0),
            _ => self.rollup.rss_bytes,
        }
    }

    /// Age in seconds; a container reports its uptime through its init process, so
    /// without one it sorts as oldest rather than newest.
    pub fn own_age(&self) -> u64 {
        match &self.kind {
            Kind::Process { proc } => proc.age_secs,
            _ => u64::MAX,
        }
    }

    pub fn is_group(&self) -> bool {
        matches!(self.kind, Kind::Session { .. } | Kind::Window { .. })
    }
}

/// Expansion state, keyed by [`NodeId`] so it survives refreshes.
#[derive(Default)]
pub struct Expansion {
    collapsed: HashSet<NodeId>,
    /// Processes expand on demand: a full process tree per pane is usually
    /// noise, so pane children start collapsed below the shell.
    expanded_procs: HashSet<NodeId>,
}

impl Expansion {
    /// Groups (session/window/pane) default to expanded; process and container
    /// subtrees do not. That default makes the first screen a map of *where*
    /// work is happening rather than a wall of every child process — and for a
    /// container it is required, since its process list is fetched on demand and
    /// a row claiming to be open with nothing under it is a lie.
    pub fn is_expanded(&self, id: &NodeId) -> bool {
        match id {
            NodeId::Process(_) | NodeId::Container(_) => self.expanded_procs.contains(id),
            _ => !self.collapsed.contains(id),
        }
    }

    /// Expansion as seen while a filter is active: everything is open.
    ///
    /// A filter searches the whole tree, not the visible rows. Without this,
    /// `/claude` finds nothing whenever the matching process sits under an
    /// unexpanded shell — the filter would only search what the reader had
    /// already opened by hand, which is the opposite of what a search is for.
    fn is_expanded_for(&self, id: &NodeId, filtering: bool) -> bool {
        // Containers are excluded: their process list is fetched on demand, so
        // force-opening them would fire a `docker exec` per container on every
        // keystroke. Their own row still matches by name and image.
        if matches!(id, NodeId::Container(_)) {
            return self.is_expanded(id);
        }
        filtering || self.is_expanded(id)
    }

    pub fn toggle(&mut self, id: &NodeId) {
        match id {
            NodeId::Process(_) | NodeId::Container(_) => {
                if !self.expanded_procs.remove(id) {
                    self.expanded_procs.insert(id.clone());
                }
            }
            _ => {
                if !self.collapsed.remove(id) {
                    self.collapsed.insert(id.clone());
                }
            }
        }
    }

    pub fn expand(&mut self, id: &NodeId) {
        match id {
            NodeId::Process(_) | NodeId::Container(_) => {
                self.expanded_procs.insert(id.clone());
            }
            _ => {
                self.collapsed.remove(id);
            }
        }
    }

    pub fn collapse(&mut self, id: &NodeId) {
        match id {
            NodeId::Process(_) | NodeId::Container(_) => {
                self.expanded_procs.remove(id);
            }
            _ => {
                self.collapsed.insert(id.clone());
            }
        }
    }

    /// Expand every process subtree currently in the tree. `E` — for when the
    /// question is "what is running anywhere" rather than "where".
    ///
    /// Only affects rows that exist *now*: a subtree one level deeper does not
    /// have rows until the tree is rebuilt. Callers that mean "open everything"
    /// must therefore loop, or use [`Self::expand_everything`].
    pub fn expand_all_procs(&mut self, rows: &[Row]) {
        for row in rows {
            if matches!(row.id, NodeId::Process(_)) {
                self.expanded_procs.insert(row.id.clone());
            }
        }
    }

    /// Mark every process in a snapshot expanded, regardless of what is
    /// currently rendered.
    ///
    /// [`Self::expand_all_procs`] can only see rows that already exist, so one
    /// call opens exactly one more level — `--plain` output silently stopped two
    /// levels down (at the login shell), hiding the `ccproxy → claude → mcp`
    /// chains that are the whole point of the view. Working from the snapshot
    /// instead of the rows has no such horizon.
    pub fn expand_everything(&mut self, snapshot: &Snapshot) {
        for key in snapshot.procs.keys() {
            self.expanded_procs.insert(NodeId::Process(key.clone()));
        }
        for procs in snapshot.container_procs.values() {
            for proc in procs {
                self.expanded_procs
                    .insert(NodeId::Process(proc.key.clone()));
            }
        }
        for container in &snapshot.containers {
            self.expanded_procs
                .insert(NodeId::Container(container.id.clone()));
        }
    }

    pub fn collapse_all_procs(&mut self) {
        self.expanded_procs.clear();
    }
}

/// Which processes to hide by default.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Noise {
    /// Hide bare shells with no interesting children — the common case, where
    /// an idle pane is one row instead of a subtree.
    Hide,
    Show,
}

/// How rows are ordered, and whether the tree structure is kept.
///
/// The tree answers "where is this running"; a sorted flat list answers "what is
/// heaviest right now". Both are needed, and mixing them is worse than either:
/// sorting *within* the tree buries the top consumer under whatever session it
/// happens to live in.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Sort {
    /// tmux order: session, window, pane index. The default, because it matches
    /// the reader's own layout.
    #[default]
    Tree,
    /// Flat, heaviest cpu first.
    Cpu,
    /// Flat, largest resident set first.
    Memory,
    /// Flat, newest first — what did I just start.
    Age,
    /// Flat, most established connections first.
    Connections,
}

impl Sort {
    /// The order `s` cycles through.
    pub const CYCLE: [Sort; 5] = [
        Sort::Tree,
        Sort::Cpu,
        Sort::Memory,
        Sort::Age,
        Sort::Connections,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Sort::Tree => "tree",
            Sort::Cpu => "cpu",
            Sort::Memory => "memory",
            Sort::Age => "newest",
            Sort::Connections => "connections",
        }
    }

    pub fn next(self) -> Sort {
        let index = Self::CYCLE
            .iter()
            .position(|sort| *sort == self)
            .unwrap_or(0);
        Self::CYCLE[(index + 1) % Self::CYCLE.len()]
    }

    /// Whether this ordering flattens the tree.
    pub fn is_flat(self) -> bool {
        self != Sort::Tree
    }
}

/// Flatten a built tree into process rows only, ordered by `sort`.
///
/// Group rows are dropped: a session header has no cpu of its own to rank, and
/// keeping them would leave headers stranded among unrelated processes. The pane
/// each process belongs to is carried on the row instead, so the reader does not
/// lose that context — see [`Row::flat_context`].
pub fn flatten(rows: Vec<Row>, sort: Sort) -> Vec<Row> {
    if !sort.is_flat() {
        return rows;
    }
    // Walking in order is what makes the pane knowable: a process row's pane is
    // the last pane row above it.
    let mut context: Option<String> = None;
    let mut flat: Vec<Row> = Vec::new();
    for mut row in rows {
        match &row.kind {
            Kind::Pane { pane } => context = Some(pane.target.clone()),
            Kind::Process { .. } | Kind::Container { .. } => {
                row.flat_context = context.clone();
                // Indentation is meaningless once the tree is gone, and keeping it
                // would imply a parent relationship the order no longer reflects.
                row.depth = 0;
                row.expandable = false;
                row.expanded = false;
                flat.push(row);
            }
            _ => {}
        }
    }

    match sort {
        Sort::Cpu => flat.sort_by(|a, b| {
            b.own_cpu()
                .partial_cmp(&a.own_cpu())
                .unwrap_or(std::cmp::Ordering::Equal)
        }),
        // Descending, so the key is negated rather than the comparison reversed.
        Sort::Memory => flat.sort_by_key(|row| std::cmp::Reverse(row.own_rss())),
        Sort::Age => flat.sort_by_key(|row| row.own_age()),
        Sort::Connections => flat.sort_by(|a, b| {
            b.connections
                .cmp(&a.connections)
                .then(b.listen_ports.len().cmp(&a.listen_ports.len()))
        }),
        Sort::Tree => {}
    }
    flat
}

/// How much of the tmux server the tree shows.
///
/// Scoping happens here, not in the collectors: port-conflict detection and
/// container attribution both need the whole server to be correct. Narrowing at
/// collection time would report "no conflict" for a port that a pane in another
/// window is in fact fighting over.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub enum Scope {
    /// Only the window tpx is running in. The default: the question is usually
    /// "what is running *here*", and 38 panes across 8 sessions buries that.
    #[default]
    CurrentWindow,
    /// Every session on the server.
    Server,
}
impl Scope {
    /// Whether a pane is in scope, given the window tpx runs in.
    ///
    /// `current` is `None` outside tmux; then even `CurrentWindow` shows
    /// everything, because there is no current window to narrow to and an empty
    /// tree would be worse than a wide one.
    fn includes(&self, pane: &Pane, current: Option<&(String, u32)>) -> bool {
        match (self, current) {
            (Scope::Server, _) | (Scope::CurrentWindow, None) => true,
            (Scope::CurrentWindow, Some((session, window))) => {
                pane.session == *session && pane.window_index == *window
            }
        }
    }
}

/// Everything the row builders need that does not change during one build.
///
/// Threading these five values through every builder pushed some signatures to
/// eight parameters, where the order of `&Snapshot` vs `&Expansion` vs the
/// conflict map is easy to get wrong silently. One borrow-only context makes the
/// call sites read as `build_pane(&ctx, pane)`.
struct Ctx<'a> {
    snapshot: &'a Snapshot,
    expansion: &'a Expansion,
    /// Whether a filter is active, which forces subtrees open so a match deeper
    /// than the reader has expanded is still found.
    filtering: bool,
    noise: Noise,
    /// Listen ports held by more than one process, computed once per build.
    conflicts: HashMap<u16, Vec<ProcKey>>,
    /// The cwd of the pane whose subtree is being built, so Claude session
    /// resolution works in tree order without app-level lookups.
    pane_cwd: Option<String>,
}

impl Ctx<'_> {
    fn is_expanded(&self, id: &NodeId) -> bool {
        self.expansion.is_expanded_for(id, self.filtering)
    }
}

/// Build the flat row list from a snapshot.
pub fn build(
    snapshot: &Snapshot,
    expansion: &Expansion,
    noise: Noise,
    filter: &Filter,
    scope: &Scope,
    current_window: Option<&(String, u32)>,
) -> Vec<Row> {
    // An active filter searches the whole tree, so every subtree is walked
    // regardless of what the reader has expanded by hand.
    let filtering = filter.is_active();
    let ctx = Ctx {
        snapshot,
        expansion,
        filtering,
        noise,
        conflicts: snapshot.port_conflicts(),
        pane_cwd: None,
    };
    let mut rows = Vec::new();

    // Sessions in tmux order; within a session, windows and panes by index —
    // the same order the user's own tmux status line shows, so the tree matches
    // their spatial memory of their own layout.
    for session in sessions_in_order(snapshot) {
        let session_panes: Vec<&Pane> = snapshot
            .panes
            .iter()
            .filter(|pane| pane.session == session)
            .filter(|pane| scope.includes(pane, current_window))
            .collect();
        // A session with nothing in scope contributes no rows at all — under
        // the default scope that is every session but one.
        if session_panes.is_empty() {
            continue;
        }
        let windows = windows_in_order(&session_panes);

        let session_id = NodeId::Session(session.clone());
        let session_rollup = rollup_of_panes(snapshot, &session_panes);
        let session_row = Row {
            id: session_id.clone(),
            kind: Kind::Session {
                name: session.clone(),
                attached: session_panes
                    .first()
                    .is_some_and(|pane| pane.session_attached),
                window_count: windows.len() as u32,
            },
            depth: 0,
            expandable: !windows.is_empty(),
            expanded: expansion.is_expanded_for(&session_id, filtering),
            rollup: session_rollup,
            listen_ports: Vec::new(),
            port_conflict: false,
            connections: 0,
            flat_context: None,
            pane_cwd: None,
        };

        let mut session_children = Vec::new();
        if session_row.expanded {
            for (window_index, window_panes) in windows {
                session_children.extend(build_window(&ctx, &session, window_index, &window_panes));
            }
        }

        // Keep matches plus the ancestors that give them context, and drop
        // groups left empty — a bare session header matching nothing is noise.
        if filtering {
            let kept = retain_matches_with_ancestors(session_children, filter);
            if kept.is_empty() {
                continue;
            }
            rows.push(session_row);
            rows.extend(kept);
        } else {
            rows.push(session_row);
            rows.extend(session_children);
        }
    }

    // Containers with no pane to hang under still exist and still burn
    // resources, so they get their own top-level group rather than vanishing —
    // on a machine with no compose labels and no live `docker` CLI to attribute
    // by, that is every container.
    //
    // Scoped out by default: a container that belongs to no pane belongs to no
    // *window* either, so under `CurrentWindow` it is the same server-wide noise
    // the scope exists to remove. `--server` (or `w`) brings it back.
    let orphans = if *scope == Scope::Server {
        unattributed_containers(snapshot)
    } else {
        Vec::new()
    };
    if !orphans.is_empty() {
        let group_id = NodeId::Session(CONTAINERS_GROUP.to_string());
        let expanded = expansion.is_expanded(&group_id);
        let mut rollup = Rollup::default();
        for container in &orphans {
            if let Some(metrics) = &container.metrics {
                rollup.cpu_pct += metrics.cpu_pct;
                rollup.rss_bytes += metrics.mem_bytes;
                rollup.proc_count += metrics.pids;
            }
        }
        let group_row = Row {
            id: group_id,
            kind: Kind::Session {
                name: CONTAINERS_GROUP.to_string(),
                attached: false,
                window_count: orphans.len() as u32,
            },
            depth: 0,
            expandable: true,
            expanded,
            rollup,
            listen_ports: Vec::new(),
            port_conflict: false,
            connections: 0,
            flat_context: None,
            pane_cwd: None,
        };

        let mut children = Vec::new();
        if expanded {
            for container in orphans {
                children.extend(build_container(&ctx, container, 1));
            }
        }
        if filter.is_active() {
            let kept: Vec<Row> = children
                .into_iter()
                .filter(|row| filter.matches_row(row))
                .collect();
            if !kept.is_empty() {
                rows.push(group_row);
                rows.extend(kept);
            }
        } else {
            rows.push(group_row);
            rows.extend(children);
        }
    }

    rows
}

/// Label of the synthetic group holding containers not tied to any pane. It uses
/// the session row kind because it is a top-level group, not a tmux session.
pub const CONTAINERS_GROUP: &str = "containers (no pane)";

/// Sessions in first-appearance order from the pane list, which is tmux's own
/// session order.
fn sessions_in_order(snapshot: &Snapshot) -> Vec<String> {
    let mut seen = Vec::new();
    for pane in &snapshot.panes {
        if !seen.contains(&pane.session) {
            seen.push(pane.session.clone());
        }
    }
    seen
}

fn windows_in_order<'a>(panes: &[&'a Pane]) -> Vec<(u32, Vec<&'a Pane>)> {
    let mut windows: Vec<(u32, Vec<&Pane>)> = Vec::new();
    for pane in panes {
        match windows
            .iter_mut()
            .find(|(index, _)| *index == pane.window_index)
        {
            Some((_, group)) => group.push(pane),
            None => windows.push((pane.window_index, vec![pane])),
        }
    }
    windows.sort_by_key(|(index, _)| *index);
    for (_, group) in windows.iter_mut() {
        group.sort_by_key(|pane| pane.pane_index);
    }
    windows
}

fn build_window(ctx: &Ctx, session: &str, window_index: u32, panes: &[&Pane]) -> Vec<Row> {
    let window_id = NodeId::Window(session.to_string(), window_index);
    let expanded = ctx.is_expanded(&window_id);
    let mut rows = vec![Row {
        id: window_id.clone(),
        kind: Kind::Window {
            name: panes
                .first()
                .map(|pane| pane.window_name.clone())
                .unwrap_or_default(),
            index: window_index,
            active: panes.iter().any(|pane| pane.window_active),
            pane_count: panes.len() as u32,
            // tmux reports zoom per window; every pane in the window carries the
            // same flag, so it belongs on the window row.
            zoomed: panes.iter().any(|pane| pane.zoomed),
        },
        depth: 1,
        expandable: !panes.is_empty(),
        expanded,
        rollup: rollup_of_panes(ctx.snapshot, panes),
        listen_ports: Vec::new(),
        port_conflict: false,
        connections: 0,
        flat_context: None,
        pane_cwd: None,
    }];

    if expanded {
        for pane in panes {
            rows.extend(build_pane(ctx, pane));
        }
    }
    rows
}

fn build_pane(ctx: &Ctx, pane: &Pane) -> Vec<Row> {
    let pane_id = NodeId::Pane(pane.target.clone());
    let expanded = ctx.is_expanded(&pane_id);
    let containers = containers_for_pane(ctx.snapshot, &pane.target);

    let mut rows = vec![Row {
        id: pane_id.clone(),
        kind: Kind::Pane {
            pane: (*pane).clone(),
        },
        depth: 2,
        expandable: true,
        expanded,
        rollup: rollup_of_pane(ctx.snapshot, pane),
        listen_ports: Vec::new(),
        port_conflict: false,
        connections: 0,
        flat_context: None,
        pane_cwd: None,
    }];

    if !expanded {
        return rows;
    }

    // The pane's own process tree, then any container attributed to it. A
    // container is a sibling of the shell, not a child: its processes live in
    // another pid namespace and are not descendants of the shell. The pane's
    // cwd is passed so Claude session ids can resolve deep in the subtree.
    let pane_cwd = Some(pane.cwd.clone());
    rows.extend(build_proc_subtree_with_cwd(
        ctx,
        pane.pid,
        3,
        pane_cwd.as_deref(),
    ));
    for container in containers {
        rows.extend(build_container(ctx, container, 3));
    }
    rows
}

/// Recursive host process subtree. `depth` is the render indent, and recursion
/// is bounded to keep a ppid cycle from blowing the stack.
/// Like [`build_proc_subtree`], but with a pane cwd carried for Claude session
/// resolution. The Ctx is borrowed, so rather than mutating it, a child Ctx is
/// constructed with the cwd field set.
fn build_proc_subtree_with_cwd(
    ctx: &Ctx,
    pid: u32,
    depth: u16,
    pane_cwd: Option<&str>,
) -> Vec<Row> {
    let child = Ctx {
        snapshot: ctx.snapshot,
        expansion: ctx.expansion,
        filtering: ctx.filtering,
        noise: ctx.noise,
        conflicts: ctx.conflicts.clone(),
        pane_cwd: pane_cwd.map(str::to_string),
    };
    build_proc_subtree(&child, pid, depth)
}

fn build_proc_subtree(ctx: &Ctx, pid: u32, depth: u16) -> Vec<Row> {
    const MAX_DEPTH: u16 = 32;
    if depth > MAX_DEPTH {
        return Vec::new();
    }
    let key = ProcKey::host(pid);
    let Some(proc) = ctx.snapshot.proc(&key) else {
        return Vec::new();
    };

    let children = ctx.snapshot.host_children(pid);
    let interesting_children: Vec<u32> = children
        .iter()
        .copied()
        .filter(|child| ctx.noise == Noise::Show || is_interesting(ctx.snapshot, *child))
        .collect();

    let id = NodeId::Process(key.clone());
    let expanded = ctx.is_expanded(&id);
    let sockets = ctx
        .snapshot
        .sockets
        .get(&key)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let listen_ports = listen_ports_of(sockets);
    let mut rows = vec![Row {
        id: id.clone(),
        kind: Kind::Process { proc: proc.clone() },
        depth,
        expandable: !interesting_children.is_empty(),
        expanded,
        rollup: subtree_rollup(ctx.snapshot, pid),
        port_conflict: listen_ports
            .iter()
            .any(|port| ctx.conflicts.contains_key(port)),
        listen_ports,
        connections: sockets
            .iter()
            .filter(|socket| socket.state == SocketState::Established)
            .count() as u32,
        flat_context: None,
        pane_cwd: ctx.pane_cwd.clone(),
    }];

    if expanded {
        for child in interesting_children {
            rows.extend(build_proc_subtree(ctx, child, depth + 1));
        }
    }
    rows
}

/// A process worth showing under [`Noise::Hide`]: it does something observable,
/// or something under it does.
///
/// cpu is deliberately not part of this test. It is a rate derived from two
/// snapshots, so on the first one every process reads 0% — a cpu-based rule would
/// make the tree visibly reshuffle a second after startup.
fn is_interesting(snapshot: &Snapshot, pid: u32) -> bool {
    let key = ProcKey::host(pid);
    let Some(proc) = snapshot.proc(&key) else {
        return false;
    };

    let holds_sockets = snapshot
        .sockets
        .get(&key)
        .is_some_and(|sockets| !sockets.is_empty());
    // A shell that only hosts other processes is scaffolding; its interesting
    // children are shown at the shell's own depth by the parent call.
    let is_shell = matches!(
        proc.name(),
        "fish" | "zsh" | "bash" | "sh" | "login" | "tmux"
    );

    if holds_sockets {
        return true;
    }
    if !is_shell {
        return true;
    }
    snapshot
        .host_children(pid)
        .iter()
        .any(|child| is_interesting(snapshot, *child))
}

fn build_container(ctx: &Ctx, container: &Container, depth: u16) -> Vec<Row> {
    let id = NodeId::Container(container.id.clone());
    let expanded = ctx.expansion.is_expanded(&id);
    let procs = ctx.snapshot.container_procs.get(&container.id);

    let mut rollup = Rollup::default();
    if let Some(metrics) = &container.metrics {
        rollup.cpu_pct = metrics.cpu_pct;
        rollup.rss_bytes = metrics.mem_bytes;
        rollup.proc_count = metrics.pids;
    }

    let mut rows = vec![Row {
        id,
        kind: Kind::Container {
            container: container.clone(),
        },
        depth,
        // Container processes are fetched on demand (`docker top` per
        // container), so expandability is not knowable until then — a running
        // container is always offered.
        expandable: container.running,
        expanded,
        rollup,
        listen_ports: container
            .ports
            .iter()
            .filter_map(|mapping| mapping.split("->").next()?.parse().ok())
            .collect(),
        port_conflict: false,
        connections: 0,
        flat_context: None,
        pane_cwd: None,
    }];

    if expanded && let Some(procs) = procs {
        rows.extend(build_container_procs(ctx, procs, depth + 1));
    }
    rows
}

/// In-container process tree. `docker top` gives ppid links inside the
/// container's namespace, so the same parent/child walk applies — but rooted at
/// the container's own pid 1, never at a host pid.
fn build_container_procs(ctx: &Ctx, procs: &[Proc], depth: u16) -> Vec<Row> {
    let by_pid: HashMap<u32, &Proc> = procs.iter().map(|proc| (proc.key.pid, proc)).collect();
    let mut rows = Vec::new();
    // Roots are processes whose parent is not itself in the container (pid 1,
    // or a process reparented to something outside the namespace view).
    let mut roots: Vec<&Proc> = procs
        .iter()
        .filter(|proc| !by_pid.contains_key(&proc.ppid))
        .collect();
    roots.sort_by_key(|proc| proc.key.pid);

    for root in roots {
        push_container_proc(ctx, procs, root, depth, &mut rows);
    }
    rows
}

fn push_container_proc(ctx: &Ctx, all: &[Proc], proc: &Proc, depth: u16, rows: &mut Vec<Row>) {
    const MAX_DEPTH: u16 = 32;
    if depth > MAX_DEPTH {
        return;
    }
    let id = NodeId::Process(proc.key.clone());
    let expanded = ctx.expansion.is_expanded(&id);
    let mut children: Vec<&Proc> = all
        .iter()
        .filter(|other| other.ppid == proc.key.pid && other.key != proc.key)
        .collect();
    children.sort_by_key(|child| child.key.pid);

    let sockets = ctx
        .snapshot
        .sockets
        .get(&proc.key)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let listen_ports = listen_ports_of(sockets);
    let mut rollup = Rollup::default();
    rollup.add_proc(proc);

    rows.push(Row {
        id,
        kind: Kind::Process { proc: proc.clone() },
        depth,
        expandable: !children.is_empty(),
        expanded,
        rollup,
        port_conflict: listen_ports
            .iter()
            .any(|port| ctx.conflicts.contains_key(port)),
        listen_ports,
        connections: sockets
            .iter()
            .filter(|socket| socket.state == SocketState::Established)
            .count() as u32,
        flat_context: None,
        pane_cwd: ctx.pane_cwd.clone(),
    });

    if expanded {
        for child in children {
            push_container_proc(ctx, all, child, depth + 1, rows);
        }
    }
}

fn containers_for_pane<'a>(snapshot: &'a Snapshot, pane_target: &str) -> Vec<&'a Container> {
    snapshot
        .containers
        .iter()
        .filter(|container| {
            container
                .attribution
                .as_ref()
                .is_some_and(|attribution| attribution.pane_target == pane_target)
        })
        .collect()
}

/// Containers not attributed to any pane — they still exist and still burn
/// resources, so they get their own group rather than being dropped.
pub fn unattributed_containers(snapshot: &Snapshot) -> Vec<&Container> {
    snapshot
        .containers
        .iter()
        .filter(|container| container.attribution.is_none())
        .collect()
}

fn listen_ports_of(sockets: &[crate::model::Socket]) -> Vec<u16> {
    let mut ports: Vec<u16> = sockets
        .iter()
        .filter(|socket| socket.state == SocketState::Listen)
        .filter_map(|socket| socket.local_port())
        .collect();
    ports.sort_unstable();
    ports.dedup();
    ports
}

fn subtree_rollup(snapshot: &Snapshot, pid: u32) -> Rollup {
    fn walk(snapshot: &Snapshot, pid: u32, depth: u16, rollup: &mut Rollup) {
        if depth > 32 {
            return;
        }
        let key = ProcKey::host(pid);
        let Some(proc) = snapshot.proc(&key) else {
            return;
        };
        rollup.add_proc(proc);
        if let Some(sockets) = snapshot.sockets.get(&key) {
            rollup.listen_ports += sockets
                .iter()
                .filter(|socket| socket.state == SocketState::Listen)
                .count() as u32;
        }
        for child in snapshot.host_children(pid) {
            walk(snapshot, *child, depth + 1, rollup);
        }
    }
    let mut rollup = Rollup::default();
    walk(snapshot, pid, 0, &mut rollup);
    rollup
}

fn rollup_of_pane(snapshot: &Snapshot, pane: &Pane) -> Rollup {
    let mut rollup = subtree_rollup(snapshot, pane.pid);
    // A container attributed to this pane counts toward the pane's footprint —
    // its cost is the pane's cost from the user's point of view, even though it
    // is not in the pane's process tree.
    for container in containers_for_pane(snapshot, &pane.target) {
        if let Some(metrics) = &container.metrics {
            rollup.cpu_pct += metrics.cpu_pct;
            rollup.rss_bytes += metrics.mem_bytes;
            rollup.proc_count += metrics.pids;
        }
    }
    rollup
}

fn rollup_of_panes(snapshot: &Snapshot, panes: &[&Pane]) -> Rollup {
    let mut total = Rollup::default();
    for pane in panes {
        total.merge(rollup_of_pane(snapshot, pane));
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build with server-wide scope. The fixtures assert across sessions, which
    /// is exactly what the default scope hides — the scope itself is covered by
    /// its own tests below.
    fn build_all(
        snapshot: &Snapshot,
        expansion: &Expansion,
        noise: Noise,
        filter: &Filter,
    ) -> Vec<Row> {
        build(snapshot, expansion, noise, filter, &Scope::Server, None)
    }
    use crate::model::{Proto, Socket};

    fn pane(session: &str, window: u32, index: u32, pid: u32, cwd: &str) -> Pane {
        Pane {
            session: session.into(),
            window_index: window,
            window_name: format!("w{window}"),
            pane_index: index,
            target: format!("{session}:{window}.{index}"),
            cwd: cwd.into(),
            current_command: "fish".into(),
            pid,
            active: false,
            window_active: false,
            session_attached: true,
            zoomed: false,
        }
    }

    fn proc(pid: u32, ppid: u32, command: &str, cpu: f32) -> Proc {
        Proc {
            key: ProcKey::host(pid),
            ppid,
            command: command.into(),
            age_secs: 60,
            cpu_pct: cpu,
            cpu_time_secs: 0.0,
            rss_bytes: 1024 * 1024,
            state: "S".into(),
            threads: None,
            fd_count: None,
        }
    }

    /// local:1.1 fish(100) -> cargo(101) -> rustc(102); local:2.1 fish(200) idle
    fn fixture() -> Snapshot {
        let mut snapshot = Snapshot::default();
        snapshot.panes = vec![
            pane("local", 1, 1, 100, "/src/app"),
            pane("local", 2, 1, 200, "/"),
        ];
        for proc in [
            proc(100, 1, "/opt/homebrew/bin/fish", 0.0),
            proc(101, 100, "cargo run --release", 12.0),
            proc(102, 101, "rustc --edition 2024 lib.rs", 90.0),
            proc(200, 1, "/opt/homebrew/bin/fish", 0.0),
        ] {
            snapshot
                .children
                .entry(proc.ppid)
                .or_default()
                .push(proc.key.pid);
            snapshot.procs.insert(proc.key.clone(), proc);
        }
        snapshot
    }

    fn listen(port: u16) -> Socket {
        Socket {
            proto: Proto::Tcp,
            local: format!("*:{port}"),
            peer: None,
            state: SocketState::Listen,
        }
    }

    #[test]
    fn default_view_shows_groups_but_not_process_subtrees() {
        let rows = build_all(
            &fixture(),
            &Expansion::default(),
            Noise::Hide,
            &Filter::default(),
        );
        let labels: Vec<String> = rows.iter().map(Row::label).collect();
        assert!(labels.contains(&"local".to_string()));
        assert!(labels.contains(&"1:w1".to_string()));
        assert!(labels.contains(&"fish".to_string()));
        // cargo is a child of the shell — hidden until the shell row is expanded.
        assert!(!labels.contains(&"cargo".to_string()));
    }

    #[test]
    fn expanding_a_process_reveals_its_children() {
        let snapshot = fixture();
        let mut expansion = Expansion::default();
        expansion.expand(&NodeId::Process(ProcKey::host(100)));
        let rows = build_all(&snapshot, &expansion, Noise::Hide, &Filter::default());
        let labels: Vec<String> = rows.iter().map(Row::label).collect();
        assert!(labels.contains(&"cargo".to_string()));
        assert!(
            !labels.contains(&"rustc".to_string()),
            "grandchild needs its own expand"
        );
    }

    #[test]
    fn collapsing_a_session_hides_its_windows() {
        let snapshot = fixture();
        let mut expansion = Expansion::default();
        expansion.collapse(&NodeId::Session("local".into()));
        let rows = build_all(&snapshot, &expansion, Noise::Hide, &Filter::default());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label(), "local");
    }

    #[test]
    fn pane_rollup_sums_the_whole_subtree() {
        let snapshot = fixture();
        let rows = build_all(
            &snapshot,
            &Expansion::default(),
            Noise::Hide,
            &Filter::default(),
        );
        let pane_row = rows
            .iter()
            .find(|row| matches!(row.kind, Kind::Pane { .. }))
            .unwrap();
        // fish + cargo + rustc
        assert_eq!(pane_row.rollup.proc_count, 3);
        assert_eq!(pane_row.rollup.cpu_pct, 102.0);
    }

    #[test]
    fn session_rollup_sums_across_windows() {
        let snapshot = fixture();
        let rows = build_all(
            &snapshot,
            &Expansion::default(),
            Noise::Hide,
            &Filter::default(),
        );
        let session = rows
            .iter()
            .find(|row| matches!(row.kind, Kind::Session { .. }))
            .unwrap();
        assert_eq!(session.rollup.proc_count, 4);
    }

    #[test]
    fn noise_hiding_keeps_shells_that_host_something_interesting() {
        let snapshot = fixture();
        // fish(100) has a cpu-burning child; fish(200) has nothing.
        assert!(is_interesting(&snapshot, 100));
        assert!(!is_interesting(&snapshot, 200));
    }

    #[test]
    fn noise_show_reveals_uninteresting_children() {
        let mut snapshot = fixture();
        let idle = proc(201, 200, "sleep 900", 0.0);
        snapshot.children.entry(200).or_default().push(201);
        snapshot.procs.insert(idle.key.clone(), idle);

        let mut expansion = Expansion::default();
        expansion.expand(&NodeId::Process(ProcKey::host(200)));

        let hidden = build_all(&snapshot, &expansion, Noise::Hide, &Filter::default());
        let shown = build_all(&snapshot, &expansion, Noise::Show, &Filter::default());
        // `sleep` holds no sockets and burns no cpu, but it is not a shell, so
        // it stays visible either way — what Hide drops is bare shell layers.
        assert_eq!(hidden.len(), shown.len());
    }

    #[test]
    fn listen_ports_and_conflicts_land_on_the_process_row() {
        let mut snapshot = fixture();
        snapshot
            .sockets
            .insert(ProcKey::host(101), vec![listen(8080)]);
        snapshot
            .sockets
            .insert(ProcKey::host(200), vec![listen(8080)]);
        let mut expansion = Expansion::default();
        expansion.expand(&NodeId::Process(ProcKey::host(100)));

        let rows = build_all(&snapshot, &expansion, Noise::Hide, &Filter::default());
        let cargo = rows.iter().find(|row| row.label() == "cargo").unwrap();
        assert_eq!(cargo.listen_ports, vec![8080]);
        assert!(cargo.port_conflict, "8080 held by two processes");
    }

    #[test]
    fn filter_matches_by_command_and_by_port() {
        let mut snapshot = fixture();
        snapshot
            .sockets
            .insert(ProcKey::host(101), vec![listen(8080)]);
        let mut expansion = Expansion::default();
        expansion.expand(&NodeId::Process(ProcKey::host(100)));

        let by_command = build_all(
            &snapshot,
            &expansion,
            Noise::Hide,
            &Filter {
                query: "cargo".into(),
            },
        );
        assert!(by_command.iter().any(|row| row.label() == "cargo"));

        let by_port = build_all(
            &snapshot,
            &expansion,
            Noise::Hide,
            &Filter {
                query: "8080".into(),
            },
        );
        assert!(by_port.iter().any(|row| row.label() == "cargo"));
    }

    #[test]
    fn filter_searches_unexpanded_subtrees() {
        // rustc is two levels below the pane's shell and nothing is expanded.
        // A filter that only searched visible rows would find nothing — the bug
        // this guards is `/claude` returning "no rows match" while claude runs.
        let rows = build_all(
            &fixture(),
            &Expansion::default(),
            Noise::Hide,
            &Filter {
                query: "rustc".into(),
            },
        );
        assert!(rows.iter().any(|row| row.label() == "rustc"));
    }

    #[test]
    fn a_match_keeps_its_ancestors_for_context() {
        let rows = build_all(
            &fixture(),
            &Expansion::default(),
            Noise::Hide,
            &Filter {
                query: "rustc".into(),
            },
        );
        let labels: Vec<String> = rows.iter().map(Row::label).collect();
        // session → window → pane → shell → cargo → rustc: the chain that says
        // *where* the match lives.
        assert!(labels.contains(&"local".to_string()));
        assert!(labels.contains(&"1:w1".to_string()));
        assert!(labels.contains(&"1.1".to_string()));
        assert!(labels.contains(&"rustc".to_string()));
        // The unrelated second window is gone.
        assert!(!labels.contains(&"2:w2".to_string()));
    }

    #[test]
    fn filter_matching_nothing_hides_the_group_headers_too() {
        let rows = build_all(
            &fixture(),
            &Expansion::default(),
            Noise::Hide,
            &Filter {
                query: "zzzznomatch".into(),
            },
        );
        assert!(rows.is_empty());
    }

    #[test]
    fn filter_by_pane_cwd_finds_the_pane() {
        let rows = build_all(
            &fixture(),
            &Expansion::default(),
            Noise::Hide,
            &Filter {
                query: "app".into(),
            },
        );
        assert!(rows.iter().any(|row| matches!(row.kind, Kind::Pane { .. })));
    }

    #[test]
    fn expansion_survives_a_rebuild_because_ids_are_stable() {
        let mut expansion = Expansion::default();
        expansion.expand(&NodeId::Process(ProcKey::host(100)));
        let first = build_all(&fixture(), &expansion, Noise::Hide, &Filter::default());
        // A new snapshot object, same world.
        let second = build_all(&fixture(), &expansion, Noise::Hide, &Filter::default());
        assert_eq!(first.len(), second.len());
        assert!(second.iter().any(|row| row.label() == "cargo"));
    }

    #[test]
    fn ppid_cycle_cannot_recurse_forever() {
        let mut snapshot = Snapshot::default();
        snapshot.panes = vec![pane("local", 1, 1, 100, "/")];
        // 100 -> 101 -> 100
        for (pid, ppid) in [(100u32, 101u32), (101, 100)] {
            let proc = proc(pid, ppid, "loop", 0.0);
            snapshot.children.entry(ppid).or_default().push(pid);
            snapshot.procs.insert(proc.key.clone(), proc);
        }
        let mut expansion = Expansion::default();
        expansion.expand_all_procs(&build_all(
            &snapshot,
            &Expansion::default(),
            Noise::Show,
            &Filter::default(),
        ));
        // Terminates rather than overflowing the stack.
        let rows = build_all(&snapshot, &expansion, Noise::Show, &Filter::default());
        assert!(!rows.is_empty());
    }

    /// local:1.1 (pid 100 chain), local:2.1 (pid 200), other:1.1 (pid 300).
    fn multi_window_fixture() -> Snapshot {
        let mut snapshot = Snapshot::default();
        snapshot.panes = vec![
            pane("local", 1, 1, 100, "/src/app"),
            pane("local", 2, 1, 200, "/"),
            pane("other", 1, 1, 300, "/elsewhere"),
        ];
        for proc in [
            proc(100, 1, "/opt/homebrew/bin/fish", 0.0),
            proc(200, 1, "/opt/homebrew/bin/fish", 0.0),
            proc(300, 1, "/opt/homebrew/bin/fish", 0.0),
        ] {
            snapshot
                .children
                .entry(proc.ppid)
                .or_default()
                .push(proc.key.pid);
            snapshot.procs.insert(proc.key.clone(), proc);
        }
        snapshot
    }

    #[test]
    fn the_default_scope_shows_only_the_window_tpx_runs_in() {
        let snapshot = multi_window_fixture();
        let current = ("local".to_string(), 1u32);
        let rows = build(
            &snapshot,
            &Expansion::default(),
            Noise::Hide,
            &Filter::default(),
            &Scope::CurrentWindow,
            Some(&current),
        );
        let panes: Vec<String> = rows
            .iter()
            .filter_map(|row| match &row.kind {
                Kind::Pane { pane } => Some(pane.target.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(panes, vec!["local:1.1"]);
        // The other session disappears entirely — no empty header left behind.
        assert!(!rows.iter().any(|row| row.label() == "other"));
    }

    #[test]
    fn the_server_scope_shows_every_session() {
        let snapshot = multi_window_fixture();
        let current = ("local".to_string(), 1u32);
        let rows = build(
            &snapshot,
            &Expansion::default(),
            Noise::Hide,
            &Filter::default(),
            &Scope::Server,
            Some(&current),
        );
        let panes = rows
            .iter()
            .filter(|row| matches!(row.kind, Kind::Pane { .. }))
            .count();
        assert_eq!(panes, 3);
        assert!(rows.iter().any(|row| row.label() == "other"));
    }

    #[test]
    fn outside_tmux_the_default_scope_still_shows_everything() {
        // With no current window there is nothing to narrow to; an empty tree
        // would be worse than a wide one.
        let rows = build(
            &multi_window_fixture(),
            &Expansion::default(),
            Noise::Hide,
            &Filter::default(),
            &Scope::CurrentWindow,
            None,
        );
        assert_eq!(
            rows.iter()
                .filter(|row| matches!(row.kind, Kind::Pane { .. }))
                .count(),
            3
        );
    }

    #[test]
    fn scoping_does_not_hide_a_port_conflict_from_another_window() {
        // The conflict map is built server-wide on purpose: a dev server in this
        // window losing to one in another window is exactly the case worth
        // catching, and narrowing collection would report "no conflict".
        let mut snapshot = multi_window_fixture();
        snapshot
            .sockets
            .insert(ProcKey::host(100), vec![listen(8080)]);
        snapshot
            .sockets
            .insert(ProcKey::host(300), vec![listen(8080)]);

        let current = ("local".to_string(), 1u32);
        let mut expansion = Expansion::default();
        expansion.expand(&NodeId::Process(ProcKey::host(100)));
        let rows = build(
            &snapshot,
            &expansion,
            Noise::Hide,
            &Filter::default(),
            &Scope::CurrentWindow,
            Some(&current),
        );
        let shell = rows
            .iter()
            .find(|row| matches!(&row.kind, Kind::Process { proc } if proc.key.pid == 100))
            .expect("the in-scope shell is present");
        assert!(
            shell.port_conflict,
            "8080 is contested by a pane in another window"
        );
    }

    #[test]
    fn unattributed_containers_are_hidden_under_the_default_scope() {
        let mut snapshot = multi_window_fixture();
        snapshot.containers = vec![Container {
            id: "abc".into(),
            short_id: "abc".into(),
            name: "orphan".into(),
            image: "alpine".into(),
            status: "running".into(),
            running: true,
            init_pid: 1,
            compose_project: None,
            compose_working_dir: None,
            network_mode: "bridge".into(),
            ports: vec![],
            metrics: None,
            attribution: None,
        }];
        let current = ("local".to_string(), 1u32);

        let narrow = build(
            &snapshot,
            &Expansion::default(),
            Noise::Hide,
            &Filter::default(),
            &Scope::CurrentWindow,
            Some(&current),
        );
        assert!(!narrow.iter().any(|row| row.label() == "orphan"));
        assert!(!narrow.iter().any(|row| row.label() == CONTAINERS_GROUP));

        let wide = build(
            &snapshot,
            &Expansion::default(),
            Noise::Hide,
            &Filter::default(),
            &Scope::Server,
            Some(&current),
        );
        assert!(wide.iter().any(|row| row.label() == CONTAINERS_GROUP));
    }

    #[test]
    fn filter_finds_a_process_by_its_pid() {
        // A pid is how a reader arrives from a log or a crash report.
        let rows = build_all(
            &fixture(),
            &Expansion::default(),
            Noise::Show,
            &Filter {
                query: "102".into(),
            },
        );
        assert!(
            rows.iter().any(|row| matches!(
                &row.kind,
                Kind::Process { proc } if proc.key.pid == 102
            )),
            "rows: {:?}",
            rows.iter().map(Row::label).collect::<Vec<_>>()
        );
    }

    #[test]
    fn the_default_sort_keeps_the_tree_intact() {
        let rows = build_all(
            &fixture(),
            &Expansion::default(),
            Noise::Hide,
            &Filter::default(),
        );
        let flat = flatten(rows.clone(), Sort::Tree);
        assert_eq!(flat.len(), rows.len(), "Tree must not flatten");
        assert!(flat.iter().any(|row| row.is_group()));
    }

    #[test]
    fn a_flat_sort_drops_group_rows_and_keeps_processes() {
        let mut expansion = Expansion::default();
        expansion.expand_everything(&fixture());
        let rows = build_all(&fixture(), &expansion, Noise::Hide, &Filter::default());
        let flat = flatten(rows, Sort::Cpu);

        assert!(
            !flat.iter().any(|row| row.is_group()),
            "no headers in a flat list"
        );
        assert!(
            !flat.iter().any(|row| matches!(row.kind, Kind::Pane { .. })),
            "pane rows are context, not rankable rows"
        );
        assert!(
            flat.iter().all(|row| row.depth == 0),
            "indent is meaningless"
        );
        assert!(flat.iter().any(|row| row.label() == "rustc"));
    }

    #[test]
    fn cpu_sort_ranks_by_a_process_own_usage_not_its_subtree() {
        // fish(0%) -> cargo(12%) -> rustc(90%). Sorting by the rollup would put the
        // shell first, since its subtree sums to 102%.
        let mut expansion = Expansion::default();
        expansion.expand_everything(&fixture());
        let rows = build_all(&fixture(), &expansion, Noise::Hide, &Filter::default());
        let flat = flatten(rows, Sort::Cpu);
        assert_eq!(flat[0].label(), "rustc", "hottest process leads");
        assert_eq!(flat[1].label(), "cargo");
    }

    #[test]
    fn a_flat_row_remembers_which_pane_it_came_from() {
        // The indent is gone, so the pane has to be carried explicitly or the row
        // says what is heavy without saying where.
        let mut expansion = Expansion::default();
        expansion.expand_everything(&fixture());
        let rows = build_all(&fixture(), &expansion, Noise::Hide, &Filter::default());
        let flat = flatten(rows, Sort::Cpu);
        let rustc = flat.iter().find(|row| row.label() == "rustc").unwrap();
        assert_eq!(rustc.flat_context.as_deref(), Some("local:1.1"));
    }

    #[test]
    fn age_sort_puts_the_newest_process_first() {
        let mut snapshot = fixture();
        let mut fresh = proc(999, 100, "just-started", 0.0);
        // The fixture ages everything at 60s, so the newcomer needs a real age.
        fresh.age_secs = 2;
        snapshot.children.entry(100).or_default().push(999);
        snapshot.procs.insert(fresh.key.clone(), fresh);
        let mut expansion = Expansion::default();
        expansion.expand_everything(&snapshot);
        let rows = build_all(&snapshot, &expansion, Noise::Hide, &Filter::default());
        let flat = flatten(rows, Sort::Age);
        assert_eq!(flat[0].label(), "just-started");
    }

    #[test]
    fn the_sort_cycle_returns_to_tree() {
        let mut sort = Sort::Tree;
        for _ in 0..Sort::CYCLE.len() {
            sort = sort.next();
        }
        assert_eq!(sort, Sort::Tree);
        assert!(!Sort::Tree.is_flat());
        assert!(Sort::Cpu.is_flat());
    }
}
