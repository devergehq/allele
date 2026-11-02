//! DEV-1158 spike: iroh 1.4.1 running inside the Allele binary.
//!
//! **Throwaway.** This exists to measure what iroh costs and how it behaves,
//! not to be built on. Pairing, identity storage and UI are out of scope.
//!
//! Two processes on one machine (or two Macs) talk over one bidirectional QUIC
//! stream carrying length-prefixed frames. The dialer connects by `EndpointId`
//! alone, so address lookup (n0 DNS / pkarr) is exercised, not bypassed.
//!
//! ```text
//! Allele --iroh-spike listen [--mode default|relay-only|direct-only] [--key-file F] [--id-file F]
//! Allele --iroh-spike dial <endpoint-id> [--mode ..] [--count N] [--interval-ms M]
//!                         [--payload BYTES] [--duration-s S] [--reconnect]
//! ```
//!
//! Output is one JSON object per line on stdout. `SIGUSR1` makes either side
//! call `Endpoint::network_change()`, the hook a real interface switch fires.
//!
//! Everything runs on the shared session-sync runtime ([`super::rt`]), the same
//! runtime a real transport would use.

use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Boxed error: the spike is throwaway, so it does not join `AlleleError`.
type SpikeErr = Box<dyn std::error::Error + Send + Sync>;
type Result<T> = std::result::Result<T, SpikeErr>;

macro_rules! spike_err {
    ($($t:tt)*) => { SpikeErr::from(format!($($t)*)) };
}
macro_rules! spike_bail {
    ($($t:tt)*) => { return Err(spike_err!($($t)*)) };
}
use iroh::endpoint::{presets, Connection};
use iroh::{Endpoint, EndpointAddr, EndpointId, SecretKey};
use serde_json::json;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// The application protocol. Both ends must agree or the handshake fails.
pub const ALPN: &[u8] = b"allele/spike-echo/0";

/// Refuse frames larger than this, so a bad length prefix cannot allocate GBs.
pub const MAX_FRAME: usize = 1 << 20;

/// If the first argument is `--iroh-spike`, run the spike and exit the process.
/// Otherwise return and let the app start normally.
pub fn exit_if_requested() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) != Some("--iroh-spike") {
        return;
    }
    let code = match super::rt::block_on(run(&args[1..])) {
        Ok(()) => 0,
        Err(e) => {
            emit(json!({ "event": "fatal", "error": format!("{e}") }));
            1
        }
    };
    std::process::exit(code);
}

/// One JSON object per line on stdout — this mode's output channel, like
/// `--capture-ui`'s printed path, not logging.
fn emit(v: serde_json::Value) {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{v}");
    let _ = out.flush();
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

// ── framing ───────────────────────────────────────────────────────────────

/// Write one frame: a big-endian `u32` length, then the payload.
pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, payload: &[u8]) -> Result<()> {
    if payload.len() > MAX_FRAME {
        spike_bail!("frame of {} bytes exceeds {MAX_FRAME}", payload.len());
    }
    w.write_all(&(payload.len() as u32).to_be_bytes()).await?;
    w.write_all(payload).await?;
    w.flush().await?;
    Ok(())
}

/// Read one frame. `Ok(None)` means the peer finished the stream cleanly
/// between frames; EOF inside a frame is an error.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME {
        spike_bail!("peer sent a {len}-byte frame, limit is {MAX_FRAME}");
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)
        .await
        .map_err(|e| spike_err!("EOF inside a frame: {e}"))?;
    Ok(Some(buf))
}

// ── options ───────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// n0 preset: IP transports plus n0 relays; iroh picks the best path.
    Default,
    /// IP transports removed, so the only path is via the relay.
    RelayOnly,
    /// Relay disabled, so the only path is direct UDP.
    DirectOnly,
}

impl Mode {
    fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "default" => Mode::Default,
            "relay-only" => Mode::RelayOnly,
            "direct-only" => Mode::DirectOnly,
            other => spike_bail!("unknown --mode {other}"),
        })
    }
}

#[derive(Debug)]
struct Opts {
    mode: Mode,
    key_file: Option<PathBuf>,
    id_file: Option<PathBuf>,
    count: u64,
    interval: Duration,
    payload: usize,
    duration: Option<Duration>,
    reconnect: bool,
    echo_timeout: Duration,
}

