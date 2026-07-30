//! Where a process's stdout/stderr actually go, and how to read them.
//!
//! macOS has no `/proc/PID/fd/1` to read, and live write-snooping (`dtrace`,
//! `fs_usage`) is blocked by System Integrity Protection even with `sudo`. So
//! output is not *captured* — it is *located*, and read from wherever it already
//! lands. Four cases, measured across ~1300 fds on a real machine:
//!
//! | target | seen | what is possible |
//! |---|---|---|
//! | `/dev/null` | 944 | nothing, and saying so is the useful answer |
//! | tty | ~30 | the owning tmux pane's scrollback, via `capture-pane` |
//! | regular file | 43 | read it directly — the richest case |
//! | pipe / unix socket | 185 | name the peer process; content is unreachable |
//!
//! The tty case is what makes this worth doing in a tmux tool: `pane_tty` maps a
//! terminal device back to a pane target, so "this process's stdout" becomes
//! "that pane's scrollback" — an answer no generic process viewer can give.

use std::collections::HashMap;

use anyhow::Result;

use super::cmd;

/// Where one of a process's output streams goes.
#[derive(Clone, Debug, PartialEq)]
pub enum Sink {
    /// Discarded. Common, and worth stating explicitly rather than showing an
    /// empty panel that looks like a failure.
    Discarded,
    /// A terminal. `pane` is the tmux pane whose scrollback holds this output —
    /// either because the tty *is* the pane's, or because the process runs inside
    /// that pane and its pty output is echoed there.
    Terminal {
        tty: String,
        pane: Option<String>,
        /// Whether the tty is the pane's own device. False means the process
        /// writes to a nested pty (a wrapper like `ccproxy` spawning `claude`),
        /// so the pane's scrollback shows it only as the wrapper relays it.
        direct: bool,
    },
    /// A regular file — a log or a redirect. Directly readable.
    File { path: String },
    /// A pipe or unix socket. `peer` names the process on the other end when it
    /// could be resolved; the bytes themselves are not reachable.
    Stream {
        kind: StreamKind,
        /// lsof's device address. Both ends of a pipe share it, which is the
        /// only handle macOS gives for finding the peer.
        device: String,
        peer: Option<Peer>,
    },
    /// The fd is closed, or lsof could not see it.
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamKind {
    Pipe,
    UnixSocket,
}

impl StreamKind {
    pub fn label(self) -> &'static str {
        match self {
            StreamKind::Pipe => "pipe",
            StreamKind::UnixSocket => "unix socket",
        }
    }
}

/// The process on the other end of a pipe or socket.
#[derive(Clone, Debug, PartialEq)]
pub struct Peer {
    pub pid: u32,
    pub name: String,
}

impl Sink {
    /// One-line description for the UI.
    pub fn summary(&self) -> String {
        match self {
            Sink::Discarded => "discarded (/dev/null)".to_string(),
            Sink::Terminal {
                tty,
                pane: Some(pane),
                direct: true,
            } => format!("tty {tty} — tmux pane {pane}"),
            Sink::Terminal {
                tty,
                pane: Some(pane),
                direct: false,
            } => format!("pty {tty} — relayed into tmux pane {pane}"),
            Sink::Terminal {
                tty, pane: None, ..
            } => format!("tty {tty} (no tmux pane)"),
            Sink::File { path } => format!("file {path}"),
            Sink::Stream {
                kind,
                peer: Some(peer),
                ..
            } => format!("{} to {} (pid {})", kind.label(), peer.name, peer.pid),
            Sink::Stream {
                kind, peer: None, ..
            } => kind.label().to_string(),
            Sink::Unknown => "unknown".to_string(),
        }
    }

    /// Whether [`read`] can produce content for this sink.
    pub fn is_readable(&self) -> bool {
        matches!(
            self,
            Sink::File { .. } | Sink::Terminal { pane: Some(_), .. }
        )
    }
}

/// Both output streams of a process.
#[derive(Clone, Debug, PartialEq)]
pub struct Streams {
    pub stdout: Sink,
    pub stderr: Sink,
}

