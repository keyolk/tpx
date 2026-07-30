//! Application state and the event loop.
//!
//! Layout is a **drill-down stack with a persistent detail pane**: the tree on
//! the left is the spine, and the right pane shows facets of the selected row.
//! Facets are tabs within the pane (`[`/`]`), so the tree never moves when the
//! reader changes what they are looking at.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::collect::{self, Collector, Update, capture::Capture};
use crate::model::{Container, Origin, Proc, ProcKey, Snapshot, Socket};
use crate::tree::{self, Expansion, Filter, Kind, NodeId, Noise, Row, Scope, Sort};

/// Facets of the selected row, shown in the detail pane as tabs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Facet {
    /// Identity, resources, tmux/container context.
    Overview,
    /// Listening + established sockets, byte counters, port conflicts.
    Network,
    /// cwd and open files.
    Files,
    /// tmux `capture-pane` output — what the pane was last doing.
    Output,
    /// Where the selected process's stdout/stderr go, and their content when it
    /// is reachable.
    Streams,
    /// What the process is doing right now, from a live stack sample.
    Stack,
    /// The environment it was started with.
    Env,
    /// Live packet capture.
    Packets,
}

impl Facet {
    pub const ALL: [Facet; 8] = [
        Facet::Overview,
        Facet::Network,
        Facet::Files,
        Facet::Output,
        Facet::Streams,
        Facet::Stack,
        Facet::Env,
        Facet::Packets,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Facet::Overview => "overview",
            Facet::Network => "net",
            Facet::Files => "files",
            Facet::Output => "output",
            Facet::Streams => "streams",
            Facet::Stack => "stack",
            Facet::Env => "env",
            Facet::Packets => "packets",
        }
    }
}

/// A modal overlay. Only one can be open, and `Esc` always closes it.
pub enum Modal {
    Help,
    /// The `x` command menu — a two-keystroke prefix for extended actions.
    /// Pressing `x` opens this; the next key selects and runs a command.
    CommandMenu,
    /// A sort submenu, reached from the command menu. The number keys select
    /// a sort directly, which avoids the cycling that was slow to reach a
    /// specific ordering.
    SortMenu,
    /// Confirm a privileged or side-effecting action before running it. The
    /// exact command is shown so nothing runs invisibly.
    Confirm {
        title: String,
        command: String,
        action: PendingAction,
    },
    /// Everything the snapshot knows that did not fit a facet — collector
    /// errors and the port-conflict summary.
    Diagnostics,
}

