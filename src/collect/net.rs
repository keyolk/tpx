//! Network state: sockets per process (`lsof`) and cumulative per-process
//! byte counters (`nettop`).
//!
//! Both are host-only. Container sockets come from a sidecar in the
//! container's network namespace — see [`crate::collect::container`].

use std::collections::HashMap;

use anyhow::Result;

use super::cmd;
use crate::model::{NetCounters, ProcKey, Proto, Socket, SocketState};

/// Every TCP/UDP socket on the host, grouped by owning process.
///
/// `-FpPnTt` asks for one tagged field per line: `p<pid>`, `f<fd>`, `t<type>`,
/// `P<protocol>`, `n<name>`, `TST=<tcp state>`. Fields arrive grouped per file,
/// under the most recent `p` line.
pub fn sockets() -> Result<HashMap<ProcKey, Vec<Socket>>> {
    let raw = cmd::run("lsof", &["-nP", "-FpPnTt", "-iTCP", "-iUDP"], cmd::FAST)?;
    Ok(parse_sockets(&raw))
}

fn parse_sockets(raw: &str) -> HashMap<ProcKey, Vec<Socket>> {
    let mut by_proc: HashMap<ProcKey, Vec<Socket>> = HashMap::new();
    let mut pid: Option<u32> = None;
    let mut proto = Proto::Tcp;
    let mut pending: Option<Socket> = None;

    // A socket's TCP state arrives *after* its name, so each socket is flushed
    // when the next one starts (or at end of input).
    let flush =
        |socket: Option<Socket>, pid: Option<u32>, map: &mut HashMap<ProcKey, Vec<Socket>>| {
            if let (Some(socket), Some(pid)) = (socket, pid) {
                map.entry(ProcKey::host(pid)).or_default().push(socket);
            }
        };

    for line in raw.lines() {
        let Some((tag, value)) = line.split_at_checked(1) else {
            continue;
        };
        match tag {
            "p" => {
                flush(pending.take(), pid, &mut by_proc);
                pid = value.parse().ok();
            }
            "P" => {
                proto = if value.eq_ignore_ascii_case("UDP") {
                    Proto::Udp
                } else {
                    Proto::Tcp
                }
            }
            "n" => {
                flush(pending.take(), pid, &mut by_proc);
                pending = parse_socket_name(value, proto);
            }
            "T" => {
                // UDP sockets have no TST line, so the default below stands.
                if let (Some(state), Some(socket)) = (value.strip_prefix("ST="), pending.as_mut()) {
                    socket.state = match state {
                        "LISTEN" => SocketState::Listen,
                        "ESTABLISHED" => SocketState::Established,
                        _ => SocketState::Other,
                    };
                }
            }
            _ => {}
        }
    }
    flush(pending.take(), pid, &mut by_proc);
    by_proc
}

/// `lsof` socket names are `local` for a listener and `local->peer` for a
/// connection. IPv6 literals are bracketed, so splitting on `->` is safe.
fn parse_socket_name(name: &str, proto: Proto) -> Option<Socket> {
    if name.is_empty() {
        return None;
    }
    let (local, peer) = match name.split_once("->") {
        Some((local, peer)) => (local, Some(peer.to_string())),
        None => (name, None),
    };
    // Without a TST line a named peer means a connection and a bare local
    // address means a listener; TCP sockets refine this from TST= below.
    let state = if peer.is_some() {
        SocketState::Established
    } else {
        SocketState::Listen
    };
    Some(Socket {
        proto,
        local: local.to_string(),
        peer,
        state,
    })
}

/// Cumulative bytes in/out per host pid.
///
/// `nettop -P -x -L 1` prints one CSV sample and exits, and needs no
/// privileges. Its process column is `name.pid`, and the name is truncated to
/// 15 characters — so the pid suffix, not the name, is the join key.
///
/// `time` must stay in `-J`: without it nettop drops the leading timestamp
/// column entirely, and the process label lands in a different field.
pub fn counters() -> Result<HashMap<u32, NetCounters>> {
    let raw = cmd::run(
        "nettop",
        &["-P", "-x", "-L", "1", "-J", "time,bytes_in,bytes_out"],
        cmd::NETTOP,
    )?;
    Ok(parse_counters(&raw))
}

