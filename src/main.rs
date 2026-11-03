use std::collections::VecDeque;
use std::net::{IpAddr, Ipv4Addr, ToSocketAddrs};
use std::os::fd::RawFd;
use std::time::{Duration, Instant};

use clap::Parser;
use crossterm::event::{poll, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Style, Stylize};
use ratatui::symbols::Marker;
use ratatui::Terminal;
use ratatui::{
    text::{Line, Span, Text},
    widgets::*,
};

#[derive(Parser, Debug, Clone)]
#[command(
    version,
    about = "pingr: ping with a live TUI + MTR-style path tracing"
)]
struct Cli {
    /// Target IP address or FQDN
    target: String,
    /// MTR-style mode: show the full route while pinging
    #[arg(short = 't', long = "tr")]
    trace: bool,
    /// Network interface to send from
    #[arg(short = 'I', long)]
    iface: Option<String>,
    /// Ping payload size in bytes (default 56)
    #[arg(short = 's', long, default_value_t = 56)]
    size: usize,
    /// Max hops in trace mode (default 30)
    #[arg(long, default_value_t = 30)]
    max_hops: usize,
    /// Round interval in ms (default 1000)
    #[arg(short = 'i', long = "interval_ms", default_value_t = 1000)]
    interval_ms: u64,
    /// Exit after N rounds (0 = run until q)
    #[arg(long, default_value_t = 0)]
    rounds: u64,
    /// Fixed Y clamp in ms (0 = auto)
    #[arg(long, default_value_t = 0)]
    ylim: u32,
    /// No TUI: one line per round
    #[arg(short = 'q', long)]
    quiet: bool,
    /// Print final MTR-style report to stdout when the TUI exits
    #[arg(long)]
    report_on_exit: bool,
    /// Plain-ASCII output (no braille glyphs); also honours NO_COLOR
    #[arg(long)]
    ascii: bool,
}

const LOG_MAX: usize = 200;

#[derive(Clone, Default)]
struct Hop {
    ip: Option<IpAddr>,
    samples: Vec<u64>,
    sent: u64,
    rcvd: u64,
    ema: u32,
    spark: Vec<u32>,
}

impl Hop {
    fn st(&self) -> (u64, u64, u64, u32, u32) {
        let s = &self.samples;
        if s.is_empty() {
            return (0, 0, 0, 0, 0);
        }
        let sum: u64 = s.iter().sum();
        let min = s.iter().min().copied().unwrap();
        let max = s.iter().max().copied().unwrap();
        let mean = sum as f64 / s.len() as f64;
        let var = s.iter().map(|x| (*x as f64 - mean).powi(2)).sum::<f64>() / s.len() as f64;
        (
            sum / s.len() as u64,
            min,
            max,
            var.sqrt() as u32,
            (100 * self.sent.saturating_sub(self.rcvd) / self.sent.max(1)) as u32,
        )
    }
}

struct Round {
    seq: u16,
    sent_at: Instant,
    target_done: bool,
    target_rtt: Option<u64>,
    hop_done: Vec<bool>,
    hop_sent_at: Vec<Option<Instant>>,
    hop_rtt: Vec<Option<u64>>,
}

struct State {
    dest: Ipv4Addr,
    dest_name: String,
    cfg: Cli,
    icmp_id: u16,
    payload: Vec<u8>,
    hops: Vec<Hop>,
    sent: u64,
    rcvd: u64,
    min_r: Option<u64>,
    max_r: Option<u64>,
    last_r: Option<u64>,
    sum: u64,
    jitter: f64,
    jit_n: u64,
    chart: VecDeque<u64>,
    log: VecDeque<(u64, Option<u64>)>,
    round: Option<Round>,
    ping_mode: bool,
    ascii: bool,
    wide: bool,
    dest_hop: Option<usize>,
}

/// Hop probes carry this bit in their ICMP seq so replies can't be confused with the target probe.
const HOP_SEQ: u16 = 0x8000;
const CHART_MAX: usize = 512;
const TRACE_GRACE_MS: u64 = 300;
const SPARK_W: usize = 16;
const SPARKS: &[u8; 10] = b" .:-=+*#%@";

#[inline]
fn grade_color(ms: u64, ascii: bool) -> Option<Color> {
    if ascii || std::env::var_os("NO_COLOR").is_some() {
        return None;
    }
    Some(if ms <= 30 {
        Color::Green
    } else if ms <= 120 {
        Color::Yellow
    } else {
        Color::Red
    })
}

const BLOCKS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// mtr-style bar: last SPARK_W samples, newest at the right, shifting left.
/// Block glyphs normally, plain ASCII levels when `ascii`.
fn sparkline(sp: &[u32], ascii: bool) -> String {
    let n = sp.len();
    let off = n.saturating_sub(SPARK_W);
    let top: u32 = sp[off..].iter().max().copied().unwrap_or(1).max(1);
    let mut o = String::with_capacity(SPARK_W * 3);
    for i in 0..SPARK_W {
        if i + off < n {
            let v = sp[off + i];
            if ascii {
                let lvl = ((9.0 * v as f64 / top as f64).min(9.0)) as usize;
                o.push(SPARKS[lvl.min(9)] as char);
            } else {
                // any real sample gets at least the lowest bar
                let lvl = ((8.0 * v as f64 / top as f64).round() as usize).clamp(1, 8);
                o.push(BLOCKS[lvl]);
            }
        } else {
            o.push(' ');
        }
    }
    o
}

/// The bar as one span per slot, each coloured by that sample's latency.
fn spark_spans(sp: &[u32], ascii: bool) -> Vec<Span<'static>> {
    let off = sp.len().saturating_sub(SPARK_W);
    sparkline(sp, ascii)
        .chars()
        .enumerate()
        .map(|(i, ch)| {
            let col = sp.get(off + i).and_then(|&v| grade_color(v as u64, ascii));
            Span::styled(
                ch.to_string(),
                Style::default().fg(col.unwrap_or(Color::Reset)),
            )
        })
        .collect()
}

