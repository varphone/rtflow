//! RTCP compound packets, reception quality statistics and RTT estimation.
mod packet;
mod stats;

pub use packet::{CompoundPacket, Packet, ReportBlock, SdesChunk, SdesItem};
pub use stats::{ReceptionSnapshot, ReceptionStats, RtcpSession, RttEstimator, ntp_middle_32};

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
                packet_type: 204,
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
}
