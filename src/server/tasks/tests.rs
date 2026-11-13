use super::*;
use std::time::Duration;

#[test]
fn sender_reports_are_compound_and_include_cname() {
    let wire = encode_sender_report(7, 8, 9, 10, 11).unwrap();
    let packets = RtcpPacket::parse(wire).unwrap();
    assert!(matches!(
        packets.first(),
        Some(RtcpPacket::SenderReport { .. })
    ));
    assert!(matches!(
        packets.get(1),
        Some(RtcpPacket::SourceDescription { .. })
    ));
}

#[test]
fn rtp_ntp_clock_advances_at_the_media_clock_rate() {
    let mut clock = RtpNtpClock {
        anchor_ntp: NTP_UNIX_EPOCH_OFFSET_SECS << 32,
        clock_rate: crate::media::clock::VIDEO_CLOCK_RATE,
        last_rtp: 90_000,
        extended_delta: 0,
    };
    assert_eq!(
        clock.ntp_for(180_000),
        (NTP_UNIX_EPOCH_OFFSET_SECS + 1) << 32
    );
    assert_eq!(
        clock.ntp_for(45_000),
        (NTP_UNIX_EPOCH_OFFSET_SECS << 32) - (1 << 31)
    );
}

#[test]
fn unix_epoch_maps_to_ntp_epoch_offset() {
    assert_eq!(
        system_time_to_ntp(std::time::UNIX_EPOCH),
        Some(NTP_UNIX_EPOCH_OFFSET_SECS << 32)
    );
    assert_eq!(
        system_time_to_ntp(std::time::UNIX_EPOCH + Duration::from_millis(500)),
        Some((NTP_UNIX_EPOCH_OFFSET_SECS << 32) | (1 << 31))
    );
}