/// An action awaiting confirmation.
#[derive(Clone, Debug)]
pub enum PendingAction {
    /// Host packet capture, which needs `sudo`. The interface is resolved when
    /// the action is built, not when it runs, so the confirmation shows the
    /// exact command — macOS has no `any` device to fall back on.
    CaptureHost {
        pid: u32,
        interface: String,
        filter: String,
    },
    /// Container capture through a sidecar, which may need an image pull.
    CaptureContainer { id: String, name: String },
    /// Send a signal to a host process.
    Signal { pid: u32, signal: &'static str },
}

/// stdout/stderr content, or the reason there is none. The error is cached too —
/// re-running a failing `lsof` on every frame would be worse than showing why.
#[derive(Default)]
pub struct StreamContent {
    pub stdout: Option<Result<String, String>>,
    pub stderr: Option<Result<String, String>>,
}

/// Lazily-fetched per-row detail, cached by [`ProcKey`] so re-selecting a row is
/// instant and a `j`/`k` sweep does not fire a `lsof` per row.
#[derive(Default)]
pub struct DetailCache {
    pub host_files: HashMap<u32, Vec<crate::collect::host::OpenFile>>,
    pub host_threads: HashMap<u32, u32>,
    pub host_fds: HashMap<u32, u32>,
    pub container_detail: HashMap<ProcKey, crate::collect::container::ProcDetail>,
    /// Located stdout/stderr per host pid, with any content already read.
    pub streams: HashMap<u32, (crate::collect::streams::Streams, StreamContent)>,
    /// Stack samples per host pid. `Err` is cached so a failing `sample` is not
    /// retried on every frame.
    pub stacks: HashMap<u32, Result<crate::collect::introspect::Sample, String>>,
    /// Environment per host pid.
    pub envs: HashMap<u32, Result<Vec<(String, String)>, String>>,
    /// Machine-wide pipe/socket ownership, reused across rows.
    ///
    /// Rebuilding it per row cost 250ms each, which made a `j`/`k` sweep with the
    /// streams facet open stutter. It goes stale as processes come and go, so `r`
    /// drops it.
    pub peer_table: Option<crate::collect::streams::PeerTable>,
    /// Pane output, keyed by pane target.
    pub pane_output: HashMap<String, String>,
}

pub struct App {
    pub snapshot: Snapshot,
    pub rows: Vec<Row>,
    pub expansion: Expansion,
    pub filter: Filter,
    pub noise: Noise,
    /// How much of the tmux server the tree covers.
    pub scope: Scope,
    /// Row ordering. Anything but `Tree` flattens to processes only.
    pub sort: Sort,
    /// Loopback connection ownership, for following an edge to its far end.
    /// Collected on first use and dropped by `r`, since pids are reused.
    pub peers: Option<collect::peers::PeerMap>,
    /// The window tpx runs in, resolved once at startup. `None` outside tmux.
    pub current_window: Option<(String, u32)>,
    pub selected: usize,
    pub facet: Facet,
    pub modal: Option<Modal>,
    /// Typing into the filter. Held separately so `/` can be cancelled with the
    /// pre-edit query restored.
    pub filter_input: Option<String>,
    pub detail: DetailCache,
    pub capture: Option<Capture>,
    pub capture_lines: Vec<String>,
    pub collector: Collector,
    pub docker_available: bool,
    pub status: Option<Status>,
    pub spinner: usize,
    pub should_quit: bool,
    /// Whether the reader has folded or unfolded anything by hand.
    ///
    /// Auto-expansion under the narrow scope must not fight them: without this,
    /// collapsing a subtree would silently reopen on the next 3s refresh.
    user_set_expansion: bool,
    /// Set when a key handler changes anything the view depends on.
    dirty: bool,
    last_spin: Instant,
    /// Wall-clock of the newest snapshot, for the "as of" header.
    pub snapshot_at: Option<Instant>,
}

pub struct Status {
    pub message: String,
    pub is_error: bool,
    at: Instant,
}

impl Status {
    /// Statuses fade rather than piling up; an error lingers long enough to
    /// read.
    fn expired(&self) -> bool {
        let ttl = if self.is_error {
            Duration::from_secs(8)
        } else {
            Duration::from_secs(4)
        };
        self.at.elapsed() > ttl
    }
}

/// Cap on cached capture lines — a bounded ring, so a long capture cannot grow
/// memory without limit.
const CAPTURE_SCROLLBACK: usize = 2_000;
/// How many lines of pane output to capture.
const PANE_OUTPUT_LINES: u16 = 200;
/// How long a stack sample runs. One second is `sample`'s practical floor and
/// enough to tell a wait from a hot loop.
const SAMPLE_MILLIS: u64 = 1000;
/// How much of a redirected log to read. Enough for real context, small enough
/// that a multi-gigabyte log is not pulled into memory.
const STREAM_TAIL_BYTES: u64 = 64 * 1024;
/// Auto-refresh interval.
///
/// This is not cosmetic: cpu is a *rate* derived from two snapshots, so without
/// a second collection every process reads 0% forever. A collection round costs
/// ~150ms of subprocess work, so 3s keeps the load negligible while making the
/// cpu column trustworthy.
const REFRESH_INTERVAL: Duration = Duration::from_secs(3);

impl Default for App {
    fn default() -> Self {
        Self::new(Scope::default())
    }
}

impl App {
    pub fn new(scope: Scope) -> Self {
        let docker_available = collect::docker_available();
        Self {
            snapshot: Snapshot::default(),
            rows: Vec::new(),
            expansion: Expansion::default(),
            filter: Filter::default(),
            noise: Noise::Hide,
            scope,
            sort: Sort::default(),
            peers: None,
            current_window: collect::tmux::current_window(),
            selected: 0,
            facet: Facet::Overview,
            modal: None,
            filter_input: None,
            detail: DetailCache::default(),
            capture: None,
            capture_lines: Vec::new(),
            collector: Collector::spawn(docker_available),
            docker_available,
            status: None,
            spinner: 0,
            should_quit: false,
            user_set_expansion: false,
            dirty: true,
            last_spin: Instant::now(),
            snapshot_at: None,
        }
    }

