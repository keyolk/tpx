//! Container state via the docker CLI, plus sidecar-based drill-down.
//!
//! # The pid-namespace boundary
//!
//! On a VM-backed runtime (OrbStack, Docker Desktop) container processes live
//! in a Linux VM and do **not** appear in the macOS host `ps` table at all.
//! `State.Pid` is a pid inside that VM. So:
//!
//! - Container processes are never merged into the host process map.
//! - A container is linked to a pane by *attribution* (compose working_dir, or
//!   a `docker` CLI invocation in the pane), never by pid ancestry.
//! - `docker top` pids are only valid as [`Origin::Container`] keys.
//!
//! `docker top` also cannot report cpu or thread counts — the daemon rejects
//! `pcpu`/`nlwp` — so per-process container metrics come from `/proc` read
//! through a sidecar sharing the container's pid namespace.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::Value;

use super::cmd;
use crate::model::{
    Attribution, AttributionReason, Container, ContainerMetrics, Pane, Proc, ProcKey, Proto,
    Socket, SocketState,
};

/// Image used for namespace-sharing sidecars. Carries ss/tcpdump/ip and a
/// shell, which distroless and scratch containers do not.
pub const SIDECAR_IMAGE: &str = "nicolaka/netshoot:latest";

/// All containers, running and stopped, with compose/port metadata.
pub fn containers() -> Result<Vec<Container>> {
    // `docker ps` alone lacks compose labels and State.Pid, so inspect the ids
    // it returns in one batched call.
    let ids_raw = cmd::run("docker", &["ps", "-a", "--format", "{{.ID}}"], cmd::DOCKER)?;
    let ids: Vec<&str> = ids_raw
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut args = vec!["inspect"];
    args.extend(ids.iter().copied());
    let raw = cmd::run("docker", &args, cmd::DOCKER)?;
    parse_inspect(&raw)
}

fn parse_inspect(raw: &str) -> Result<Vec<Container>> {
    let entries: Vec<Value> = serde_json::from_str(raw).context("parse docker inspect")?;
    Ok(entries.iter().filter_map(parse_container).collect())
}

fn parse_container(entry: &Value) -> Option<Container> {
    let id = entry.get("Id")?.as_str()?.to_string();
    let labels = entry.pointer("/Config/Labels");
    let label = |key: &str| {
        labels
            .and_then(|labels| labels.get(key))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };

    Some(Container {
        short_id: id.chars().take(12).collect(),
        name: entry
            .pointer("/Name")
            .and_then(Value::as_str)
            .map(|name| name.trim_start_matches('/').to_string())
            .unwrap_or_default(),
        image: entry
            .pointer("/Config/Image")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        status: entry
            .pointer("/State/Status")
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_string(),
        running: entry
            .pointer("/State/Running")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        init_pid: entry
            .pointer("/State/Pid")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32,
        compose_project: label("com.docker.compose.project"),
        compose_working_dir: label("com.docker.compose.project.working_dir"),
        network_mode: entry
            .pointer("/HostConfig/NetworkMode")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        ports: parse_ports(entry.pointer("/NetworkSettings/Ports")),
        metrics: None,
        attribution: None,
        id,
    })
}

/// `NetworkSettings.Ports` maps `"8080/tcp"` to a list of host bindings, or to
/// null when the port is exposed but unpublished.
fn parse_ports(ports: Option<&Value>) -> Vec<String> {
    let Some(map) = ports.and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut formatted: Vec<String> = map
        .iter()
        .flat_map(|(container_port, bindings)| {
            let bindings = bindings.as_array().map(Vec::as_slice).unwrap_or(&[]);
            bindings
                .iter()
                .filter_map(|binding| binding.get("HostPort")?.as_str())
                .map(|host_port| format!("{host_port}->{container_port}"))
                .collect::<Vec<_>>()
        })
        .collect();
    formatted.sort();
    formatted
}

