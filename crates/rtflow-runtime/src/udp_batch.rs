//! Linux `sendmmsg` batching with a portable fallback.
use bytes::Bytes;
use std::io;
use std::net::SocketAddr;

/// Send up to `packets.len()` UDP datagrams to one target.
///
/// Returns the number of datagrams accepted by the kernel. A partial send is
/// possible; callers must send the remainder. On non-Linux targets this is a
/// simple sequential fallback.
#[doc(hidden)]
pub async fn send_batch(
    socket: &tokio::net::UdpSocket,
    packets: &[Bytes],
    target: SocketAddr,
) -> io::Result<usize> {
    use std::os::fd::AsRawFd;

    if packets.is_empty() {
        return Ok(0);
    }
    #[cfg(target_os = "linux")]
    {
        use nix::sys::socket::{MsgFlags, MultiHeaders, SockaddrStorage, sendmmsg};
        use std::io::IoSlice;

        let mut headers = MultiHeaders::<SockaddrStorage>::preallocate(packets.len(), None);
        let slices = packets
            .iter()
            .map(|packet| [IoSlice::new(packet)])
            .collect::<Vec<_>>();
        let addresses = (0..packets.len())
            .map(|_| Some(SockaddrStorage::from(target)))
            .collect::<Vec<_>>();
        let sent = sendmmsg(
            socket.as_raw_fd(),
            &mut headers,
            slices.iter(),
            addresses,
            [],
            MsgFlags::empty(),
        )
        .map_err(|error| io::Error::from_raw_os_error(error as i32))?
        .count();
        Ok(sent)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let mut sent = 0;
        for packet in packets {
            socket.send_to(packet, target).await?;
            sent += 1;
        }
        Ok(sent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn batches_multiple_datagrams() {
        let receiver = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let sender = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let target = receiver.local_addr().unwrap();
        let packets = [Bytes::from_static(b"one"), Bytes::from_static(b"two")];
        let sent = send_batch(&sender, &packets, target).await.unwrap();
        assert_eq!(sent, 2);
        let mut received = Vec::new();
        for _ in 0..2 {
            let mut buffer = [0u8; 16];
            let (size, _) = tokio::time::timeout(
                std::time::Duration::from_secs(1),
                receiver.recv_from(&mut buffer),
            )
            .await
            .unwrap()
            .unwrap();
            received.push(buffer[..size].to_vec());
        }
        received.sort();
        assert_eq!(received, vec![b"one".to_vec(), b"two".to_vec()]);
    }
}