/// Locate stdout and stderr for one host process.
///
/// `owning_pane` is the pane whose process tree contains `pid`, which the caller
/// already knows. It matters because a wrapper that spawns its child on a nested
/// pty (`ccproxy` → `claude`) leaves the child writing to a tty that is *not* any
/// pane's device — without the fallback those processes report "no tmux pane"
/// even though their output is visible in a pane.
pub fn locate(pid: u32, owning_pane: Option<&str>) -> Result<Streams> {
    // `-a` is mandatory: lsof combines selection flags with OR, so `-p PID -d 1,2`
    // without it returns fd 1/2 for *every* process on the machine (681 here) and
    // the parser would report the first one's streams as this process's.
    let raw = cmd::run(
        "lsof",
        &[
            "-nP",
            "-a",
            "-p",
            &pid.to_string(),
            "-d",
            "1,2",
            "-F",
            "ftdn",
        ],
        cmd::FAST,
    )?;
    let mut streams = parse_streams(&raw);

    let pane_by_tty = pane_tty_map().unwrap_or_default();
    for sink in [&mut streams.stdout, &mut streams.stderr] {
        if let Sink::Terminal { tty, pane, direct } = sink {
            match pane_by_tty.get(tty.as_str()) {
                Some(target) => {
                    *pane = Some(target.clone());
                    *direct = true;
                }
                None => {
                    *pane = owning_pane.map(str::to_string);
                    *direct = false;
                }
            }
        }
    }
    Ok(streams)
}

/// Locate stdout/stderr for **every** host process in one pass.
///
/// A per-process `lsof` costs ~25ms; over a server-wide tree that was 26 seconds
/// of subprocess spawns. One bulk call covering every fd 1/2 on the machine takes
/// 0.23s total, so any caller annotating more than a handful of processes should
/// use this instead of [`locate`].
pub fn locate_all() -> Result<HashMap<u32, Streams>> {
    let raw = cmd::run("lsof", &["-nP", "-d", "1,2", "-F", "pftdn"], cmd::FAST)?;
    let mut by_pid = parse_all(&raw);

    let pane_by_tty = pane_tty_map().unwrap_or_default();
    for streams in by_pid.values_mut() {
        for sink in [&mut streams.stdout, &mut streams.stderr] {
            if let Sink::Terminal { tty, pane, direct } = sink
                && let Some(target) = pane_by_tty.get(tty.as_str())
            {
                *pane = Some(target.clone());
                *direct = true;
            }
        }
    }
    Ok(by_pid)
}

/// Parse a bulk `lsof -F pftdn` listing into per-pid streams.
fn parse_all(raw: &str) -> HashMap<u32, Streams> {
    let mut by_pid: HashMap<u32, Streams> = HashMap::new();
    let mut pid = 0u32;
    let mut fd = String::new();
    let mut kind = String::new();
    let mut device = String::new();

    for line in raw.lines() {
        let Some((tag, value)) = line.split_at_checked(1) else {
            continue;
        };
        match tag {
            "p" => pid = value.parse().unwrap_or(0),
            "f" => {
                fd = value.to_string();
                kind.clear();
                device.clear();
            }
            "t" => kind = value.to_string(),
            "d" => device = value.to_string(),
            "n" => {
                if pid == 0 {
                    continue;
                }
                let sink = classify(&kind, value, &device);
                let entry = by_pid.entry(pid).or_insert_with(|| Streams {
                    stdout: Sink::Unknown,
                    stderr: Sink::Unknown,
                });
                match fd.as_str() {
                    "1" => entry.stdout = sink,
                    "2" => entry.stderr = sink,
                    _ => {}
                }
            }
            _ => {}
        }
    }
    by_pid
}

/// Fill in the pane for any terminal sink that is a nested pty rather than a
/// pane's own device, using the pane that owns the process.
///
/// Split from [`locate_all`] because the owning pane is only known to the caller
/// walking the tree, not to the bulk collector.
pub fn attach_owning_pane(streams: &mut Streams, owning_pane: Option<&str>) {
    for sink in [&mut streams.stdout, &mut streams.stderr] {
        if let Sink::Terminal { pane, direct, .. } = sink
            && pane.is_none()
            && !*direct
        {
            *pane = owning_pane.map(str::to_string);
        }
    }
}

