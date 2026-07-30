//! Live packet capture for a selected process or container.
//!
//! Two very different paths, because macOS and Linux namespaces differ:
//!
//! - **Container**: a sidecar joined to the container's network namespace runs
//!   `tcpdump` with `NET_RAW`. No host privileges at all.
//! - **Host**: `/dev/bpf*` is owned by `root:access_bpf`, so `tcpdump` needs
//!   `sudo`. macOS BPF also cannot filter by pid, so the process's own sockets
//!   are turned into a BPF host/port expression — the capture is *scoped to the
//!   process's traffic* rather than truly per-process.
//!
//! Captures always run bounded (`-c` packet count and a wall-clock timeout) so
//! a dump can never become an unattended background tap.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;

use anyhow::{Result, bail};

use crate::model::{Socket, SocketState};

use super::container::SIDECAR_IMAGE;

/// What a capture is pointed at.
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    /// A host process, filtered to the endpoints its sockets currently use.
    HostProcess { pid: u32, filter: String },
    /// A container's whole network namespace.
    Container { id: String, name: String },
}

/// How many packets a single capture collects before exiting on its own.
pub const PACKET_LIMIT: u32 = 200;

/// A running capture. Dropping it kills the child process — a capture must
/// never outlive the view that started it.
pub struct Capture {
    pub target: Target,
    /// The exact command line that was run, shown verbatim in the UI so a
    /// privileged capture is never invisible.
    pub command_line: String,
    child: Child,
    lines: Receiver<String>,
}

impl Capture {
    /// Non-blocking drain of whatever the capture has emitted since last call.
    pub fn drain(&mut self) -> Vec<String> {
        self.lines.try_iter().collect()
    }

    /// Whether the child has exited (packet limit reached, or an error).
    pub fn finished(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)) | Err(_))
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Build a BPF filter matching a host process's current endpoints.
///
/// Returns `None` when the process holds no usable sockets — capturing
/// unfiltered host traffic "for" a process would be misleading, so the caller
/// must refuse rather than fall back to a full tap.
pub fn filter_for_sockets(sockets: &[Socket]) -> Option<String> {
    let mut terms: Vec<String> = Vec::new();
    for socket in sockets {
        match socket.state {
            // A listener's port is the stable identity; its peers are unknown.
            SocketState::Listen => {
                if let Some(port) = socket.local_port() {
                    terms.push(format!("port {port}"));
                }
            }
            // For a live connection both ports pin the flow exactly.
            SocketState::Established => {
                let local = socket.local_port();
                let peer = socket.peer.as_deref().and_then(port_of);
                if let (Some(local), Some(peer)) = (local, peer) {
                    terms.push(format!("(port {local} and port {peer})"));
                }
            }
            SocketState::Other => {}
        }
    }
    terms.sort();
    terms.dedup();
    if terms.is_empty() {
        return None;
    }
    // BPF expressions have a compiler limit; a process with hundreds of
    // connections would blow past it, so cap the term count.
    terms.truncate(24);
    Some(terms.join(" or "))
}

fn port_of(endpoint: &str) -> Option<u16> {
    endpoint.rsplit(':').next()?.parse().ok()
}

/// Interface a host capture listens on.
///
/// macOS has **no `any` device** — `tcpdump -D` lists only real interfaces, and
/// `-i any` fails with `ioctl(SIOCIFCREATE): Operation not permitted` because
/// tcpdump tries to *create* an interface by that name. That is a Linux-only
/// pseudo-device, and using it here silently captured nothing.
///
/// The interface is chosen from the sockets being captured: a loopback-only
/// process needs `lo0`, anything else needs the primary route's interface.
pub fn host_interface(sockets: &[Socket]) -> String {
    let endpoints = sockets
        .iter()
        .flat_map(|socket| std::iter::once(socket.local.as_str()).chain(socket.peer.as_deref()));
    let all_loopback = {
        let mut seen_any = false;
        let only_loopback = endpoints
            .inspect(|_| seen_any = true)
            .all(|endpoint| endpoint.starts_with("127.") || endpoint.starts_with("[::1]"));
        seen_any && only_loopback
    };
    if all_loopback {
        return "lo0".to_string();
    }
    primary_interface().unwrap_or_else(|| "lo0".to_string())
}

/// The interface carrying the default route, e.g. `en0`.
fn primary_interface() -> Option<String> {
    let raw = super::cmd::run("route", &["-n", "get", "default"], super::cmd::FAST).ok()?;
    raw.lines()
        .find_map(|line| line.trim().strip_prefix("interface: "))
        .map(|name| name.trim().to_string())
}

