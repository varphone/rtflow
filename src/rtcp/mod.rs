//! Minimal RTCP compound packet support (SR, RR and BYE).
use crate::{Error, Result};
use bytes::{Buf, Bytes, BytesMut};
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Packet {
    SenderReport {
        ssrc: u32,
        ntp: u64,
        rtp_timestamp: u32,
        packets: u32,
        octets: u32,
    },
    ReceiverReport {
        ssrc: u32,
    },
    Bye {
        ssrc: u32,
    },
}
impl Packet {
    pub fn encode(&self) -> Bytes {
        let (first, pt, body) = match self {
            Self::SenderReport {
                ssrc,
                ntp,
                rtp_timestamp,
                packets,
                octets,
            } => {
                let mut b = BytesMut::with_capacity(24);
                b.extend_from_slice(&ssrc.to_be_bytes());
                b.extend_from_slice(&ntp.to_be_bytes());
                b.extend_from_slice(&rtp_timestamp.to_be_bytes());
                b.extend_from_slice(&packets.to_be_bytes());
                b.extend_from_slice(&octets.to_be_bytes());
                (0x80, 200, b)
            }
            Self::ReceiverReport { ssrc } => (0x80, 201, BytesMut::from(&ssrc.to_be_bytes()[..])),
            Self::Bye { ssrc } => (0x81, 203, BytesMut::from(&ssrc.to_be_bytes()[..])),
        };
        let words = (body.len() / 4) as u16;
        let mut out = BytesMut::with_capacity(body.len() + 4);
        out.extend_from_slice(&[first, pt]);
        out.extend_from_slice(&words.to_be_bytes());
        out.extend_from_slice(&body);
        out.freeze()
    }
    pub fn parse(mut data: Bytes) -> Result<Vec<Self>> {
        let mut out = Vec::new();
        while !data.is_empty() {
            if data.len() < 4 {
                return Err(Error::Media("short RTCP header".into()));
            }
            let v = data[0] >> 6;
            if v != 2 {
                return Err(Error::Media("unsupported RTCP version".into()));
            }
            let count = data[0] & 0x1f;
            let pt = data[1];
            let len = (u16::from_be_bytes([data[2], data[3]]) as usize + 1) * 4;
            if len > data.len() {
                return Err(Error::Media("truncated RTCP packet".into()));
            }
            let body = data.slice(4..len);
            data.advance(len);
            match pt {
                200 if body.len() >= 24 => out.push(Self::SenderReport {
                    ssrc: u32::from_be_bytes(body[0..4].try_into().unwrap()),
                    ntp: u64::from_be_bytes(body[4..12].try_into().unwrap()),
                    rtp_timestamp: u32::from_be_bytes(body[12..16].try_into().unwrap()),
                    packets: u32::from_be_bytes(body[16..20].try_into().unwrap()),
                    octets: u32::from_be_bytes(body[20..24].try_into().unwrap()),
                }),
                201 if body.len() >= 4 => out.push(Self::ReceiverReport {
                    ssrc: u32::from_be_bytes(body[0..4].try_into().unwrap()),
                }),
                203 if count == 1 && body.len() >= 4 => out.push(Self::Bye {
                    ssrc: u32::from_be_bytes(body[0..4].try_into().unwrap()),
                }),
                _ => {}
            }
        }
        Ok(out)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sr_round_trip() {
        let p = Packet::SenderReport {
            ssrc: 1,
            ntp: 2,
            rtp_timestamp: 3,
            packets: 4,
            octets: 5,
        };
        assert_eq!(Packet::parse(p.encode()).unwrap(), vec![p])
    }
    #[test]
    fn rr_and_bye_round_trip() {
        for packet in [Packet::ReceiverReport { ssrc: 7 }, Packet::Bye { ssrc: 8 }] {
            assert_eq!(Packet::parse(packet.encode()).unwrap(), vec![packet]);
        }
    }
}