/// Parse one `docker stats --format '{{json .}}'` line into metrics.
///
/// `docker stats --no-stream` costs ~2s, so the caller streams the non-`--no-stream`
/// form in a background thread and feeds lines here.
pub fn parse_stats_line(line: &str) -> Option<(String, ContainerMetrics)> {
    // Streaming stats repaints the screen, so lines arrive wrapped in ANSI
    // cursor control sequences that must be stripped before JSON parsing.
    let json_start = line.find('{')?;
    let json_end = line.rfind('}')?;
    let entry: Value = serde_json::from_str(&line[json_start..=json_end]).ok()?;

    let id = entry.get("Container")?.as_str()?.to_string();
    let (mem_bytes, mem_limit_bytes) = parse_pair(entry.get("MemUsage")?.as_str()?);
    let (net_in_bytes, net_out_bytes) = parse_pair(entry.get("NetIO")?.as_str()?);
    let (block_read_bytes, block_write_bytes) = parse_pair(entry.get("BlockIO")?.as_str()?);

    Some((
        id,
        ContainerMetrics {
            cpu_pct: entry
                .get("CPUPerc")
                .and_then(Value::as_str)
                .and_then(|value| value.trim_end_matches('%').parse().ok())
                .unwrap_or(0.0),
            mem_bytes,
            mem_limit_bytes,
            pids: entry
                .get("PIDs")
                .and_then(Value::as_str)
                .and_then(|value| value.parse().ok())
                .unwrap_or(0),
            net_in_bytes,
            net_out_bytes,
            block_read_bytes,
            block_write_bytes,
        },
    ))
}

/// `docker stats` renders sizes as `"2.5MiB / 15.66GiB"`.
fn parse_pair(value: &str) -> (u64, u64) {
    let mut halves = value.split('/');
    let first = halves.next().map(parse_size).unwrap_or(0);
    let second = halves.next().map(parse_size).unwrap_or(0);
    (first, second)
}

fn parse_size(value: &str) -> u64 {
    let value = value.trim();
    let split = value
        .find(|ch: char| ch.is_alphabetic())
        .unwrap_or(value.len());
    let (number, unit) = value.split_at(split);
    let Ok(number) = number.trim().parse::<f64>() else {
        return 0;
    };
    // docker mixes decimal (kB/MB, from NetIO/BlockIO) and binary (KiB/MiB,
    // from MemUsage) units in the same output.
    let scale = match unit.trim() {
        "B" | "" => 1.0,
        "kB" | "KB" => 1e3,
        "MB" => 1e6,
        "GB" => 1e9,
        "TB" => 1e12,
        "KiB" => 1024.0,
        "MiB" => 1024f64.powi(2),
        "GiB" => 1024f64.powi(3),
        "TiB" => 1024f64.powi(4),
        _ => 1.0,
    };
    (number * scale) as u64
}