fn csum(d: &[u8]) -> u16 {
    let mut s: u32 = 0;
    for i in (0..d.len()).step_by(2) {
        s += if i + 1 < d.len() {
            u16::from_be_bytes([d[i], d[i + 1]]) as u32
        } else {
            (d[i] as u32) << 8
        };
    }
    while s >> 16 != 0 {
        s = (s & 0xFFFF) + (s >> 16);
    }
    !(s as u16)
}

fn icmp_echo(id: u16, seq: u16, pl: &[u8]) -> Vec<u8> {
    let mut p = vec![0u8; 8];
    p[0] = 8;
    p[4..6].copy_from_slice(&id.to_be_bytes());
    p[6..8].copy_from_slice(&seq.to_be_bytes());
    p.extend_from_slice(pl);
    let full = p.clone();
    let c = csum(&full);
    p[2..4].copy_from_slice(&c.to_be_bytes());
    p
}

fn die(msg: &str) -> ! {
    eprintln!("pingr: {msg}");
    std::process::exit(1);
}

fn resolve_target(name: &str) -> (Ipv4Addr, String) {
    if let Ok(ip) = name.parse::<Ipv4Addr>() {
        return (ip, name.into());
    }
    match (name, 0u16).to_socket_addrs() {
        Ok(it) => match it
            .filter_map(|a| match a.ip() {
                IpAddr::V4(v) => Some(v),
                _ => None,
            })
            .next()
        {
            Some(v) => (v, format!("{name} ({v})")),
            None => die(&format!(
                "{name} has no IPv4 address (IPv6 not supported yet)"
            )),
        },
        Err(e) => die(&format!("resolve {name}: {e}")),
    }
}

fn sock_addr(ip: Ipv4Addr) -> libc::sockaddr_in {
    libc::sockaddr_in {
        sin_family: 2,
        sin_port: 0,
        sin_addr: libc::in_addr {
            s_addr: (u32::from(ip)).to_be() as libc::c_uint,
        },
        sin_zero: [0; 8],
    }
}

fn bind_iface(fd: RawFd, name: &str) {
    match std::ffi::CString::new(name) {
        Ok(dev) => unsafe {
            if libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_BINDTODEVICE,
                dev.as_ptr() as *const _,
                dev.as_bytes().len() as libc::socklen_t,
            ) < 0
            {
                let e = std::io::Error::last_os_error();
                die(&format!("bind to {name} failed: {e}"));
            }
        },
        Err(_) => die("bad interface name"),
    }
}

fn open_icmp(iface: &Option<String>) -> (RawFd, bool) {
    if iface.is_some() {
        match unsafe { libc::socket(2, libc::SOCK_DGRAM as _, 1) } {
            fd if fd >= 0 => {
                bind_iface(fd, iface.as_ref().unwrap());
                return (fd, true);
            }
            _ => {}
        }
        match unsafe { libc::socket(2, libc::SOCK_RAW as _, 1) } {
            fd if fd >= 0 => {
                bind_iface(fd, iface.as_ref().unwrap());
                return (fd, false);
            }
            _ => {}
        }
        die(&"no ICMP socket privilege");
    }
    let mut fd = unsafe { libc::socket(2, libc::SOCK_DGRAM as _, 1) };
    if fd >= 0 {
        return (fd, true);
    }
    fd = unsafe { libc::socket(2, libc::SOCK_RAW as _, 1) };
    if fd >= 0 {
        return (fd, false);
    }
    die(&"no ICMP socket privilege: run `sudo sysctl -w net.ipv4.ping_group_range='1 65534'` or `sudo setcap cap_net_raw+ep $(which pingr)`");
}

fn set_ttl(fd: RawFd, ttl: u8) -> bool {
    let t = ttl as libc::c_int;
    unsafe {
        libc::setsockopt(
            fd,
            libc::IPPROTO_IP,
            libc::IP_TTL,
            &t as *const _ as *const _,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        ) == 0
    }
}

fn send_probe(fd: RawFd, dst: Ipv4Addr, ttl: u8, pkt: &[u8]) -> bool {
    set_ttl(fd, ttl); // best effort
    unsafe {
        let n = libc::sendto(
            fd,
            pkt.as_ptr() as *const _,
            pkt.len(),
            0,
            &sock_addr(dst) as *const _ as *const libc::sockaddr as *mut _,
            16,
        );
        n == pkt.len() as isize
    }
}

fn recv_pkt(fd: RawFd, buf: &mut [u8]) -> Option<(usize, Ipv4Addr)> {
    unsafe {
        let mut a: libc::sockaddr_in = std::mem::zeroed();
        let mut l = 16;
        let n = libc::recvfrom(
            fd,
            buf.as_mut_ptr() as *mut _,
            buf.len(),
            0,
            &mut a as *mut _ as *mut libc::sockaddr,
            &mut l,
        );
        if n <= 0 {
            return None;
        }
        let v = (a.sin_addr.s_addr as u32).to_be();
        Some((n as usize, Ipv4Addr::from(v)))
    }
}

/// Ping sockets don't deliver ICMP errors via recv: with IP_RECVERR they queue them instead.
fn enable_recverr(fd: RawFd) -> bool {
    let on: libc::c_int = 1;
    unsafe {
        libc::setsockopt(
            fd,
            libc::IPPROTO_IP,
            libc::IP_RECVERR,
            &on as *const _ as *const _,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        ) == 0
    }
}

