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
  NACK entries, PLI, FIR, APP and records the latest REMB bitrate.
- Publisher emits periodic SR (+SDES) over UDP/interleaved; SRTP uses SRTCP.

## Known gaps

1. `RtcpInterval` implements a bandwidth/membership estimate with randomized
   intervals and BYE construction; it is opt-in (the session default remains a
   fixed interval).
2. RTCP SSRC collision is detected (`RtcpSession::collision_detected`), but no
   automatic SSRC switch/restart is performed.
3. PLI/FIR/NACK are forwarded to local sources (`MountHandle::
   request_keyframe`); an external publisher or retransmission cache cannot
   currently act on NACK.
4. RTCP XR has typed RRTR/DLRR helpers; VoIP metrics blocks remain opaque.
5. Reduced-size RTCP is available via `CompoundPacket::new_reduced`; profile
   interoperability is not enforced.
