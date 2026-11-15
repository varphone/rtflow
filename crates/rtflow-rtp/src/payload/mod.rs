//! RFC 6184, RFC 7798, RFC 3640 and G.711 payload packetization.
//!
//! [`Packetizer`] fragments encoded access units into RTP packets;
//! [`Depacketizer`] reassembles them (per-codec logic in `depacketizer/`).
mod av1;
mod depacketizer;
mod packetizer;
#[cfg(test)]
mod tests;

pub use depacketizer::Depacketizer;
pub use packetizer::Packetizer;
