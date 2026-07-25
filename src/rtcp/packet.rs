use crate::{Error, Result};
use bytes::{Buf, Bytes, BytesMut};

const MAX_REPORT_BLOCKS: usize = 31;
const MIN_SIGNED_24: i32 = -8_388_608;
const MAX_SIGNED_24: i32 = 8_388_607;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReportBlock {
    pub source_ssrc: u32,
    pub fraction_lost: u8,
    pub cumulative_lost: i32,
    pub extended_highest_sequence: u32,
    pub interarrival_jitter: u32,
    pub last_sender_report: u32,
    pub delay_since_last_sender_report: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SdesItem {
    pub kind: u8,
    pub value: Bytes,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SdesChunk {
    pub ssrc: u32,
    pub items: Vec<SdesItem>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum Packet {
    SenderReport {
        ssrc: u32,
        ntp: u64,
        rtp_timestamp: u32,
        packets: u32,
        octets: u32,
    },
    SenderReportWithReports {
        ssrc: u32,
        ntp: u64,
        rtp_timestamp: u32,
        packets: u32,
        octets: u32,
        reports: Vec<ReportBlock>,
    },
    ReceiverReport {
        ssrc: u32,
    },
    ReceiverReportWithReports {
        ssrc: u32,
        reports: Vec<ReportBlock>,
    },
    SourceDescription {
        chunks: Vec<SdesChunk>,
    },
    Bye {
        ssrc: u32,
    },
    ByeSources {
        sources: Vec<u32>,
        reason: Option<String>,
    },
    Unknown {
        packet_type: u8,
        count: u8,
        payload: Bytes,
    },
}

impl Packet {
    pub fn encode(&self) -> Bytes {
        self.try_encode().unwrap_or_default()
    }

    pub fn try_encode(&self) -> Result<Bytes> {
        let (count, packet_type, body) = match self {
            Self::SenderReport {
                ssrc,
                ntp,
                rtp_timestamp,
                packets,
                octets,
            } => (
                0,
                200,
                sender_body(*ssrc, *ntp, *rtp_timestamp, *packets, *octets),
            ),
            Self::SenderReportWithReports {
                ssrc,
                ntp,
                rtp_timestamp,
                packets,
                octets,
                reports,
            } => {
                validate_count(reports.len(), "RTCP sender report blocks")?;
                let mut body = sender_body(*ssrc, *ntp, *rtp_timestamp, *packets, *octets);
                encode_reports(&mut body, reports)?;
                (reports.len() as u8, 200, body)
            }
            Self::ReceiverReport { ssrc } => (0, 201, BytesMut::from(&ssrc.to_be_bytes()[..])),
            Self::ReceiverReportWithReports { ssrc, reports } => {
                validate_count(reports.len(), "RTCP receiver report blocks")?;
                let mut body = BytesMut::from(&ssrc.to_be_bytes()[..]);
                encode_reports(&mut body, reports)?;
                (reports.len() as u8, 201, body)
            }
            Self::SourceDescription { chunks } => {
                validate_count(chunks.len(), "RTCP SDES chunks")?;
                (chunks.len() as u8, 202, encode_sdes(chunks)?)
            }
            Self::Bye { ssrc } => (1, 203, BytesMut::from(&ssrc.to_be_bytes()[..])),
            Self::ByeSources { sources, reason } => {
                validate_count(sources.len(), "RTCP BYE sources")?;
                if sources.is_empty() {
                    return Err(Error::Media("RTCP BYE has no sources".into()));
                }
                let mut body = BytesMut::with_capacity(sources.len() * 4 + 256);
                for source in sources {
                    body.extend_from_slice(&source.to_be_bytes());
                }
                if let Some(reason) = reason {
                    let reason = reason.as_bytes();
                    let length =
                        u8::try_from(reason.len()).map_err(|_| Error::Limit("RTCP BYE reason"))?;
                    body.extend_from_slice(&[length]);
                    body.extend_from_slice(reason);
                    pad_word(&mut body);
                }
                (sources.len() as u8, 203, body)
            }
            Self::Unknown {
                packet_type,
                count,
                payload,
            } => {
                if *count > 31 || payload.len() % 4 != 0 {
                    return Err(Error::Media("invalid unknown RTCP packet".into()));
                }
                (*count, *packet_type, BytesMut::from(payload.as_ref()))
            }
        };
        encode_packet(count, packet_type, body)
    }

    pub fn parse(mut data: Bytes) -> Result<Vec<Self>> {
        let mut packets = Vec::new();
        while !data.is_empty() {
            if data.len() < 4 {
                return Err(Error::Media("short RTCP header".into()));
            }
            let first = data[0];
            if first >> 6 != 2 {
                return Err(Error::Media("unsupported RTCP version".into()));
            }
            let padded = first & 0x20 != 0;
            let count = first & 0x1f;
            let packet_type = data[1];
            let length = (usize::from(u16::from_be_bytes([data[2], data[3]])) + 1)
                .checked_mul(4)
                .ok_or(Error::Limit("RTCP packet length"))?;
            if length < 4 || length > data.len() {
                return Err(Error::Media("truncated RTCP packet".into()));
            }
            let mut body = data.slice(4..length);
            data.advance(length);
            if padded {
                let padding = body
                    .last()
                    .copied()
                    .map(usize::from)
                    .ok_or_else(|| Error::Media("invalid RTCP padding".into()))?;
                if padding == 0 || padding > body.len() {
                    return Err(Error::Media("invalid RTCP padding".into()));
                }
                body = body.slice(..body.len() - padding);
            }
            packets.push(parse_packet(packet_type, count, body)?);
        }
        Ok(packets)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CompoundPacket {
    pub packets: Vec<Packet>,
}

impl CompoundPacket {
    pub fn new(packets: Vec<Packet>) -> Result<Self> {
        if packets.is_empty() {
            return Err(Error::Media("empty RTCP compound packet".into()));
        }
        if !matches!(
            packets.first(),
            Some(
                Packet::SenderReport { .. }
                    | Packet::SenderReportWithReports { .. }
                    | Packet::ReceiverReport { .. }
                    | Packet::ReceiverReportWithReports { .. }
            )
        ) {
            return Err(Error::Media(
                "RTCP compound packet must start with SR or RR".into(),
            ));
        }
        Ok(Self { packets })
    }

    pub fn try_encode(&self) -> Result<Bytes> {
        if self.packets.is_empty() {
            return Err(Error::Media("empty RTCP compound packet".into()));
        }
        let mut output = BytesMut::new();
        for packet in &self.packets {
            output.extend_from_slice(&packet.try_encode()?);
        }
        Ok(output.freeze())
    }

    pub fn parse(data: Bytes) -> Result<Self> {
        Self::new(Packet::parse(data)?)
    }
}

fn sender_body(ssrc: u32, ntp: u64, timestamp: u32, packets: u32, octets: u32) -> BytesMut {
    let mut body = BytesMut::with_capacity(24);
    body.extend_from_slice(&ssrc.to_be_bytes());
    body.extend_from_slice(&ntp.to_be_bytes());
    body.extend_from_slice(&timestamp.to_be_bytes());
    body.extend_from_slice(&packets.to_be_bytes());
    body.extend_from_slice(&octets.to_be_bytes());
    body
}

fn validate_count(count: usize, name: &'static str) -> Result<()> {
    if count > MAX_REPORT_BLOCKS {
        Err(Error::Limit(name))
    } else {
        Ok(())
    }
}

fn encode_packet(count: u8, packet_type: u8, body: BytesMut) -> Result<Bytes> {
    if !body.len().is_multiple_of(4) {
        return Err(Error::Media("RTCP body is not 32-bit aligned".into()));
    }
    let words = u16::try_from(body.len() / 4).map_err(|_| Error::Limit("RTCP packet length"))?;
    let mut output = BytesMut::with_capacity(body.len() + 4);
    output.extend_from_slice(&[0x80 | count, packet_type]);
    output.extend_from_slice(&words.to_be_bytes());
    output.extend_from_slice(&body);
    Ok(output.freeze())
}

fn encode_reports(output: &mut BytesMut, reports: &[ReportBlock]) -> Result<()> {
    for report in reports {
        if !(MIN_SIGNED_24..=MAX_SIGNED_24).contains(&report.cumulative_lost) {
            return Err(Error::Limit("RTCP cumulative packet loss"));
        }
        output.extend_from_slice(&report.source_ssrc.to_be_bytes());
        output.extend_from_slice(&[report.fraction_lost]);
        let lost = report.cumulative_lost as u32;
        output.extend_from_slice(&[(lost >> 16) as u8, (lost >> 8) as u8, lost as u8]);
        output.extend_from_slice(&report.extended_highest_sequence.to_be_bytes());
        output.extend_from_slice(&report.interarrival_jitter.to_be_bytes());
        output.extend_from_slice(&report.last_sender_report.to_be_bytes());
        output.extend_from_slice(&report.delay_since_last_sender_report.to_be_bytes());
    }
    Ok(())
}

fn parse_packet(packet_type: u8, count: u8, body: Bytes) -> Result<Packet> {
    match packet_type {
        200 => {
            let expected = 24 + usize::from(count) * 24;
            if body.len() != expected {
                return Err(Error::Media("invalid RTCP sender report length".into()));
            }
            let reports = parse_reports(&body[24..], count)?;
            let fields = (
                read_u32(&body, 0)?,
                read_u64(&body, 4)?,
                read_u32(&body, 12)?,
                read_u32(&body, 16)?,
                read_u32(&body, 20)?,
            );
            if reports.is_empty() {
                Ok(Packet::SenderReport {
                    ssrc: fields.0,
                    ntp: fields.1,
                    rtp_timestamp: fields.2,
                    packets: fields.3,
                    octets: fields.4,
                })
            } else {
                Ok(Packet::SenderReportWithReports {
                    ssrc: fields.0,
                    ntp: fields.1,
                    rtp_timestamp: fields.2,
                    packets: fields.3,
                    octets: fields.4,
                    reports,
                })
            }
        }
        201 => {
            let expected = 4 + usize::from(count) * 24;
            if body.len() != expected {
                return Err(Error::Media("invalid RTCP receiver report length".into()));
            }
            let ssrc = read_u32(&body, 0)?;
            let reports = parse_reports(&body[4..], count)?;
            if reports.is_empty() {
                Ok(Packet::ReceiverReport { ssrc })
            } else {
                Ok(Packet::ReceiverReportWithReports { ssrc, reports })
            }
        }
        202 => Ok(Packet::SourceDescription {
            chunks: parse_sdes(body, count)?,
        }),
        203 => parse_bye(body, count),
        _ => Ok(Packet::Unknown {
            packet_type,
            count,
            payload: body,
        }),
    }
}

fn parse_reports(data: &[u8], count: u8) -> Result<Vec<ReportBlock>> {
    if data.len() != usize::from(count) * 24 {
        return Err(Error::Media("invalid RTCP report blocks".into()));
    }
    let mut reports = Vec::with_capacity(usize::from(count));
    for block in data.chunks_exact(24) {
        let raw_lost =
            (u32::from(block[5]) << 16) | (u32::from(block[6]) << 8) | u32::from(block[7]);
        let cumulative_lost = if raw_lost & 0x80_0000 != 0 {
            (raw_lost | 0xff00_0000) as i32
        } else {
            raw_lost as i32
        };
        reports.push(ReportBlock {
            source_ssrc: read_u32(block, 0)?,
            fraction_lost: block[4],
            cumulative_lost,
            extended_highest_sequence: read_u32(block, 8)?,
            interarrival_jitter: read_u32(block, 12)?,
            last_sender_report: read_u32(block, 16)?,
            delay_since_last_sender_report: read_u32(block, 20)?,
        });
    }
    Ok(reports)
}

fn encode_sdes(chunks: &[SdesChunk]) -> Result<BytesMut> {
    let mut output = BytesMut::new();
    for chunk in chunks {
        output.extend_from_slice(&chunk.ssrc.to_be_bytes());
        for item in &chunk.items {
            if item.kind == 0 {
                return Err(Error::Media("RTCP SDES item type zero is reserved".into()));
            }
            let length =
                u8::try_from(item.value.len()).map_err(|_| Error::Limit("RTCP SDES item"))?;
            output.extend_from_slice(&[item.kind, length]);
            output.extend_from_slice(&item.value);
        }
        output.extend_from_slice(&[0]);
        pad_word(&mut output);
    }
    Ok(output)
}

fn parse_sdes(body: Bytes, count: u8) -> Result<Vec<SdesChunk>> {
    let mut position = 0usize;
    let mut chunks = Vec::with_capacity(usize::from(count));
    for _ in 0..count {
        let chunk_start = position;
        let ssrc = read_u32(&body, position)?;
        position += 4;
        let mut items = Vec::new();
        loop {
            let kind = *body
                .get(position)
                .ok_or_else(|| Error::Media("truncated RTCP SDES chunk".into()))?;
            position += 1;
            if kind == 0 {
                break;
            }
            let length = usize::from(
                *body
                    .get(position)
                    .ok_or_else(|| Error::Media("truncated RTCP SDES item".into()))?,
            );
            position += 1;
            let end = position
                .checked_add(length)
                .ok_or(Error::Limit("RTCP SDES item"))?;
            if end > body.len() {
                return Err(Error::Media("truncated RTCP SDES item".into()));
            }
            items.push(SdesItem {
                kind,
                value: body.slice(position..end),
            });
            position = end;
        }
        let chunk_length = position - chunk_start;
        position = position
            .checked_add((4 - chunk_length % 4) % 4)
            .ok_or(Error::Limit("RTCP SDES padding"))?;
        if position > body.len() {
            return Err(Error::Media("truncated RTCP SDES padding".into()));
        }
        chunks.push(SdesChunk { ssrc, items });
    }
    if position != body.len() {
        return Err(Error::Media("trailing RTCP SDES data".into()));
    }
    Ok(chunks)
}

fn parse_bye(body: Bytes, count: u8) -> Result<Packet> {
    if count == 0 {
        return Err(Error::Media("RTCP BYE has no sources".into()));
    }
    let source_bytes = usize::from(count) * 4;
    if body.len() < source_bytes {
        return Err(Error::Media("truncated RTCP BYE sources".into()));
    }
    let mut sources = Vec::with_capacity(usize::from(count));
    for offset in (0..source_bytes).step_by(4) {
        sources.push(read_u32(&body, offset)?);
    }
    let reason = if body.len() == source_bytes {
        None
    } else {
        let length = usize::from(body[source_bytes]);
        let start = source_bytes + 1;
        let end = start
            .checked_add(length)
            .ok_or(Error::Limit("RTCP BYE reason"))?;
        if end > body.len() {
            return Err(Error::Media("truncated RTCP BYE reason".into()));
        }
        Some(
            std::str::from_utf8(&body[start..end])
                .map_err(|_| Error::Media("RTCP BYE reason is not UTF-8".into()))?
                .to_owned(),
        )
    };
    if sources.len() == 1 && reason.is_none() {
        Ok(Packet::Bye { ssrc: sources[0] })
    } else {
        Ok(Packet::ByeSources { sources, reason })
    }
}

fn read_u32(data: &[u8], offset: usize) -> Result<u32> {
    let bytes = data
        .get(offset..offset + 4)
        .ok_or_else(|| Error::Media("truncated RTCP field".into()))?;
    Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn read_u64(data: &[u8], offset: usize) -> Result<u64> {
    let bytes = data
        .get(offset..offset + 8)
        .ok_or_else(|| Error::Media("truncated RTCP field".into()))?;
    Ok(u64::from_be_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ]))
}

fn pad_word(output: &mut BytesMut) {
    let padding = (4 - output.len() % 4) % 4;
    output.extend_from_slice(&[0; 3][..padding]);
}
