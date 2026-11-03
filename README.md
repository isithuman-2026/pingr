# pingr

`ping` with a live terminal UI, plus an MTR-style path trace.

- **Ping mode** (default): sent/received/loss, min/avg/max/jitter, a live RTT
  scatter chart and a scrolling packet log.
- **Trace mode** (`-t`/`--tr`): everything above plus a per-hop table (loss, last,
  avg, best, worst, stdev, EMA and a coloured latency bar per hop). Probing stops
  at the destination, so you do not get 30 rows of nothing.

Single binary, IPv4 only, Linux only. No root needed once unprivileged ICMP is
enabled (see below).

This is a fun side project, not a production tool.

## Build

Needs a stable Rust toolchain and a C linker (`gcc`).

```sh
cargo build --release
# binary: target/release/pingr
```

## Unprivileged ICMP (one-time setup)

pingr opens an ICMP datagram socket, which Linux only allows for groups inside
`net.ipv4.ping_group_range`. Many distros ship it disabled (`1 0`). Pick one:

```sh
# 1. allow everyone (persistent)
sudo sysctl -w net.ipv4.ping_group_range="1 65534"
echo 'net.ipv4.ping_group_range=1 65534' | sudo tee /etc/sysctl.d/99-ping.conf

# 2. or give just this binary raw-socket capability (redo after each rebuild)
sudo setcap cap_net_raw+ep target/release/pingr

# 3. or run it as root
sudo ./target/release/pingr 1.1.1.1
```

With capability or root, pingr falls back to a raw socket and says so on stderr.

## Usage

```
pingr [OPTIONS] <TARGET>

Options:
  -t, --tr                  trace mode: show the path hop by hop
  -I, --iface <IFACE>       bind the send socket to an interface
  -s, --size <SIZE>         payload bytes (default 56)
      --max-hops <N>        max hops in trace mode (default 30)
  -i, --interval_ms <MS>    time between rounds (default 1000)
      --rounds <N>          exit after N rounds (0 = until quit, default 0)
      --ylim <MS>           fixed chart Y axis in ms (0 = auto, default 0)
  -q, --quiet               no TUI: one line per round
      --report-on-exit      print an MTR-style report to stdout on exit (with --tr)
      --ascii               plain ASCII, no block/braille glyphs (also honours NO_COLOR)
```

```sh
pingr 1.1.1.1                       # ping, run until q
pingr example.com                   # shows the resolved address: example.com (93.184.x.x)
pingr -t 1.1.1.1                    # trace mode
pingr -q --rounds 3 1.1.1.1         # quiet, 3 rounds, then exit
pingr -s 128 -i 500 --ylim 50 host  # bigger payload, 500ms cadence, fixed scale
```

Keys (shown in the top border): `q`/`Esc` quit, `c` clear chart, `f` toggle extra
columns (trace mode).

### Layout

1. Stats bar: target, sent/rcvd/loss, min/avg/max/jitter, key hints.
2. RTT chart: one point per reply.
3. Hop table (trace mode only): per-hop stats, bar coloured green/yellow/red by latency.
4. Packet log: fills the remaining space, timeouts in red.

## How tracing works

Each round sends one echo request to the target plus, in trace mode, one probe per
TTL. Routers answer expiring probes with ICMP "time exceeded", and the probe's
sequence number says which hop replied. The destination answering a hop probe marks
the end of the path.

Unprivileged ping sockets do not deliver ICMP errors through `recv`; they queue them
on the socket's error queue. pingr sets `IP_RECVERR` and reads that queue with
`recvmsg(MSG_ERRQUEUE)` to get the router addresses. Raw sockets see the errors
directly.

Some routers never answer expiring probes, so hops can show blank. That is normal
and matches what `mtr` and `traceroute` show.

## Development

```sh
cargo test      # unit tests, including a TestBackend render check of the TUI
cargo clippy
```

## Credits and history

- First version written with the [Pi](https://github.com/badlogic/pi-mono) coding
  agent harness and Qwen 3.8.
- Later bug fixes, tests and the unprivileged trace path were done with Claude Code.
- Ideas borrowed from [mtr](https://github.com/traviscross/mtr) (the per-hop table
  and the ping/trace model) and from [mtr-rs](https://github.com/seitzbg/mtr-rs)
  (its documentation of the `IP_RECVERR` fallback for unprivileged ICMP). No code was
  copied from either; both are GPL-licensed and pingr is MIT.
- Built with [ratatui](https://ratatui.rs), [crossterm](https://github.com/crossterm-rs/crossterm)
  and [clap](https://github.com/clap-rs/clap).

## License

MIT, see [LICENSE](LICENSE).