/// Processes inside a container's own pid namespace.
///
/// **Not** `docker top`: on a VM-backed runtime that reports pids from the VM's
/// namespace (`397`, `1573741`), not the container's (`1`, `7`). Those pids
/// cannot be joined against `ss -p` output or `/proc/<pid>`, which both speak
/// the container's namespace — so using them would silently mis-attribute every
/// socket and every metric.
///
/// Reads `/proc` through a sidecar sharing the container's pid namespace rather
/// than shelling out to the container's own `ps`. `ps` output is not portable:
/// busybox (which buildkit and most alpine images ship) prints elapsed time as
/// `11d22` and rss as `226m`, where coreutils prints `11-22:00:00` and `231424`.
/// Parsing both dialects would be guesswork, and a misparse silently drops the
/// row. `/proc` is the same everywhere and works for scratch/distroless images
/// that have no `ps` at all.
pub fn processes(container_id: &str) -> Result<Vec<Proc>> {
    // /proc/<pid>/stat holds ppid, state and rss (in pages) at fixed positions,
    // but comm can contain spaces and parens — so cmdline is read separately
    // and stat is only mined for the numeric fields after the final ')'.
    //
    // PROBE_MARKER lets the sidecar's own shell be identified by its command
    // line: matching the script text would break the moment the script is
    // edited, and the sidecar sees itself in the shared namespace.
    let script = format!(
        r#": {PROBE_MARKER}
for d in /proc/[0-9]*; do
  pid=${{d#/proc/}}
  [ -r "$d/stat" ] || continue
  printf '%s\t%s\t%s\n' "$pid" "$(cat $d/stat 2>/dev/null)" "$(tr '\0' ' ' < $d/cmdline 2>/dev/null)"
done
printf 'UPTIME\t%s\n' "$(cut -d' ' -f1 /proc/uptime)"
"#
    );
    let pid_ns = format!("--pid=container:{container_id}");
    let raw = cmd::run(
        "docker",
        &["run", "--rm", &pid_ns, SIDECAR_IMAGE, "sh", "-c", &script],
        Duration::from_secs(25),
    )?;
    Ok(strip_probe(parse_proc_table(container_id, &raw)))
}

/// Sentinel embedded in every probe command line so the probe can recognize —
/// and hide — itself and its children in the namespace it is inspecting.
const PROBE_MARKER: &str = "tpx-probe-do-not-show";

/// Drop the probe's own processes. The `ps`/`sh` we just started is not part of
/// the container's workload and would flicker in and out of the tree.
fn strip_probe(procs: Vec<Proc>) -> Vec<Proc> {
    let probe_pids: Vec<u32> = procs
        .iter()
        .filter(|proc| {
            proc.command.contains(PROBE_MARKER) || proc.command.starts_with("ps -eo pid,ppid")
        })
        .map(|proc| proc.key.pid)
        .collect();
    procs
        .into_iter()
        .filter(|proc| !probe_pids.contains(&proc.key.pid) && !probe_pids.contains(&proc.ppid))
        .collect()
}

/// Parse the sidecar's `/proc` dump: `pid \t <stat line> \t <cmdline>`.
fn parse_proc_table(container_id: &str, raw: &str) -> Vec<Proc> {
    // Uptime anchors process age: /proc/<pid>/stat field 22 is the start time in
    // clock ticks since boot, so age = uptime - starttime/HZ.
    let uptime_secs: f64 = raw
        .lines()
        .find_map(|line| line.strip_prefix("UPTIME\t"))
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(0.0);

    raw.lines()
        .filter_map(|line| parse_proc_row(container_id, line, uptime_secs))
        .collect()
}

/// Linux `USER_HZ`. Fixed at 100 on every architecture Docker runs on, which is
/// what `getconf CLK_TCK` reports inside the sidecar.
const USER_HZ: f64 = 100.0;
/// Page size for the `rss` field of `/proc/<pid>/stat`, which counts pages.
const PAGE_SIZE: u64 = 4096;

fn parse_proc_row(container_id: &str, line: &str, uptime_secs: f64) -> Option<Proc> {
    let mut parts = line.splitn(3, '\t');
    let pid: u32 = parts.next()?.trim().parse().ok()?;
    let stat = parts.next()?;
    let cmdline = parts.next().unwrap_or("").trim();

    // comm sits in parens and may itself contain parens and spaces, so the
    // numeric fields start after the *last* ')'.
    let after_comm = &stat[stat.rfind(')')? + 1..];
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    // Indices are relative to field 3 (state) of /proc/<pid>/stat.
    let state = fields.first()?.to_string();
    let ppid: u32 = fields.get(1)?.parse().ok()?;
    let start_ticks: f64 = fields
        .get(19)
        .and_then(|value| value.parse().ok())
        .unwrap_or(0.0);
    let rss_pages: u64 = fields
        .get(21)
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);

    let comm = stat[stat.find('(')? + 1..stat.rfind(')')?].to_string();
    // A kernel thread has an empty cmdline; its comm is the only name it has.
    let command = if cmdline.is_empty() {
        format!("[{comm}]")
    } else {
        cmdline.to_string()
    };
    let age_secs = (uptime_secs - start_ticks / USER_HZ).max(0.0) as u64;

    Some(Proc {
        key: ProcKey::in_container(container_id, pid),
        ppid,
        command,
        age_secs,
        cpu_pct: 0.0,
        cpu_time_secs: 0.0,
        rss_bytes: rss_pages * PAGE_SIZE,
        state,
        threads: None,
        fd_count: None,
    })
}

/// Per-process detail read from `/proc` inside the container's pid namespace.
///
/// Requires a sidecar: `docker exec` cannot help for a distroless container,
/// and `docker top` has no thread/fd columns. Costs one container start
/// (~0.5s), so this is on-demand only.
pub fn proc_detail(container_id: &str, pid: u32) -> Result<ProcDetail> {
    let script = format!(
        "cat /proc/{pid}/status 2>/dev/null | grep -E '^(Name|Threads|VmRSS|State):'; \
         echo FDS=$(ls /proc/{pid}/fd 2>/dev/null | wc -l); \
         echo CWD=$(readlink /proc/{pid}/cwd 2>/dev/null)"
    );
    let pid_ns = format!("--pid=container:{container_id}");
    let raw = cmd::run(
        "docker",
        &["run", "--rm", &pid_ns, SIDECAR_IMAGE, "sh", "-c", &script],
        Duration::from_secs(20),
    )?;
    Ok(parse_proc_detail(&raw))
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProcDetail {
    pub threads: Option<u32>,
    pub fd_count: Option<u32>,
    pub rss_bytes: Option<u64>,
    pub state: Option<String>,
    pub cwd: Option<String>,
}

fn parse_proc_detail(raw: &str) -> ProcDetail {
    let mut detail = ProcDetail::default();
    for line in raw.lines() {
        let Some((key, value)) = line.split_once(&[':', '='][..]) else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "Threads" => detail.threads = value.parse().ok(),
            "FDS" => detail.fd_count = value.parse().ok(),
            // /proc reports VmRSS in kB.
            "VmRSS" => {
                detail.rss_bytes = value
                    .split_whitespace()
                    .next()
                    .and_then(|kib| kib.parse::<u64>().ok())
                    .map(|kib| kib * 1024);
            }
            "State" => detail.state = Some(value.to_string()),
            "CWD" if !value.is_empty() => detail.cwd = Some(value.to_string()),
            _ => {}
        }
    }
    detail
}

/// Sockets inside a container's network namespace, via a sidecar running `ss`.
///
/// Joining net and pid namespaces both is what makes `ss -p` able to name the
/// owning process; with net alone the Process column comes back empty.
pub fn sockets(container_id: &str) -> Result<HashMap<ProcKey, Vec<Socket>>> {
    let net_ns = format!("--net=container:{container_id}");
    let pid_ns = format!("--pid=container:{container_id}");
    let raw = cmd::run(
        "docker",
        &[
            "run",
            "--rm",
            &net_ns,
            &pid_ns,
            SIDECAR_IMAGE,
            "ss",
            "-tunap",
        ],
        Duration::from_secs(20),
    )?;
    Ok(parse_ss(container_id, &raw))
}

/// `ss -tunap` rows: `Netid State Recv-Q Send-Q Local Peer Process`, where
/// Process is `users:(("name",pid=7,fd=3),...)`.
fn parse_ss(container_id: &str, raw: &str) -> HashMap<ProcKey, Vec<Socket>> {
    let mut by_proc: HashMap<ProcKey, Vec<Socket>> = HashMap::new();
    for line in raw.lines().skip(1) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 6 {
            continue;
        }
        let proto = if fields[0].starts_with("udp") {
            Proto::Udp
        } else {
            Proto::Tcp
        };
        let state = match fields[1] {
            "LISTEN" | "UNCONN" => SocketState::Listen,
            "ESTAB" => SocketState::Established,
            _ => SocketState::Other,
        };
        let peer = (state != SocketState::Listen).then(|| fields[5].to_string());
        let socket = Socket {
            proto,
            local: fields[4].to_string(),
            peer,
            state,
        };

        // A socket with no owning process still matters (it is a held port), so
        // it is filed under pid 0 rather than dropped.
        for pid in parse_ss_pids(&fields[6..].join(" ")) {
            by_proc
                .entry(ProcKey::in_container(container_id, pid))
                .or_default()
                .push(socket.clone());
        }
    }
    by_proc
}