/// Pop one queued ICMP error: (router that sent it, ICMP type, our original packet in `buf`).
fn recv_err(fd: RawFd, buf: &mut [u8]) -> Option<(Ipv4Addr, u8, usize)> {
    unsafe {
        let mut iov = libc::iovec {
            iov_base: buf.as_mut_ptr() as *mut _,
            iov_len: buf.len(),
        };
        let mut ctl = [0u64; 32]; // u64 for cmsg alignment
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = ctl.as_mut_ptr() as *mut _;
        msg.msg_controllen = std::mem::size_of_val(&ctl) as _;
        let n = libc::recvmsg(fd, &mut msg, libc::MSG_ERRQUEUE | libc::MSG_DONTWAIT);
        if n < 0 {
            return None;
        }
        let mut c = libc::CMSG_FIRSTHDR(&msg);
        while !c.is_null() {
            if (*c).cmsg_level == libc::IPPROTO_IP && (*c).cmsg_type == libc::IP_RECVERR {
                let ee = libc::CMSG_DATA(c) as *const libc::sock_extended_err;
                if (*ee).ee_origin == libc::SO_EE_ORIGIN_ICMP as u8 {
                    // SO_EE_OFFENDER: the sockaddr that follows the struct
                    let off = ee.add(1) as *const libc::sockaddr_in;
                    let ip = Ipv4Addr::from(u32::from_be((*off).sin_addr.s_addr));
                    return Some((ip, (*ee).ee_type, n as usize));
                }
            }
            c = libc::CMSG_NXTHDR(&msg, c);
        }
        Some((Ipv4Addr::UNSPECIFIED, 0, n as usize)) // consumed, nothing useful
    }
}

/// Read everything ready on `fd` and feed it to `dispatch`.
fn drain(st: &mut State, fd: RawFd, buf: &mut [u8], ms: i32) {
    while sel_ready(fd, ms) {
        if let Some((n, src)) = recv_pkt(fd, buf) {
            dispatch(st, &buf[..n], src);
        } else if !st.ping_mode {
            break;
        } else if let Some((router, typ, n)) = recv_err(fd, buf) {
            // the queued payload is our own echo request: seq lives at bytes 6..8
            if typ == 11 && n >= 8 {
                let seq = u16::from_be_bytes([buf[6], buf[7]]);
                hop_time_exceeded(st, 0, seq, router);
            }
        } else {
            break;
        }
    }
}

fn sel_ready(fd: RawFd, ms: i32) -> bool {
    unsafe {
        let mut s: std::mem::MaybeUninit<libc::fd_set> = std::mem::MaybeUninit::uninit();
        let p = s.as_mut_ptr();
        libc::FD_ZERO(p);
        libc::FD_SET(fd, p);
        let mut tv = libc::timeval {
            tv_sec: (ms / 1000) as i64,
            tv_usec: (ms % 1000) as libc::suseconds_t * 1000,
        };
        libc::select(
            fd + 1,
            p,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut tv,
        ) > 0
    }
}

fn record_hop(st: &mut State, hpos: usize, src: Ipv4Addr) {
    let Some(r) = st.round.as_mut() else { return };
    if r.hop_done[hpos] {
        return;
    }
    let Some(t) = r.hop_sent_at[hpos] else { return };
    if t.elapsed() >= Duration::from_secs(5) {
        return;
    }
    let rtt = t.elapsed().as_micros() as u64 / 1000;
    r.hop_rtt[hpos] = Some(rtt);
    r.hop_done[hpos] = true;
    let h = &mut st.hops[hpos];
    let ms_ = rtt as u32;
    h.ip = Some(IpAddr::V4(src));
    h.rcvd += 1;
    h.ema = if h.ema == 0 {
        ms_
    } else {
        (ms_ + 7 * h.ema) / 8
    };
    h.samples.push(rtt);
    if h.samples.len() > 32 {
        h.samples.remove(0);
    }
    h.spark.push(ms_);
    if h.spark.len() > SPARK_W {
        h.spark.remove(0);
    }
}

/// A router answered our hop probe (`in_id`/`in_seq` = the probe's own ICMP header) with TTL exceeded.
fn hop_time_exceeded(st: &mut State, in_id: u16, in_seq: u16, src: Ipv4Addr) {
    if in_seq & HOP_SEQ == 0 {
        return;
    } // expired target probe, not a hop probe
    let hop = in_seq & !HOP_SEQ; // send seq = hop index + 1
    if !st.ping_mode && in_id != st.icmp_id.wrapping_add(hop) {
        return;
    }
    let hpos = hop as usize;
    if hpos == 0 || hpos > st.hops.len() {
        return;
    }
    record_hop(st, hpos - 1, src);
}

fn dispatch(st: &mut State, data: &[u8], src: Ipv4Addr) {
    // ping socket: data = ICMP header + rest; raw: 20-byte IP prefix first.
    let d: &[u8] = match st.ping_mode {
        true => data,
        false => data.get(20..).unwrap_or(&[]),
    };
    if d.len() < 8 {
        return;
    }
    let typ = d[0];
    let id = u16::from_be_bytes([d[4], d[5]]);
    let seq = u16::from_be_bytes([d[6], d[7]]);
    if typ == 0 && seq & HOP_SEQ != 0 {
        // a hop probe reached the destination: that hop is the end of the path
        let hop = (seq & !HOP_SEQ) as usize;
        if hop == 0 || hop > st.hops.len() {
            return;
        }
        if !st.ping_mode && id != st.icmp_id.wrapping_add(hop as u16) {
            return;
        }
        let d = st.dest_hop.map_or(hop - 1, |x| x.min(hop - 1));
        st.dest_hop = Some(d);
        for h in st.hops.iter_mut().skip(d + 1) {
            *h = Hop::default();
        }
        if hop - 1 <= d {
            record_hop(st, hop - 1, src);
        }
    } else if typ == 0 {
        // ping socket: kernel already demuxed to us; raw: check our id
        if !st.ping_mode && id != st.icmp_id {
            return;
        }
        let Some(r) = st.round.as_mut() else { return };
        if seq != r.seq || r.target_done {
            return;
        }
        let rtt = r.sent_at.elapsed().as_micros() as u64 / 1000;
        r.target_rtt = Some(rtt);
        r.target_done = true;
        st.rcvd += 1;
        st.min_r = Some(st.min_r.map_or(rtt, |m| m.min(rtt)));
        st.max_r = Some(st.max_r.map_or(rtt, |m| m.max(rtt)));
        st.sum += rtt;
        if let Some(pv) = st.last_r {
            let dv = (rtt as i64 - pv as i64).unsigned_abs() as f64;
            st.jit_n += 1;
            st.jitter += (dv - st.jitter) / st.jit_n as f64;
        }
        st.last_r = Some(rtt);
        st.chart.push_back(rtt);
        while st.chart.len() > CHART_MAX {
            st.chart.pop_front();
        }
        st.log.push_back((r.seq as u64, Some(rtt)));
    } else if typ == 11 {
        // error: ICMP header + (raw: orig IP hdr) + orig ICMP header...
        if d.len() < 36 {
            return;
        }
        let inner = &d[28..];
        let in_id = u16::from_be_bytes([inner[4], inner[5]]);
        let in_seq = u16::from_be_bytes([inner[6], inner[7]]);
        hop_time_exceeded(st, in_id, in_seq, src);
    }
    while st.log.len() > LOG_MAX {
        st.log.pop_front();
    }
}

