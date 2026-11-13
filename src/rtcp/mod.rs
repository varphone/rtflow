//! RTCP compound packets, reception quality statistics and RTT estimation.
mod packet;
mod stats;

pub use packet::{CompoundPacket, NackEntry, Packet, ReportBlock, SdesChunk, SdesItem, XrBlock};
pub use stats::{
    FeedbackSnapshot, ReceptionSnapshot, ReceptionStats, RtcpInterval, RtcpSession, RttEstimator,
    ntp_middle_32,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rtp::{RtpHeader, RtpPacket};
    use bytes::Bytes;
    use std::time::Duration;
    use tokio::time::Instant;

    fn report_block() -> ReportBlock {
        ReportBlock {
            source_ssrc: 22,
            fraction_lost: 32,
            cumulative_lost: -2,
            extended_highest_sequence: 0x1_0002,
            interarrival_jitter: 45,
            last_sender_report: 66,
            delay_since_last_sender_report: 77,
        }
    }

    #[test]
    fn legacy_packets_round_trip() {
        let packets = [
            Packet::SenderReport {
                ssrc: 1,
                ntp: 2,
                rtp_timestamp: 3,
                packets: 4,
                octets: 5,
            },
            Packet::ReceiverReport { ssrc: 7 },
            Packet::Bye { ssrc: 8 },
        ];
        for packet in packets {
            assert_eq!(Packet::parse(packet.encode()).unwrap(), vec![packet]);
        }
    }

    #[test]
    fn reports_sdes_bye_and_unknown_round_trip() {
        let packets = vec![
            Packet::ReceiverReportWithReports {
                ssrc: 11,
                reports: vec![report_block()],
            },
            Packet::SourceDescription {
                chunks: vec![SdesChunk {
                    ssrc: 11,
                    items: vec![SdesItem {
                        kind: 1,
                        value: Bytes::from_static(b"device@example"),
                    }],
                }],
            },
            Packet::ByeSources {
                sources: vec![11, 22],
                reason: Some("shutdown".into()),
            },
            Packet::Unknown {
                packet_type: 208,
                count: 0,
                payload: Bytes::from_static(&[1, 2, 3, 4]),
            },
        ];
        let mut wire = Vec::new();
        for packet in &packets {
            wire.extend_from_slice(&packet.try_encode().unwrap());
        }
        assert_eq!(Packet::parse(Bytes::from(wire)).unwrap(), packets);
    }

    #[test]
    fn compound_requires_report_first() {
        assert!(CompoundPacket::new(vec![Packet::Bye { ssrc: 1 }]).is_err());
        let compound = CompoundPacket::new(vec![
            Packet::ReceiverReport { ssrc: 1 },
            Packet::SourceDescription {
                chunks: vec![SdesChunk {
                    ssrc: 1,
                    items: vec![SdesItem {
                        kind: 1,
                        value: Bytes::from_static(b"camera"),
                    }],
                }],
            },
        ])
        .unwrap();
        assert_eq!(
            CompoundPacket::parse(compound.try_encode().unwrap()).unwrap(),
            compound
        );
    }

    #[test]
    fn reception_reports_loss_wrap_jitter_and_sender_report_delay() {
        let start = Instant::now();
        let mut stats = ReceptionStats::new(9, 90_000, start).unwrap();
        for (sequence, timestamp, elapsed_ms) in
            [(65_534, 0, 0), (65_535, 3_000, 34), (1, 9_000, 101)]
        {
            stats
                .observe_rtp(
                    &RtpPacket {
                        header: RtpHeader {
                            marker: false,
                            payload_type: 96,
                            sequence,
                            timestamp,
                            ssrc: 9,
                        },
                        payload: Bytes::from_static(b"x"),
                    },
                    start + Duration::from_millis(elapsed_ms),
                )
                .unwrap();
        }
        let ntp = 0x1122_3344_5566_7788;
        stats.observe_sender_report(ntp, start + Duration::from_secs(1));
        let report = stats.report_block(start + Duration::from_millis(1_500));
        assert_eq!(report.extended_highest_sequence, 65_537);
        assert_eq!(report.cumulative_lost, 1);
        assert_eq!(report.last_sender_report, ntp_middle_32(ntp));
        assert!((32_767..=32_769).contains(&report.delay_since_last_sender_report));
        assert!(report.interarrival_jitter > 0);
    }

    #[test]
    fn rtt_and_scheduled_compound_report() {
        let start = Instant::now();
        let ntp = 0x0102_0304_0506_0708;
        let mut rtt = RttEstimator::new(4).unwrap();
        rtt.record_sender_report(ntp, start);
        let mut block = report_block();
        block.last_sender_report = ntp_middle_32(ntp);
        block.delay_since_last_sender_report = 16_384;
        let measured = rtt
            .observe_report(&block, start + Duration::from_millis(750))
            .unwrap();
        assert!((Duration::from_millis(499)..=Duration::from_millis(501)).contains(&measured));

        let mut session = RtcpSession::new(
            1,
            9,
            90_000,
            Bytes::from_static(b"gateway"),
            Duration::from_secs(1),
            start,
        )
        .unwrap();
        assert!(session.poll_report(start).unwrap().is_none());
        let compound = session
            .poll_report(start + Duration::from_secs(1))
            .unwrap()
            .unwrap();
        assert_eq!(compound.packets.len(), 2);
        assert!(compound.try_encode().is_ok());
    }

    #[test]
    fn malformed_known_packets_are_rejected() {
        assert!(Packet::parse(Bytes::from_static(&[0x80, 200, 0, 1, 0, 0, 0, 0])).is_err());
        assert!(Packet::parse(Bytes::from_static(&[0x80, 201, 0, 2, 0, 0, 0, 1])).is_err());
        assert!(Packet::parse(Bytes::from_static(&[0xa0, 203, 0, 1, 0, 0, 0, 0])).is_err());
    }
    #[test]
    fn app_feedback_and_xr_round_trip() {
        let app = Packet::App {
            subtype: 3,
            name: *b"TEST",
            data: Bytes::from_static(&[1, 2, 3, 4, 5, 6, 7, 8]),
        };
        let nack = Packet::nack(
            7,
            9,
            &[
                NackEntry {
                    packet_id: 100,
                    lost: 0b1010_0000_0000_0001,
                },
                NackEntry {
                    packet_id: 200,
                    lost: 0,
                },
            ],
        )
        .unwrap();
        let pli = Packet::pli(7, 9);
        let fir = Packet::fir(7, 9, 9, 4);
        let remb = Packet::remb(7, 9, 1_500_000, &[9]).unwrap();
        let xr = Packet::ExtendedReport {
            ssrc: 7,
            blocks: vec![
                XrBlock {
                    block_type: 4,
                    data: Bytes::from_static(&[0, 0, 0, 1, 0, 0, 0, 2]),
                },
                XrBlock {
                    block_type: 5,
                    data: Bytes::from_static(&[3, 0, 0, 0]),
                },
            ],
        };
        let packets = vec![app, nack.clone(), pli, fir, remb, xr];
        let mut wire = Vec::new();
        for packet in &packets {
            wire.extend_from_slice(&packet.try_encode().unwrap());
        }
        assert_eq!(Packet::parse(Bytes::from(wire)).unwrap(), packets);
        assert_eq!(nack.nack_entries().unwrap().len(), 2);
        assert_eq!(nack.nack_entries().unwrap()[0].packet_id, 100);
    }

    #[test]
    fn malformed_feedback_app_and_xr_are_rejected() {
        // RTPFB with a 2-byte body.
        assert!(Packet::parse(Bytes::from_static(&[0x81, 205, 0, 0, 1, 2])).is_err());
        // Truncated APP body (header claims 8 bytes, only 6 present).
        assert!(Packet::parse(Bytes::from_static(&[0x80, 204, 0, 1, b'T', b'E'])).is_err());
        // XR block length runs past the packet.
        assert!(
            Packet::parse(Bytes::from_static(&[
                0x80, 207, 0, 3, 0, 0, 0, 1, 4, 0, 0, 4
            ]))
            .is_err()
        );
    }
    #[test]
    fn rtcp_feedback_counters_and_remb_decode() {
        let now = Instant::now();
        let mut session = RtcpSession::new(
            7,
            9,
            90_000,
            Bytes::from_static(b"client"),
            Duration::from_secs(5),
            now,
        )
        .unwrap();
        session.on_rtcp(
            &Packet::nack(
                7,
                9,
                &[NackEntry {
                    packet_id: 10,
                    lost: 1,
                }],
            )
            .unwrap(),
            now,
        );
        session.on_rtcp(&Packet::pli(7, 9), now);
        session.on_rtcp(&Packet::fir(7, 9, 9, 1), now);
        let remb = Packet::remb(7, 9, 1_500_000, &[9]).unwrap();
        assert_eq!(remb.remb_bitrate(), Some(1_500_000));
        session.on_rtcp(&remb, now);
        session.on_rtcp(
            &Packet::App {
                subtype: 1,
                name: *b"TEST",
                data: Bytes::new(),
            },
            now,
        );
        let feedback = session.feedback();
        assert_eq!(feedback.nack_entries, 1);
        assert_eq!(feedback.pli, 1);
        assert_eq!(feedback.fir, 1);
        assert_eq!(feedback.remb, Some(1_500_000));
        assert_eq!(feedback.app, 1);
    }
    #[test]
    fn rtcp_interval_uses_membership_bandwidth_and_randomization() {
        let mut interval = RtcpInterval::new(50, 5, 1_000_000.0, 100.0)
            .unwrap()
            .with_seed(42);
        let first = interval.next_interval();
        assert!(first >= Duration::from_millis(1250));
        assert!(first <= Duration::from_secs(2));
        let second = interval.next_interval();
        assert!(second >= Duration::from_secs(5));
        assert!(second <= Duration::from_secs(15));
        interval.observe_packet_size(200);
        assert!(RtcpInterval::new(0, 0, 1.0, 1.0).is_err());
        assert!(RtcpInterval::new(2, 3, 1.0, 1.0).is_err());
    }

    #[test]
    fn session_uses_dynamic_interval_after_first_report() {
        let now = Instant::now();
        let mut session = RtcpSession::new(
            7,
            9,
            90_000,
            Bytes::from_static(b"client"),
            Duration::from_secs(60),
            now,
        )
        .unwrap()
        .with_interval(
            RtcpInterval::new(50, 5, 1_000_000.0, 100.0)
                .unwrap()
                .with_seed(7),
        );
        assert!(
            session
                .poll_report(now + Duration::from_secs(60))
                .unwrap()
                .is_some()
        );
        assert!(
            session
                .poll_report(now + Duration::from_secs(75))
                .unwrap()
                .is_some(),
            "dynamic interval should not wait the fixed 60s"
        );
    }

    #[test]
    fn bye_compound_starts_with_bye() {
        let now = Instant::now();
        let session = RtcpSession::new(
            7,
            9,
            90_000,
            Bytes::from_static(b"client"),
            Duration::from_secs(5),
            now,
        )
        .unwrap();
        let bye = session.bye(Some("shutdown")).unwrap();
        assert!(matches!(bye.packets[0], Packet::ReceiverReport { .. }));
        assert!(matches!(bye.packets[1], Packet::ByeSources { .. }));
        assert!(bye.try_encode().is_ok());
    }
    #[test]
    fn reduced_size_feedback_and_typed_xr() {
        let nack = Packet::nack(
            1,
            2,
            &[NackEntry {
                packet_id: 5,
                lost: 0,
            }],
        )
        .unwrap();
        let compound = CompoundPacket::new_reduced(vec![nack]).unwrap();
        let encoded = compound.try_encode().unwrap();
        assert_eq!(Packet::parse(encoded).unwrap().len(), 1);

        let rttr = XrBlock::rttr(0x0102_0304_0506_0708);
        assert_eq!(rttr.rttr_timestamp(), Some(0x0102_0304_0506_0708));
        let dlrr = XrBlock::dlrr(&[(10, 20)]);
        assert_eq!((dlrr.block_type, dlrr.data.len()), (5, 12));
        assert!(CompoundPacket::new_reduced(Vec::new()).is_err());
    }

    #[test]
    fn ssrc_collision_is_detected() {
        let now = Instant::now();
        let mut session = RtcpSession::new(
            7,
            9,
            90_000,
            Bytes::from_static(b"client"),
            Duration::from_secs(5),
            now,
        )
        .unwrap();
        assert!(!session.collision_detected());
        session.on_rtcp(&Packet::ReceiverReport { ssrc: 7 }, now);
        assert!(session.collision_detected());
    }
}