fn parse_ss_pids(process_field: &str) -> Vec<u32> {
    let mut pids: Vec<u32> = process_field
        .split("pid=")
        .skip(1)
        .filter_map(|rest| {
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            digits.parse().ok()
        })
        .collect();
    if pids.is_empty() {
        pids.push(0);
    }
    pids
}

/// Tie containers back to the panes that plausibly launched them.
///
/// Both rules are heuristic, and the reason travels with the link so the UI can
/// show *why* — an unlabeled guess would be worse than no link at all.
pub fn attribute(containers: &mut [Container], panes: &[Pane], procs: &HashMap<ProcKey, Proc>) {
    // Ownership is resolved once for every pid, rather than re-walking ancestry
    // per (container, pane) pair — that product is ~500k walks on a busy server.
    let owner = pane_owners(panes, procs);
    let docker_procs: Vec<(&Proc, &str)> = procs
        .values()
        .filter(|proc| proc.command.contains("docker"))
        .filter_map(|proc| {
            owner
                .get(&proc.key.pid)
                .map(|target| (proc, target.as_str()))
        })
        .collect();

    for container in containers.iter_mut() {
        container.attribution = attribution_for(container, panes, &docker_procs);
    }
}

/// pid -> owning pane target, for every process descended from a pane's shell.
///
/// Built by walking each pane's subtree downward, which visits each process
/// once, instead of walking every process's ancestry upward per pane.
fn pane_owners(panes: &[Pane], procs: &HashMap<ProcKey, Proc>) -> HashMap<u32, String> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for proc in procs.values() {
        children.entry(proc.ppid).or_default().push(proc.key.pid);
    }

    let mut owner = HashMap::new();
    for pane in panes {
        let mut stack = vec![pane.pid];
        while let Some(pid) = stack.pop() {
            // A pid already claimed means either a shared ancestor or a cycle;
            // either way the first pane to reach it wins and recursion stops.
            if owner.insert(pid, pane.target.clone()).is_some() {
                continue;
            }
            if let Some(kids) = children.get(&pid) {
                stack.extend(kids.iter().copied());
            }
        }
    }
    owner
}