/// Snapshot of which processes hold which pipe/socket devices.
///
/// Building it costs a machine-wide `lsof` (~250ms), so a caller stepping through
/// rows should build it once and reuse it rather than paying that per row — a
/// `j`/`k` sweep with the streams facet open would otherwise stall for a quarter
/// second on every process that owns a pipe.
pub struct PeerTable(Holders);

impl PeerTable {
    pub fn collect() -> Result<Self> {
        let raw = cmd::run("lsof", &["-nP", "-F", "pcftdn"], cmd::FAST)?;
        Ok(Self(parse_holders(&raw)))
    }
}

/// Resolve the peers of any pipe/socket sinks using a prebuilt [`PeerTable`].
pub fn resolve_peers_with(streams: &mut Streams, self_pid: u32, table: &PeerTable) {
    for sink in [&mut streams.stdout, &mut streams.stderr] {
        if let Sink::Stream { device, peer, .. } = sink
            && peer.is_none()
        {
            *peer = pick_peer(&table.0, device, self_pid);
        }
    }
}

/// Resolve peers with a freshly collected table. Convenient for one-shot use;
/// prefer [`resolve_peers_with`] when more than one process is involved.
pub fn resolve_peers(streams: &mut Streams, self_pid: u32) -> Result<()> {
    let needs_peer = [&streams.stdout, &streams.stderr]
        .iter()
        .any(|sink| matches!(sink, Sink::Stream { peer: None, .. }));
    if !needs_peer {
        return Ok(());
    }

    let raw = cmd::run("lsof", &["-nP", "-F", "pcftdn"], cmd::FAST)?;
    let holders = parse_holders(&raw);
    // A pipe's two ends share a device address; the peer is whoever else holds it.
    for sink in [&mut streams.stdout, &mut streams.stderr] {
        if let Sink::Stream { device, peer, .. } = sink
            && peer.is_none()
        {
            *peer = pick_peer(&holders, device, self_pid);
        }
    }
    Ok(())
}

/// Device address -> the processes holding it.
type Holders = HashMap<String, Vec<Peer>>;

fn pick_peer(holders: &Holders, device: &str, self_pid: u32) -> Option<Peer> {
    holders
        .get(device)?
        .iter()
        .find(|peer| peer.pid != self_pid)
        .cloned()
}

/// Read whatever content a sink can offer.
///
/// `max_bytes` bounds a log file read: a 250 MB log must not be pulled into
/// memory to show its tail.
pub fn read(sink: &Sink, max_bytes: u64) -> Result<String> {
    match sink {
        Sink::File { path } => tail_file(path, max_bytes),
        Sink::Terminal {
            pane: Some(pane), ..
        } => {
            // The pane's scrollback *is* this process's stdout — anything it
            // wrote to the tty is what the pane shows.
            super::tmux::capture_pane(pane, 400)
        }
        Sink::Terminal { pane: None, .. } => {
            anyhow::bail!("writes to a terminal outside tmux — no scrollback to read")
        }
        Sink::Discarded => anyhow::bail!("output is discarded (/dev/null)"),
        Sink::Stream { kind, peer, .. } => {
            let peer = peer
                .as_ref()
                .map(|peer| format!(" to {} (pid {})", peer.name, peer.pid))
                .unwrap_or_default();
            anyhow::bail!(
                "output goes through a {}{peer}; macOS blocks reading it \
                 (SIP disables dtrace/fs_usage even under sudo)",
                kind.label()
            )
        }
        Sink::Unknown => anyhow::bail!("stream not found — the fd may be closed"),
    }
}

/// Last `max_bytes` of a file, starting at a line boundary.
fn tail_file(path: &str, max_bytes: u64) -> Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let from = len.saturating_sub(max_bytes);
    file.seek(SeekFrom::Start(from))?;
    let mut buffer = Vec::new();
    file.take(max_bytes).read_to_end(&mut buffer)?;

    let text = String::from_utf8_lossy(&buffer).into_owned();
    // A mid-file read almost certainly starts mid-line; dropping the partial
    // first line avoids showing a fragment as if it were a record.
    if from > 0
        && let Some(newline) = text.find('\n')
    {
        return Ok(text[newline + 1..].to_string());
    }
    Ok(text)
}

