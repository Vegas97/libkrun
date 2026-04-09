use std::cmp;
use std::convert::TryInto;
use std::io::Write;

use utils::eventfd::EventFd;
use vm_memory::{ByteValued, GuestMemory, GuestMemoryMmap};

use super::super::{
    ActivateError, ActivateResult, BalloonError, DeviceQueue, DeviceState, QueueConfig,
    VirtioDevice,
};
use super::{defs, defs::uapi};
use crate::virtio::InterruptTransport;

// Inflate queue.
pub(crate) const IFQ_INDEX: usize = 0;
// Deflate queue.
pub(crate) const DFQ_INDEX: usize = 1;
// Stats queue.
pub(crate) const STQ_INDEX: usize = 2;
// Page-hinting queue.
pub(crate) const PHQ_INDEX: usize = 3;
// Free page reporting queue.
pub(crate) const FRQ_INDEX: usize = 4;

// Supported features.
pub(crate) const AVAIL_FEATURES: u64 = (1 << uapi::VIRTIO_F_VERSION_1 as u64)
    | (1 << uapi::VIRTIO_BALLOON_F_STATS_VQ as u64)
    | (1 << uapi::VIRTIO_BALLOON_F_FREE_PAGE_HINT as u64)
    | (1 << uapi::VIRTIO_BALLOON_F_REPORTING as u64);

#[derive(Copy, Clone, Debug, Default)]
#[repr(C, packed)]
pub struct VirtioBalloonConfig {
    /* Number of pages host wants Guest to give up. */
    num_pages: u32,
    /* Number of pages we've actually got in balloon. */
    actual: u32,
    /* Free page report command id, readonly by guest */
    free_page_report_cmd_id: u32,
    /* Stores PAGE_POISON if page poisoning is in use */
    poison_val: u32,
}

// Safe because it only has data and has no implicit padding.
unsafe impl ByteValued for VirtioBalloonConfig {}

pub struct Balloon {
    pub(crate) queues: Option<Vec<DeviceQueue>>,
    pub(crate) avail_features: u64,
    pub(crate) acked_features: u64,
    pub(crate) activate_evt: EventFd,
    pub(crate) device_state: DeviceState,
    config: VirtioBalloonConfig,
}

impl Balloon {
    pub fn new() -> super::Result<Balloon> {
        Ok(Balloon {
            queues: None,
            avail_features: AVAIL_FEATURES,
            acked_features: 0,
            activate_evt: EventFd::new(utils::eventfd::EFD_NONBLOCK)
                .map_err(BalloonError::EventFd)?,
            device_state: DeviceState::Inactive,
            config: VirtioBalloonConfig::default(),
        })
    }

    pub fn id(&self) -> &str {
        defs::BALLOON_DEV_ID
    }

    /// Set the balloon target size (number of 4KB pages the host wants the guest to give up).
    /// Signals a config change interrupt if the device is activated.
    pub fn set_num_pages(&mut self, num_pages: u32) {
        self.config.num_pages = num_pages;
        self.device_state.signal_config_change();
    }

    /// Get the current balloon target (num_pages).
    pub fn num_pages(&self) -> u32 {
        self.config.num_pages
    }

    /// Get the actual number of pages currently in the balloon (as reported by the guest).
    pub fn actual(&self) -> u32 {
        self.config.actual
    }

    pub fn process_frq(&mut self) -> bool {
        debug!("balloon: process_frq()");
        let mem = match self.device_state {
            DeviceState::Activated(ref mem, _) => mem,
            // This should never happen, it's been already validated in the event handler.
            DeviceState::Inactive => unreachable!(),
        };

        let queues = self
            .queues
            .as_mut()
            .expect("queues should exist when activated");
        let mut have_used = false;

        while let Some(head) = queues[FRQ_INDEX].queue.pop(mem) {
            let index = head.index;
            for desc in head.into_iter() {
                let host_addr = mem.get_host_address(desc.addr).unwrap();
                debug!(
                    "balloon: should release guest_addr={:?} host_addr={:p} len={}",
                    desc.addr, host_addr, desc.len
                );
                unsafe {
                    libc::madvise(
                        host_addr as *mut libc::c_void,
                        desc.len.try_into().unwrap(),
                        libc::MADV_DONTNEED,
                    )
                };
            }

            have_used = true;
            if let Err(e) = queues[FRQ_INDEX].queue.add_used(mem, index, 0) {
                error!("failed to add used elements to the queue: {e:?}");
            }
        }

        have_used
    }
}

impl VirtioDevice for Balloon {
    fn avail_features(&self) -> u64 {
        self.avail_features
    }

    fn acked_features(&self) -> u64 {
        self.acked_features
    }