fn attribution_for(
    container: &Container,
    panes: &[Pane],
    docker_procs: &[(&Proc, &str)],
) -> Option<Attribution> {
    // A compose project records the directory it was launched from, which is
    // the strongest signal available.
    if let Some(working_dir) = &container.compose_working_dir
        && let Some(pane) = panes.iter().find(|pane| pane.cwd == *working_dir)
    {
        return Some(Attribution {
            pane_target: pane.target.clone(),
            reason: AttributionReason::ComposeWorkingDir,
        });
    }

    // Otherwise look for a live `docker` CLI in some pane's subtree naming this
    // container — `docker logs -f app`, `docker exec -it app sh`.
    //
    // The name must appear as a whole argument, not a substring: the CLI always
    // takes a container as its own argv entry, and substring matching would let
    // `docker run api:dev` claim an unrelated container named `api`. The image
    // name is excluded for the same reason — a bare tag is not evidence.
    let needles = [container.name.as_str(), container.short_id.as_str()];
    for (proc, pane_target) in docker_procs {
        let names_container = proc.command.split_whitespace().any(|arg| {
            needles
                .iter()
                .any(|needle| !needle.is_empty() && arg == *needle)
        });
        if names_container {
            return Some(Attribution {
                pane_target: pane_target.to_string(),
                reason: AttributionReason::DockerCliArgs,
            });
        }
    }
    None
}