/// Parse `lsof -F ftdn` for fds 1 and 2.
fn parse_streams(raw: &str) -> Streams {
    let mut streams = Streams {
        stdout: Sink::Unknown,
        stderr: Sink::Unknown,
    };
    let mut fd = String::new();
    let mut kind = String::new();
    let mut device = String::new();

    for line in raw.lines() {
        let Some((tag, value)) = line.split_at_checked(1) else {
            continue;
        };
        match tag {
            "f" => {
                fd = value.to_string();
                kind.clear();
                device.clear();
            }
            "t" => kind = value.to_string(),
            "d" => device = value.to_string(),
            "n" => {
                let sink = classify(&kind, value, &device);
                match fd.as_str() {
                    "1" => streams.stdout = sink,
                    "2" => streams.stderr = sink,
                    _ => {}
                }
            }
            _ => {}
        }
    }
    streams
}

fn classify(kind: &str, name: &str, device: &str) -> Sink {
    match kind {
        "CHR" if name == "/dev/null" => Sink::Discarded,
        // A tty is a CHR device under /dev/tty* or /dev/pty*.
        "CHR" if name.starts_with("/dev/tty") || name.starts_with("/dev/pty") => Sink::Terminal {
            tty: name.to_string(),
            pane: None,
            direct: false,
        },
        "REG" => Sink::File {
            path: name.to_string(),
        },
        "PIPE" => Sink::Stream {
            kind: StreamKind::Pipe,
            device: device.to_string(),
            peer: None,
        },
        "unix" => Sink::Stream {
            kind: StreamKind::UnixSocket,
            device: device.to_string(),
            peer: None,
        },
        // Any other CHR device (a printer, /dev/console) is not readable here.
        "CHR" => Sink::Terminal {
            tty: name.to_string(),
            pane: None,
            direct: false,
        },
        _ => Sink::Unknown,
    }
}

/// Parse a machine-wide `lsof -F pcftdn` into device -> holders.
fn parse_holders(raw: &str) -> Holders {
    let mut holders: Holders = HashMap::new();
    let mut pid = 0u32;
    let mut name = String::new();
    let mut device = String::new();

    for line in raw.lines() {
        let Some((tag, value)) = line.split_at_checked(1) else {
            continue;
        };
        match tag {
            "p" => pid = value.parse().unwrap_or(0),
            "c" => name = value.to_string(),
            "d" => device = value.to_string(),
            "n" => {
                if !device.is_empty() && pid != 0 {
                    let entry = holders.entry(device.clone()).or_default();
                    if !entry.iter().any(|peer| peer.pid == pid) {
                        entry.push(Peer {
                            pid,
                            name: name.clone(),
                        });
                    }
                }
            }
            _ => {}
        }
    }
    holders
}

