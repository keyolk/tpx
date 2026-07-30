//! Core data types shared by collectors and the UI.
//!
//! The central abstraction is [`Origin`] — a process id is only meaningful
//! together with the namespace it was observed in. macOS host pids and
//! in-container Linux pids live in disjoint spaces (on OrbStack/Docker Desktop
//! the container's pids do not appear in the host `ps` output at all), so a
//! bare `u32` is never a safe key.

use std::collections::HashMap;

/// Which pid namespace a process id belongs to.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum Origin {
    /// The macOS host, observed via `ps`.
    Host,
    /// Inside a container's pid namespace, observed via `docker top`.
    /// Holds the container's full id.
    Container(String),
}

/// A process id qualified by its namespace. The only safe process key.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct ProcKey {
    pub origin: Origin,
    pub pid: u32,
}

impl ProcKey {
    pub fn host(pid: u32) -> Self {
        Self {
            origin: Origin::Host,
            pid,
        }
    }

    pub fn in_container(container_id: &str, pid: u32) -> Self {
        Self {
            origin: Origin::Container(container_id.to_string()),
            pid,
        }
    }
}

/// One process as observed by a collector.
#[derive(Clone, Debug)]
pub struct Proc {
    pub key: ProcKey,
    pub ppid: u32,
    /// Full command line, unshortened. The UI shortens for display.
    pub command: String,
    /// Wall-clock age in seconds.
    pub age_secs: u64,
    /// Current cpu usage as a percent of one core, derived from the change in
    /// [`Self::cpu_time_secs`] between two snapshots. Zero on the first
    /// snapshot, since a rate needs two samples.
    pub cpu_pct: f32,
    /// Total cpu time consumed since the process started. The raw counter that
    /// [`Self::cpu_pct`] is derived from — `ps`'s own `%cpu` on macOS is a
    /// lifetime average and reads near zero for a long-lived busy process.
    pub cpu_time_secs: f64,
    /// Resident set size in bytes.
    pub rss_bytes: u64,
    /// `ps` state field (`S`, `R`, `Z`, `Ss+`, …). Empty when unknown.
    pub state: String,
    /// Thread count. `None` on the host — macOS `ps` has no `nlwp`/`thcount`
    /// keyword, so this is filled lazily per-process via `ps -M`.
    pub threads: Option<u32>,
    /// Open file-descriptor count, filled lazily (a full `lsof` per process).
    pub fd_count: Option<u32>,
}

impl Proc {
    /// Argv[0] basename — the short name for narrow columns.
    pub fn name(&self) -> &str {
        let first = self.command.split_whitespace().next().unwrap_or("");
        first.rsplit('/').next().unwrap_or(first)
    }

    /// Command line with the noise stripped, for display.
    ///
    /// A real command line is mostly interpreter paths and inlined config:
    /// `node /Users/me/.bun/bin/qmd mcp` and a `claude` invocation carrying a
    /// 200-character `--settings` JSON blob. Printed raw, the argument text
    /// swamps the tree structure it is supposed to annotate. These are the same
    /// rules `tmux.sh procs` used, which is what made its output readable.
    pub fn display_command(&self, max_width: usize) -> String {
        let home = std::env::var("HOME").unwrap_or_default();
        let mut command = self.command.clone();
        if !home.is_empty() {
            command = command.replace(&home, "~");
        }
        for prefix in NOISE_PATH_PREFIXES {
            command = strip_path_prefixes(&command, prefix);
        }
        truncate_display(&command, max_width)
    }
}

/// Directory prefixes that carry no information in a process listing — every
/// argument starting with one is reduced to its basename-ward remainder.
const NOISE_PATH_PREFIXES: [&str; 6] = [
    "/opt/homebrew/bin/",
    "~/.bun/bin/",
    "~/.bun/install/global/node_modules/",
    "~/.local/bin/",
    "/usr/local/bin/",
    "/usr/bin/",
];

