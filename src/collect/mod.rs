//! Collectors. Every one of these shells out, so none of them may run on the
//! UI thread — [`Collector`] owns a worker thread and delivers finished
//! [`Snapshot`]s over a channel.

pub mod capture;
pub mod cmd;
pub mod container;
pub mod host;
pub mod introspect;
pub mod net;
pub mod peers;
pub mod streams;
pub mod tmux;

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::thread;
use std::time::{Duration, Instant};

use crate::model::{CollectError, Snapshot};

/// What the worker thread sends back.
pub enum Update {
    /// A complete world snapshot.
    Snapshot(Box<Snapshot>),
    /// Live container metrics from the `docker stats` stream, applied on top of
    /// the current snapshot. Streaming avoids the ~2s cost of
    /// `docker stats --no-stream` on every refresh.
    ContainerMetrics(String, crate::model::ContainerMetrics),
}

/// Background collector. One worker thread refreshes on request; the UI never
/// blocks on a collection.
pub struct Collector {
    updates: Receiver<Update>,
    requests: Sender<()>,
    /// Set once a refresh is requested, cleared when its snapshot lands — the
    /// UI shows a spinner from this rather than guessing.
    pub in_flight: bool,
}

impl Collector {
    /// Spawn the worker and request an initial snapshot.
    pub fn spawn(include_docker: bool) -> Self {
        let (update_tx, updates) = channel();
        let (requests, request_rx) = channel();

        let snapshot_tx = update_tx.clone();
        thread::spawn(move || {
            // cpu is a rate, so it needs the previous round's counters. The
            // worker owns that history: keeping it here means every consumer
            // sees a snapshot whose cpu_pct is already correct.
            let mut previous: Option<(HashMap<u32, f64>, Instant)> = None;

            // A refresh request that arrives mid-collection is coalesced: the
            // worker drains the queue and collects once, so holding `r` cannot
            // build an unbounded backlog of `ps`/`docker` calls.
            while request_rx.recv().is_ok() {
                while matches!(request_rx.try_recv(), Ok(())) {}
                let mut snapshot = collect(include_docker);
                let now = Instant::now();
                let counters: HashMap<u32, f64> = snapshot
                    .procs
                    .values()
                    .map(|proc| (proc.key.pid, proc.cpu_time_secs))
                    .collect();
                if let Some((before, at)) = &previous {
                    derive_cpu(&mut snapshot, before, now.duration_since(*at));
                }
                previous = Some((counters, now));

                if snapshot_tx
                    .send(Update::Snapshot(Box::new(snapshot)))
                    .is_err()
                {
                    break;
                }
            }
        });

        if include_docker {
            spawn_stats_stream(update_tx);
        }

        let collector = Self {
            updates,
            requests,
            in_flight: true,
        };
        let _ = collector.requests.send(());
        collector
    }

    /// Ask for a fresh snapshot. Cheap and idempotent — the worker coalesces.
    pub fn request(&mut self) {
        if self.requests.send(()).is_ok() {
            self.in_flight = true;
        }
    }

