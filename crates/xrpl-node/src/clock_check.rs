//! Independence Step 9: a check on the clock the validator signs with.
//!
//! Every validation carries `sfSigningTime` from the system clock (`validation::sign_validation`), and nothing
//! checked that clock. rippled 3.x does not either: `TimeKeeper::now()` is `system_clock::now()` moved to the
//! Ripple epoch (ffi/vendor/rippled `src/xrpld/core/TimeKeeper.h:66`, "It's possible for servers to have a
//! different value for network time ... This is fine."); the old SNTPClock is gone, and only the
//! `[sntp_servers]` section name is left (`ConfigSections.h:82`). It leaves the clock to the host's NTP. This
//! module goes past rippled: a small SNTP client (RFC 4330) asks [`SERVERS`] every [`PERIOD`] and keeps the
//! median offset, so a host whose NTP has quietly stopped shows up before its drift matters.
//!
//! Peers accept a validation signed up to 3 minutes in the past or 5 minutes in the future
//! (`isCurrent`, `src/xrpld/consensus/Validations.h:162-163`), so a 1-second error does no harm by itself. It
//! means the host's time-keeping has failed, and drift only grows. Hence:
//!   - `XRPL_CLOCK_CHECK=1`: watch and log only (a line when the offset changes, `clock` in `/api/engine`);
//!   - `XRPL_CLOCK_GATE=1`: also skip signing while the measured |offset| is over [`GATE_LIMIT_MS`]. It runs
//!     the watcher too. "No answer" never gates: time servers we cannot reach must not stop signing, and
//!     neither must a single server's word ([`MIN_ANSWERS_TO_GATE`]) or a reading gone stale.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

/// The time servers asked each round, three operators so one bad answer is outvoted.
pub const SERVERS: &[&str] = &["time.google.com", "time.cloudflare.com", "pool.ntp.org"];

/// How often the servers are asked.
pub const PERIOD: Duration = Duration::from_secs(300);

/// One server's answer must come within this.
pub const QUERY_TIMEOUT: Duration = Duration::from_secs(3);

/// The gate's limit on |offset|.
pub const GATE_LIMIT_MS: u64 = 1000;

/// Fewer answers than this never gate: one server alone (a pool volunteer, say) may be the wrong one.
pub const MIN_ANSWERS_TO_GATE: usize = 2;

/// A reading older than this never gates: the watcher missed three rounds, which is "no answer".
pub const STALE_AFTER: Duration = Duration::from_secs(3 * 300);

/// Seconds from the NTP era (1900-01-01) to the Unix epoch.
const NTP_UNIX_OFFSET: u64 = 2_208_988_800;

/// An NTP timestamp: seconds since 1900 in the high 32 bits, the fraction of a second in the low 32.
pub type NtpTimestamp = u64;

/// `t` as an NTP timestamp. The 32-bit seconds wrap in 2036 (era 1); every difference below is taken
/// modulo 2^64 and read as signed, RFC 4330 section 3's rule, so the wrap does not matter.
pub fn ntp_from_system(t: SystemTime) -> NtpTimestamp {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = (d.as_secs() + NTP_UNIX_OFFSET) & 0xFFFF_FFFF;
    let frac = (u64::from(d.subsec_nanos()) << 32) / 1_000_000_000;
    (secs << 32) | frac
}

/// The client request (RFC 4330 section 4): LI 0, version 4, mode 3 (client), and our send time in the
/// Transmit Timestamp, which the server echoes back as the Originate Timestamp.
pub fn request_packet(t1: NtpTimestamp) -> [u8; 48] {
    let mut p = [0u8; 48];
    p[0] = (4 << 3) | 3;
    p[40..48].copy_from_slice(&t1.to_be_bytes());
    p
}

/// A server's reply, checked: its Receive (T2) and Transmit (T3) timestamps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reply {
    pub t2: NtpTimestamp,
    pub t3: NtpTimestamp,
    pub stratum: u8,
}

