//! Who talks to whom: resolving the local end of a TCP connection to the process
//! on the other side.
//!
//! A socket row says `127.0.0.1:57872->127.0.0.1:63553`, which names a *port*, not
//! a process. The peer is whoever holds the mirrored pair
//! `127.0.0.1:63553->127.0.0.1:57872` — so one bulk TCP listing is enough to turn
//! every loopback connection into a process-to-process edge. On this machine that
//! resolved 33 local connections in 0.08s.
//!
//! Only loopback pairs resolve. A connection to `160.79.104.10:443` has no local
//! peer to find, and pretending otherwise would be worse than saying "remote".

use std::collections::HashMap;

use anyhow::Result;

use super::cmd;
use crate::model::ProcKey;

/// A process reachable at the far end of a local connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnPeer {
    pub pid: u32,
    pub name: String,
}

/// `local_endpoint -> peer`, where the endpoint is the *remote* side as seen from
/// the asking process.
///
/// Keyed by the endpoint string rather than by pid: one process can hold many
/// connections, and the question is always "who is at this address".
#[derive(Default)]
pub struct PeerMap {
    by_endpoint: HashMap<String, ConnPeer>,
}

impl PeerMap {
    /// One bulk `lsof -iTCP`, parsed into an endpoint index.
    pub fn collect() -> Result<Self> {
        let raw = cmd::run("lsof", &["-nP", "-iTCP", "-F", "pcn"], cmd::FAST)?;
        Ok(Self::parse(&raw))
    }

    fn parse(raw: &str) -> Self {
        let mut by_endpoint = HashMap::new();
        let mut pid = 0u32;
        let mut name = String::new();

        for line in raw.lines() {
            let Some((tag, value)) = line.split_at_checked(1) else {
                continue;
            };
            match tag {
                "p" => pid = value.parse().unwrap_or(0),
                "c" => name = value.to_string(),
                "n" => {
                    // A listener has no `->`; only connections define an edge.
                    if let Some((local, _)) = value.split_once("->")
                        && pid != 0
                    {
                        by_endpoint.insert(
                            local.to_string(),
                            ConnPeer {
                                pid,
                                name: name.clone(),
                            },
                        );
                    }
                }
                _ => {}
            }
        }
        Self { by_endpoint }
    }

    /// The process at `remote`, if it is a local process.
    ///
    /// `self_pid` guards the case where a process connects to itself — returning
    /// the asking process as its own peer would be technically true and useless.
    pub fn peer_of(&self, remote: &str, self_pid: u32) -> Option<&ConnPeer> {
        let peer = self.by_endpoint.get(remote)?;
        (peer.pid != self_pid).then_some(peer)
    }

    /// Whether an endpoint is loopback, i.e. whether a peer could exist at all.
    pub fn is_local(endpoint: &str) -> bool {
        endpoint.starts_with("127.0.0.1:")
            || endpoint.starts_with("[::1]:")
            || endpoint.starts_with("localhost:")
    }
}

/// One process-to-process edge over loopback.
#[derive(Clone, Debug, PartialEq)]
pub struct Edge {
    pub from: ProcKey,
    pub to: ProcKey,
    pub to_name: String,
    /// Port on the receiving side — usually the service being called.
    pub port: u16,
}