/// tty device -> tmux pane target, for the terminal case.
fn pane_tty_map() -> Result<HashMap<String, String>> {
    let raw = cmd::run(
        "tmux",
        &[
            "list-panes",
            "-a",
            "-F",
            "#{pane_tty}\t#{session_name}:#{window_index}.#{pane_index}",
        ],
        cmd::FAST,
    )?;
    Ok(raw
        .lines()
        .filter_map(|line| {
            let (tty, target) = line.split_once('\t')?;
            Some((tty.to_string(), target.to_string()))
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dev_null_is_reported_as_discarded_not_as_an_error() {
        let raw = "f1\ntCHR\nd0x1000003\nn/dev/null\nf2\ntCHR\nd0x1000003\nn/dev/null\n";
        let streams = parse_streams(raw);
        assert_eq!(streams.stdout, Sink::Discarded);
        assert_eq!(streams.stderr, Sink::Discarded);
        assert!(!streams.stdout.is_readable());
        assert_eq!(streams.stdout.summary(), "discarded (/dev/null)");
    }

    /// Real output for a `claude` process: stdout on a tty, stderr on a pipe.
    #[test]
    fn a_tty_stdout_and_pipe_stderr_are_classified_separately() {
        let raw = "f1\ntCHR\nd0x10000003\nn/dev/ttys003\n\
                   f2\ntPIPE\nd0xad33b2dc5e8aace4\nn->0x97643e7e87499166\n";
        let streams = parse_streams(raw);
        assert_eq!(
            streams.stdout,
            Sink::Terminal {
                tty: "/dev/ttys003".into(),
                pane: None,
                direct: false
            }
        );
        assert_eq!(
            streams.stderr,
            Sink::Stream {
                kind: StreamKind::Pipe,
                device: "0xad33b2dc5e8aace4".into(),
                peer: None
            }
        );
    }

    #[test]
    fn a_redirected_log_file_is_readable() {
        let raw = "f1\ntREG\nd0x1000000\nn/private/tmp/srv.log\n";
        let streams = parse_streams(raw);
        assert_eq!(
            streams.stdout,
            Sink::File {
                path: "/private/tmp/srv.log".into()
            }
        );
        assert!(streams.stdout.is_readable());
    }

    #[test]
    fn a_unix_socket_is_named_but_not_readable() {
        let raw = "f1\ntunix\nd0xec51c6f98dd75b8a\nn->0xd0c230641c1611d4\n";
        let streams = parse_streams(raw);
        assert!(matches!(
            streams.stdout,
            Sink::Stream {
                kind: StreamKind::UnixSocket,
                ..
            }
        ));
        assert!(!streams.stdout.is_readable());
        // The refusal must explain *why*, since "no output" looks like a bug.
        let error = read(&streams.stdout, 1024).unwrap_err().to_string();
        assert!(error.contains("unix socket"), "{error}");
        assert!(error.contains("SIP"), "{error}");
    }

    #[test]
    fn a_tty_becomes_readable_once_it_maps_to_a_pane() {
        let with_pane = Sink::Terminal {
            tty: "/dev/ttys016".into(),
            pane: Some("local:1.1".into()),
            direct: true,
        };
        assert!(with_pane.is_readable());
        assert_eq!(
            with_pane.summary(),
            "tty /dev/ttys016 — tmux pane local:1.1"
        );

        let without = Sink::Terminal {
            tty: "/dev/ttys099".into(),
            pane: None,
            direct: false,
        };
        assert!(!without.is_readable());

        // A nested pty (ccproxy → claude) is not the pane's own device, but its
        // output is relayed there, so it is still worth reading.
        let nested = Sink::Terminal {
            tty: "/dev/ttys077".into(),
            pane: Some("local:1.2".into()),
            direct: false,
        };
        assert!(nested.is_readable());
        assert_eq!(
            nested.summary(),
            "pty /dev/ttys077 — relayed into tmux pane local:1.2"
        );
    }

    #[test]
    fn a_closed_fd_is_unknown_rather_than_silently_empty() {
        let streams = parse_streams("");
        assert_eq!(streams.stdout, Sink::Unknown);
        assert!(read(&streams.stdout, 1024).is_err());
    }

    #[test]
    fn pipe_holders_index_by_device_and_skip_duplicates() {
        let raw = "p100\ncbun\nf1\ntPIPE\nd0xAA\nn->0xBB\n\
                   p200\ncnode\nf0\ntPIPE\nd0xAA\nn->0xBB\n";
        let holders = parse_holders(raw);
        let peers = &holders["0xAA"];
        assert_eq!(peers.len(), 2);
        // The peer of pid 100 is the *other* holder.
        let peer = pick_peer(&holders, "0xAA", 100).unwrap();
        assert_eq!(peer.pid, 200);
        assert_eq!(peer.name, "node");
    }

    #[test]
    fn tail_of_a_file_starts_at_a_line_boundary() {
        let dir = std::env::temp_dir();
        let path = dir.join("tpx-tail-test.log");
        std::fs::write(&path, "first line\nsecond line\nthird line\n").unwrap();

        // A window small enough to land mid-file drops the partial first line.
        let text = tail_file(path.to_str().unwrap(), 20).unwrap();
        assert!(!text.starts_with("cond"), "got: {text:?}");
        assert!(text.ends_with('\n'));

        // A window larger than the file returns all of it.
        let whole = tail_file(path.to_str().unwrap(), 10_000).unwrap();
        assert!(whole.starts_with("first line"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_growing_log_reads_its_newest_lines_on_each_call() {
        // What `r` relies on: re-reading must see writes that landed since the
        // last read, not a cached prefix.
        let path = std::env::temp_dir().join("tpx-growing-test.log");
        std::fs::write(&path, "line-1\n").unwrap();
        let sink = Sink::File {
            path: path.to_str().unwrap().to_string(),
        };

        let first = read(&sink, 4096).unwrap();
        assert!(first.contains("line-1"));

        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(file, "line-2").unwrap();
        drop(file);

        let second = read(&sink, 4096).unwrap();
        assert!(second.contains("line-2"), "got: {second:?}");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn the_tail_window_bounds_what_is_read_from_a_huge_log() {
        // A multi-gigabyte log must not be pulled into memory whole.
        let path = std::env::temp_dir().join("tpx-huge-test.log");
        let body: String = (0..5_000).map(|i| format!("line-{i}\n")).collect();
        std::fs::write(&path, &body).unwrap();

        let sink = Sink::File {
            path: path.to_str().unwrap().to_string(),
        };
        let text = read(&sink, 1_024).unwrap();
        assert!(text.len() <= 1_024, "read {} bytes", text.len());
        // And it is the *end* of the log, which is where the answer is.
        assert!(text.contains("line-4999"), "tail missing");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn the_bulk_form_keeps_each_pid_streams_separate() {
        // The per-process form needed `-a` to avoid mixing processes; the bulk
        // form must attribute each fd to the pid tag that preceded it.
        let raw = "p100\nf1\ntCHR\nn/dev/null\nf2\ntREG\nd0x1\nn/tmp/a.log\n\
                   p200\nf1\ntCHR\nn/dev/ttys016\nf2\ntPIPE\nd0xAA\nn->0xBB\n";
        let by_pid = parse_all(raw);
        assert_eq!(by_pid.len(), 2);

        assert_eq!(by_pid[&100].stdout, Sink::Discarded);
        assert_eq!(
            by_pid[&100].stderr,
            Sink::File {
                path: "/tmp/a.log".into()
            }
        );
        assert!(matches!(by_pid[&200].stdout, Sink::Terminal { .. }));
        assert!(matches!(
            by_pid[&200].stderr,
            Sink::Stream {
                kind: StreamKind::Pipe,
                ..
            }
        ));
    }

    #[test]
    fn a_process_holding_only_one_of_the_two_fds_is_still_recorded() {
        let by_pid = parse_all("p300\nf2\ntREG\nd0x1\nn/tmp/err.log\n");
        assert_eq!(by_pid[&300].stdout, Sink::Unknown);
        assert!(matches!(by_pid[&300].stderr, Sink::File { .. }));
    }

    #[test]
    fn the_owning_pane_fills_in_only_for_a_nested_pty() {
        // A pane's own device already resolved during the bulk pass; overwriting
        // it with the owning pane would be wrong when they differ.
        let mut direct = Streams {
            stdout: Sink::Terminal {
                tty: "/dev/ttys016".into(),
                pane: Some("local:1.1".into()),
                direct: true,
            },
            stderr: Sink::Unknown,
        };
        attach_owning_pane(&mut direct, Some("local:9.9"));
        assert_eq!(
            direct.stdout,
            Sink::Terminal {
                tty: "/dev/ttys016".into(),
                pane: Some("local:1.1".into()),
                direct: true
            }
        );

        // A nested pty has no pane of its own, so it takes the owner's.
        let mut nested = Streams {
            stdout: Sink::Terminal {
                tty: "/dev/ttys019".into(),
                pane: None,
                direct: false,
            },
            stderr: Sink::Unknown,
        };
        attach_owning_pane(&mut nested, Some("local:1.1"));
        assert_eq!(
            nested.stdout,
            Sink::Terminal {
                tty: "/dev/ttys019".into(),
                pane: Some("local:1.1".into()),
                direct: false
            }
        );
    }
}
