//! RTMP simple handshake (C0/C1/C2 + S0/S1/S2).
//!
//! Implements the non-encrypted "simple" handshake scheme.
//! The complex (digest) handshake used by Flash is not required
//! for FFmpeg/OBS interoperability.
use bytes::{BufMut, BytesMut};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Handshake packet size (C1/S1/C2/S2 are 1536 bytes each).
pub const HANDSHAKE_SIZE: usize = 1536;

/// RTMP protocol version byte in C0/S0.
pub const RTMP_VERSION: u8 = 3;

/// Perform the server-side handshake.
///
/// Sequence:
/// 1. Read C0 (1 byte) + C1 (1536 bytes)
/// 2. Send S0 (1 byte) + S1 (1536 bytes) + S2 (1536 bytes)
/// 3. Read C2 (1536 bytes)
pub async fn server_handshake<S>(stream: &mut S) -> std::io::Result<()>
where
    S: AsyncReadExt + AsyncWriteExt + Unpin,
{
    // Read C0
    let version = stream.read_u8().await?;
    if version != RTMP_VERSION {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("unsupported RTMP version: {version}"),
        ));
    }

    // Read C1
    let mut c1 = vec![0u8; HANDSHAKE_SIZE];
    stream.read_exact(&mut c1).await?;

    // Build S0 + S1 + S2
    let mut response = BytesMut::with_capacity(1 + HANDSHAKE_SIZE * 2);

    // S0: version
    response.put_u8(RTMP_VERSION);

    // S1: time (4 bytes) + zero (4 bytes) + random (1528 bytes)
    let s1_time = 0u32; // server uptime, can be 0
    response.put_u32(s1_time);
    response.put_u32(0); // zero field
    // Fill with pseudo-random data
    let mut s1_random = vec![0u8; HANDSHAKE_SIZE - 8];
    fill_pseudo_random(&mut s1_random);
    response.put_slice(&s1_random);

    // S2: echo of C1 (time from C1 + our time + random echo)
    // Simple handshake: S2 = C1 with time2 set
    let c1_time = u32::from_be_bytes([c1[0], c1[1], c1[2], c1[3]]);
    response.put_u32(c1_time); // echo client time
    response.put_u32(s1_time); // our time
    response.put_slice(&c1[8..]); // echo random data

    stream.write_all(&response).await?;
    stream.flush().await?;

    // Read C2
    let mut c2 = vec![0u8; HANDSHAKE_SIZE];
    stream.read_exact(&mut c2).await?;

    // C2 validation is lenient: we don't strictly verify the echo
    Ok(())
}

/// Perform the client-side handshake (for RTMP client connections).
pub async fn client_handshake<S>(stream: &mut S) -> std::io::Result<()>
where
    S: AsyncReadExt + AsyncWriteExt + Unpin,
{
    // Send C0 + C1
    let mut c0c1 = BytesMut::with_capacity(1 + HANDSHAKE_SIZE);
    c0c1.put_u8(RTMP_VERSION);
    c0c1.put_u32(0); // time
    c0c1.put_u32(0); // zero
    let mut random = vec![0u8; HANDSHAKE_SIZE - 8];
    fill_pseudo_random(&mut random);
    c0c1.put_slice(&random);
    stream.write_all(&c0c1).await?;
    stream.flush().await?;

    // Read S0
    let version = stream.read_u8().await?;
    if version != RTMP_VERSION {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("unsupported RTMP version: {version}"),
        ));
    }

    // Read S1
    let mut s1 = vec![0u8; HANDSHAKE_SIZE];
    stream.read_exact(&mut s1).await?;

    // Read S2
    let mut s2 = vec![0u8; HANDSHAKE_SIZE];
    stream.read_exact(&mut s2).await?;

    // Send C2: echo of S1
    let mut c2 = BytesMut::with_capacity(HANDSHAKE_SIZE);
    let s1_time = u32::from_be_bytes([s1[0], s1[1], s1[2], s1[3]]);
    c2.put_u32(s1_time);
    c2.put_u32(0); // our time
    c2.put_slice(&s1[8..]);
    stream.write_all(&c2).await?;
    stream.flush().await?;

    Ok(())
}

fn fill_pseudo_random(buf: &mut [u8]) {
    // Simple deterministic "random" for reproducibility in tests.
    // In production this could use getrandom, but RTMP handshake
    // random data doesn't need cryptographic quality.
    let mut state: u32 = 0xDEAD_BEEF;
    for byte in buf.iter_mut() {
        state = state.wrapping_mul(1664525).wrapping_add(1013904223);
        *byte = (state >> 16) as u8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    #[tokio::test]
    async fn handshake_roundtrip() {
        let (mut client, mut server) = duplex(8192);

        let server_task = tokio::spawn(async move {
            server_handshake(&mut server).await
        });

        let client_task = tokio::spawn(async move {
            client_handshake(&mut client).await
        });

        let (server_result, client_result) = tokio::join!(server_task, client_task);
        server_result.unwrap().unwrap();
        client_result.unwrap().unwrap();
    }
}