fn parse_counters(raw: &str) -> HashMap<u32, NetCounters> {
    let mut by_pid = HashMap::new();
    for line in raw.lines() {
        let fields: Vec<&str> = line.split(',').collect();
        if fields.len() < 4 {
            continue;
        }
        // fields: time, name.pid, bytes_in, bytes_out. The header row's second
        // field has no `.pid` suffix, which is how it gets skipped.
        let Some((_, pid)) = fields[1].rsplit_once('.') else {
            continue;
        };
        let Ok(pid) = pid.parse::<u32>() else {
            continue;
        };

        by_pid.insert(
            pid,
            NetCounters {
                bytes_in: fields[2].trim().parse().unwrap_or(0),
                bytes_out: fields[3].trim().parse().unwrap_or(0),
            },
        );
    }
    by_pid
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_listener_and_connection_for_one_pid() {
        let raw = "p1503\nf3\ntIPv4\nPTCP\nn127.0.0.1:52404\nTST=LISTEN\nTQR=0\n\
                   f9\ntIPv4\nPTCP\nn127.0.0.1:52404->127.0.0.1:62646\nTST=ESTABLISHED\n";
        let sockets = parse_sockets(raw);
        let held = &sockets[&ProcKey::host(1503)];
        assert_eq!(held.len(), 2);
        assert_eq!(held[0].state, SocketState::Listen);
        assert_eq!(held[0].peer, None);
        assert_eq!(held[1].state, SocketState::Established);
        assert_eq!(held[1].peer.as_deref(), Some("127.0.0.1:62646"));
    }

    #[test]
    fn attributes_sockets_to_the_right_pid() {
        let raw = "p100\nf3\ntIPv4\nPTCP\nn*:8080\nTST=LISTEN\n\
                   p200\nf4\ntIPv4\nPTCP\nn*:9090\nTST=LISTEN\n";
        let sockets = parse_sockets(raw);
        assert_eq!(sockets[&ProcKey::host(100)][0].local, "*:8080");
        assert_eq!(sockets[&ProcKey::host(200)][0].local, "*:9090");
    }

    #[test]
    fn udp_without_tcp_state_stays_a_listener() {
        let raw = "p300\nf5\ntIPv4\nPUDP\nn*:53\n";
        let sockets = parse_sockets(raw);
        let socket = &sockets[&ProcKey::host(300)][0];
        assert_eq!(socket.proto, Proto::Udp);
        assert_eq!(socket.state, SocketState::Listen);
    }

    #[test]
    fn ipv6_peers_split_on_the_arrow_not_the_colon() {
        let socket = parse_socket_name("[::1]:5173->[::1]:60123", Proto::Tcp).unwrap();
        assert_eq!(socket.local, "[::1]:5173");
        assert_eq!(socket.peer.as_deref(), Some("[::1]:60123"));
        assert_eq!(socket.local_port(), Some(5173));
    }

    #[test]
    fn counters_join_on_pid_suffix_not_truncated_name() {
        // nettop truncates names to 15 chars, e.g. `com.crowdstrike.845`.
        let raw = ",bytes_in,bytes_out,\n\
                   22:32:01.101937,com.crowdstrike.845,32549477,1444005185,\n\
                   22:32:01.101939,claude.exe.51313,126041,15558883,\n";
        let counters = parse_counters(raw);
        assert_eq!(counters.len(), 2);
        assert_eq!(counters[&845].bytes_in, 32_549_477);
        assert_eq!(counters[&51313].bytes_out, 15_558_883);
    }

    #[test]
    fn counters_skip_the_header_row() {
        // The real header carries no `.pid` in its second field.
        let raw = "time,,bytes_in,bytes_out,\n22:32:01.1,launchd.1,0,0,\n";
        let counters = parse_counters(raw);
        assert_eq!(counters.len(), 1);
        assert!(counters.contains_key(&1));
    }
}