fn start_round(st: &mut State, fd: RawFd) {
    let seq = (st.sent as u16) & !HOP_SEQ;
    let mut r = Round {
        seq,
        sent_at: Instant::now(),
        target_done: false,
        target_rtt: None,
        hop_done: (0..st.cfg.max_hops).map(|_| false).collect(),
        hop_sent_at: (0..st.cfg.max_hops).map(|_| None).collect(),
        hop_rtt: (0..st.cfg.max_hops).map(|_| None).collect(),
    };
    let target_ttl = std::cmp::min(st.cfg.max_hops + 1, 254) as u8;
    let pkt = icmp_echo(st.icmp_id, seq, &st.payload);
    if !send_probe(fd, st.dest, target_ttl, &pkt) {
        r.target_done = true;
    }
    if st.cfg.trace {
        let nhops = st.dest_hop.map_or(st.cfg.max_hops, |d| d + 1);
        for i in 0..nhops {
            if i >= 254 {
                break;
            } // TTL is a u8
            let ttl = (i + 1) as u8;
            let hp = icmp_echo(
                st.icmp_id.wrapping_add(ttl as u16),
                HOP_SEQ | (i + 1) as u16,
                &st.payload,
            );
            if send_probe(fd, st.dest, ttl, &hp) {
                r.hop_sent_at[i] = Some(Instant::now());
            } else {
                r.hop_done[i] = true;
            }
        }
    }
    st.sent += 1;
    st.round = Some(r);
}

fn end_round(st: &mut State) {
    let Some(r) = st.round.take() else { return };
    let n = if st.cfg.trace {
        st.dest_hop.map_or(st.hops.len(), |d| d + 1)
    } else {
        0
    };
    for h in st.hops.iter_mut().take(n) {
        h.sent += 1;
    }
    if r.target_rtt.is_none() {
        st.log.push_back((r.seq as u64, None));
    }
    while st.log.len() > LOG_MAX {
        st.log.pop_front();
    }
}

/// Ping mode: done when the target answers. Trace mode: also give slower routers a moment
/// to answer, since ICMP errors often arrive just after the target's reply.
fn round_finished(st: &State) -> bool {
    let Some(r) = st.round.as_ref() else {
        return true;
    };
    if !r.target_done {
        return false;
    }
    !st.cfg.trace
        || r.sent_at.elapsed() >= Duration::from_millis(r.target_rtt.unwrap_or(0) + TRACE_GRACE_MS)
}

fn quiet_loop(st: &mut State, fd: RawFd) {
    use std::io::Write;
    let mut rounds = 0u64;
    loop {
        start_round(st, fd);
        let mut buf = [0u8; 16384];
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            let ms = std::cmp::max(
                std::cmp::min(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .as_millis(),
                    100,
                ) as i32,
                1,
            );
            drain(st, fd, &mut buf, ms);
            if round_finished(st) {
                break;
            }
        }
        let rtt = st.round.as_ref().and_then(|r| r.target_rtt);
        end_round(st);
        let line = match rtt {
            Some(v) => format!(
                "seq {}: reply from {}  time={}ms",
                st.sent - 1,
                st.dest_name,
                v
            ),
            None => format!("seq {}: no response from {}", st.sent - 1, st.dest_name),
        };
        let mut so = std::io::stdout();
        let _ = writeln!(so, "{}", line);
        let _ = so.flush();
        rounds += 1;
        if st.cfg.rounds > 0 && rounds >= st.cfg.rounds {
            break;
        }
        std::thread::sleep(Duration::from_millis(
            st.cfg.interval_ms.saturating_sub(200),
        ));
    }
}

fn report_lines(st: &State) -> Vec<String> {
    let mut out = vec![
        format!("HOST: {}", st.dest_name),
        format!("Snt  Loss%  Last  Avg  Best  Wrst  StDev  EMA"),
        "------".to_string(),
    ];
    for (i, h) in st.hops.iter().enumerate() {
        if h.sent == 0 {
            continue;
        }
        if h.ip.is_none() {
            out.push(format!("{:>4}  ?   ?%   ?    ?    ?     ?     ?", i + 1));
            continue;
        }
        let (avg, min, max, sd, loss) = h.st();
        let last = h.samples.last().copied().unwrap_or(0);
        out.push(format!(
            "{:>4}  {:>3}%  {:>4}  {:>4}  {:>4}  {:>4}  {:>4}  {:>4}  {}",
            i + 1,
            loss,
            last,
            avg,
            min,
            max,
            sd,
            h.ema,
            h.ip.unwrap()
        ));
    }
    out
}

fn hop_header(wide: bool) -> Vec<&'static str> {
    if wide {
        vec![
            "HOP", "HOST", "LOSS%", "SNT", "RCV", "LAST", "AVG", "BEST", "WRST", "STDEV", "EMA",
            "GRAPH",
        ]
    } else {
        vec!["HOP", "HOST", "LOSS%", "SNT", "LAST", "AVG", "EMA", "GRAPH"]
    }
}

