# tpx — tmux-aware process explorer

A TUI that answers "what is actually running in my tmux server, and what is it
doing" — across host processes *and* container processes, in one tree.

Successor to `tmux.sh procs`, which prints a one-shot tree of the current
window. `tmux.sh procs` stays as-is for scripting; `tpx` is the interactive
surface.

```
tpx                # the TUI, scoped to the window it runs in
tpx --server       # ...every session on the server (or press `w`)
tpx --plain            # one-shot text tree, for pipes and scripts
tpx --plain --all      # ...including processes the noise filter hides
tpx --plain --streams  # ...annotated with where each stdout/stderr goes
```

## Scope

By default the tree shows **only the window tpx is running in** — the usual
question is "what is running *here*", and 28 panes across 10 sessions buries
that. `--server`, or `w` in the TUI, widens to everything; the header always
states which of the two is active.

Scoping is applied when rows are built, not when data is collected. Port-conflict
detection stays server-wide as a result: a dev server in this window losing
`:8080` to one in another window is exactly the case worth catching, and it would
be invisible if collection were narrowed. Containers that belong to no pane are
hidden under the default scope and reappear under `--server`.

## What it shows

The tree is `session → window → pane → process`, with containers attached where
they can be attributed to a pane and grouped separately where they cannot.
Each row carries current cpu, RSS, listening ports and established connection
count, rolled up so a collapsed session still shows its whole footprint.

Eight facets on the selected row (`1`..`8`, or `Tab`):

| facet | shows |
|---|---|
| `overview` | identity, resources, threads/fds, tmux or container context |
| `net` | sockets, byte counters, port conflicts, **and the process at the far end of each loopback connection** |
| `files` | cwd and open files (sockets excluded — they are in `net`) |
| `output` | `tmux capture-pane` of the owning pane — what it was last doing |
| `streams` | where stdout/stderr go, and their tail when reachable |
| `stack` | what it is doing right now, from a live stack sample (`S`) |
| `env` | the environment it was started with, secrets masked |
| `packets` | live `tcpdump`, scoped to the selection |

## Navigating

The tree is the spine, but the interesting relationships do not run along it:

- **`p` / `P`** jump to the **parent process** or to the **process on the other
  end of a local connection**. Distinct from `h`/`l`, which move within the
  display — a peer may live in another pane entirely, and a parent may be folded
  out of view.
- **`s`** cycles the ordering: `tree` → `cpu` → `memory` → `newest` →
  `connections`. Anything but `tree` flattens to processes only, because sorting
  *within* the tree buries the top consumer under whichever session it happens to
  live in. Flat rows carry `@session:window.pane` so "what is heavy" still answers
  "and where".
- **`/`** searches the whole tree, including collapsed subtrees, and matches
  command lines, cwds, listening ports and **pids**.

A flat ordering shows each process's *own* cpu and memory, not its subtree's:
ranking by own-cpu while displaying the rollup made a correctly sorted list read
as unsorted.

## Introspection

Two questions the process table cannot answer, both available for processes you
own without any privilege escalation:

**`S` — what is it doing right now.** `sample` walks the live task's stacks; the
leaf frame is the answer. Threads parked in `kevent64`/`__psynch_cvwait` are
marked as waiting, so a Node process with 21 threads and none doing work reads as
idle rather than busy. Never automatic: sampling blocks for a wall-clock second.

**`7` — what environment it runs in.** The flags, endpoints and toggles that
explain the behavior. Grouped config / paths / secrets, and values whose names
look sensitive (`TOKEN`, `SECRET`, `KEY`, `SESSION`, …) are replaced with
`<hidden, N chars>` — a process listing is somewhere tokens leak by accident, and
knowing a variable is *set* is usually the whole question.

`?` lists every key. `!` opens diagnostics: which collectors failed and which
listen ports are contested.

## The two things worth knowing

**Host and container pids are different namespaces.** On a VM-backed runtime
(OrbStack, Docker Desktop) container processes do not appear in the host `ps`
table at all, and `State.Pid` is a pid inside the VM. Every process is therefore
keyed by `(namespace, pid)`, container rows are marked `⧉`, and a container is
linked to a pane by *attribution* (compose working-dir, or a live `docker` CLI in
the pane) — never by pid ancestry. The attribution reason is always shown,
because it is a heuristic.

