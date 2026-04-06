// Copyright 2020 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::{io, mem, result};
use virtio_bindings::virtio_net::virtio_net_hdr_v1;

use super::QueueConfig;

pub const MAX_BUFFER_SIZE: usize = 65562;
const QUEUE_SIZE: u16 = 1024;
pub const NUM_QUEUES: usize = 2;
pub static QUEUE_CONFIG: [QueueConfig; NUM_QUEUES] = [QueueConfig::new(QUEUE_SIZE); NUM_QUEUES];

mod backend;
pub mod device;
#[cfg(target_os = "linux")]
mod tap;
mod unixgram;
mod unixstream;
mod worker;

// https://docs.oasis-open.org/virtio/virtio/v1.1/csprd01/virtio-v1.1-csprd01.html#x1-2050006
const VNET_HDR_LEN: usize = mem::size_of::<virtio_net_hdr_v1>();

// This initializes to all 0 the virtio_net_hdr part of a buf and return the length of the header
fn write_virtio_net_hdr(buf: &mut [u8]) -> usize {
    buf[0..VNET_HDR_LEN].fill(0);
    VNET_HDR_LEN
}

/// Virtio-net header field offsets (virtio_net_hdr_v1)
const VNET_FLAGS_OFFSET: usize = 0;
const VNET_CSUM_START_OFFSET: usize = 6;
const VNET_CSUM_OFFSET_OFFSET: usize = 8;

const VIRTIO_NET_HDR_F_NEEDS_CSUM: u8 = 1;

