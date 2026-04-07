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

pub use self::device::Net;
#[derive(Debug)]
pub enum Error {
    /// EventFd error.
    EventFd(io::Error),
}

pub type Result<T> = result::Result<T, Error>;

#[cfg(test)]
mod stress_tests {
    use super::device::{Net, VirtioNetBackend};
    use crate::legacy::DummyIrqChip;
    use crate::legacy::IrqChip;
    use crate::virtio::queue::tests::VirtQueue;
    use crate::virtio::{DeviceQueue, InterruptTransport, VirtioDevice};
    use std::os::unix::net::UnixDatagram;
    use std::sync::Arc;
    use utils::eventfd::{EventFd, EFD_NONBLOCK};
    use vm_memory::{GuestAddress, GuestMemoryMmap};

    const STRESS_CYCLES: usize = 80;

    /// Stress-tests the virtio-net activate/reset cycle to verify that
    /// the worker lifecycle fix (stop_fd + thread join + Net::reset)
    /// prevents the BadActivate panic under repeated VM create/delete.
    ///
    /// Before the fix, this would panic around cycle ~38 due to leaked
    /// worker threads and EventFd exhaustion.
    #[test]
    fn activate_reset_cycle_no_bad_activate() {
        let mac = [0x02, 0x00, 0x00, 0x00, 0x00, 0x01];

        for cycle in 0..STRESS_CYCLES {
            // Create a fresh socketpair each cycle — the worker takes
            // ownership of the fd via OwnedFd, so we need a new one.
            let (sock_a, _sock_b) = UnixDatagram::pair().unwrap();
            let backend_fd = {
                use std::os::fd::IntoRawFd;
                sock_a.into_raw_fd()
            };

            // Create a fresh Net device each cycle (simulates VM create).
            let mut net = Net::new(
                format!("stress-net-{cycle}"),
                VirtioNetBackend::UnixgramFd(backend_fd),
                mac,
                0,
            )
            .unwrap();

            // Set up minimal guest memory with two virtqueues (RX + TX).
            let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
            let vq_rx = VirtQueue::new(GuestAddress(0), &mem, 16);
            let vq_tx = VirtQueue::new(GuestAddress(0x4000), &mem, 16);

            let queue_rx = vq_rx.create_queue();
            let queue_tx = vq_tx.create_queue();

            let evt_rx = Arc::new(EventFd::new(EFD_NONBLOCK).unwrap());
            let evt_tx = Arc::new(EventFd::new(EFD_NONBLOCK).unwrap());

            let dq_rx = DeviceQueue::new(queue_rx, evt_rx);
            let dq_tx = DeviceQueue::new(queue_tx, evt_tx);

            let irqchip: IrqChip = DummyIrqChip::new().into();
            let interrupt =
                InterruptTransport::new(irqchip, format!("stress-{cycle}")).unwrap();

            // Activate — spawns worker thread.
            net.activate(mem, interrupt, vec![dq_rx, dq_tx])
                .unwrap_or_else(|e| {
                    panic!("BadActivate on cycle {cycle}: {e:?}");
                });

            assert!(
                net.is_activated(),
                "device should be activated on cycle {cycle}"
            );

            // Reset — signals worker to stop, joins thread.
            assert!(
                net.reset(),
                "reset should succeed on cycle {cycle}"
            );

            assert!(
                !net.is_activated(),
                "device should be inactive after reset on cycle {cycle}"
            );
        }
    }
}