    /// Non-blocking poll for finished work.
    pub fn poll(&mut self) -> Vec<Update> {
        let mut updates = Vec::new();
        loop {
            match self.updates.try_recv() {
                Ok(update) => {
                    if matches!(update, Update::Snapshot(_)) {
                        self.in_flight = false;
                    }
                    updates.push(update);
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return updates,
            }
        }
    }
}

/// Fill in `cpu_pct` from the growth in each process's cpu-time counter.
///
/// A process that consumed 1.5s of cpu over a 3s wall-clock gap was using 50% of
/// one core. Processes absent from the previous round keep 0% rather than being
/// credited with their whole lifetime's cpu on first sight.
fn derive_cpu(snapshot: &mut Snapshot, before: &HashMap<u32, f64>, elapsed: Duration) {
    let elapsed = elapsed.as_secs_f64();
    if elapsed <= 0.0 {
        return;
    }
    for proc in snapshot.procs.values_mut() {
        let Some(previous) = before.get(&proc.key.pid) else {
            continue;
        };
        // A pid reused by a new process can show a *decrease*; clamping at zero
        // is right, since the new process's true usage is unknown.
        let delta = (proc.cpu_time_secs - previous).max(0.0);
        proc.cpu_pct = ((delta / elapsed) * 100.0) as f32;
    }
}

/// Follow `docker stats` (streaming form) and forward each container's metrics.
fn spawn_stats_stream(updates: Sender<Update>) {
    thread::spawn(move || {
        loop {
            let Ok(mut child) = std::process::Command::new("docker")
                .args(["stats", "--format", "{{json .}}"])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn()
            else {
                return; // No docker; nothing to stream, ever.
            };

            if let Some(stdout) = child.stdout.take() {
                use std::io::BufRead;
                for line in std::io::BufReader::new(stdout)
                    .lines()
                    .map_while(Result::ok)
                {
                    let Some((id, metrics)) = container::parse_stats_line(&line) else {
                        continue;
                    };
                    if updates.send(Update::ContainerMetrics(id, metrics)).is_err() {
                        let _ = child.kill();
                        return; // UI is gone.
                    }
                }
            }
            let _ = child.wait();
            // The stream ends when the daemon restarts or every container
            // stops. Back off before reconnecting so a docker-less machine does
            // not spin.
            thread::sleep(Duration::from_secs(5));
        }
    });
}

/// One full collection round. Each source failing independently is normal
/// (docker absent, nettop restricted) — failures are recorded in the snapshot
/// and shown, never swallowed into an empty panel.
fn collect(include_docker: bool) -> Snapshot {
    let mut snapshot = Snapshot::default();

    match tmux::panes() {
        Ok(panes) => snapshot.panes = panes,
        Err(error) => snapshot.errors.push(CollectError {
            source: "tmux",
            message: error.to_string(),
        }),
    }

    match host::processes() {
        Ok((procs, children)) => {
            snapshot.procs = procs;
            snapshot.children = children;
        }
        Err(error) => snapshot.errors.push(CollectError {
            source: "ps",
            message: error.to_string(),
        }),
    }

    match net::sockets() {
        Ok(sockets) => snapshot.sockets = sockets,
        Err(error) => snapshot.errors.push(CollectError {
            source: "lsof",
            message: error.to_string(),
        }),
    }

    match net::counters() {
        Ok(counters) => snapshot.net_counters = counters,
        Err(error) => snapshot.errors.push(CollectError {
            source: "nettop",
            message: error.to_string(),
        }),
    }

    if include_docker {
        match container::containers() {
            Ok(mut containers) => {
                container::attribute(&mut containers, &snapshot.panes, &snapshot.procs);
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

/// Whether a docker daemon is reachable. Checked once at startup so the whole
/// container axis can be skipped rather than failing on every refresh.
pub fn docker_available() -> bool {
    cmd::run(
        "docker",
        &["version", "--format", "{{.Server.Version}}"],
        cmd::DOCKER,
    )
    .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Proc, ProcKey};

    fn snapshot_with(pid: u32, cpu_time_secs: f64) -> Snapshot {
        let mut snapshot = Snapshot::default();
        let proc = Proc {
            key: ProcKey::host(pid),
            ppid: 1,
            command: "worker".into(),
            age_secs: 100,
            cpu_pct: 0.0,
            cpu_time_secs,
            rss_bytes: 0,
            state: "R".into(),
            threads: None,
            fd_count: None,
        };
        snapshot.procs.insert(proc.key.clone(), proc);
        snapshot
    }

    #[test]
    fn cpu_rate_comes_from_the_counter_delta() {
        let mut now = snapshot_with(100, 11.5);
        let before = HashMap::from([(100u32, 10.0)]);
        // 1.5s of cpu over a 3s gap is 50% of one core.
        derive_cpu(&mut now, &before, Duration::from_secs(3));
        assert_eq!(now.procs[&ProcKey::host(100)].cpu_pct, 50.0);
    }

    #[test]
    fn a_multithreaded_process_can_exceed_one_hundred_percent() {
        let mut now = snapshot_with(100, 16.0);
        let before = HashMap::from([(100u32, 10.0)]);
        derive_cpu(&mut now, &before, Duration::from_secs(2));
        assert_eq!(now.procs[&ProcKey::host(100)].cpu_pct, 300.0);
    }

    #[test]
    fn a_process_unseen_last_round_is_not_credited_its_whole_lifetime() {
        let mut now = snapshot_with(100, 5_000.0);
        derive_cpu(&mut now, &HashMap::new(), Duration::from_secs(3));
        assert_eq!(now.procs[&ProcKey::host(100)].cpu_pct, 0.0);
    }

    #[test]
    fn a_reused_pid_with_a_lower_counter_clamps_to_zero() {
        let mut now = snapshot_with(100, 0.5);
        let before = HashMap::from([(100u32, 900.0)]);
        derive_cpu(&mut now, &before, Duration::from_secs(3));
        assert_eq!(now.procs[&ProcKey::host(100)].cpu_pct, 0.0);
    }

    #[test]
    fn a_zero_length_gap_is_not_divided_by() {
        let mut now = snapshot_with(100, 11.5);
        let before = HashMap::from([(100u32, 10.0)]);
        derive_cpu(&mut now, &before, Duration::ZERO);
        assert_eq!(now.procs[&ProcKey::host(100)].cpu_pct, 0.0);
    }
}