/// Compute the ones-complement checksum over a byte slice.
///
/// Sums all 16-bit big-endian words (with a trailing byte shifted left if odd length),
/// folds carries, and returns the ones-complement (bitwise NOT).
fn ones_complement_checksum(data: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    let mut i = 0;
    while i + 1 < data.len() {
        sum += u16::from_be_bytes([data[i], data[i + 1]]) as u32;
        i += 2;
    }
    if i < data.len() {
        sum += (data[i] as u32) << 8;
    }
    // Fold 32-bit sum to 16 bits
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

/// If the virtio-net header in `buf` has VIRTIO_NET_HDR_F_NEEDS_CSUM set,
/// compute and store the checksum in the frame, then clear the flag.
///
/// This must be called before the frame is sent to backends that strip the
/// virtio-net header (unix socket backends). TAP backends pass the header
/// to the kernel which handles checksum offload natively.
///
/// The guest stores a pseudo-header checksum at `csum_start + csum_offset`.
/// We compute the ones-complement sum over all bytes from `csum_start` to
/// end of frame (which includes the pseudo-header checksum), fold, complement,
/// and store the result — exactly matching Linux's `skb_checksum_help()`.
fn finalize_checksum(buf: &mut [u8]) {
    if buf.len() < VNET_HDR_LEN {
        return;
    }

    if buf[VNET_FLAGS_OFFSET] & VIRTIO_NET_HDR_F_NEEDS_CSUM == 0 {
        return;
    }

    let csum_start =
        u16::from_le_bytes([buf[VNET_CSUM_START_OFFSET], buf[VNET_CSUM_START_OFFSET + 1]])
            as usize;
    let csum_offset =
        u16::from_le_bytes([buf[VNET_CSUM_OFFSET_OFFSET], buf[VNET_CSUM_OFFSET_OFFSET + 1]])
            as usize;

    // Absolute positions within buf (past the virtio-net header)
    let data_start = VNET_HDR_LEN + csum_start;
    let csum_field = VNET_HDR_LEN + csum_start + csum_offset;

    if data_start >= buf.len() || csum_field + 2 > buf.len() {
        log::warn!(
            "finalize_checksum: invalid csum_start={csum_start} csum_offset={csum_offset} \
             buf_len={}",
            buf.len()
        );
        return;
    }

    let checksum = ones_complement_checksum(&buf[data_start..]);
    buf[csum_field..csum_field + 2].copy_from_slice(&checksum.to_be_bytes());

    // Clear the flag — checksum is now finalized
    buf[VNET_FLAGS_OFFSET] &= !VIRTIO_NET_HDR_F_NEEDS_CSUM;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ones_complement_checksum_basic() {
        // RFC 1071 example: the 16-bit words 0x0001, 0xf203, 0xf4f5, 0xf6f7
        // Sum = 0x0001 + 0xf203 + 0xf4f5 + 0xf6f7 = 0x2ddf0
        // Fold: 0xddf0 + 0x0002 = 0xddf2 (no further carry)
        // ~0xddf2 = 0x220d
        let data = [0x00, 0x01, 0xf2, 0x03, 0xf4, 0xf5, 0xf6, 0xf7];
        assert_eq!(ones_complement_checksum(&data), 0x220d);
    }

    #[test]
    fn ones_complement_checksum_odd_length() {
        // Single byte 0xFF → treated as 0xFF00
        // ~0xFF00 = 0x00FF
        let data = [0xFF];
        assert_eq!(ones_complement_checksum(&data), 0x00FF);
    }

    #[test]
    fn ones_complement_checksum_all_zeros() {
        let data = [0u8; 20];
        // Sum = 0, ~0 = 0xFFFF
        assert_eq!(ones_complement_checksum(&data), 0xFFFF);
    }

    #[test]
    fn finalize_checksum_computes_tcp_checksum() {
        // Build a minimal frame: [vnet_hdr (12 bytes)][eth (14 bytes)][IP (20 bytes)][TCP (20 bytes)]
        // Total: 12 + 14 + 20 + 20 = 66 bytes
        let mut buf = vec![0u8; 66];

        // Set NEEDS_CSUM flag
        buf[VNET_FLAGS_OFFSET] = VIRTIO_NET_HDR_F_NEEDS_CSUM;

        // csum_start = 34 (14 eth + 20 IP = offset of TCP header relative to frame start)
        buf[VNET_CSUM_START_OFFSET] = 34;
        buf[VNET_CSUM_START_OFFSET + 1] = 0;

        // csum_offset = 16 (TCP checksum field is at offset 16 within TCP header)
        buf[VNET_CSUM_OFFSET_OFFSET] = 16;
        buf[VNET_CSUM_OFFSET_OFFSET + 1] = 0;

        // Fill TCP header area with some known data (after vnet_hdr)
        // TCP starts at buf[VNET_HDR_LEN + 34] = buf[46]
        let tcp_start = VNET_HDR_LEN + 34;
        for i in 0..20 {
            buf[tcp_start + i] = (i as u8).wrapping_mul(17);
        }

        // Guest leaves pseudo-header checksum = 0 at the checksum field
        // (simulates the simplest case; in practice the guest stores
        //  the ones-complement sum of the pseudo-header here)
        let csum_field = tcp_start + 16;
        buf[csum_field] = 0;
        buf[csum_field + 1] = 0;

        finalize_checksum(&mut buf);

        // Flag should be cleared
        assert_eq!(
            buf[VNET_FLAGS_OFFSET] & VIRTIO_NET_HDR_F_NEEDS_CSUM,
            0,
            "NEEDS_CSUM flag should be cleared after finalization"
        );

        // Standard verification: when checksum field was 0 before computation,
        // re-summing all bytes (now including the stored checksum) should yield 0.
        // This is the textbook ones-complement checksum verification property.
        let verification = ones_complement_checksum(&buf[tcp_start..]);
        assert_eq!(
            verification, 0,
            "checksum verification: re-sum with stored checksum should yield 0"
        );
    }

    #[test]
    fn finalize_checksum_with_pseudo_header() {
        // Simulate a real TCP SYN: guest stores pseudo-header checksum at the
        // checksum field, we finalize it, then verify the end-to-end result.
        let mut buf = vec![0u8; 66];
        buf[VNET_FLAGS_OFFSET] = VIRTIO_NET_HDR_F_NEEDS_CSUM;
        buf[VNET_CSUM_START_OFFSET] = 34;
        buf[VNET_CSUM_OFFSET_OFFSET] = 16;

        let tcp_start = VNET_HDR_LEN + 34;
        // Fill TCP data
        for i in 0..20 {
            buf[tcp_start + i] = (i as u8).wrapping_mul(17);
        }

        // Simulate pseudo-header: src=10.0.2.1, dst=10.0.2.2, proto=6(TCP), len=20
        let pseudo_header: [u8; 12] = [
            10, 0, 2, 1, // src IP
            10, 0, 2, 2, // dst IP
            0, 6, // zero + protocol
            0, 20, // TCP length
        ];
        let pseudo_csum = !ones_complement_checksum(&pseudo_header); // raw sum (not complemented)

        // Store pseudo-header checksum at csum field
        let csum_field = tcp_start + 16;
        buf[csum_field..csum_field + 2].copy_from_slice(&pseudo_csum.to_be_bytes());

        finalize_checksum(&mut buf);

        // Verify: the receiver reconstructs pseudo-header and sums it with the segment.
        // Sum of (pseudo_header + TCP segment with final checksum) should fold to 0xFFFF.
        let mut verify_sum: u32 = 0;
        // Add pseudo-header contribution
        for i in (0..pseudo_header.len()).step_by(2) {
            verify_sum += u16::from_be_bytes([pseudo_header[i], pseudo_header[i + 1]]) as u32;
        }
        // Add TCP segment (which now contains the final checksum)
        for i in (0..20).step_by(2) {
            verify_sum +=
                u16::from_be_bytes([buf[tcp_start + i], buf[tcp_start + i + 1]]) as u32;
        }
        // Fold
        while verify_sum >> 16 != 0 {
            verify_sum = (verify_sum & 0xFFFF) + (verify_sum >> 16);
        }

        assert_eq!(
            verify_sum as u16, 0xFFFF,
            "receiver-side verification: pseudo_header + segment should sum to 0xFFFF"
        );
    }

    #[test]
    fn finalize_checksum_skips_when_no_flag() {
        let mut buf = vec![0u8; 66];
        // No NEEDS_CSUM flag set
        buf[VNET_FLAGS_OFFSET] = 0;
        let original = buf.clone();

        finalize_checksum(&mut buf);

        // Buffer unchanged
        assert_eq!(buf, original);
    }

    #[test]
    fn finalize_checksum_handles_short_buffer() {
        let mut buf = vec![0u8; 4]; // Too short for vnet header
        buf[0] = VIRTIO_NET_HDR_F_NEEDS_CSUM;

        finalize_checksum(&mut buf); // Should not panic
    }
}

pub use self::device::Net;
#[derive(Debug)]
pub enum Error {
    /// EventFd error.
    EventFd(io::Error),
}

pub type Result<T> = result::Result<T, Error>;