/// The command line a host capture would run, for the confirmation prompt.
/// Nothing is executed — the UI shows this and waits for an explicit `y`.
pub fn host_command_line(pid: u32, interface: &str, filter: &str) -> String {
    format!("sudo tcpdump -i {interface} -n -l -c {PACKET_LIMIT} -q {filter}  # pid {pid}",)
}

/// Start a host capture. Requires `sudo`; the caller must have shown
/// [`host_command_line`] and gotten confirmation first.
///
/// The caller must leave the alternate screen before this and re-enter after:
/// [`authenticate_sudo`] runs a prompt on the real terminal.
pub fn start_host(pid: u32, interface: &str, filter: &str) -> Result<Capture> {
    // The capture child gets no stdin (a background tap must never sit waiting
    // on a tty), so sudo cannot prompt from inside it — it would block forever
    // with the prompt invisible. Authenticate first, on the real terminal, and
    // let the capture reuse the resulting credential cache.
    authenticate_sudo()?;

    let limit = PACKET_LIMIT.to_string();
    // -l line-buffers, so packets stream out instead of arriving in one block
    // when the buffer fills.
    let args = vec![
        "-n", "tcpdump", "-i", interface, "-n", "-l", "-c", &limit, "-q", filter,
    ];
    spawn(
        "sudo",
        &args,
        Target::HostProcess {
            pid,
            filter: filter.to_string(),
        },
        host_command_line(pid, interface, filter),
    )
}

/// Prime sudo's credential cache, prompting on the inherited terminal.
///
/// `sudo -v` validates and caches without running anything, so the password is
/// typed once where the user can actually see the prompt. A failure here (wrong
/// password, or not a sudoer) is reported instead of leaving a wedged child.
fn authenticate_sudo() -> Result<()> {
    if Command::new("sudo")
        .arg("-n")
        .arg("true")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
    {
        return Ok(()); // Already cached; no prompt needed.
    }

    let status = Command::new("sudo")
        .arg("-v")
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()?;
    if !status.success() {
        bail!("sudo authentication failed or was cancelled");
    }
    Ok(())
}

/// The command line a container capture would run.
///
/// `-i any` is correct *here* and only here: the sidecar is Linux, where `any`
/// is a real pseudo-device — and it is what makes a container capture see both
/// `lo` and `eth0` in one run.
pub fn container_command_line(container_id: &str) -> String {
    format!(
        "docker run --rm --net=container:{container_id} --cap-add=NET_RAW {SIDECAR_IMAGE} \
         tcpdump -i any -n -l -c {PACKET_LIMIT} -q",
    )
}

/// Start a capture inside a container's network namespace. Needs no host
/// privileges — `NET_RAW` inside the sidecar is enough.
pub fn start_container(container_id: &str, name: &str) -> Result<Capture> {
    let net_ns = format!("--net=container:{container_id}");
    let limit = PACKET_LIMIT.to_string();
    let args = vec![
        "run",
        "--rm",
        &net_ns,
        "--cap-add=NET_RAW",
        SIDECAR_IMAGE,
        "tcpdump",
        "-i",
        "any",
        "-n",
        "-l",
        "-c",
        &limit,
        "-q",
    ];
    spawn(
        "docker",
        &args,
        Target::Container {
            id: container_id.to_string(),
            name: name.to_string(),
        },
        container_command_line(container_id),
    )
}

fn spawn(program: &str, args: &[&str], target: Target, command_line: String) -> Result<Capture> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let (sender, lines) = channel();
    // tcpdump writes its banner and errors to stderr and packets to stdout;
    // both matter, so both are forwarded into the same stream.
    if let Some(stdout) = child.stdout.take() {
        forward(stdout, sender.clone());
    }
    if let Some(stderr) = child.stderr.take() {
        forward(stderr, sender);
    }
    Ok(Capture {
        target,
        command_line,
        child,
        lines,
    })
}

/// Pump a pipe into the channel from its own thread — reading a capture pipe
/// on the UI thread would block the whole app between packets.
fn forward(pipe: impl std::io::Read + Send + 'static, sender: Sender<String>) {
    thread::spawn(move || {
        for line in BufReader::new(pipe).lines().map_while(Result::ok) {
            if sender.send(line).is_err() {
                break; // Capture dropped; stop reading.
            }
        }
    });
}

/// Whether host capture is possible without `sudo` — true when the user is in
/// the `access_bpf` group. Used to label the prompt accurately.
pub fn host_capture_needs_sudo() -> bool {
    let Ok(groups) = super::cmd::run("id", &["-Gn"], super::cmd::FAST) else {
        return true;
    };
    !groups.split_whitespace().any(|group| group == "access_bpf")
}