/// Parse and check a reply to the request sent at `t1` (RFC 4330 section 5): mode 4 (server), not the
/// "clock not synchronized" alarm (LI 3), stratum 1-15 (0 is a kiss-o'-death), our `t1` echoed back, and a
/// non-zero Transmit Timestamp.
pub fn parse_reply(buf: &[u8], t1: NtpTimestamp) -> Result<Reply, String> {
    if buf.len() < 48 {
        return Err(format!("short reply ({} bytes)", buf.len()));
    }
    let ts = |at: usize| u64::from_be_bytes(buf[at..at + 8].try_into().expect("8 bytes"));
    let (li, mode, stratum) = (buf[0] >> 6, buf[0] & 7, buf[1]);
    if mode != 4 {
        return Err(format!("mode {mode}, not a server reply"));
    }
    if li == 3 {
        return Err("the server's clock is not synchronized (LI 3)".into());
    }
    if stratum == 0 || stratum > 15 {
        return Err(format!("stratum {stratum} (kiss-o'-death or unsynchronized)"));
    }
    if ts(24) != t1 {
        return Err("the reply does not echo our request".into());
    }
    let (t2, t3) = (ts(32), ts(40));
    if t3 == 0 {
        return Err("no transmit timestamp".into());
    }
    Ok(Reply { t2, t3, stratum })
}

/// A signed 32.32 fixed-point span in microseconds, rounded to the nearest.
fn fixed_to_micros(fx: i128) -> i64 {
    let scaled = fx * 1_000_000;
    let half = 1i128 << 31;
    ((if scaled >= 0 { scaled + half } else { scaled - half }) / (1i128 << 32)) as i64
}

/// The clock offset and round-trip delay in microseconds (RFC 4330 section 5):
/// offset = ((T2 - T1) + (T3 - T4)) / 2, delay = (T4 - T1) - (T3 - T2). A positive offset means the server's
/// clock is ahead of ours: our clock is slow.
pub fn offset_and_delay_us(t1: NtpTimestamp, t2: NtpTimestamp, t3: NtpTimestamp, t4: NtpTimestamp) -> (i64, i64) {
    let d = |a: NtpTimestamp, b: NtpTimestamp| i128::from(a.wrapping_sub(b) as i64);
    let offset = (d(t2, t1) + d(t3, t4)) / 2;
    let delay = d(t4, t1) - d(t3, t2);
    (fixed_to_micros(offset), fixed_to_micros(delay))
}

/// The median of the answers' offsets (the mean of the middle two for an even count). None with no answer.
pub fn median(offsets: &[i64]) -> Option<i64> {
    let mut v = offsets.to_vec();
    v.sort_unstable();
    let n = v.len();
    match n {
        0 => None,
        _ if n % 2 == 1 => Some(v[n / 2]),
        _ => Some(((i128::from(v[n / 2 - 1]) + i128::from(v[n / 2])) / 2) as i64),
    }
}

/// One server's answer: (offset, round-trip delay) in milliseconds, or why it gave none.
pub type ServerAnswer = Result<(i64, i64), String>;

/// One round of questions.
#[derive(Debug, Clone)]
pub struct Reading {
    pub at: Instant,
    pub at_unix: u64,
    /// The median offset in milliseconds; None when no server answered.
    pub offset_ms: Option<i64>,
    pub answers: usize,
    /// Each server and its answer.
    pub servers: Vec<(String, ServerAnswer)>,
}

/// The gate's verdict (`XRPL_CLOCK_GATE=1`): Some(reason) to skip signing, None to sign. Only a fresh reading
/// with at least [`MIN_ANSWERS_TO_GATE`] answers whose median |offset| is over [`GATE_LIMIT_MS`] skips; no
/// reading, no answer, one answer, or a stale reading all sign.
pub fn gate_verdict(gate_on: bool, reading: Option<&Reading>, now: Instant) -> Option<String> {
    let r = reading.filter(|_| gate_on)?;
    let offset = r.offset_ms?;
    if r.answers < MIN_ANSWERS_TO_GATE || now.saturating_duration_since(r.at) > STALE_AFTER {
        return None;
    }
    (offset.unsigned_abs() > GATE_LIMIT_MS).then(|| {
        format!(
            "clock offset {offset:+} ms (median of {} time servers) is over the {GATE_LIMIT_MS} ms limit",
            r.answers
        )
    })
}

/// Whether a new reading is worth a log line: the first one, an answer gained or lost, a crossing of the
/// gate limit, or a move of 100 ms or more since the last line.
pub fn worth_logging(last_logged: Option<Option<i64>>, now: Option<i64>) -> bool {
    let over = |o: i64| o.unsigned_abs() > GATE_LIMIT_MS;
    match (last_logged, now) {
        (None, _) => true,
        (Some(None), None) => false,
        (Some(None), Some(_)) | (Some(Some(_)), None) => true,
        (Some(Some(a)), Some(b)) => over(a) != over(b) || a.abs_diff(b) >= 100,
    }
}