    /// Move the selection to a process by pid, if it is in the tree.
    ///
    /// Returns whether the jump landed. A jump that silently does nothing is worse
    /// than one that says the target is out of scope.
    pub fn jump_to_pid(&mut self, pid: u32) -> bool {
        let found = self
            .rows
            .iter()
            .position(|row| matches!(&row.kind, Kind::Process { proc } if proc.key.pid == pid));
        match found {
            Some(index) => {
                self.selected = index;
                self.after_jump();
                true
            }
            None => false,
        }
    }

    /// Jump to the process on the other end of the selected process's busiest
    /// local connection.
    ///
    /// This is the payoff of resolving loopback peers: `ccproxy` talking to
    /// `claude` is one keypress from either side, and a dev server's caller is
    /// reachable without knowing its pid.
    pub fn jump_to_peer(&mut self) {
        let Some(proc) = self.selected_proc().cloned() else {
            self.set_error("select a process first");
            return;
        };
        if self.peers.is_none() {
            self.peers = collect::peers::PeerMap::collect().ok();
        }
        let Some(peers) = &self.peers else {
            self.set_error("could not read the connection table");
            return;
        };

        let sockets = self.selected_sockets();
        let target = sockets
            .iter()
            .filter_map(|socket| socket.peer.as_deref())
            .filter(|remote| collect::peers::PeerMap::is_local(remote))
            .find_map(|remote| peers.peer_of(remote, proc.key.pid))
            .cloned();

        match target {
            Some(peer) => {
                let name = peer.name.clone();
                if self.jump_to_pid(peer.pid) {
                    self.set_status(format!("jumped to {name} ({})", peer.pid));
                } else {
                    // The peer exists but sits outside the current scope; saying so
                    // beats a jump that appears to do nothing.
                    self.set_error(format!(
                        "{name} ({}) is outside the current scope — press w",
                        peer.pid
                    ));
                }
            }
            None => self.set_error("no local connection to follow"),
        }
    }

    /// Jump to the parent process of the selection.
    ///
    /// Distinct from `h`, which moves to the row above in the *display*. Under a
    /// flat sort there is no row above to mean anything, and even in the tree the
    /// parent may be collapsed out of view.
    pub fn jump_to_parent(&mut self) {
        let Some(proc) = self.selected_proc().cloned() else {
            return;
        };
        if proc.ppid <= 1 {
            self.set_error("no parent process");
            return;
        }
        if !self.jump_to_pid(proc.ppid) {
            self.set_error(format!("parent {} is not in view", proc.ppid));
        }
    }

    /// Selection moved by a jump rather than a step: the same invalidation as a
    /// keyboard move, but without touching the capture (a jump is navigation, and
    /// the reader may be following a connection while a capture runs).
    fn after_jump(&mut self) {
        self.ensure_facet_data();
        self.dirty = true;
    }

    pub fn selected_row(&self) -> Option<&Row> {
        self.rows.get(self.selected)
    }

    /// Drain collector updates and pump the capture stream. Returns whether the
    /// view needs a repaint — the loop idles at 0 fps when nothing changed.
    pub fn tick(&mut self) -> bool {
        // Periodic refresh. Requesting is cheap and the worker coalesces, so a
        // slow round cannot pile up behind this.
        if !self.collector.in_flight
            && self
                .snapshot_at
                .is_some_and(|at| at.elapsed() >= REFRESH_INTERVAL)
        {
            self.collector.request();
        }

        for update in self.collector.poll() {
            match update {
                Update::Snapshot(snapshot) => {
                    self.apply_snapshot(*snapshot);
                }
                Update::ContainerMetrics(id, metrics) => {
                    if let Some(container) = self
                        .snapshot
                        .containers
                        .iter_mut()
                        .find(|container| container.id == id)
                    {
                        container.metrics = Some(metrics);
                        self.dirty = true;
                    }
                }
            }
        }

        if let Some(capture) = self.capture.as_mut() {
            let new_lines = capture.drain();
            if !new_lines.is_empty() {
                self.capture_lines.extend(new_lines);
                let overflow = self.capture_lines.len().saturating_sub(CAPTURE_SCROLLBACK);
                self.capture_lines.drain(..overflow);
                self.dirty = true;
            }
            if capture.finished() {
                self.capture_lines.push("— capture finished —".to_string());
                self.capture = None;
                self.dirty = true;
            }
        }

        // The spinner is the one thing that animates, and only while a
        // collection is actually in flight.
        if (self.collector.in_flight || self.capture.is_some())
            && self.last_spin.elapsed() >= Duration::from_millis(120)
        {
            self.spinner = self.spinner.wrapping_add(1);
            self.last_spin = Instant::now();
            self.dirty = true;
        }

        if self.status.as_ref().is_some_and(Status::expired) {
            self.status = None;
            self.dirty = true;
        }

        std::mem::take(&mut self.dirty)
    }