/// Whether the sidecar image is already present, so the UI can warn about a
/// pull before blocking on one.
pub fn sidecar_image_present() -> bool {
    cmd::run("docker", &["image", "inspect", SIDECAR_IMAGE], cmd::DOCKER).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_compose_labels_and_ports() {
        let raw = r#"[{
            "Id": "abc123def456789",
            "Name": "/api",
            "Config": {
                "Image": "api:dev",
                "Labels": {
                    "com.docker.compose.project": "shop",
                    "com.docker.compose.project.working_dir": "/Users/g/src/shop"
                }
            },
            "State": {"Status": "running", "Running": true, "Pid": 4242},
            "HostConfig": {"NetworkMode": "bridge"},
            "NetworkSettings": {"Ports": {
                "8080/tcp": [{"HostIp": "0.0.0.0", "HostPort": "18080"}],
                "9000/tcp": null
            }}
        }]"#;
        let containers = parse_inspect(raw).unwrap();
        let container = &containers[0];
        assert_eq!(container.name, "api");
        assert_eq!(container.short_id, "abc123def456");
        assert_eq!(container.compose_project.as_deref(), Some("shop"));
        assert_eq!(container.ports, vec!["18080->8080/tcp"]);
        assert_eq!(container.init_pid, 4242);
    }

    #[test]
    fn missing_labels_do_not_become_empty_strings() {
        let raw = r#"[{
            "Id": "x",
            "Name": "/bare",
            "Config": {"Image": "alpine", "Labels": {"com.docker.compose.project": ""}},
            "State": {"Status": "exited", "Running": false, "Pid": 0},
            "HostConfig": {"NetworkMode": "none"},
            "NetworkSettings": {"Ports": {}}
        }]"#;
        let containers = parse_inspect(raw).unwrap();
        assert_eq!(containers[0].compose_project, None);
        assert!(!containers[0].running);
    }

    #[test]
    fn stats_line_parses_through_ansi_wrapping() {
        let line = "\x1b[2J\x1b[H{\"BlockIO\":\"2.94GB / 463kB\",\"CPUPerc\":\"12.50%\",\
                    \"Container\":\"bab77447c4531f0e\",\"ID\":\"bab77447c453\",\
                    \"MemPerc\":\"0.02%\",\"MemUsage\":\"2.5MiB / 15.66GiB\",\
                    \"Name\":\"app\",\"NetIO\":\"13.6kB / 126B\",\"PIDs\":\"7\"}\x1b[K";
        let (id, metrics) = parse_stats_line(line).unwrap();
        assert_eq!(id, "bab77447c4531f0e");
        assert_eq!(metrics.cpu_pct, 12.5);
        assert_eq!(metrics.pids, 7);
        // MemUsage is binary (MiB), NetIO is decimal (kB) — both in one row.
        assert_eq!(metrics.mem_bytes, (2.5 * 1024.0 * 1024.0) as u64);
        assert_eq!(metrics.net_in_bytes, 13_600);
        assert_eq!(metrics.net_out_bytes, 126);
        assert_eq!(metrics.block_read_bytes, 2_940_000_000);
    }

    /// Real `/proc/<pid>/stat` from a buildkitd container, plus the uptime line.
    const PROC_DUMP: &str = "1\t1 (docker-init) S 0 1 1 0 -1 4194560 200 0 0 0 1 2 0 0 20 0 1 0 100 4358144 50 18446744073709551615 1 1 0 0 0 0 0 0 0 0 0 0 17 8 0 0 0 0 0\t/sbin/docker-init -- /usr/bin/buildkitd-entrypoint \n\
7\t7 (buildkitd) S 1 7 1 0 -1 4194560 2276805 292717353 911209 51078 71189 52382 1920821 212083 20 0 24 0 3949 2527727616 60934 18446744073709551615 1 1 0 0 0 0 0 0 2143420159 0 0 0 17 8 0 0 0 0 0 0 0 0 0 0 0 0 0\t/usr/bin/buildkitd --allow-insecure-entitlement=network.host \n\
UPTIME\t1031222.66\n";

    #[test]
    fn proc_table_parses_ppid_state_and_rss_pages() {
        let procs = parse_proc_table("cafe123", PROC_DUMP);
        assert_eq!(procs.len(), 2);
        let buildkitd = procs.iter().find(|proc| proc.key.pid == 7).unwrap();
        assert_eq!(buildkitd.ppid, 1);
        assert_eq!(buildkitd.state, "S");
        // stat field `rss` counts pages, not kB.
        assert_eq!(buildkitd.rss_bytes, 60_934 * 4096);
        assert_eq!(buildkitd.name(), "buildkitd");
        assert_eq!(buildkitd.key, ProcKey::in_container("cafe123", 7));
    }

    #[test]
    fn proc_table_age_comes_from_uptime_minus_starttime() {
        let procs = parse_proc_table("c", PROC_DUMP);
        let buildkitd = procs.iter().find(|proc| proc.key.pid == 7).unwrap();
        // 1031222.66 - 3949/100 ≈ 1031183
        assert_eq!(buildkitd.age_secs, 1_031_183);
    }

    #[test]
    fn proc_table_handles_comm_containing_spaces_and_parens() {
        let raw = "42\t42 (my (odd) proc) R 7 42 0 0 -1 0 0 0 0 0 0 0 0 0 20 0 3 0 500 0 128 0 1 1 0 0 0 0 0 0 0 0 0 0 17 8 0 0 0 0 0\t\nUPTIME\t100.0\n";
        let procs = parse_proc_table("c", raw);
        assert_eq!(procs.len(), 1);
        assert_eq!(procs[0].ppid, 7);
        assert_eq!(procs[0].state, "R");
        // Empty cmdline (kernel thread): comm is the only name available.
        assert_eq!(procs[0].command, "[my (odd) proc]");
    }

    #[test]
    fn probe_processes_are_stripped_from_the_tree() {
        let procs = vec![
            Proc {
                key: ProcKey::in_container("c", 7),
                ppid: 1,
                command: "/usr/bin/buildkitd".into(),
                age_secs: 10,
                cpu_pct: 0.0,
                cpu_time_secs: 0.0,
                rss_bytes: 0,
                state: "S".into(),
                threads: None,
                fd_count: None,
            },
            Proc {
                key: ProcKey::in_container("c", 999),
                ppid: 1,
                command: "ps -eo pid,ppid,rss,stat,etime,args".into(),
                age_secs: 0,
                cpu_pct: 0.0,
                cpu_time_secs: 0.0,
                rss_bytes: 0,
                state: "R".into(),
                threads: None,
                fd_count: None,
            },
        ];
        let kept = strip_probe(procs);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].key.pid, 7);
    }

    #[test]
    fn proc_detail_reads_threads_fds_and_rss() {
        let raw = "Name:\tbuildkitd\nState:\tS (sleeping)\nVmRSS:\t  236548 kB\n\
                   Threads:\t24\nFDS=27\nCWD=/\n";
        let detail = parse_proc_detail(raw);
        assert_eq!(detail.threads, Some(24));
        assert_eq!(detail.fd_count, Some(27));
        assert_eq!(detail.rss_bytes, Some(236_548 * 1024));
        assert_eq!(detail.cwd.as_deref(), Some("/"));
    }

    #[test]
    fn ss_rows_map_to_container_scoped_pids() {
        let raw = "Netid State  Recv-Q Send-Q Local Address:Port Peer Address:Port Process\n\
                   tcp   LISTEN 0      4096   0.0.0.0:53         0.0.0.0:*  users:((\"coredns\",pid=7,fd=12))\n\
                   tcp   ESTAB  0      0      10.0.0.5:443       10.0.0.9:51234 users:((\"coredns\",pid=7,fd=15))";
        let sockets = parse_ss("cafe123", raw);
        let held = &sockets[&ProcKey::in_container("cafe123", 7)];
        assert_eq!(held.len(), 2);
        assert_eq!(held[0].state, SocketState::Listen);
        assert_eq!(held[0].local_port(), Some(53));
        assert_eq!(held[1].peer.as_deref(), Some("10.0.0.9:51234"));
    }

    #[test]
    fn ss_row_without_a_process_is_kept_under_pid_zero() {
        let raw = "Netid State  Recv-Q Send-Q Local Peer Process\n\
                   tcp   LISTEN 0      128    0.0.0.0:8080 0.0.0.0:* ";
        let sockets = parse_ss("c", raw);
        assert!(sockets.contains_key(&ProcKey::in_container("c", 0)));
    }

    #[test]
    fn ss_multi_process_socket_files_under_each_pid() {
        assert_eq!(
            parse_ss_pids("users:((\"a\",pid=7,fd=3),(\"b\",pid=9,fd=4))"),
            vec![7, 9]
        );
        assert_eq!(parse_ss_pids(""), vec![0]);
    }

    fn pane(target: &str, cwd: &str, pid: u32) -> Pane {
        let (session, rest) = target.split_once(':').unwrap();
        Pane {
            session: session.to_string(),
            window_index: 0,
            window_name: String::new(),
            pane_index: rest.split('.').nth(1).unwrap().parse().unwrap(),
            target: target.to_string(),
            cwd: cwd.to_string(),
            current_command: "fish".into(),
            pid,
            active: false,
            window_active: false,
            session_attached: true,
            zoomed: false,
        }
    }

    fn container_named(name: &str) -> Container {
        Container {
            id: "abcdef012345678".into(),
            short_id: "abcdef012345".into(),
            name: name.into(),
            image: "api:dev".into(),
            status: "running".into(),
            running: true,
            init_pid: 999,
            compose_project: None,
            compose_working_dir: None,
            network_mode: "bridge".into(),
            ports: vec![],
            metrics: None,
            attribution: None,
        }
    }

    #[test]
    fn attributes_compose_container_to_pane_with_matching_cwd() {
        let mut container = container_named("api");
        container.compose_project = Some("shop".into());
        container.compose_working_dir = Some("/Users/g/src/shop".into());
        let panes = vec![
            pane("local:1.1", "/Users/g", 10),
            pane("local:2.1", "/Users/g/src/shop", 20),
        ];

        attribute(
            std::slice::from_mut(&mut container),
            &panes,
            &HashMap::new(),
        );
        let attribution = container.attribution.unwrap();
        assert_eq!(attribution.pane_target, "local:2.1");
        assert_eq!(attribution.reason, AttributionReason::ComposeWorkingDir);
    }

    #[test]
    fn attributes_via_docker_cli_in_a_pane_subtree() {
        let mut container = container_named("api");
        let panes = vec![pane("local:3.1", "/Users/g", 100)];
        let mut procs = HashMap::new();
        // fish(100) -> docker logs -f api(101)
        procs.insert(
            ProcKey::host(101),
            Proc {
                key: ProcKey::host(101),
                ppid: 100,
                command: "docker logs -f api".into(),
                age_secs: 5,
                cpu_pct: 0.0,
                cpu_time_secs: 0.0,
                rss_bytes: 0,
                state: "S".into(),
                threads: None,
                fd_count: None,
            },
        );

        attribute(std::slice::from_mut(&mut container), &panes, &procs);
        assert_eq!(
            container.attribution.unwrap().reason,
            AttributionReason::DockerCliArgs
        );
    }

    #[test]
    fn container_with_no_signal_gets_no_attribution() {
        let mut container = container_named("orphan");
        let panes = vec![pane("local:1.1", "/Users/g", 10)];
        attribute(
            std::slice::from_mut(&mut container),
            &panes,
            &HashMap::new(),
        );
        assert!(container.attribution.is_none());
    }

    #[test]
    fn ppid_cycle_does_not_hang_pane_ownership() {
        let mut procs = HashMap::new();
        for (pid, ppid) in [(1u32, 2u32), (2, 1)] {
            procs.insert(
                ProcKey::host(pid),
                Proc {
                    key: ProcKey::host(pid),
                    ppid,
                    command: "loop".into(),
                    age_secs: 0,
                    cpu_pct: 0.0,
                    cpu_time_secs: 0.0,
                    rss_bytes: 0,
                    state: "S".into(),
                    threads: None,
                    fd_count: None,
                },
            );
        }
        // Terminates rather than spinning on the cycle.
        let owner = pane_owners(&[pane("local:1.1", "/", 1)], &procs);
        assert_eq!(owner.get(&1).map(String::as_str), Some("local:1.1"));
    }

    #[test]
    fn pane_ownership_covers_the_whole_subtree() {
        let mut procs = HashMap::new();
        // fish(100) -> cargo(101) -> rustc(102)
        for (pid, ppid) in [(100u32, 1u32), (101, 100), (102, 101)] {
            procs.insert(
                ProcKey::host(pid),
                Proc {
                    key: ProcKey::host(pid),
                    ppid,
                    command: "x".into(),
                    age_secs: 0,
                    cpu_pct: 0.0,
                    cpu_time_secs: 0.0,
                    rss_bytes: 0,
                    state: "S".into(),
                    threads: None,
                    fd_count: None,
                },
            );
        }
        let owner = pane_owners(&[pane("local:1.1", "/", 100)], &procs);
        assert_eq!(owner.get(&102).map(String::as_str), Some("local:1.1"));
        assert!(
            !owner.contains_key(&1),
            "the pane's parent is not owned by it"
        );
    }

    #[test]
    fn image_name_alone_is_not_attribution_evidence() {
        let mut container = container_named("api");
        // The pane runs docker against a *different* container built from the
        // same image; matching on `api:dev` would wrongly claim this one.
        let panes = vec![pane("local:3.1", "/Users/g", 100)];
        let mut procs = HashMap::new();
        procs.insert(
            ProcKey::host(101),
            Proc {
                key: ProcKey::host(101),
                ppid: 100,
                command: "docker run api:dev".into(),
                age_secs: 5,
                cpu_pct: 0.0,
                cpu_time_secs: 0.0,
                rss_bytes: 0,
                state: "S".into(),
                threads: None,
                fd_count: None,
            },
        );
        attribute(std::slice::from_mut(&mut container), &panes, &procs);
        assert!(container.attribution.is_none());
    }
}