/// One table row as (text, colour) pairs, in the same order as `hop_header`.
fn hop_cells(i: usize, h: &Hop, wide: bool, ascii: bool) -> Vec<(String, Option<Color>)> {
    let (avg, min, max, sd, loss) = h.st();
    let last = h.samples.last().copied().unwrap_or(0);
    let bad = (h.ip.is_some() && loss >= 100) || (h.ip.is_none() && h.sent > 0);
    let mut c = vec![
        ((i + 1).to_string(), None),
        (
            h.ip.map_or_else(|| format!("hop {}", i + 1), |ip| ip.to_string()),
            if h.ip.is_none() {
                Some(Color::DarkGray)
            } else {
                None
            },
        ),
        (
            format!("{loss}%"),
            if bad { Some(Color::Red) } else { None },
        ),
        (h.sent.to_string(), None),
    ];
    if wide {
        c.push((h.rcvd.to_string(), None));
    }
    c.push((format!("{last}ms"), grade_color(last, ascii)));
    c.push((format!("{avg}ms"), None));
    if wide {
        c.push((format!("{min}ms"), None));
        c.push((format!("{max}ms"), None));
        c.push((sd.to_string(), None));
    }
    c.push((format!("{}ms", h.ema), None));
    c.push((sparkline(&h.spark, ascii), None));
    c
}

fn draw<B: ratatui::backend::Backend>(t: &mut Terminal<B>, st: &State) {
    let _ = t
        .draw(|f| {
            let area = f.area();
            // ping mode has no middle section: the packet log takes the space
            let shown = st
                .hops
                .iter()
                .filter(|h| !(h.ip.is_none() && h.sent == 0))
                .count() as u16;
            let mid_h = if st.cfg.trace {
                (shown + 3).min(area.height / 2)
            } else {
                0
            };
            let chunks = Layout::vertical([
                Constraint::Length(3),
                Constraint::Percentage(40),
                Constraint::Length(mid_h),
                Constraint::Min(3),
            ])
            .split(area);

            // --- stats bar ---
            let loss_s = if st.sent > 0 {
                format!(
                    "{:.1}%",
                    100.0 * (st.sent - st.rcvd) as f64 / st.sent as f64
                )
            } else {
                "-".into()
            };
            let min_s = st.min_r.map_or_else(|| "-".into(), |v| format!("{v}ms"));
            let max_s = st.max_r.map_or_else(|| "-".into(), |v| format!("{v}ms"));
            let avg_s = if st.rcvd > 0 {
                format!("{}ms", st.sum / st.rcvd)
            } else {
                "-".into()
            };
            let jit_s = if st.jit_n > 0 {
                format!("{:.1}ms", st.jitter)
            } else {
                "-".into()
            };
            let stats = format!(
                "pingr {}  sent={} rcvd={} loss={} min={} max={} avg={} jitter={}",
                st.dest_name, st.sent, st.rcvd, loss_s, min_s, max_s, avg_s, jit_s
            );
            f.render_widget(
                Paragraph::new(Text::from(stats)).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Color::Cyan)
                        .title(Line::from(vec![
                            Span::styled(" pingr ", Style::default().fg(Color::Yellow).bold()),
                            Span::raw(" "),
                        ]))
                        .title(
                            Line::from(if st.cfg.trace {
                                " q quit  c clear  f columns "
                            } else {
                                " q quit  c clear "
                            })
                            .right_aligned(),
                        ),
                ),
                chunks[0],
            );

            // --- chart ---
            let vals: Vec<f64> = st.chart.iter().map(|&v| v as f64).collect();
            let n = vals.len();
            let y_max = if st.cfg.ylim > 0 {
                st.cfg.ylim as f64 + 5.0
            } else {
                vals.iter().cloned().fold(0.0f64, f64::max) * 1.2 + 5.0
            };
            let pts: Vec<(f64, f64)> = vals
                .iter()
                .enumerate()
                .map(|(i, v)| (i as f64, *v))
                .collect();
            let mk = if st.ascii {
                Marker::Dot
            } else {
                Marker::Braille
            };
            let scatters: Vec<Dataset> = vec![Dataset::default()
                .graph_type(GraphType::Scatter)
                .marker(mk)
                .data(&pts)
                .style(Style::default().fg(Color::Green))];
            let y_labels = vec![
                "0".to_string(),
                ((y_max / 2.0) as u64).to_string(),
                (y_max as u64).to_string(),
            ];
            let chart = Chart::new(scatters)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Color::DarkGray)
                        .title(" RTT per reply (ms) "),
                )
                .x_axis(
                    Axis::default()
                        .bounds([n.saturating_sub(80) as f64, (n + 8) as f64])
                        .style(Style::default().fg(Color::DarkGray)),
                )
                .y_axis(
                    Axis::default()
                        .bounds([0.0, y_max])
                        .labels(y_labels)
                        .style(Style::default().fg(Color::DarkGray)),
                );

            f.render_widget(chart, chunks[1]);

            // --- hops or stats detail ---
            if st.cfg.trace {
                let wide = st.wide;
                let mut rows: Vec<Row> = vec![
                    Row::new(hop_header(wide)).style(Style::default().fg(Color::Yellow).bold())
                ];
                for (i, h) in st.hops.iter().enumerate() {
                    if h.ip.is_none() && h.sent == 0 {
                        continue;
                    }
                    let cells = hop_cells(i, h, wide, st.ascii);
                    let last = cells.len() - 1;
                    let cells = cells.into_iter().enumerate().map(|(k, (t, c))| {
                        if k == last {
                            Cell::from(Line::from(spark_spans(&h.spark, st.ascii)))
                        } else {
                            Cell::from(Span::styled(
                                t,
                                Style::default().fg(c.unwrap_or(Color::Reset)),
                            ))
                        }
                    });
                    rows.push(Row::new(cells));
                }
                let widths: Vec<u16> = if wide {
                    vec![4, 20, 7, 5, 5, 9, 9, 9, 9, 7, 9, SPARK_W as u16 + 2]
                } else {
                    vec![4, 24, 7, 5, 9, 9, 9, SPARK_W as u16 + 2]
                };
                let table = Table::new(rows, widths.into_iter().map(Constraint::Length)).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Color::DarkGray)
                        .title(Line::from(" Path (per-hop)   [f] columns ")),
                );
                f.render_widget(table, chunks[2]);
            }

            // --- packet log ---
            let mut log_text = Text::default();
            for (seq, rtt) in st.log.iter().rev() {
                let (txt, col) = match rtt {
                    Some(v) => (
                        format!("  seq {seq}: reply from {}  time={}ms", st.dest_name, v),
                        grade_color(*v, st.ascii).unwrap_or(Color::Reset),
                    ),
                    None => (
                        format!("  seq {seq}: no response from {}", st.dest_name),
                        Color::Red,
                    ),
                };
                log_text.push_line(Line::from(Span::styled(txt, Style::default().fg(col))));
            }
            f.render_widget(
                Paragraph::new(log_text).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Color::DarkGray)
                        .title(" Packet Log "),
                ),
                chunks[3],
            );
        })
        .is_err();
}