fn parse_opts(args: &[String]) -> Result<Opts> {
    let mut o = Opts {
        mode: Mode::Default,
        key_file: None,
        id_file: None,
        count: 200,
        interval: Duration::from_millis(50),
        payload: 64,
        duration: None,
        reconnect: false,
        echo_timeout: Duration::from_secs(10),
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = || it.next().ok_or_else(|| spike_err!("{a} needs a value"));
        match a.as_str() {
            "--mode" => o.mode = Mode::parse(val()?)?,
            "--key-file" => o.key_file = Some(PathBuf::from(val()?)),
            "--id-file" => o.id_file = Some(PathBuf::from(val()?)),
            "--count" => o.count = val()?.parse()?,
            "--interval-ms" => o.interval = Duration::from_millis(val()?.parse()?),
            "--payload" => o.payload = val()?.parse()?,
            "--duration-s" => o.duration = Some(Duration::from_secs(val()?.parse()?)),
            "--reconnect" => o.reconnect = true,
            "--echo-timeout-ms" => o.echo_timeout = Duration::from_millis(val()?.parse()?),
            other => spike_bail!("unknown option {other}"),
        }
    }
    Ok(o)
}

async fn run(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("listen") => listen(parse_opts(&args[1..])?).await,
        Some("dial") => {
            let id: EndpointId = args
                .get(1)
                .ok_or_else(|| spike_err!("dial needs an endpoint id"))?
                .parse()
                .map_err(|e| spike_err!("not an EndpointId: {e}"))?;
            dial(id, parse_opts(&args[2..])?).await
        }
        _ => spike_bail!("usage: --iroh-spike listen|dial <endpoint-id> [options]"),
    }
}

// ── endpoint ──────────────────────────────────────────────────────────────

fn load_or_create_key(path: Option<&PathBuf>) -> Result<SecretKey> {
    let Some(path) = path else {
        return Ok(SecretKey::generate());
    };
    if let Ok(bytes) = std::fs::read(path) {
        let arr: [u8; 32] = bytes
            .as_slice()
            .try_into()
            .map_err(|_| spike_err!("key file is not 32 bytes"))?;
        return Ok(SecretKey::from_bytes(&arr));
    }
    let key = SecretKey::generate();
    std::fs::write(path, key.to_bytes())?;
    Ok(key)
}

async fn bind(mode: Mode, key: SecretKey, alpns: Vec<Vec<u8>>) -> Result<Endpoint> {
    let builder = match mode {
        Mode::Default => Endpoint::builder(presets::N0),
        Mode::RelayOnly => Endpoint::builder(presets::N0).clear_ip_transports(),
        Mode::DirectOnly => Endpoint::builder(presets::N0DisableRelay),
    };
    let t = Instant::now();
    let ep = builder.secret_key(key).alpns(alpns).bind().await?;
    let bind_ms = t.elapsed().as_millis();
    // `online` waits for a home relay (or, relay disabled, a net report).
    let online = tokio::time::timeout(Duration::from_secs(20), ep.online()).await;
    emit(json!({
        "event": "bound",
        "mode": format!("{mode:?}"),
        "endpoint_id": ep.id().to_string(),
        "bind_ms": bind_ms,
        "online_ms": t.elapsed().as_millis(),
        "online": online.is_ok(),
        "addr": format!("{:?}", ep.addr()),
        "bound_sockets": format!("{:?}", ep.bound_sockets()),
    }));
    Ok(ep)
}

/// Call `network_change()` on every SIGUSR1 — the simulated interface switch.
fn spawn_network_change_trigger(ep: Endpoint) {
    tokio::spawn(async move {
        let Ok(mut sig) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1())
        else {
            return;
        };
        while sig.recv().await.is_some() {
            emit(json!({ "event": "network_change_triggered", "t_ms": now_ms() }));
            ep.network_change().await;
        }
    });
}

/// Print every path event (opened / closed / selected) with a timestamp.
fn spawn_path_logger(conn: Connection, side: &'static str) {
    tokio::spawn(async move {
        use n0_future::StreamExt;
        let mut events = conn.path_events();
        while let Some(ev) = events.next().await {
            emit(
                json!({ "event": "path", "side": side, "t_ms": now_ms(), "detail": format!("{ev:?}") }),
            );
        }
    });
}

