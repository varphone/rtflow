# RTCP support matrix

Scope: RFC 3550 RTCP plus RFC 4585/5104/5506 feedback in `src/rtcp/`.

## Packets

| Packet | Status | Notes |
|---|---|---|
| SR / SR+RR (PT 200) | ✅ | NTP, RTP timestamp, packet/octet counts, report blocks |
| RR / RR+SR (PT 201) | ✅ | fraction/cumulative loss, EHSN, jitter, LSR/DLSR |
| SDES (PT 202) | ✅ | arbitrary items/chunks, CNAME in reports |
| BYE (PT 203) | ✅ | multiple sources + reason |
| APP (PT 204) | ✅ | subtype, 4-byte name, padded data |
| RTPFB (PT 205) | ✅ | generic FCI; NACK (FMT 1) typed helpers |
| PSFB (PT 206) | ✅ | generic FCI; PLI (1), FIR (4), REMB (15) helpers |
| XR (PT 207) | ✅ | generic extended-report blocks (type + payload) |
| Unknown PT | ✅ | preserved as `Unknown` |

## Compound / validation

- Version 2, 32-bit alignment, padding, count/body length validation.
- Compound packets must start with SR/RR.
- Report blocks limited to 31 (5-bit count).
- SDES chunk/item lengths and BYE reason are bounds-checked.
- Feedback FCI and XR blocks must be 32-bit aligned and within the packet.

## Statistics / feedback

- Reception: packet/expected/lost, EHSN, interarrival jitter, LSR/DLSR, RTT.
- `RtcpSession` emits scheduled RR + SDES CNAME; `FeedbackSnapshot` counts
  NACK entries, PLI, FIR, APP and records the latest REMB bitrate. Server
  sender reports use `ServerConfig::rtcp_report_interval` (default 5 s).
- Publisher emits periodic SR (+SDES) over UDP/interleaved; SRTP uses SRTCP.
- Server receives publisher SR and emits periodic RR (+SDES) on UDP,
  `rtcp-mux`, and interleaved publisher sessions; `rtcp_publisher_sender_reports`
  exposes the received SR count.

## Optional / application-level follow-ups

The wire protocol is implemented; these are policy/application choices rather
than missing packet types:

1. `RtcpInterval` is opt-in (sessions default to a configured fixed interval).
2. SSRC collision is detected, but automatic SSRC switch/restart is left to
   the application.
3. PLI/FIR/NACK request a keyframe from a local source; UDP senders keep a
   512-packet protected RTP cache and retransmit NACK-requested packets
   (including SRTP, reusing the original index). Interleaved senders use a
   mount-level NACK feedback channel and the same cache/retransmission policy.
4. XR RRTR/DLRR have typed helpers; VoIP metrics blocks are exposed as opaque
   XR payloads.
5. Reduced-size RTCP is available via `CompoundPacket::new_reduced`; profile
   interoperability is not enforced.