/// `XRPL_CLOCK_CHECK=1` (or the gate, which needs the watcher).
pub fn check_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("XRPL_CLOCK_CHECK").as_deref() == Ok("1") || gate_on())
}

/// `XRPL_CLOCK_GATE=1`.
pub fn gate_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("XRPL_CLOCK_GATE").as_deref() == Ok("1"))
}

static LAST: Mutex<Option<Reading>> = Mutex::new(None);
static ROUNDS: AtomicU64 = AtomicU64::new(0);
static NO_ANSWER_ROUNDS: AtomicU64 = AtomicU64::new(0);
static SKIPPED: AtomicU64 = AtomicU64::new(0);

/// Ask one server once: (offset, delay) in microseconds.
async fn query(host: &str) -> Result<(i64, i64), String> {
    let addrs: Vec<std::net::SocketAddr> =
        tokio::net::lookup_host((host, 123)).await.map_err(|e| format!("resolve: {e}"))?.collect();
    let addr = addrs.iter().find(|a| a.is_ipv4()).or(addrs.first()).copied().ok_or("resolve: no address")?;
    let local = if addr.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" };
    let sock = tokio::net::UdpSocket::bind(local).await.map_err(|e| format!("bind: {e}"))?;
    sock.connect(addr).await.map_err(|e| format!("connect: {e}"))?;
    let t1 = ntp_from_system(SystemTime::now());
    sock.send(&request_packet(t1)).await.map_err(|e| format!("send: {e}"))?;
    let mut buf = [0u8; 128];
    let n = tokio::time::timeout(QUERY_TIMEOUT, sock.recv(&mut buf))
        .await
        .map_err(|_| "no answer".to_string())?
        .map_err(|e| format!("recv: {e}"))?;
    let t4 = ntp_from_system(SystemTime::now());
    let r = parse_reply(&buf[..n], t1)?;
    Ok(offset_and_delay_us(t1, r.t2, r.t3, t4))
}

/// One round: every server at once, then the median.
async fn round() -> Reading {
    let answers = futures::future::join_all(SERVERS.iter().map(|h| query(h))).await;
    let servers: Vec<(String, ServerAnswer)> = SERVERS
        .iter()
        .zip(answers)
        .map(|(h, a)| (h.to_string(), a.map(|(o, d)| (o / 1000, d / 1000))))
        .collect();
    let offsets: Vec<i64> = servers.iter().filter_map(|(_, a)| a.as_ref().ok().map(|(o, _)| *o)).collect();
    Reading {
        at: Instant::now(),
        at_unix: SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
        offset_ms: median(&offsets),
        answers: offsets.len(),
        servers,
    }
}

fn describe(r: &Reading) -> String {
    let each: Vec<String> = r
        .servers
        .iter()
        .map(|(h, a)| match a {
            Ok((o, d)) => format!("{h} {o:+} ms (rtt {d} ms)"),
            Err(e) => format!("{h}: {e}"),
        })
        .collect();
    match r.offset_ms {
        Some(o) => format!("offset {o:+} ms, median of {} [{}]", r.answers, each.join("; ")),
        None => format!("no time server answered [{}]", each.join("; ")),
    }
}

async fn watch() {
    let mut last_logged: Option<Option<i64>> = None;
    loop {
        let r = round().await;
        ROUNDS.fetch_add(1, Ordering::Relaxed);
        if r.offset_ms.is_none() {
            NO_ANSWER_ROUNDS.fetch_add(1, Ordering::Relaxed);
        }
        if worth_logging(last_logged, r.offset_ms) {
            let warn = if r.offset_ms.is_some_and(|o| o.unsigned_abs() > GATE_LIMIT_MS) { " ⚠ over the limit" } else { "" };
            eprintln!("[clock] {}{warn}", describe(&r));
            last_logged = Some(r.offset_ms);
        }
        *LAST.lock().unwrap_or_else(|e| e.into_inner()) = Some(r);
        tokio::time::sleep(PERIOD).await;
    }
}