/// "relay", "ip", or "none" for the currently selected path.
fn selected_path(conn: &Connection) -> (&'static str, Option<u128>) {
    for p in conn.paths().iter() {
        if p.is_selected() {
            let kind = if p.is_relay() {
                "relay"
            } else if p.is_ip() {
                "ip"
            } else {
                "other"
            };
            return (kind, Some(p.rtt().as_micros()));
        }
    }
    ("none", None)
}

// ── listener ──────────────────────────────────────────────────────────────

async fn listen(o: Opts) -> Result<()> {
    let key = load_or_create_key(o.key_file.as_ref())?;
    let ep = bind(o.mode, key, vec![ALPN.to_vec()]).await?;
    if let Some(f) = &o.id_file {
        std::fs::write(f, ep.id().to_string())?;
    }
    spawn_network_change_trigger(ep.clone());
    while let Some(incoming) = ep.accept().await {
        tokio::spawn(async move {
            let conn = match incoming.await {
                Ok(c) => c,
                Err(e) => {
                    emit(json!({ "event": "accept_error", "error": e.to_string() }));
                    return;
                }
            };
            emit(
                json!({ "event": "accepted", "remote": conn.remote_id().to_string(), "t_ms": now_ms() }),
            );
            spawn_path_logger(conn.clone(), "listener");
            while let Ok((mut send, mut recv)) = conn.accept_bi().await {
                tokio::spawn(async move {
                    while let Ok(Some(frame)) = read_frame(&mut recv).await {
                        if write_frame(&mut send, &frame).await.is_err() {
                            break;
                        }
                    }
                    let _ = send.finish();
                });
            }
            emit(
                json!({ "event": "conn_closed", "reason": format!("{:?}", conn.close_reason()), "t_ms": now_ms() }),
            );
        });
    }
    Ok(())
}

// ── dialer ────────────────────────────────────────────────────────────────

#[derive(Default)]
struct Samples {
    rtt_us: Vec<u128>,
    relay: u64,
    ip: u64,
    errors: u64,
    reconnects: u64,
    longest_outage_ms: u128,
}

/// Percentile over a sorted slice (nearest-rank).
pub fn percentile(sorted: &[u128], p: f64) -> Option<u128> {
    if sorted.is_empty() {
        return None;
    }
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    Some(sorted[rank.clamp(1, sorted.len()) - 1])
}

async fn connect(ep: &Endpoint, id: EndpointId) -> Result<Connection> {
    let t = Instant::now();
    let conn = ep.connect(EndpointAddr::new(id), ALPN).await?;
    let (kind, path_rtt) = selected_path(&conn);
    emit(json!({
        "event": "connected",
        "connect_ms": t.elapsed().as_millis(),
        "selected_path": kind,
        "path_rtt_us": path_rtt,
        "t_ms": now_ms(),
    }));
    spawn_path_logger(conn.clone(), "dialer");
    Ok(conn)
}

