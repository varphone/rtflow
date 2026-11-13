# RTSP 1.0 client support matrix

Scope: RFC 2326 client behaviour in `src/client/`. The server side lives in
`src/server/` and is tracked separately.

## Methods

| Method | Status | Notes |
|---|---|---|
| OPTIONS | ✅ | `RtspClient::options`; `Public`/`Supported`/`Server` in `ServerCapabilities` |
| DESCRIBE | ✅ | SDP parse, `Content-Base`/`Content-Location`, `a=rtcp-mux`, redirect (same scheme/host) |
| SETUP | ✅ | UDP, TCP interleaved, multicast, SRTP; one Session per aggregate URL; 461 fallback |
| PLAY | ✅ | `Range`, `Scale`, `Speed`; initial Range from `ClientOptions::play_range` |
| PAUSE | ✅ | optional `Range` |
| TEARDOWN | ✅ | waits for the response; Drop aborts tasks |
| GET_PARAMETER | ✅ | `ClientSession::get_parameter`, keep-alive |
| SET_PARAMETER | ✅ | `ClientSession::set_parameter` (text/parameters body) |
| ANNOUNCE / RECORD | ✅ | `RtspClient::publish` + `PublisherSession::push`; UDP and TCP interleaved, multi-track |
| REDIRECT | ⚠️ | 3xx `Location` on OPTIONS/DESCRIBE is followed; server-initiated `REDIRECT` requests are not handled |
| UNSUPPORTED | ⚠️ | parsed as a method but no client-side `Require` negotiation |

## Headers and behaviour

| Area | Status |
|---|---|
| CSeq / response pairing | ✅ exact match; stale interleaved responses skipped |
| Session / timeout | ✅ `Session`, `timeout=`, keep-alive capped at half timeout |
| Authentication | ✅ Basic/Digest for OPTIONS/DESCRIBE/SETUP and control; interleaved control retries Digest |
| Transport | ✅ UDP, TCP interleaved, multicast, SRTP/RTSPS; fallback on 461/IO; publishing UDP and interleaved |
| Range/RTP-Info | `Range` sent; `RTP-Info` `seq` primes the jitter buffer; `rtptime` anchors PTS when a non-zero NPT range is returned |
| Scale/Speed | ✅ sent on PLAY (`PlayRequest`) |
| `Require`/`Proxy-Require` | ❌ not emitted; 551 `Unsupported` surfaces as `RtspStatus` |
| Content-* | ✅ request/response lengths; `Content-Base` |
| 1xx/3xx/4xx/5xx | 3xx redirect (limited origin), 4xx/5xx typed `RtspStatus`, 401 retry |
| Pipelining | ❌ one outstanding control request per session |

## Concurrency, latency, memory

- Each `ClientSession` owns its sockets, control task, media tasks and
  cancellation token; no process-global media lock. Verified with 32
  concurrent TCP clients (`tests/client/concurrency.rs`).
- Media delivery is event-driven (jitter deadline, no polling); TCP uses
  `TCP_NODELAY`; output queues use `try_send` and drop on overflow.
- Defaults: 40 ms jitter, 128 packets, output queue 64 frames / 8 MiB,
  UDP receive buffer 4 MiB per track (override with `udp_recv_buffer`).
- `ClientOptions::low_memory()`: 30 ms jitter, 32 packets, queue 16 /
  2 MiB, UDP receive buffer 256 KiB. The output queue enforces both a frame
  count and a byte budget (`max_output_bytes`); the largest frame is capped
  by `Depacketizer` at 16 MiB.

## Known gaps / next increments

1. Publisher RTCP (SR/RR) and SRTP publishing.
2. Server-initiated `REDIRECT` request handling.
3. `Require`/`Proxy-Require` negotiation and `551 Unsupported` details.
4. Byte-bounded output queue (currently frame-count bounded; a small number of
   very large frames can still consume significant memory).
5. RTSP-over-HTTP tunnelling and RTSP 2.0 are explicitly out of scope.
