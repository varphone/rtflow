//! Steady-state allocation budget for the RTP packetization path.

use bytes::Bytes;
use rtflow::media::{AccessUnit, Codec};
use rtflow::rtp::Packetizer;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

struct CountingAllocator;
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

fn video_frame(size: usize) -> AccessUnit {
    let mut data = vec![0u8; size];
    data[..4].copy_from_slice(&[0, 0, 0, 1]);
    data[4] = 0x65;
    AccessUnit::video(0, Codec::H264, Duration::ZERO, 0, true, Bytes::from(data))
}

#[test]
fn packetization_allocations_are_bounded_per_packet() {
    let mut packetizer = Packetizer::new(Codec::H264, 96, 1, 1200).unwrap();
    let frame = video_frame(20_000);
    let mut wire = bytes::BytesMut::new();
    // Warm up lazily allocated buffers/dictionaries.
    for _ in 0..5 {
        for mut packet in packetizer.packetize(&frame).unwrap() {
            packet.header.sequence = 1;
            packet.encode_into(&mut wire);
        }
    }
    let before = ALLOCATIONS.load(Ordering::Relaxed);
    let mut packets = 0usize;
    for _ in 0..50 {
        for mut packet in packetizer.packetize(&frame).unwrap() {
            packet.header.sequence = 1;
            packet.encode_into(&mut wire);
            packets += 1;
        }
    }
    let allocations = ALLOCATIONS.load(Ordering::Relaxed) - before;
    let per_packet = allocations as f64 / packets as f64;
    eprintln!("packetizer: {packets} packets, {allocations} allocations, {per_packet:.2}/packet");
    assert!(
        per_packet <= 1.0,
        "packetization allocates {per_packet:.2} per RTP packet"
    );
}