**cpu is a rate, not `ps %cpu`.** macOS `ps` reports cpu-time ÷ elapsed-time, a
lifetime average: a 12-day-old process pinning a core reads `0.2%`. `tpx`
samples the cpu-time counter and reports the delta, which is why it
auto-refreshes every 3s. Observed difference on a real process: 29.6% actual
against 7.9% from `ps`.

## Reading stdout / stderr

macOS has no `/proc/PID/fd/1`, and SIP blocks live write-snooping (`dtrace`,
`fs_usage`) even under `sudo`. So the `streams` facet does not *capture* output —
it *locates* it and reads from wherever it already lands. Measured across ~1300
fds on a real machine:

| fd target | seen | what you get |
|---|---|---|
| `/dev/null` | 944 | "discarded" — stated, not an empty panel |
| tty | ~30 | the owning tmux pane's scrollback |
| regular file | 43 | the log's tail, re-read on `r` |
| pipe / unix socket | 185 | the peer process named; bytes unreachable |

The tty case is what a tmux-aware tool can do that a generic process viewer
cannot: `pane_tty` maps a terminal device back to a pane, so "this process's
stdout" becomes "that pane's scrollback".

It also handles the nested case. A wrapper that spawns its child on its own pty
(`ccproxy` → `claude`) leaves the child writing to a tty that is no pane's
device; tpx falls back to the pane that *owns the process tree* and labels the
difference (`pty … — relayed into tmux pane local:1.2`), so those processes are
readable instead of reporting "no tmux pane".

When output is genuinely unreachable, the reason is shown in place of the
content — a pipe with the peer named beats a blank panel that looks like a bug.

`--plain --streams` gives the same information as a text tree. Both paths batch
their `lsof` work: naming the peer of a pipe needs a machine-wide listing, and
doing that per process took 26s on a server-wide tree against 0.5s batched.

## Packet capture

Bounded to 200 packets per run, and the exact command is shown for confirmation
before anything executes.

- **Container**: a `nicolaka/netshoot` sidecar joined to the container's network
  namespace with `NET_RAW`. No host privileges, works for distroless/scratch.
- **Host**: needs `sudo`, because macOS restricts `/dev/bpf*` to the `access_bpf`
  group. macOS BPF cannot filter by pid, so the process's own sockets are
  compiled into a BPF port expression — the capture is scoped to the process's
  traffic rather than truly per-process. A process holding no sockets is refused
  rather than silently tapping everything.

Container process detail also uses a sidecar, reading `/proc` through a shared
pid namespace. That is deliberate: busybox `ps` (buildkit, alpine) prints elapsed
time as `11d22` and rss as `226m` where coreutils prints `11-22:00:00` and
`231424`, and a misparse silently drops rows.

## Data sources

| source | used for | notes |
|---|---|---|
| `tmux list-panes -a` | topology | whole server; the tree narrows it, see Scope |
| `tmux display-message -t $TMUX_PANE` | which window tpx runs in | not the client's *current* window, which drifts when the reader switches away |
| `ps -axo …,time,…` | processes | `time` is the cpu-rate input |
| `lsof -FpPnTt` | sockets, open files, stdout/stderr targets | one bulk call; per-pid calls are lazy and need `-a` to AND their selectors |
| `nettop -P -x -L 1` | per-process byte counters | no privileges needed |
| `lsof -iTCP` | who is at the far end of a loopback connection | mirrored `A:x->B:y` / `B:y->A:x` pairs; one bulk call (~0.08s) |
| `sample` | live stack sample | no privileges for your own processes; `dtrace` does not survive SIP |
| `ps eww` | environment of a process | only for processes you own |
| `docker inspect` / `stats` | containers | `stats` is streamed; `--no-stream` costs 2s |
| netshoot sidecar | in-container `/proc`, `ss`, `tcpdump` | on demand only |

Collectors run on a worker thread and fail independently — a missing docker or a
restricted `nettop` degrades that axis and is reported in `!`, never rendered as
an empty world.

## Environment

- `NO_COLOR` — monochrome; state stays readable via letters and reverse video.
- `TPX_ASCII=1` — ASCII fold markers and spinner for terminals without
  box-drawing glyphs.

## Development

```
make check   # fmt + clippy + test
make install # -> ~/.local/bin/tpx
```