    fn apply_snapshot(&mut self, mut snapshot: Snapshot) {
        // Container metrics arrive on their own stream, so carry the ones we
        // already have onto the new snapshot rather than blanking them until the
        // next stats line lands.
        let previous: HashMap<String, _> = self
            .snapshot
            .containers
            .iter()
            .filter_map(|container| {
                container
                    .metrics
                    .clone()
                    .map(|metrics| (container.id.clone(), metrics))
            })
            .collect();
        for container in snapshot.containers.iter_mut() {
            if container.metrics.is_none() {
                container.metrics = previous.get(&container.id).cloned();
            }
        }
        // Container process trees are fetched on demand, so they must survive a
        // snapshot swap or an expanded container would collapse on every refresh.
        snapshot.container_procs = std::mem::take(&mut self.snapshot.container_procs);
        // Container sockets are keyed by container-namespace pids, which the host
        // collectors never produce, so the fresh snapshot has none. Without this
        // they would disappear from an expanded container after one refresh.
        let container_sockets = self
            .snapshot
            .sockets
            .iter()
            .filter(|(key, _)| matches!(key.origin, Origin::Container(_)))
            .map(|(key, sockets)| (key.clone(), sockets.clone()));
        snapshot.sockets.extend(container_sockets);

        self.snapshot = snapshot;
        self.snapshot_at = Some(Instant::now());

        // Under the narrow default scope, open every process subtree as the
        // snapshot lands. A pane's shell is never the interesting row — the
        // `ccproxy → claude → mcp` chain under it is — and hiding that behind two
        // keypresses made the first screen strictly less informative than
        // `tmux.sh procs`, which always printed the whole tree.
        //
        // Only for `CurrentWindow`: server-wide that is 200+ rows, where the
        // collapsed map of *where* work lives is the more useful default.
        if self.scope == Scope::CurrentWindow && !self.user_set_expansion {
            self.expansion.expand_everything(&self.snapshot);
        }
        self.rebuild();
    }

    /// Rebuild the row list, keeping the selection on the *same node* rather
    /// than the same index — processes come and go between snapshots.
    pub fn rebuild(&mut self) {
        let anchor = self.selected_row().map(|row| row.id.clone());
        let built = tree::build(
            &self.snapshot,
            &self.expansion,
            self.noise,
            &self.filter,
            &self.scope,
            self.current_window.as_ref(),
        );
        self.rows = tree::flatten(built, self.sort);
        let last = self.rows.len().saturating_sub(1);
        // Falling back to the old *index* would be wrong: if the anchor row is
        // gone the index now points at an unrelated row, which reads as the
        // selection jumping on its own. Clamping is only for the empty case.
        self.selected = match anchor.and_then(|id| self.rows.iter().position(|row| row.id == id)) {
            Some(found) => found,
            None => self.selected.min(last),
        };
        self.dirty = true;
    }

    pub fn set_status(&mut self, message: impl Into<String>) {
        self.status = Some(Status {
            message: message.into(),
            is_error: false,
            at: Instant::now(),
        });
        self.dirty = true;
    }

    pub fn set_error(&mut self, message: impl Into<String>) {
        self.status = Some(Status {
            message: message.into(),
            is_error: true,
            at: Instant::now(),
        });
        self.dirty = true;
    }