fn main() {
    let cfg = Cli::parse();
    let (dest, name) = resolve_target(&cfg.target);
    let (fd, ping_mode) = open_icmp(&cfg.iface);
    if !ping_mode {
        eprintln!("pingr: using raw socket (root/cap mode)");
    } else if !enable_recverr(fd) {
        eprintln!("pingr: IP_RECVERR failed, --tr hops will be blank");
    }

    let mut payload = vec![0u8; cfg.size];
    for (i, b) in payload.iter_mut().enumerate() {
        *b = (i as u8).wrapping_add(0xA0);
    }

    let mut st = State {
        dest,
        dest_name: name,
        cfg: cfg.clone(),
        icmp_id: std::process::id() as u16,
        payload,
        hops: (0..cfg.max_hops).map(|_| Hop::default()).collect(),
        sent: 0,
        rcvd: 0,
        min_r: None,
        max_r: None,
        last_r: None,
        sum: 0,
        jitter: 0.0,
        jit_n: 0,
        chart: VecDeque::new(),
        log: VecDeque::new(),
        round: None,
        ping_mode,
        ascii: cfg.ascii || std::env::var_os("NO_COLOR").is_some(),
        wide: false,
        dest_hop: None,
    };

    if cfg.quiet {
        quiet_loop(&mut st, fd);
        if let Ok(f) = std::env::var("PINGR_HOPS") {
            use std::io::Write;
            let mut o = std::fs::File::create(&f).expect("hops file");
            for (i, h) in st.hops.iter().enumerate() {
                if h.sent == 0 {
                    continue;
                }
                let (avg, min, max, sd, rc) = h.st();
                let _ = writeln!(
                    o,
                    "hop {} {:?} sent={} loss={}% avg={} min={} max={} sd={}",
                    i + 1,
                    h.ip,
                    h.sent,
                    rc,
                    avg,
                    min,
                    max,
                    sd
                );
            }
        }
        return;
    }

    let mut term = Terminal::new(CrosstermBackend::new(std::io::stdout())).expect("init terminal");
    enable_raw_mode().expect("raw mode");
    let _ = execute!(std::io::stdout(), EnterAlternateScreen);

    let mut quit = false;
    let interval = Duration::from_millis(cfg.interval_ms);
    let mut next_round = Instant::now();
    let mut cleared = false;

    while !quit {
        if poll(Duration::from_millis(10)).unwrap_or(false) {
            if let Ok(e) = crossterm::event::read() {
                match e {
                    Event::Key(k) if k.kind == KeyEventKind::Press => match k.code {
                        KeyCode::Char('q') | KeyCode::Esc => quit = true,
                        KeyCode::Char('c') => {
                            st.chart.clear();
                            cleared = true;
                        }
                        KeyCode::Char('f') if st.cfg.trace => st.wide = !st.wide,
                        _ => {}
                    },
                    _ => {}
                }
            }
        }

        if !quit && Instant::now() >= next_round {
            start_round(&mut st, fd);
            let mut buf = [0u8; 16384];
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut drawn = Instant::now();
            while Instant::now() < deadline {
                let ms = std::cmp::max(
                    std::cmp::min(
                        deadline
                            .saturating_duration_since(Instant::now())
                            .as_millis(),
                        40,
                    ) as i32,
                    1,
                );
                drain(&mut st, fd, &mut buf, ms);
                if poll(Duration::ZERO).unwrap_or(false) {
                    if let Ok(e) = crossterm::event::read() {
                        match e {
                            Event::Key(k) if k.kind == KeyEventKind::Press => match k.code {
                                KeyCode::Char('q') | KeyCode::Esc => {
                                    quit = true;
                                    break;
                                }
                                KeyCode::Char('c') => {
                                    st.chart.clear();
                                }
                                KeyCode::Char('f') if st.cfg.trace => st.wide = !st.wide,
                                _ => {}
                            },
                            _ => {}
                        }
                    }
                }
                if Instant::now().saturating_duration_since(drawn).as_millis() >= 100 || cleared {
                    cleared = false;
                    draw(&mut term, &st);
                    drawn = Instant::now();
                }
                if round_finished(&st) {
                    break;
                }
            }
            end_round(&mut st);
            next_round = Instant::now() + interval;
            if cfg.rounds > 0 && st.sent >= cfg.rounds {
                quit = true;
            }
            draw(&mut term, &st);
        }
    }

    let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
    let _ = disable_raw_mode();
    if cfg.report_on_exit && cfg.trace {
        use std::io::Write;
        let mut so = std::io::stdout();
        for l in report_lines(&st) {
            let _ = writeln!(so, "{}", l);
        }
        let _ = so.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    // Craft a TTL-exceeded reply (ping-socket layout):
    // 8B ICMP error hdr + 20B orig IP + 8B orig ICMP hdr
    fn ttl_exceeded(id: u16, hop_seq: u16) -> Vec<u8> {
        let mut v = vec![0u8; 40];
        v[0] = 11;
        v[16] = 4; // type=11; proto=ICMP inside orig IP hdr
        v[32..34].copy_from_slice(&id.to_be_bytes()); // inner ICMP id
        v[34..36].copy_from_slice(&hop_seq.to_be_bytes()); // inner seq = hop+1
        v
    }

    #[test]
    fn hop_attribution_ping_mode() {
        let mut st = State {
            dest: Ipv4Addr::LOCALHOST,
            dest_name: "t".into(),
            cfg: Cli::parse_from(["pingr", "t"]),
            icmp_id: 0x1234,
            payload: vec![0u8; 8],
            hops: (0..4).map(|_| Hop::default()).collect(),
            sent: 0,
            rcvd: 0,
            min_r: None,
            max_r: None,
            last_r: None,
            sum: 0,
            jitter: 0.0,
            jit_n: 0,
            chart: VecDeque::new(),
            log: VecDeque::new(),
            round: None,
            ping_mode: true,
            ascii: false,
            wide: false,
            dest_hop: None,
        };
        let r = Round {
            seq: 0,
            sent_at: Instant::now(),
            target_done: true,
            target_rtt: Some(1),
            hop_done: vec![false; 4],
            hop_sent_at: (0..4).map(|_| Some(Instant::now())).collect(),
            hop_rtt: vec![None; 4],
        };
        st.round = Some(r);
        // hop 3 (ttl=3) replies
        dispatch(
            &mut st,
            &ttl_exceeded(0x1234, HOP_SEQ | 3),
            Ipv4Addr::new(10, 0, 0, 5),
        );
        assert_eq!(st.hops[2].ip, Some(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5))));
        assert_eq!(st.hops[2].samples.len(), 1);
        // ping mode: ids are kernel-rewritten, attribution via inner seq only;
        // out-of-range hop index dropped
        dispatch(
            &mut st,
            &ttl_exceeded(0x1234, HOP_SEQ | 3),
            Ipv4Addr::new(10, 0, 0, 9),
        );
        assert_eq!(st.hops[2].rcvd, 1); // already done this round, ignored
        dispatch(
            &mut st,
            &ttl_exceeded(0x1234, HOP_SEQ | 99),
            Ipv4Addr::new(10, 0, 0, 9),
        );
        // sparkline: 16 slots, newest right, scales to max
        let sp = sparkline(&[1, 5, 10, 50], true);
        assert_eq!(sp.len(), SPARK_W);
        assert_eq!(sp.as_bytes()[3] as u8, SPARKS[9]);
        assert_eq!(sp.as_bytes()[2] as u8, SPARKS[1]); // 10/50 -> lvl floor(9*10/50)=1
        let empty = sparkline(&[], true);
        assert_eq!(empty, " ".repeat(SPARK_W));
        let _ = st;
    }

    fn test_state(nhops: usize, trace: bool) -> State {
        let mut cfg = Cli::parse_from(["pingr", "t"]);
        cfg.trace = trace;
        cfg.max_hops = nhops;
        State {
            dest: Ipv4Addr::new(10, 9, 9, 9),
            dest_name: "t".into(),
            cfg,
            icmp_id: 0x1234,
            payload: vec![0u8; 8],
            hops: (0..nhops).map(|_| Hop::default()).collect(),
            sent: 0,
            rcvd: 0,
            min_r: None,
            max_r: None,
            last_r: None,
            sum: 0,
            jitter: 0.0,
            jit_n: 0,
            chart: VecDeque::new(),
            log: VecDeque::new(),
            round: None,
            ping_mode: true,
            ascii: false,
            wide: false,
            dest_hop: None,
        }
    }

    fn echo_reply(seq: u16) -> Vec<u8> {
        let mut v = vec![0u8; 8];
        v[6..8].copy_from_slice(&seq.to_be_bytes());
        v
    }

    /// Open a round for `seq` whose probe was sent `age_ms` ago, then deliver a reply.
    fn reply_after(st: &mut State, seq: u16, age_ms: u64) {
        let nh = st.hops.len();
        st.round = Some(Round {
            seq,
            sent_at: Instant::now() - Duration::from_millis(age_ms),
            target_done: false,
            target_rtt: None,
            hop_done: vec![false; nh],
            hop_sent_at: vec![None; nh],
            hop_rtt: vec![None; nh],
        });
        dispatch(st, &echo_reply(seq), st.dest);
    }

    #[test]
    fn loss_is_lost_share_not_received_share() {
        let h = Hop {
            sent: 10,
            rcvd: 8,
            samples: vec![5; 8],
            ..Hop::default()
        };
        assert_eq!(h.st().4, 20);
        // sample window is capped at 32; loss must not depend on it
        let h = Hop {
            sent: 100,
            rcvd: 100,
            samples: vec![5; 32],
            ..Hop::default()
        };
        assert_eq!(h.st().4, 0);
    }

    #[test]
    fn jitter_is_reported_after_second_reply() {
        let mut st = test_state(1, false);
        reply_after(&mut st, 0, 10);
        reply_after(&mut st, 1, 30);
        assert_eq!(st.jit_n, 1);
        assert!(st.jitter >= 15.0, "jitter {}", st.jitter);
    }

    #[test]
    fn hop_cells_match_header_in_both_layouts() {
        let h = Hop {
            ip: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            sent: 4,
            rcvd: 4,
            ema: 7,
            samples: vec![3, 9],
            spark: vec![3, 9],
            ..Hop::default()
        };
        for wide in [false, true] {
            let hdr = hop_header(wide);
            let cells = hop_cells(0, &h, wide, true);
            assert_eq!(hdr.len(), cells.len());
            let at = |name: &str| {
                cells[hdr.iter().position(|x| *x == name).unwrap()]
                    .0
                    .clone()
            };
            assert_eq!(at("EMA"), "7ms");
            assert_eq!(at("GRAPH"), sparkline(&h.spark, true));
            if wide {
                assert_eq!(at("BEST"), "3ms");
                assert_eq!(at("WRST"), "9ms");
            }
        }
    }

    #[test]
    fn destination_hop_ends_the_path() {
        let mut st = test_state(6, true);
        let dest = st.dest;
        let nh = 6;
        st.round = Some(Round {
            seq: 0,
            sent_at: Instant::now(),
            target_done: false,
            target_rtt: None,
            hop_done: vec![false; nh],
            hop_sent_at: (0..nh).map(|_| Some(Instant::now())).collect(),
            hop_rtt: vec![None; nh],
        });
        // hops 5 and 4 both answer from the destination; 4 is the real end
        dispatch(&mut st, &echo_reply(HOP_SEQ | 5), dest);
        dispatch(&mut st, &echo_reply(HOP_SEQ | 4), dest);
        assert_eq!(st.dest_hop, Some(3));
        assert!(st.hops[3].ip.is_some());
        assert!(
            st.hops[4].ip.is_none(),
            "hop past destination must be wiped"
        );
        end_round(&mut st);
        assert_eq!(st.hops[3].sent, 1);
        assert_eq!(st.hops[4].sent, 0);
        assert_eq!(st.hops[3].st().4, 0);
    }

    #[test]
    fn hop_echo_reply_is_not_a_target_reply() {
        let mut st = test_state(4, true);
        reply_after(&mut st, 2, 5);
        assert_eq!(st.rcvd, 1);
        st.round = None;
        reply_after(&mut st, 3, 5);
        let dest = st.dest;
        dispatch(&mut st, &echo_reply(HOP_SEQ | 3), dest); // hop reply, other seq space
        assert_eq!(st.rcvd, 2);
    }

    #[test]
    fn chart_is_bounded() {
        let mut st = test_state(1, false);
        for i in 0..(CHART_MAX as u16 + 50) {
            reply_after(&mut st, i, 1);
        }
        assert_eq!(st.chart.len(), CHART_MAX);
    }

    #[test]
    fn ttl_is_set_as_int() {
        let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
        assert!(fd >= 0);
        assert!(set_ttl(fd, 7));
        let mut v: libc::c_int = 0;
        let mut l = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        unsafe {
            libc::getsockopt(
                fd,
                libc::IPPROTO_IP,
                libc::IP_TTL,
                &mut v as *mut _ as *mut _,
                &mut l,
            );
        }
        assert_eq!(v, 7);
    }

    #[test]
    fn recv_err_returns_offender_and_type() {
        // closed loopback UDP port -> kernel queues ICMP dest-unreachable (type 3), no privilege needed
        let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
        assert!(fd >= 0 && enable_recverr(fd));
        let probe = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe); // now closed
        let mut a = sock_addr(Ipv4Addr::LOCALHOST);
        a.sin_port = port.to_be();
        let msg = [0xA5u8; 8];
        unsafe {
            libc::sendto(
                fd,
                msg.as_ptr() as *const _,
                8,
                0,
                &a as *const _ as *const libc::sockaddr,
                16,
            );
        }
        let mut buf = [0u8; 64];
        let (ip, typ, n) = recv_err(fd, &mut buf).expect("queued error");
        assert_eq!((ip, typ, n), (Ipv4Addr::LOCALHOST, 3, 8));
        assert_eq!(&buf[..8], &msg);
        assert!(recv_err(fd, &mut buf).is_none(), "queue now empty");
    }

    #[test]
    fn time_exceeded_by_seq_alone_fills_the_hop() {
        let mut st = test_state(4, true);
        st.round = Some(Round {
            seq: 0,
            sent_at: Instant::now(),
            target_done: false,
            target_rtt: None,
            hop_done: vec![false; 4],
            hop_sent_at: (0..4).map(|_| Some(Instant::now())).collect(),
            hop_rtt: vec![None; 4],
        });
        hop_time_exceeded(&mut st, 0, HOP_SEQ | 2, Ipv4Addr::new(10, 0, 0, 1));
        assert_eq!(st.hops[1].ip, Some(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))));
        hop_time_exceeded(&mut st, 0, 2, Ipv4Addr::new(10, 0, 0, 2)); // target probe seq: ignored
        assert_eq!(st.hops[1].rcvd, 1);
    }

    fn screen(st: &State, w: u16, h: u16) -> Vec<String> {
        let mut t = Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        draw(&mut t, st);
        let buf = t.backend().buffer().clone();
        (0..h)
            .map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect())
            .collect()
    }

    #[test]
    fn key_hints_visible_in_both_modes() {
        for trace in [false, true] {
            let st = test_state(4, trace);
            let txt = screen(&st, 100, 30).join("\n");
            assert!(
                txt.contains("q quit") && txt.contains("c clear"),
                "trace={trace}"
            );
            assert_eq!(txt.contains("f columns"), trace);
        }
    }

    #[test]
    fn packet_log_fills_the_screen_in_ping_mode() {
        let mut st = test_state(1, false);
        for i in 0..100u64 {
            st.log.push_back((i, Some(3)));
        }
        let n = screen(&st, 100, 40)
            .iter()
            .filter(|l| l.contains("reply from"))
            .count();
        assert!(n >= 18, "only {n} log lines visible");
    }

    #[test]
    fn fqdn_target_shows_ip_in_brackets() {
        let (ip, label) = resolve_target("localhost");
        assert_eq!(label, format!("localhost ({ip})"));
        assert_eq!(resolve_target("1.2.3.4").1, "1.2.3.4");
    }

    #[test]
    fn graph_column_uses_coloured_blocks() {
        let spans = spark_spans(&[1, 50, 200], false);
        assert_eq!(spans.len(), SPARK_W);
        assert_eq!(spans[2].content, "█");
        assert_eq!(spans[0].style.fg, Some(Color::Green));
        assert_eq!(spans[2].style.fg, Some(Color::Red));
        assert!(spark_spans(&[1, 50], true)
            .iter()
            .all(|s| s.style.fg == Some(Color::Reset)));
    }
}