/// Drop `prefix` wherever it begins a whitespace-delimited token, plus the
/// `node_modules/` and `mise` interpreter paths that vary per install.
fn strip_path_prefixes(command: &str, prefix: &str) -> String {
    command
        .split(' ')
        .map(|token| {
            let token = token.strip_prefix(prefix).unwrap_or(token);
            // `.../node_modules/.bin/foo` and `.../node_modules/foo` → `foo`
            match token.rsplit_once("/node_modules/") {
                Some((_, rest)) => rest.strip_prefix(".bin/").unwrap_or(rest),
                None => token,
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Truncate to `max_width` display cells, marking the cut.
///
/// Cell width, not byte or char count: a CJK path would otherwise overflow the
/// column it was measured against.
fn truncate_display(text: &str, max_width: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    if max_width == 0 {
        return String::new();
    }
    let mut width = 0;
    let mut cut = None;
    for (index, ch) in text.char_indices() {
        let char_width = ch.width().unwrap_or(0);
        if width + char_width > max_width.saturating_sub(3) {
            cut = Some(index);
            break;
        }
        width += char_width;
    }
    match cut {
        Some(index) => format!("{}...", &text[..index]),
        None => text.to_string(),
    }
}

/// A socket a process holds. Both listening and established sockets use this;
/// `peer` distinguishes them.
#[derive(Clone, Debug, PartialEq)]
pub struct Socket {
    pub proto: Proto,
    pub local: String,
    /// `None` for a listening socket.
    pub peer: Option<String>,
    pub state: SocketState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Proto {
    Tcp,
    Udp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SocketState {
    Listen,
    Established,
    Other,
}

impl Socket {
    /// The port half of `local`, for conflict detection.
    pub fn local_port(&self) -> Option<u16> {
        self.local.rsplit(':').next()?.parse().ok()
    }
}

/// Cumulative per-process network counters from `nettop`. macOS reports these
/// per process, not per socket, so they cannot be attributed to a single peer.
#[derive(Clone, Copy, Debug, Default)]
pub struct NetCounters {
    pub bytes_in: u64,
    pub bytes_out: u64,
}

/// A tmux pane and the shell process rooted in it.
#[derive(Clone, Debug)]
pub struct Pane {
    pub session: String,
    pub window_index: u32,
    pub window_name: String,
    pub pane_index: u32,
    /// `session:window.pane` — the tmux target string, and the pane's identity.
    pub target: String,
    pub cwd: String,
    /// The pane's foreground command as tmux reports it.
    pub current_command: String,
    /// Root process of the pane (the shell). Host namespace by definition.
    pub pid: u32,
    pub active: bool,
    pub window_active: bool,
    pub session_attached: bool,
    pub zoomed: bool,
}

/// A container, with the host-side pid of its init process when the runtime
/// exposes one that the host can actually see.
#[derive(Clone, Debug)]
pub struct Container {
    pub id: String,
    pub short_id: String,
    pub name: String,
    pub image: String,
    pub status: String,
    pub running: bool,
    /// `State.Pid` as the daemon reports it. On a VM-backed runtime
    /// (OrbStack, Docker Desktop) this pid lives in the VM and is NOT
    /// resolvable on the host — never look it up in host `ps`.
    pub init_pid: u32,
    pub compose_project: Option<String>,
    pub compose_working_dir: Option<String>,
    pub network_mode: String,
    /// Published host->container port mappings, pre-formatted.
    pub ports: Vec<String>,
    pub metrics: Option<ContainerMetrics>,
    /// Why this container is linked to a pane, if it is.
    pub attribution: Option<Attribution>,
}

impl Container {
    /// Display name. A kubelet-managed container's real name is a machine-built
    /// string like
    /// `k8s_coredns_coredns-58db975755-8r9tf_kube-system_92e2…_3`, which is far
    /// too wide for a tree row — the container and pod names are the readable
    /// part, so those are what is shown.
    pub fn display_name(&self) -> String {
        let Some(rest) = self.name.strip_prefix("k8s_") else {
            return self.name.clone();
        };
        let mut fields = rest.split('_');
        let container = fields.next().unwrap_or(rest);
        let pod = fields.next().unwrap_or("");
        // The pause container of a pod sandbox is named `k8s_POD_<pod>_…`.
        if container == "POD" {
            return format!("{pod} (sandbox)");
        }
        if pod.is_empty() {
            container.to_string()
        } else {
            format!("{container} · {pod}")
        }
    }

    /// Display image. An image referenced only by digest shows as
    /// `sha256:97e04611ad43…`, where the full 64-hex digest is noise; the short
    /// form still identifies it.
    pub fn display_image(&self) -> String {
        match self.image.strip_prefix("sha256:") {
            Some(digest) => format!("sha256:{}", &digest[..digest.len().min(12)]),
            None => self.image.clone(),
        }
    }

    /// Whether this container is managed by a kubelet rather than started by a
    /// human. They are numerous and rarely the thing being debugged, so the UI
    /// can group them separately.
    pub fn is_kubernetes(&self) -> bool {
        self.name.starts_with("k8s_")
    }
}

/// Live resource usage from `docker stats`.
#[derive(Clone, Debug, Default)]
pub struct ContainerMetrics {
    pub cpu_pct: f32,
    pub mem_bytes: u64,
    pub mem_limit_bytes: u64,
    pub pids: u32,
    pub net_in_bytes: u64,
    pub net_out_bytes: u64,
    pub block_read_bytes: u64,
    pub block_write_bytes: u64,
}

/// How a container was tied back to a tmux pane. Attribution is heuristic —
/// the container's own metadata never names the pane that started it — so the
/// reason is carried along and shown, letting the reader judge it.
#[derive(Clone, Debug)]
pub struct Attribution {
    /// Pane target string.
    pub pane_target: String,
    pub reason: AttributionReason,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttributionReason {
    /// A compose project whose working_dir matches the pane's cwd.
    ComposeWorkingDir,
    /// A `docker` CLI process in the pane names this container or image.
    DockerCliArgs,
}

impl AttributionReason {
    pub fn label(self) -> &'static str {
        match self {
            Self::ComposeWorkingDir => "compose cwd",
            Self::DockerCliArgs => "docker cli",
        }
    }
}

/// One consistent observation of the whole world. The UI always renders from a
/// single snapshot so no frame mixes two collection rounds.
#[derive(Clone, Default)]
pub struct Snapshot {
    pub panes: Vec<Pane>,
    /// Every host process, keyed for O(1) lookup during tree building.
    pub procs: HashMap<ProcKey, Proc>,
    /// pid -> children, host namespace. Built once per snapshot.
    pub children: HashMap<u32, Vec<u32>>,
    pub sockets: HashMap<ProcKey, Vec<Socket>>,
    pub net_counters: HashMap<u32, NetCounters>,
    pub containers: Vec<Container>,
    /// In-container process trees, keyed by container id.
    pub container_procs: HashMap<String, Vec<Proc>>,
    /// Collectors that failed this round, with their reason. Shown rather than
    /// silently rendering an empty panel.
    pub errors: Vec<CollectError>,
}

#[derive(Clone, Debug)]
pub struct CollectError {
    pub source: &'static str,
    pub message: String,
}

impl Snapshot {
    pub fn proc(&self, key: &ProcKey) -> Option<&Proc> {
        self.procs.get(key)
    }

    pub fn host_children(&self, pid: u32) -> &[u32] {
        self.children.get(&pid).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn container(&self, id: &str) -> Option<&Container> {
        self.containers.iter().find(|c| c.id == id)
    }

    /// Ports listened on by more than one process — a real, actionable signal
    /// when two panes both try to own a dev-server port.
    pub fn port_conflicts(&self) -> HashMap<u16, Vec<ProcKey>> {
        let mut by_port: HashMap<u16, Vec<ProcKey>> = HashMap::new();
        for (key, sockets) in &self.sockets {
            for socket in sockets {
                if socket.state != SocketState::Listen {
                    continue;
                }
                let Some(port) = socket.local_port() else {
                    continue;
                };
                let holders = by_port.entry(port).or_default();
                if !holders.contains(key) {
                    holders.push(key.clone());
                }
            }
        }
        by_port.retain(|_, holders| holders.len() > 1);
        by_port
    }
}

/// Aggregate resource usage of a subtree, computed for window/pane headers.
#[derive(Clone, Copy, Debug, Default)]
pub struct Rollup {
    pub proc_count: u32,
    pub cpu_pct: f32,
    pub rss_bytes: u64,
    pub listen_ports: u32,
}

impl Rollup {
    pub fn add_proc(&mut self, proc: &Proc) {
        self.proc_count += 1;
        self.cpu_pct += proc.cpu_pct;
        self.rss_bytes += proc.rss_bytes;
    }

    pub fn merge(&mut self, other: Rollup) {
        self.proc_count += other.proc_count;
        self.cpu_pct += other.cpu_pct;
        self.rss_bytes += other.rss_bytes;
        self.listen_ports += other.listen_ports;
    }
}

/// Human-readable byte size, compact enough for a table cell.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [(u64, &str); 4] = [(1 << 30, "G"), (1 << 20, "M"), (1 << 10, "K"), (1, "B")];
    for (scale, suffix) in UNITS {
        if bytes >= scale {
            let value = bytes as f64 / scale as f64;
            return if value >= 100.0 || scale == 1 {
                format!("{:.0}{suffix}", value)
            } else {
                format!("{value:.1}{suffix}")
            };
        }
    }
    "0B".to_string()
}

/// Compact age: `3d4h`, `5h12m`, `7m`, `42s`.
pub fn human_age(total_secs: u64) -> String {
    let days = total_secs / 86_400;
    let hours = (total_secs % 86_400) / 3_600;
    let minutes = (total_secs % 3_600) / 60;
    let seconds = total_secs % 60;
    if days > 0 {
        return if hours > 0 {
            format!("{days}d{hours}h")
        } else {
            format!("{days}d")
        };
    }
    if hours > 0 {
        return if minutes > 0 {
            format!("{hours}h{minutes}m")
        } else {
            format!("{hours}h")
        };
    }
    if minutes > 0 {
        return format!("{minutes}m");
    }
    format!("{seconds}s")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc_with(pid: u32, command: &str) -> Proc {
        Proc {
            key: ProcKey::host(pid),
            ppid: 1,
            command: command.to_string(),
            age_secs: 0,
            cpu_pct: 0.0,
            cpu_time_secs: 0.0,
            rss_bytes: 0,
            state: String::new(),
            threads: None,
            fd_count: None,
        }
    }

    #[test]
    fn proc_name_strips_path_and_args() {
        assert_eq!(proc_with(1, "/opt/homebrew/bin/fish -l").name(), "fish");
        assert_eq!(proc_with(1, "cargo run --release").name(), "cargo");
    }

    #[test]
    fn host_and_container_pids_are_distinct_keys() {
        assert_ne!(ProcKey::host(7), ProcKey::in_container("abc", 7));
    }

    #[test]
    fn socket_local_port_parses_ipv4_and_ipv6() {
        let listen = |local: &str| Socket {
            proto: Proto::Tcp,
            local: local.to_string(),
            peer: None,
            state: SocketState::Listen,
        };
        assert_eq!(listen("127.0.0.1:8080").local_port(), Some(8080));
        assert_eq!(listen("[::1]:5173").local_port(), Some(5173));
        assert_eq!(listen("*:443").local_port(), Some(443));
    }

    #[test]
    fn port_conflicts_reports_only_multi_holder_ports() {
        let mut snapshot = Snapshot::default();
        let listen = |port: u16| Socket {
            proto: Proto::Tcp,
            local: format!("*:{port}"),
            peer: None,
            state: SocketState::Listen,
        };
        snapshot
            .sockets
            .insert(ProcKey::host(1), vec![listen(8080), listen(9000)]);
        snapshot
            .sockets
            .insert(ProcKey::host(2), vec![listen(8080)]);

        let conflicts = snapshot.port_conflicts();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[&8080].len(), 2);
    }

    #[test]
    fn human_bytes_switches_precision_at_100() {
        assert_eq!(human_bytes(0), "0B");
        assert_eq!(human_bytes(13_504), "13.2K");
        assert_eq!(human_bytes(236_548 * 1024), "231M");
        assert_eq!(human_bytes(1_288_490_189), "1.2G");
    }

    #[test]
    fn human_age_is_compact() {
        assert_eq!(human_age(42), "42s");
        assert_eq!(human_age(7 * 60), "7m");
        assert_eq!(human_age(5 * 3600 + 12 * 60), "5h12m");
        assert_eq!(human_age(3 * 86_400 + 4 * 3600), "3d4h");
        assert_eq!(human_age(3 * 86_400), "3d");
    }

    fn container_named(name: &str, image: &str) -> Container {
        Container {
            id: "abc".into(),
            short_id: "abc".into(),
            name: name.into(),
            image: image.into(),
            status: "running".into(),
            running: true,
            init_pid: 1,
            compose_project: None,
            compose_working_dir: None,
            network_mode: "bridge".into(),
            ports: vec![],
            metrics: None,
            attribution: None,
        }
    }

    #[test]
    fn kubelet_container_names_shorten_to_container_and_pod() {
        let container = container_named(
            "k8s_coredns_coredns-58db975755-8r9tf_kube-system_92e22aea-459e-4f2a-ba68-3b40d43d2090_3",
            "x",
        );
        assert_eq!(
            container.display_name(),
            "coredns · coredns-58db975755-8r9tf"
        );
        assert!(container.is_kubernetes());
    }

    #[test]
    fn pod_sandbox_containers_are_labelled_as_such() {
        let container = container_named(
            "k8s_POD_coredns-58db975755-8r9tf_kube-system_92e2_3",
            "rancher/mirrored-pause:3.6",
        );
        assert_eq!(
            container.display_name(),
            "coredns-58db975755-8r9tf (sandbox)"
        );
    }

    #[test]
    fn ordinary_container_names_are_untouched() {
        let container = container_named("beautiful_feynman", "cl-ms-check");
        assert_eq!(container.display_name(), "beautiful_feynman");
        assert!(!container.is_kubernetes());
        assert_eq!(container.display_image(), "cl-ms-check");
    }

    #[test]
    fn digest_only_images_shorten_but_stay_identifiable() {
        let container = container_named(
            "x",
            "sha256:97e04611ad43405a2e5863ae17c6f1bc9181bdefdaa78627c432ef754a4eb108",
        );
        assert_eq!(container.display_image(), "sha256:97e04611ad43");
    }
}