/// Start the watcher when `XRPL_CLOCK_CHECK=1` or `XRPL_CLOCK_GATE=1` (inside the tokio runtime), with one
/// startup line. Off: nothing runs and nothing is logged.
pub fn start() {
    static STARTED: OnceLock<()> = OnceLock::new();
    if !check_on() || STARTED.set(()).is_err() {
        return;
    }
    let mode = if gate_on() {
        format!("GATE: no validation is signed while the median |offset| is over {GATE_LIMIT_MS} ms (an unanswered or stale check never gates)")
    } else {
        "watch only (XRPL_CLOCK_GATE unset)".to_string()
    };
    eprintln!("[clock] ON — SNTP offset from {} every {} min; {mode}", SERVERS.join(", "), PERIOD.as_secs() / 60);
    tokio::spawn(watch());
}

/// The signing path's question (`XRPL_CLOCK_GATE=1`): Some(reason) to skip this validation, counted.
pub fn gate_skip() -> Option<String> {
    if !gate_on() {
        return None;
    }
    let why = gate_verdict(true, LAST.lock().unwrap_or_else(|e| e.into_inner()).as_ref(), Instant::now())?;
    SKIPPED.fetch_add(1, Ordering::Relaxed);
    Some(why)
}

/// `clock` in `/api/engine`.
pub fn json() -> Value {
    let last = LAST.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let mut j = json!({
        "check": check_on(),
        "gate": gate_on(),
        "limit_ms": GATE_LIMIT_MS,
        "rounds": ROUNDS.load(Ordering::Relaxed),
        "no_answer_rounds": NO_ANSWER_ROUNDS.load(Ordering::Relaxed),
        "skipped_validations": SKIPPED.load(Ordering::Relaxed),
    });
    if let Some(r) = last {
        j["offset_ms"] = json!(r.offset_ms);
        j["answers"] = json!(r.answers);
        j["at_unix"] = json!(r.at_unix);
        j["servers"] = r
            .servers
            .iter()
            .map(|(h, a)| match a {
                Ok((o, d)) => json!({"server": h, "offset_ms": o, "rtt_ms": d}),
                Err(e) => json!({"server": h, "error": e}),
            })
            .collect();
    }
    j
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-10-13T12:00:00Z as an NTP timestamp, plus `ms` milliseconds.
    fn at(ms: i64) -> NtpTimestamp {
        let base = (1_791_892_800u64 + NTP_UNIX_OFFSET) << 32;
        base.wrapping_add_signed(((i128::from(ms) << 32) / 1000) as i64)
    }

    fn reply(t1: NtpTimestamp, t2: NtpTimestamp, t3: NtpTimestamp) -> [u8; 48] {
        let mut p = [0u8; 48];
        p[0] = (4 << 3) | 4; // LI 0, version 4, mode 4
        p[1] = 2;
        p[24..32].copy_from_slice(&t1.to_be_bytes());
        p[32..40].copy_from_slice(&t2.to_be_bytes());
        p[40..48].copy_from_slice(&t3.to_be_bytes());
        p
    }

    #[test]
    fn the_request_is_a_version_4_client_packet_carrying_our_send_time() {
        let p = request_packet(at(0));
        assert_eq!(p[0], 0x23);
        assert_eq!(u64::from_be_bytes(p[40..48].try_into().unwrap()), at(0));
        assert!(p[1..40].iter().all(|&b| b == 0));
    }

    #[test]
    fn the_unix_epoch_converts_to_the_ntp_era() {
        assert_eq!(ntp_from_system(UNIX_EPOCH), NTP_UNIX_OFFSET << 32);
        let half = UNIX_EPOCH + Duration::from_millis(500);
        assert_eq!(ntp_from_system(half), (NTP_UNIX_OFFSET << 32) | 0x8000_0000);
    }

    #[test]
    fn a_good_reply_parses_and_bad_ones_are_refused() {
        let t1 = at(0);
        let ok = reply(t1, at(10), at(11));
        assert_eq!(parse_reply(&ok, t1), Ok(Reply { t2: at(10), t3: at(11), stratum: 2 }));
        assert!(parse_reply(&ok[..47], t1).unwrap_err().contains("short"));
        assert!(parse_reply(&ok, at(1)).unwrap_err().contains("echo"), "another request's reply");
        let mut kod = ok;
        kod[1] = 0;
        assert!(parse_reply(&kod, t1).unwrap_err().contains("kiss"));
        let mut alarm = ok;
        alarm[0] |= 3 << 6;
        assert!(parse_reply(&alarm, t1).unwrap_err().contains("LI 3"));
        let mut client = ok;
        client[0] = 0x23;
        assert!(parse_reply(&client, t1).unwrap_err().contains("mode 3"));
        let mut blank = ok;
        blank[40..48].fill(0);
        assert!(parse_reply(&blank, t1).is_err());
    }

    #[test]
    fn offset_and_delay_follow_rfc_4330() {
        // Our clock 1.5 s slow, 40 ms each way, 2 ms in the server: T1=0, T2=1540, T3=1542, T4=82 (ours).
        let (o, d) = offset_and_delay_us(at(0), at(1540), at(1542), at(82));
        assert_eq!((o / 1000, d / 1000), (1500, 80));
        // Our clock 250 ms fast: the offset is negative.
        let (o, d) = offset_and_delay_us(at(0), at(-240), at(-239), at(21));
        assert_eq!((o / 1000, d / 1000), (-250, 20));
        // Exact to the microsecond on a symmetric path.
        let (o, _) = offset_and_delay_us(at(1000), at(1010), at(1010), at(1020));
        assert!(o.abs() <= 1, "{o}");
    }

    #[test]
    fn the_2036_era_wrap_does_not_break_the_arithmetic() {
        // T1 just before the 32-bit seconds wrap, the server's stamps just after it.
        let t1: NtpTimestamp = 0xFFFF_FFFF_0000_0000;
        let t2 = t1.wrapping_add(2 << 32);
        let t3 = t2;
        let t4 = t1.wrapping_add(2 << 31); // 1 s later on our clock
        let (o, d) = offset_and_delay_us(t1, t2, t3, t4);
        assert_eq!((o, d), (1_500_000, 1_000_000));
    }

    #[test]
    fn the_median_outvotes_one_bad_server() {
        assert_eq!(median(&[]), None);
        assert_eq!(median(&[7]), Some(7));
        assert_eq!(median(&[3, 90_000, 5]), Some(5));
        assert_eq!(median(&[-10, 30]), Some(10));
        assert_eq!(median(&[i64::MAX, i64::MAX]), Some(i64::MAX));
    }

    fn reading(offset_ms: Option<i64>, answers: usize, age: Duration) -> (Reading, Instant) {
        let now = Instant::now() + age;
        (Reading { at: now - age, at_unix: 0, offset_ms, answers, servers: vec![] }, now)
    }

    #[test]
    fn the_gate_skips_only_on_a_fresh_agreed_offset_over_one_second() {
        let fresh = Duration::from_secs(10);
        let (r, now) = reading(Some(1_200), 3, fresh);
        assert!(gate_verdict(true, Some(&r), now).unwrap().contains("+1200 ms"));
        let (r, now) = reading(Some(-1_001), 2, fresh);
        assert!(gate_verdict(true, Some(&r), now).is_some(), "a slow clock gates as a fast one does");
        // Within the limit, or the switch off: sign.
        let (r, now) = reading(Some(1_000), 3, fresh);
        assert_eq!(gate_verdict(true, Some(&r), now), None);
        let (r, now) = reading(Some(5_000), 3, fresh);
        assert_eq!(gate_verdict(false, Some(&r), now), None);
    }

    #[test]
    fn no_answer_never_stops_signing() {
        let fresh = Duration::from_secs(10);
        assert_eq!(gate_verdict(true, None, Instant::now()), None, "no reading yet");
        let (r, now) = reading(None, 0, fresh);
        assert_eq!(gate_verdict(true, Some(&r), now), None, "no server answered");
        let (r, now) = reading(Some(9_000), 1, fresh);
        assert_eq!(gate_verdict(true, Some(&r), now), None, "one server alone");
        let (r, now) = reading(Some(9_000), 3, STALE_AFTER + Duration::from_secs(1));
        assert_eq!(gate_verdict(true, Some(&r), now), None, "the watcher went quiet");
    }

    #[test]
    fn a_log_line_marks_each_change_worth_seeing() {
        assert!(worth_logging(None, None));
        assert!(worth_logging(None, Some(3)));
        assert!(!worth_logging(Some(Some(3)), Some(40)));
        assert!(worth_logging(Some(Some(3)), Some(103)));
        assert!(worth_logging(Some(Some(950)), Some(1_001)), "crossing the limit");
        assert!(worth_logging(Some(Some(3)), None) && worth_logging(Some(None), Some(3)));
        assert!(!worth_logging(Some(None), None));
    }
}