/// Reject a capture request that cannot be honored, with the reason. Keeps the
/// "why is this disabled" answer next to the rule instead of in the UI.
pub fn check_host_capture(sockets: &[Socket]) -> Result<String> {
    match filter_for_sockets(sockets) {
        Some(filter) => Ok(filter),
        None => bail!("process holds no TCP/UDP sockets — nothing to capture"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Proto;

    fn listen(port: u16) -> Socket {
        Socket {
            proto: Proto::Tcp,
            local: format!("*:{port}"),
            peer: None,
            state: SocketState::Listen,
        }
    }

    fn established(local: u16, peer: u16) -> Socket {
        Socket {
            proto: Proto::Tcp,
            local: format!("127.0.0.1:{local}"),
            peer: Some(format!("127.0.0.1:{peer}")),
            state: SocketState::Established,
        }
    }

    #[test]
    fn listener_filter_uses_the_port() {
        assert_eq!(filter_for_sockets(&[listen(8080)]).unwrap(), "port 8080");
    }

    #[test]
    fn established_filter_pins_both_ports() {
        assert_eq!(
            filter_for_sockets(&[established(52404, 62646)]).unwrap(),
            "(port 52404 and port 62646)"
        );
    }

    #[test]
    fn duplicate_terms_collapse() {
        let filter = filter_for_sockets(&[listen(8080), listen(8080), listen(9000)]).unwrap();
        assert_eq!(filter, "port 8080 or port 9000");
    }

    #[test]
    fn no_sockets_means_no_capture_rather_than_a_full_tap() {
        assert!(filter_for_sockets(&[]).is_none());
        assert!(check_host_capture(&[]).is_err());
    }

    #[test]
    fn term_count_is_capped_for_the_bpf_compiler() {
        let sockets: Vec<Socket> = (1..100u16).map(|i| listen(1000 + i)).collect();
        let filter = filter_for_sockets(&sockets).unwrap();
        assert_eq!(filter.matches(" or ").count(), 23);
    }

    #[test]
    fn command_lines_are_shown_verbatim_and_bounded() {
        let host = host_command_line(9923, "en0", "port 8080");
        assert!(host.starts_with("sudo tcpdump"));
        assert!(host.contains(&format!("-c {PACKET_LIMIT}")));
        // macOS has no `any` device; using it captured nothing at all.
        assert!(
            !host.contains("-i any"),
            "host capture must name a real interface"
        );
        assert!(host.contains("-i en0"));

        let container = container_command_line("cafe123");
        assert!(container.contains("--net=container:cafe123"));
        assert!(container.contains("--cap-add=NET_RAW"));
        assert!(
            !container.contains("sudo"),
            "container capture must not need sudo"
        );
    }

    #[test]
    fn socket_with_unparseable_peer_is_skipped() {
        let broken = Socket {
            proto: Proto::Tcp,
            local: "127.0.0.1:100".into(),
            peer: Some("garbage".into()),
            state: SocketState::Established,
        };
        assert!(filter_for_sockets(&[broken]).is_none());
    }

    #[test]
    fn a_loopback_only_process_captures_on_lo0() {
        let local_only = Socket {
            proto: Proto::Tcp,
            local: "127.0.0.1:49742".into(),
            peer: None,
            state: SocketState::Listen,
        };
        assert_eq!(host_interface(&[local_only]), "lo0");
    }

    #[test]
    fn an_ipv6_loopback_socket_also_maps_to_lo0() {
        let local_only = Socket {
            proto: Proto::Tcp,
            local: "[::1]:5173".into(),
            peer: None,
            state: SocketState::Listen,
        };
        assert_eq!(host_interface(&[local_only]), "lo0");
    }

    #[test]
    fn a_process_talking_off_box_does_not_capture_on_loopback() {
        let external = Socket {
            proto: Proto::Tcp,
            local: "192.168.0.208:52633".into(),
            peer: Some("54.180.38.29:443".into()),
            state: SocketState::Established,
        };
        // Whatever the primary interface is, it must not be loopback — a
        // loopback capture would see none of this traffic.
        assert_ne!(host_interface(&[external]), "lo0");
    }

    #[test]
    fn a_mixed_process_uses_the_routed_interface_not_loopback() {
        let sockets = vec![
            Socket {
                proto: Proto::Tcp,
                local: "127.0.0.1:49742".into(),
                peer: None,
                state: SocketState::Listen,
            },
            Socket {
                proto: Proto::Tcp,
                local: "192.168.0.208:52633".into(),
                peer: Some("54.180.38.29:443".into()),
                state: SocketState::Established,
            },
        ];
        assert_ne!(host_interface(&sockets), "lo0");
    }
}
