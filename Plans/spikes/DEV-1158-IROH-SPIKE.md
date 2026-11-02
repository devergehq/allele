# DEV-1158 — iroh spike results (throwaway branch, do not merge)

Measured 2026-11-02 on one Mac (M4 Max, 16 cores, macOS 26 / Darwin 25.5, rustc 1.93.1),
home Wi-Fi in Australia. Two endpoints = two `Allele --iroh-spike` processes on the same host.
Base: `origin/master` @ 3060e21. iroh **=1.4.1** (latest on crates.io on 2026-11-02).

## How to reproduce

```sh
cargo build --release
B=target/release/Allele
$B --iroh-spike listen [--mode default|relay-only|direct-only] --key-file L.key --id-file L.id &
$B --iroh-spike dial "$(cat L.id)" [--mode ...] --count 500 --interval-ms 20 [--reconnect --echo-timeout-ms 2000]
kill -USR1 <pid>   # either side calls Endpoint::network_change()
```

The dialer connects by `EndpointId` only (n0 DNS/pkarr address lookup), sends length-prefixed
frames (u32 BE + payload, 64 B default) on one bi-stream, the listener echoes, the dialer times
each round trip and reads the selected path from `Connection::paths()`.

## Runtime (same host — direct numbers are loopback-class, not internet)

| Run | Connect | Selected path | RTT p50 | p95 | p99 | max | Errors |
|---|---|---|---|---|---|---|---|
| default, 500 msgs | 718 ms (starts on relay) | ip after ~480 ms | 0.57 ms | 1.6 ms | 4.4 ms | 479 ms (msg 1, pre-holepunch) | 0 |
| default, 300 msgs (final binary) | 739 ms | ip | 0.72 ms | 1.1 ms | 1.3 ms | 519 ms (msg 1) | 0 |
| relay-only, 200 msgs | 721 ms | relay usw1 | 393 ms | 477 ms | 489 ms | 525 ms | 0 |
| relay-only, 50 msgs (final binary) | 751 ms | relay usw1 | 503 ms | 522 ms | 744 ms | 744 ms | 0 |
| direct-only dialer → default listener | — | — | — | — | — | — | **timed out** |

Relay distance from this network (TCP connect = 1 RTT, 3 samples, s):
usw1 0.18–0.27 · use1 0.22–0.30 · aps1 0.25–0.32 · euc1 0.35–0.43. iroh's net report picked
usw1. A relayed round trip is ≈ 2 × client↔relay RTT, so ~400 ms is the floor with n0 relays
from Australia.

## Network change / restart

| Scenario | Result |
|---|---|
| `network_change()` on both sides, direct path (×2) | 0 errors, no outage |
| `network_change()` on both sides, relay-only | 0 errors, no outage |
| Listener restart, same key (same EndpointId, new port), naive reconnect | **33.9 s outage** — new dial waited behind the dead connection until QUIC idle timeout |
| Same, but `conn.close()` the stale connection before redialing, 2 s echo timeout | **4.1 s outage** (≈3.5 s is the restarted peer's own bind→online) |

Not tested here (needs hardware/human): real Wi-Fi↔hotspot switch, sleep/wake, 24 h soak.

## Build cost (`cargo build --release`, separate fresh target dirs, sequential)

| | master | spike | delta |
|---|---|---|---|
| Clean build | 60.0 s | 87.5 s | **+27.5 s (+46%)** |
| Incremental (touch `src/sync/rt.rs`), 3 runs | 11.5 / 11.7 / 11.8 s | 12.9 / 12.5 / 12.6 s | **+0.9 s (+8%)** |

(An earlier master clean build measured 75.0 s; cold vs warm disk cache. Treat ±15 s as noise.)

## Binary size (`target/release/Allele`)

| | master | spike (iroh defaults) | delta | spike (no metrics/portmapper) | delta |
|---|---|---|---|---|---|
| raw | 26,602,832 | 40,288,992 | **+13.7 MB (+51%)** | 37,413,920 | +10.8 MB |
| stripped | 23,026,192 | 34,002,720 | +11.0 MB (+48%) | — | — |
| zipped (ditto) | 8,460,911 | 13,950,427 | **+5.5 MB (+65%)** | 12,860,207 | +4.4 MB |

## Dependencies (`cargo tree -d -e normal`)

| Crate | master | spike | Note |
|---|---|---|---|
| quinn | — | — | iroh 1.x uses n0's fork **noq 1.4.0** (single version) |
| tokio | 1.52.1 | 1.52.1 | single — iroh runs on the existing `sync::rt` runtime |
| hyper | 1.10.1 | 1.10.1 | single |
| reqwest | 0.12.28 | **0.12.28 + 0.13.5** | rust-s3 on 0.12, iroh on 0.13 |
| TLS | native-tls 0.2.18 (Security.framework) | native-tls **+ rustls 0.23.46 + ring 0.17.14** | two TLS stacks |
| rand | 0.8, 0.9 | 0.8, 0.9, **0.10** | |
| crypto | — | digest/sha2 0.10+0.11, curve25519-dalek 4+5, der 0.7+0.8 | age (0.10-era) vs iroh (0.11-era) |

Crate names with >1 version: 25 → 42. Resolution succeeded first try — no version conflict with
GPUI, rust-s3 or age; the cost is duplication, not breakage.

## API used (iroh 1.4.1)

`Endpoint::builder(presets::N0)` · `presets::N0DisableRelay` · `Builder::clear_ip_transports()`
(relay-only) · `.secret_key(SecretKey)` · `SecretKey::generate()/from_bytes/to_bytes` ·
`.alpns(vec![ALPN])` · `.bind().await` · `Endpoint::online()` · `id() -> EndpointId` ·
`addr() -> EndpointAddr` · `EndpointAddr::new(id)` · `connect(addr, ALPN)` · `accept()` ·
`Connection::{open_bi, accept_bi, remote_id, paths, path_events, close, close_reason}` ·
`Path::{is_selected, is_relay, is_ip, rtt}` · `Endpoint::network_change()` · `RelayMode`
(via presets) · `Endpoint::close()`.