    fn set_acked_features(&mut self, acked_features: u64) {
        self.acked_features = acked_features
    }

    fn device_type(&self) -> u32 {
        uapi::VIRTIO_ID_BALLOON
    }

    fn device_name(&self) -> &str {
        "balloon"
    }

    fn queue_config(&self) -> &[QueueConfig] {
        &defs::QUEUE_CONFIG
    }

    fn read_config(&self, offset: u64, mut data: &mut [u8]) {
        let config_slice = self.config.as_slice();
        let config_len = config_slice.len() as u64;
        if offset >= config_len {
            error!("Failed to read config space");
            return;
        }
        if let Some(end) = offset.checked_add(data.len() as u64) {
            // This write can't fail, offset and end are checked against config_len.
            data.write_all(&config_slice[offset as usize..cmp::min(end, config_len) as usize])
                .unwrap();
        }
    }

    fn write_config(&mut self, offset: u64, data: &[u8]) {
        // The guest driver writes the `actual` field at offset 4 to report
        // how many pages are currently in the balloon.
        let config_slice = self.config.as_mut_slice();
        let config_len = config_slice.len() as u64;
        if offset >= config_len {
            error!("Failed to write config space");
            return;
        }
        // Only allow writes to the `actual` field (offset 4, 4 bytes).
        let actual_offset = 4u64;
        let actual_end = 8u64;
        if offset >= actual_offset && offset + data.len() as u64 <= actual_end {
            let start = offset as usize;
            let end = start + data.len();
            config_slice[start..end].copy_from_slice(data);
        } else {
            warn!(
                "balloon: guest driver attempted to write non-actual config (offset={:x}, len={:x})",
                offset,
                data.len()
            );
        }
    }

    fn activate(
        &mut self,
        mem: GuestMemoryMmap,
        interrupt: InterruptTransport,
        queues: Vec<DeviceQueue>,
    ) -> ActivateResult {
        if queues.len() != defs::NUM_QUEUES {
            error!(
                "Cannot perform activate. Expected {} queue(s), got {}",
                defs::NUM_QUEUES,
                queues.len()
            );
            return Err(ActivateError::BadActivate);
        }

        if self.activate_evt.write(1).is_err() {
            error!("Cannot write to activate_evt",);
            return Err(ActivateError::BadActivate);
        }

        self.queues = Some(queues);
        self.device_state = DeviceState::Activated(mem, interrupt);

        Ok(())
    }

    fn is_activated(&self) -> bool {
        self.device_state.is_activated()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_balloon_new() {
        let balloon = Balloon::new().unwrap();
        assert_eq!(balloon.num_pages(), 0);
        assert_eq!(balloon.actual(), 0);
        assert!(!balloon.is_activated());
        assert_eq!(balloon.id(), defs::BALLOON_DEV_ID);
    }

    #[test]
    fn test_set_num_pages() {
        let mut balloon = Balloon::new().unwrap();
        // Before activation, set_num_pages should not panic (signal_config_change is a no-op).
        balloon.set_num_pages(1024);
        assert_eq!(balloon.num_pages(), 1024);

        balloon.set_num_pages(0);
        assert_eq!(balloon.num_pages(), 0);
    }

    #[test]
    fn test_read_config() {
        let mut balloon = Balloon::new().unwrap();
        balloon.set_num_pages(42);

        // Read num_pages from config offset 0.
        let mut buf = [0u8; 4];
        balloon.read_config(0, &mut buf);
        assert_eq!(u32::from_le_bytes(buf), 42);

        // Read actual from config offset 4 (should be 0).
        balloon.read_config(4, &mut buf);
        assert_eq!(u32::from_le_bytes(buf), 0);
    }

    #[test]
    fn test_write_config_actual() {
        let mut balloon = Balloon::new().unwrap();

        // Guest writes `actual` at offset 4.
        let actual_val: u32 = 100;
        balloon.write_config(4, &actual_val.to_le_bytes());
        assert_eq!(balloon.actual(), 100);

        // Write to offset 0 (num_pages) should be rejected.
        let before = balloon.num_pages();
        balloon.write_config(0, &999u32.to_le_bytes());
        assert_eq!(balloon.num_pages(), before);
    }

    #[test]
    fn test_read_config_out_of_bounds() {
        let balloon = Balloon::new().unwrap();
        let mut buf = [0u8; 4];
        // Reading past config should not panic.
        balloon.read_config(100, &mut buf);
    }

    #[test]
    fn test_device_type() {
        let balloon = Balloon::new().unwrap();
        assert_eq!(balloon.device_type(), uapi::VIRTIO_ID_BALLOON);
    }
}