async fn dial(id: EndpointId, o: Opts) -> Result<()> {
    let ep = bind(o.mode, load_or_create_key(o.key_file.as_ref())?, vec![]).await?;
    spawn_network_change_trigger(ep.clone());

    let mut conn = connect(&ep, id).await?;
    let (mut send, mut recv) = conn.open_bi().await?;
    let mut s = Samples::default();
    let started = Instant::now();
    let mut outage_since: Option<Instant> = None;
    let mut seq: u64 = 0;

    loop {
        let done = match o.duration {
            Some(d) => started.elapsed() >= d,
            None => seq >= o.count,
        };
        if done {
            break;
        }
        seq += 1;
        let mut payload = vec![0u8; o.payload.max(8)];
        payload[..8].copy_from_slice(&seq.to_be_bytes());

        let t = Instant::now();
        let result = async {
            write_frame(&mut send, &payload).await?;
            let echo = tokio::time::timeout(o.echo_timeout, read_frame(&mut recv))
                .await
                .map_err(|_| spike_err!("echo timed out"))??
                .ok_or_else(|| spike_err!("stream finished"))?;
            if echo != payload {
                spike_bail!("echo mismatch at seq {seq}");
            }
            Ok::<(), SpikeErr>(())
        }
        .await;

        match result {
            Ok(()) => {
                let rtt = t.elapsed().as_micros();
                if let Some(since) = outage_since.take() {
                    let ms = since.elapsed().as_millis();
                    s.longest_outage_ms = s.longest_outage_ms.max(ms);
                    emit(json!({ "event": "recovered", "outage_ms": ms, "t_ms": now_ms() }));
                }
                let (kind, path_rtt) = selected_path(&conn);
                match kind {
                    "relay" => s.relay += 1,
                    "ip" => s.ip += 1,
                    _ => {}
                }
                s.rtt_us.push(rtt);
                if seq <= 3 || seq.is_multiple_of(100) {
                    emit(
                        json!({ "event": "sample", "seq": seq, "rtt_us": rtt, "path": kind, "path_rtt_us": path_rtt }),
                    );
                }
            }
            Err(e) => {
                s.errors += 1;
                outage_since.get_or_insert_with(Instant::now);
                emit(
                    json!({ "event": "error", "seq": seq, "error": format!("{e}"), "t_ms": now_ms() }),
                );
                if !o.reconnect {
                    break;
                }
                // Drop the stale connection first; otherwise the new dial
                // waits behind it until QUIC's idle timeout declares it dead.
                conn.close(1u32.into(), b"reconnect");
                tokio::time::sleep(Duration::from_millis(200)).await;
                match connect(&ep, id).await {
                    Ok(c) => match c.open_bi().await {
                        Ok((sd, rv)) => {
                            conn = c;
                            send = sd;
                            recv = rv;
                            s.reconnects += 1;
                        }
                        Err(e) => emit(json!({ "event": "reopen_error", "error": e.to_string() })),
                    },
                    Err(e) => emit(json!({ "event": "reconnect_error", "error": format!("{e}") })),
                }
            }
        }
        tokio::time::sleep(o.interval).await;
    }

    let mut sorted = s.rtt_us.clone();
    sorted.sort_unstable();
    let mean = if sorted.is_empty() {
        0
    } else {
        sorted.iter().sum::<u128>() / sorted.len() as u128
    };
    emit(json!({
        "event": "summary",
        "mode": format!("{:?}", o.mode),
        "payload_bytes": o.payload.max(8),
        "ok": sorted.len(),
        "errors": s.errors,
        "reconnects": s.reconnects,
        "longest_outage_ms": s.longest_outage_ms,
        "samples_on_relay": s.relay,
        "samples_on_ip": s.ip,
        "rtt_us": {
            "min": sorted.first(),
            "p50": percentile(&sorted, 50.0),
            "p95": percentile(&sorted, 95.0),
            "p99": percentile(&sorted, 99.0),
            "max": sorted.last(),
            "mean": mean,
        },
        "elapsed_s": started.elapsed().as_secs_f64(),
    }));
    conn.close(0u32.into(), b"done");
    ep.close().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        super::super::rt::block_on(async {
            let (mut a, mut b) = tokio::io::duplex(4096);
            write_frame(&mut a, b"hello").await.unwrap();
            write_frame(&mut a, b"").await.unwrap();
            drop(a);
            assert_eq!(read_frame(&mut b).await.unwrap().unwrap(), b"hello");
            assert_eq!(read_frame(&mut b).await.unwrap().unwrap(), b"");
            assert!(read_frame(&mut b).await.unwrap().is_none());
        });
    }

    #[test]
    fn oversized_length_prefix_is_refused() {
        super::super::rt::block_on(async {
            let (mut a, mut b) = tokio::io::duplex(64);
            a.write_all(&u32::MAX.to_be_bytes()).await.unwrap();
            assert!(read_frame(&mut b).await.is_err());
        });
    }

    #[test]
    fn eof_inside_a_frame_is_an_error() {
        super::super::rt::block_on(async {
            let (mut a, mut b) = tokio::io::duplex(64);
            a.write_all(&10u32.to_be_bytes()).await.unwrap();
            a.write_all(b"abc").await.unwrap();
            drop(a);
            assert!(read_frame(&mut b).await.is_err());
        });
    }

    #[test]
    fn percentile_nearest_rank() {
        let v: Vec<u128> = (1..=100).collect();
        assert_eq!(percentile(&v, 50.0), Some(50));
        assert_eq!(percentile(&v, 95.0), Some(95));
        assert_eq!(percentile(&v, 100.0), Some(100));
        assert_eq!(percentile(&[], 50.0), None);
    }

    #[test]
    fn mode_parses() {
        assert_eq!(Mode::parse("relay-only").unwrap(), Mode::RelayOnly);
        assert!(Mode::parse("bogus").is_err());
    }
}