    pub fn touch(&mut self) {
        self.dirty = true;
    }

    /// Sockets held by the selected row's process, if it is one.
    pub fn selected_sockets(&self) -> &[Socket] {
        self.selected_row()
            .and_then(|row| match &row.kind {
                Kind::Process { proc } => self.snapshot.sockets.get(&proc.key),
                _ => None,
            })
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    pub fn selected_proc(&self) -> Option<&Proc> {
        match &self.selected_row()?.kind {
            Kind::Process { proc } => Some(proc),
            _ => None,
        }
    }

    pub fn selected_container(&self) -> Option<&Container> {
        match &self.selected_row()?.kind {
            Kind::Container { container } => Some(container),
            _ => None,
        }
    }

    /// The pane a row belongs to — a process row inherits its ancestor pane, so
    /// the Output facet works anywhere in a pane's subtree.
    pub fn selected_pane_target(&self) -> Option<String> {
        let selected = self.selected_row()?;
        if let Kind::Pane { pane } = &selected.kind {
            return Some(pane.target.clone());
        }
        // Walk back up the flat row list to the nearest shallower pane row.
        self.rows[..=self.selected]
            .iter()
            .rev()
            .find_map(|row| match &row.kind {
                Kind::Pane { pane } if row.depth < selected.depth => Some(pane.target.clone()),
                _ => None,
            })
    }

    /// Load whatever the current facet needs for the selected row. Called after
    /// navigation, so each fetch happens once and is cached.
    pub fn ensure_facet_data(&mut self) {
        match self.facet {
            Facet::Files => self.load_files(),
            Facet::Output => self.load_pane_output(),
            Facet::Streams => self.load_streams(),
            Facet::Stack => {} // Explicit only: sampling costs a wall-clock second.
            Facet::Env => self.load_env(),
            Facet::Overview => self.load_proc_detail(),
            // The net facet resolves loopback peers so the socket list names
            // processes rather than ports. One bulk lsof (~0.08s), cached.
            Facet::Network => {
                if self.peers.is_none() {
                    self.peers = collect::peers::PeerMap::collect().ok();
                    self.dirty = true;
                }
            }
            Facet::Packets => {}
        }
    }

    fn load_files(&mut self) {
        let Some(proc) = self.selected_proc().cloned() else {
            return;
        };
        match &proc.key.origin {
            Origin::Host => {
                if self.detail.host_files.contains_key(&proc.key.pid) {
                    return;
                }
                match collect::host::open_files(proc.key.pid) {
                    Ok(files) => {
                        self.detail.host_files.insert(proc.key.pid, files);
                        self.dirty = true;
                    }
                    Err(error) => self.set_error(format!("lsof: {error}")),
                }
            }
            // In-container open files would need another sidecar round-trip per
            // process; the cwd from proc_detail is the useful part and it is
            // already loaded by the Overview facet.
            Origin::Container(_) => self.load_proc_detail(),
        }
    }

    fn load_proc_detail(&mut self) {
        let Some(proc) = self.selected_proc().cloned() else {
            return;
        };
        match &proc.key.origin {
            Origin::Host => {
                if self.detail.host_threads.contains_key(&proc.key.pid) {
                    return;
                }
                if let Ok(threads) = collect::host::thread_count(proc.key.pid) {
                    self.detail.host_threads.insert(proc.key.pid, threads);
                    self.dirty = true;
                }
                if let Ok(fds) = collect::host::fd_count(proc.key.pid) {
                    self.detail.host_fds.insert(proc.key.pid, fds);
                    self.dirty = true;
                }
            }
            Origin::Container(id) => {
                if self.detail.container_detail.contains_key(&proc.key) {
                    return;
                }
                // One sidecar start; explicit because it is not free.
                match collect::container::proc_detail(id, proc.key.pid) {
                    Ok(detail) => {
                        self.detail
                            .container_detail
                            .insert(proc.key.clone(), detail);
                        self.dirty = true;
                    }
                    Err(error) => self.set_error(format!("sidecar: {error}")),
                }
            }
        }
    }

    /// Sample the selected process's stacks.
    ///
    /// Never automatic: `sample` blocks for a wall-clock second by construction, so
    /// arrowing through rows would stall the UI once per row. `S` asks for it.
    pub fn sample_selected(&mut self) {
        let Some(proc) = self.selected_proc().cloned() else {
            self.set_error("select a process to sample");
            return;
        };
        if !matches!(proc.key.origin, Origin::Host) {
            self.set_error("cannot sample a container process from the host");
            return;
        }
        self.set_status(format!("sampling {} for 1s…", proc.name()));
        let result = collect::introspect::sample(proc.key.pid, SAMPLE_MILLIS)
            .map_err(|error| error.to_string());
        match &result {
            Ok(sample) => {
                let busy = sample.threads.iter().filter(|t| !t.is_waiting()).count();
                self.set_status(format!(
                    "{} threads, {busy} doing work",
                    sample.threads.len()
                ));
            }
            Err(error) => self.set_error(error.clone()),
        }
        self.detail.stacks.insert(proc.key.pid, result);
        self.facet = Facet::Stack;
        self.dirty = true;
    }

    fn load_env(&mut self) {
        let Some(proc) = self.selected_proc().cloned() else {
            return;
        };
        if !matches!(proc.key.origin, Origin::Host) {
            return;
        }
        if self.detail.envs.contains_key(&proc.key.pid) {
            return;
        }
        let result =
            collect::introspect::environment(proc.key.pid).map_err(|error| error.to_string());
        self.detail.envs.insert(proc.key.pid, result);
        self.dirty = true;
    }

    /// Locate and read the selected process's output streams.
    ///
    /// Bounded to the tail: a redirected log can be hundreds of megabytes, and
    /// only its end is ever the answer to "what is this doing".
    fn load_streams(&mut self) {
        let Some(proc) = self.selected_proc().cloned() else {
            return;
        };
        // Container processes live in another namespace, where host `lsof`
        // cannot see their fds at all.
        if !matches!(proc.key.origin, Origin::Host) {
            return;
        }
        if self.detail.streams.contains_key(&proc.key.pid) {
            return;
        }

        let pane = self.selected_pane_target();
        let mut located = match collect::streams::locate(proc.key.pid, pane.as_deref()) {
            Ok(streams) => streams,
            Err(error) => {
                self.set_error(format!("lsof: {error}"));
                return;
            }
        };
        if self.detail.peer_table.is_none() {
            self.detail.peer_table = collect::streams::PeerTable::collect().ok();
        }
        if let Some(table) = &self.detail.peer_table {
            collect::streams::resolve_peers_with(&mut located, proc.key.pid, table);
        }

        let read = |sink: &collect::streams::Sink| {
            Some(collect::streams::read(sink, STREAM_TAIL_BYTES).map_err(|error| error.to_string()))
        };
        let content = StreamContent {
            stdout: read(&located.stdout),
            stderr: read(&located.stderr),
        };
        self.detail.streams.insert(proc.key.pid, (located, content));
        self.dirty = true;
    }

    /// Re-read the selected process's streams, dropping the cached copy.
    pub fn refresh_streams(&mut self) {
        if let Some(proc) = self.selected_proc() {
            let pid = proc.key.pid;
            self.detail.streams.remove(&pid);
        }
        // Both tables name processes by pid, so they go stale as processes exit.
        self.detail.peer_table = None;
        self.peers = None;
        self.load_streams();
    }

    fn load_pane_output(&mut self) {
        let Some(target) = self.selected_pane_target() else {
            return;
        };
        if self.detail.pane_output.contains_key(&target) {
            return;
        }
        match collect::tmux::capture_pane(&target, PANE_OUTPUT_LINES) {
            Ok(output) => {
                self.detail.pane_output.insert(target, output);
                self.dirty = true;
            }
            Err(error) => self.set_error(format!("capture-pane: {error}")),
        }
    }

    /// Re-read the selected pane's output, discarding the cached copy.
    pub fn refresh_pane_output(&mut self) {
        if let Some(target) = self.selected_pane_target() {
            self.detail.pane_output.remove(&target);
            self.load_pane_output();
        }
    }

    /// Fetch a container's process tree. On-demand because it costs a
    /// `docker exec` or a sidecar start per container.
    pub fn load_container_procs(&mut self, container_id: &str) {
        if self.snapshot.container_procs.contains_key(container_id) {
            return;
        }
        match collect::container::processes(container_id) {
            Ok(procs) => {
                self.snapshot
                    .container_procs
                    .insert(container_id.to_string(), procs);
                // Container sockets come from the same namespace and are only
                // meaningful next to those pids, so they load together.
                if let Ok(sockets) = collect::container::sockets(container_id) {
                    self.snapshot.sockets.extend(sockets);
                }
                self.rebuild();
            }
            Err(error) => self.set_error(format!("docker: {error}")),
        }
    }

    /// Stop any running capture. Called on navigation — a capture belongs to the
    /// row that started it.
    pub fn stop_capture(&mut self) {
        if self.capture.take().is_some() {
            self.capture_lines.push("— capture stopped —".to_string());
            self.dirty = true;
        }
    }

    /// Record that the reader is now driving expansion themselves, so the
    /// narrow-scope auto-expand stops overriding them on each refresh.
    pub fn mark_expansion_manual(&mut self) {
        self.user_set_expansion = true;
    }

    /// Drop hand-made folds and re-apply the fold state the new scope wants:
    /// everything open when narrow, a collapsed map when server-wide.
    pub fn reset_expansion_for_scope(&mut self) {
        self.expansion = Expansion::default();
        self.user_set_expansion = false;
        if self.scope == Scope::CurrentWindow {
            self.expansion.expand_everything(&self.snapshot);
        }
    }

    pub fn expansion_toggle(&mut self) {
        self.user_set_expansion = true;
        let Some(row) = self.selected_row() else {
            return;
        };
        if !row.expandable {
            return;
        }
        let id = row.id.clone();
        // Expanding a container is what triggers its (expensive) process fetch.
        if let NodeId::Container(container_id) = &id
            && !self.expansion.is_expanded(&id)
        {
            let container_id = container_id.clone();
            self.load_container_procs(&container_id);
        }
        self.expansion.toggle(&id);
        self.rebuild();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn facet_tabs_cover_every_axis_once() {
        // The tab strip is built from ALL, so a facet missing here is invisible.
        assert_eq!(Facet::ALL.len(), 8);
        let titles: Vec<&str> = Facet::ALL.iter().map(|facet| facet.title()).collect();
        assert_eq!(
            titles,
            [
                "overview", "net", "files", "output", "streams", "stack", "env", "packets"
            ]
        );
    }

    #[test]
    fn status_ttl_is_longer_for_errors() {
        let error = Status {
            message: "x".into(),
            is_error: true,
            at: Instant::now(),
        };
        let info = Status {
            message: "x".into(),
            is_error: false,
            at: Instant::now(),
        };
        assert!(!error.expired() && !info.expired());
    }

    use crate::model::{Pane, Proc, ProcKey};

    fn app_with_snapshot() -> App {
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
        for (pid, ppid) in [(100u32, 1u32), (101, 100), (102, 100)] {
            let proc = Proc {
                key: ProcKey::host(pid),
                ppid,
                command: format!("worker-{pid}"),
                age_secs: 10,
                cpu_pct: 1.0,
                cpu_time_secs: 1.0,
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
    fn rebuild_keeps_the_selection_on_the_same_node() {
        let mut app = app_with_snapshot();
        app.selected = app.rows.len() - 1;
        let anchor = app.rows[app.selected].id.clone();
        app.rebuild();
        assert_eq!(app.rows[app.selected].id, anchor);
    }

    #[test]
    fn a_vanished_selection_does_not_silently_land_on_another_row() {
        let mut app = app_with_snapshot();
        app.expansion
            .expand(&crate::tree::NodeId::Process(ProcKey::host(100)));
        app.rebuild();
        // Select the last child, then remove it from the world.
        app.selected = app.rows.len() - 1;
        let vanished = app.rows[app.selected].id.clone();
        app.snapshot.procs.remove(&ProcKey::host(102));
        app.snapshot
            .children
            .get_mut(&100)
            .unwrap()
            .retain(|pid| *pid != 102);
        app.rebuild();

        assert_ne!(app.rows[app.selected].id, vanished);
        // And the index is still inside the list rather than dangling past it.
        assert!(app.selected < app.rows.len());
    }

    #[test]
    fn container_sockets_survive_a_snapshot_swap() {
        let mut app = app_with_snapshot();
        let container_key = ProcKey::in_container("cafe", 7);
        app.snapshot.sockets.insert(
            container_key.clone(),
            vec![crate::model::Socket {
                proto: crate::model::Proto::Tcp,
                local: "0.0.0.0:53".into(),
                peer: None,
                state: crate::model::SocketState::Listen,
            }],
        );
        app.snapshot.container_procs.insert("cafe".into(), vec![]);

        // A fresh host-only snapshot, as the collector produces.
        app.apply_snapshot(Snapshot::default());

        assert!(
            app.snapshot.sockets.contains_key(&container_key),
            "host collectors never re-observe container sockets, so they must be carried over"
        );
        assert!(app.snapshot.container_procs.contains_key("cafe"));
    }

    #[test]
    fn the_narrow_scope_opens_process_subtrees_without_a_keypress() {
        // The shell is never the interesting row; the chain under it is. Before
        // this, the first screen showed one `fish` per pane and hid everything
        // `tmux.sh procs` used to print.
        let mut app = App::new(Scope::CurrentWindow);
        app.current_window = None; // no tmux: everything is in scope
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
        for (pid, ppid, command) in [
            (100u32, 1u32, "fish"),
            (101, 100, "ccproxy claude"),
            (102, 101, "claude"),
            (103, 102, "node qmd mcp"),
        ] {
            let proc = Proc {
                key: ProcKey::host(pid),
                ppid,
                command: command.into(),
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

        app.apply_snapshot(snapshot);

        let labels: Vec<String> = app.rows.iter().map(|row| row.label()).collect();
        // The whole chain, four levels deep, from one snapshot.
        assert!(labels.contains(&"ccproxy".to_string()), "{labels:?}");
        assert!(labels.contains(&"claude".to_string()), "{labels:?}");
        assert!(labels.contains(&"node".to_string()), "{labels:?}");
    }

    #[test]
    fn a_refresh_does_not_reopen_what_the_reader_folded() {
        let mut app = App::new(Scope::CurrentWindow);
        app.current_window = None;
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
        app.apply_snapshot(snapshot.clone());
        assert!(app.rows.iter().any(|row| row.label() == "proc-101"));

        // Fold the shell by hand, then let a refresh land.
        app.mark_expansion_manual();
        app.expansion
            .collapse(&crate::tree::NodeId::Process(ProcKey::host(100)));
        app.rebuild();
        app.apply_snapshot(snapshot);

        assert!(
            !app.rows.iter().any(|row| row.label() == "proc-101"),
            "auto-expand must not fight a hand-made fold"
        );
    }

    #[test]
    fn jumping_to_a_parent_crosses_the_display_order() {
        // `h` moves to the row above; a parent jump follows the ppid link, which
        // under a flat sort is somewhere else entirely.
        let mut app = app_with_snapshot();
        app.expansion.expand_everything(&app.snapshot.clone());
        app.rebuild();
        let child = app
            .rows
            .iter()
            .position(|row| matches!(&row.kind, Kind::Process { proc } if proc.key.pid == 101))
            .unwrap();
        app.selected = child;

        app.jump_to_parent();
        let landed = app.selected_proc().unwrap();
        assert_eq!(landed.key.pid, 100);
    }

    #[test]
    fn a_root_process_reports_that_it_has_no_parent() {
        let mut app = app_with_snapshot();
        // pid 100's ppid is 1, which is not a real target.
        let root = app
            .rows
            .iter()
            .position(|row| matches!(&row.kind, Kind::Process { proc } if proc.key.pid == 100))
            .unwrap();
        app.selected = root;
        app.jump_to_parent();
        assert!(app.status.as_ref().is_some_and(|status| status.is_error));
        // And the selection did not move somewhere arbitrary.
        assert_eq!(app.selected, root);
    }

    #[test]
    fn a_jump_to_a_pid_outside_the_view_fails_rather_than_moving() {
        let mut app = app_with_snapshot();
        app.selected = 0;
        assert!(!app.jump_to_pid(999_999));
        assert_eq!(app.selected, 0);
    }
}