/// Build the loopback edges among a set of processes.
///
/// Used for the "who calls this service" view: an edge into a pane's process from
/// another pane is the interesting case, and it is invisible in a plain socket
/// list.
pub fn edges(sockets: &HashMap<ProcKey, Vec<crate::model::Socket>>, peers: &PeerMap) -> Vec<Edge> {
    let mut edges = Vec::new();
    for (key, held) in sockets {
        for socket in held {
            let Some(remote) = socket.peer.as_deref() else {
                continue;
            };
            if !PeerMap::is_local(remote) {
                continue;
            }
            let Some(peer) = peers.peer_of(remote, key.pid) else {
                continue;
            };
            let Some(port) = remote.rsplit(':').next().and_then(|p| p.parse().ok()) else {
                continue;
            };
            edges.push(Edge {
                from: key.clone(),
                to: ProcKey::host(peer.pid),
                to_name: peer.name.clone(),
                port,
            });
        }
    }
    // Stable order so the panel does not reshuffle between refreshes.
    edges.sort_by(|a, b| {
        a.from
            .pid
            .cmp(&b.from.pid)
            .then(a.to.pid.cmp(&b.to.pid))
            .then(a.port.cmp(&b.port))
    });
    edges.dedup();
    edges
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Proto, Socket, SocketState};

    /// Real shape: ccproxy and claude connected over loopback, plus an outbound
    /// TLS connection with no local peer.
    const SAMPLE: &str = "p99968\nccproxy\nf9\nn127.0.0.1:57872->127.0.0.1:63553\n\
                          f16\nn172.30.1.39:64032->160.79.104.10:443\n\
                          p99970\ncclaude.ex\nf9\nn127.0.0.1:63553->127.0.0.1:57872\n\
                          p832\nczoom.us\nf49\nn*:8080\n";

    #[test]
    fn a_loopback_connection_resolves_to_the_peer_process() {
        let peers = PeerMap::parse(SAMPLE);
        // ccproxy's remote end is claude's local end.
        let peer = peers.peer_of("127.0.0.1:63553", 99968).unwrap();
        assert_eq!(peer.pid, 99970);
        assert_eq!(peer.name, "claude.ex");
    }

    #[test]
    fn a_remote_address_has_no_local_peer() {
        let peers = PeerMap::parse(SAMPLE);
        assert!(peers.peer_of("160.79.104.10:443", 99968).is_none());
        assert!(!PeerMap::is_local("160.79.104.10:443"));
        assert!(PeerMap::is_local("127.0.0.1:443"));
        assert!(PeerMap::is_local("[::1]:443"));
    }

    #[test]
    fn a_listener_does_not_create_an_edge() {
        // `*:8080` has no `->`, so it must not land in the endpoint index.
        let peers = PeerMap::parse(SAMPLE);
        assert!(peers.peer_of("*:8080", 1).is_none());
    }

    #[test]
    fn a_process_is_never_its_own_peer() {
        let raw = "p100\ncself\nf3\nn127.0.0.1:1111->127.0.0.1:2222\n\
                   f4\nn127.0.0.1:2222->127.0.0.1:1111\n";
        let peers = PeerMap::parse(raw);
        // Both ends belong to pid 100; asking as 100 must yield nothing.
        assert!(peers.peer_of("127.0.0.1:2222", 100).is_none());
        // But another process asking does see it.
        assert_eq!(peers.peer_of("127.0.0.1:2222", 999).unwrap().pid, 100);
    }

    #[test]
    fn edges_name_the_receiving_process_and_port() {
        let peers = PeerMap::parse(SAMPLE);
        let mut sockets = HashMap::new();
        sockets.insert(
            ProcKey::host(99968),
            vec![
                Socket {
                    proto: Proto::Tcp,
                    local: "127.0.0.1:57872".into(),
                    peer: Some("127.0.0.1:63553".into()),
                    state: SocketState::Established,
                },
                // Outbound TLS: no local peer, so no edge.
                Socket {
                    proto: Proto::Tcp,
                    local: "172.30.1.39:64032".into(),
                    peer: Some("160.79.104.10:443".into()),
                    state: SocketState::Established,
                },
            ],
        );

        let edges = edges(&sockets, &peers);
        assert_eq!(edges.len(), 1, "only the loopback pair is an edge");
        assert_eq!(edges[0].from, ProcKey::host(99968));
        assert_eq!(edges[0].to, ProcKey::host(99970));
        assert_eq!(edges[0].to_name, "claude.ex");
        assert_eq!(edges[0].port, 63553);
    }

    #[test]
    fn a_listening_socket_contributes_no_edge() {
        let peers = PeerMap::parse(SAMPLE);
        let mut sockets = HashMap::new();
        sockets.insert(
            ProcKey::host(1),
            vec![Socket {
                proto: Proto::Tcp,
                local: "*:8080".into(),
                peer: None,
                state: SocketState::Listen,
            }],
        );
        assert!(edges(&sockets, &peers).is_empty());
    }
}
