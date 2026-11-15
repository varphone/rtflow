use rtflow_core::error::Result;
use rtflow_rtsp::{self as rtsp, InterleavedFrame, Message};
use bytes::BytesMut;
#[derive(Clone, Debug)]
pub enum InboundFrame {
    Rtsp(Message),
    Interleaved(InterleavedFrame),
}
#[derive(Debug)]
pub struct ConnectionFramer {
    buffer: BytesMut,
    max_header: usize,
    max_body: usize,
    max_interleaved: usize,
}
impl ConnectionFramer {
    pub fn new(max_header: usize, max_body: usize, max_interleaved: usize) -> Self {
        Self {
            buffer: BytesMut::with_capacity(4096),
            max_header,
            max_body,
            max_interleaved,
        }
    }
    pub fn push(&mut self, data: &[u8]) {
        self.buffer.extend_from_slice(data)
    }
    pub fn next_frame(&mut self) -> Result<Option<InboundFrame>> {
        if self.buffer.first() == Some(&b'$') {
            return InterleavedFrame::parse(&mut self.buffer, self.max_interleaved)
                .map(|value| value.map(InboundFrame::Interleaved));
        }
        rtsp::parse(&mut self.buffer, self.max_header, self.max_body)
            .map(|value| value.map(InboundFrame::Rtsp))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mixed_frames() {
        let mut f = ConnectionFramer::new(1024, 1024, 1024);
        f.push(b"RTSP/1.0 200 OK\r\nCSeq: 1\r\n\r\n$\x00\x00\x01x");
        assert!(matches!(
            f.next_frame().unwrap(),
            Some(InboundFrame::Rtsp(_))
        ));
        assert!(matches!(
            f.next_frame().unwrap(),
            Some(InboundFrame::Interleaved(_))
        ));
    }
}
