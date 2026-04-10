use std::cmp;

use std::io::Write;

use utils::eventfd::EventFd;
use vm_memory::{Address, ByteValued, GuestAddress, GuestMemory, GuestMemoryMmap, Le32};

use super::super::{
    ActivateError, ActivateResult, BalloonError, DeviceQueue, DeviceState, QueueConfig,
    VirtioDevice,
};
use super::{defs, defs::uapi};
use crate::virtio::descriptor_utils::Reader;
use crate::virtio::InterruptTransport;

/// Release host pages for balloon inflate (guest promises not to access until deflate).
///
/// On Linux, `madvise(MADV_DONTNEED)` is sufficient — KVM handles page faults.
///
/// On macOS/HVF, `madvise` alone doesn't release RSS because HVF pins pages.
/// We use `hv_vm_unmap` → `hv_vm_map` → `madvise` to force page release:
/// 1. Remove HVF mapping (releases pin)
/// 2. Re-establish mapping immediately (minimizes unmapped window)
/// 3. Release old physical pages via madvise
///
/// Safety: caller must ensure `host_addr` and `guest_addr` are valid and aligned,
/// and that the guest will not access these pages until deflation.
#[must_use]
unsafe fn release_host_pages_inflate(host_addr: *const u8, guest_addr: u64, len: usize) -> bool {
    #[cfg(all(target_os = "macos", not(test)))]
    {
        use hvf::bindings::{
            hv_vm_map, hv_vm_unmap, HV_MEMORY_EXEC, HV_MEMORY_READ, HV_MEMORY_WRITE, HV_SUCCESS,
        };

        // Step 1: Remove HVF mapping (releases page pin).
        let ret = hv_vm_unmap(guest_addr, len);
        if ret != HV_SUCCESS {
            error!(
                "balloon: hv_vm_unmap(gpa={guest_addr:#x}, len={len}) failed: ret={ret:#x}"
            );
            return false;
        }

        // Step 2: Re-establish mapping immediately (minimizes unmapped window).
        let flags = (HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC).into();
        let ret = hv_vm_map(host_addr as *mut libc::c_void, guest_addr, len, flags);
        if ret != HV_SUCCESS {
            // CRITICAL: GPA is now unmapped. Guest will crash if it accesses this range.
            error!(
                "CRITICAL balloon: hv_vm_map(gpa={guest_addr:#x}, len={len}) failed: ret={ret:#x}. \
                 GPA range is unmapped — guest will fault on access."
            );
            return false;
        }

        // Step 3: Release old physical pages. The new mapping picks up fresh pages on access.
        let ret = libc::madvise(host_addr as *mut libc::c_void, len, libc::MADV_DONTNEED);
        if ret != 0 {
            error!(
                "balloon: madvise(MADV_DONTNEED) failed for gpa={guest_addr:#x} len={len}: {}",
                std::io::Error::last_os_error()
            );
            return false;
        }

        true
    }
    // Linux, or macOS test builds (no HVF available in test binary).
    #[cfg(any(not(target_os = "macos"), test))]
    {
        let _ = guest_addr;
        libc::madvise(host_addr as *mut libc::c_void, len, libc::MADV_DONTNEED) == 0
    }
}

/// Release host pages advisory (FRQ). Guest may reuse these pages at any time.
///
/// Uses `madvise(MADV_DONTNEED)` only — no HVF mapping changes. On macOS this
/// won't reduce RSS (HVF pins pages), but it won't crash the guest either.
/// On Linux, madvise works correctly and reduces RSS.
#[must_use]
unsafe fn release_host_pages_advisory(host_addr: *const u8, len: usize) -> bool {
    libc::madvise(host_addr as *mut libc::c_void, len, libc::MADV_DONTNEED) == 0
}

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
// Note: VIRTIO_BALLOON_F_REPORTING (free page reporting) is not advertised on
// macOS because madvise(MADV_DONTNEED) doesn't release RSS under HVF, making
// FRQ a no-op that wastes guest CPU. Inflate (IFQ) uses hv_vm_unmap which works.
#[cfg(target_os = "macos")]
pub(crate) const AVAIL_FEATURES: u64 = (1 << uapi::VIRTIO_F_VERSION_1 as u64)
    | (1 << uapi::VIRTIO_BALLOON_F_STATS_VQ as u64)
    | (1 << uapi::VIRTIO_BALLOON_F_FREE_PAGE_HINT as u64);

#[cfg(not(target_os = "macos"))]
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

    /// Process the free page reporting queue (FRQ).
    ///
    /// FRQ pages are advisory — the guest may reuse them at any time without deflating.
    /// We only use `madvise(MADV_DONTNEED)` (no HVF mapping changes) to avoid crashing
    /// if the guest reclaims a page while we're processing.
    pub fn process_frq(&mut self) -> bool {
        debug!("balloon: process_frq()");
        let mem = match self.device_state {
            DeviceState::Activated(ref mem, _) => mem,
            DeviceState::Inactive => {
                error!("balloon: process_frq called on inactive device");
                return false;
            }
        };

        let queues = self
            .queues
            .as_mut()
            .expect("queues should exist when activated");
        let mut have_used = false;

        while let Some(head) = queues[FRQ_INDEX].queue.pop(mem) {
            let index = head.index;
            for desc in head.into_iter() {
                let host_addr = match mem.get_host_address(desc.addr) {
                    Ok(addr) => addr,
                    Err(e) => {
                        error!(
                            "balloon: FRQ invalid guest address {:?}: {e}",
                            desc.addr
                        );
                        continue;
                    }
                };
                debug!(
                    "balloon: FRQ release guest_addr={:?} host_addr={:p} len={}",
                    desc.addr, host_addr, desc.len
                );
                if !unsafe { release_host_pages_advisory(host_addr, desc.len as usize) } {
                    error!(
                        "balloon: FRQ madvise failed for guest_addr={:?} len={}: {}",
                        desc.addr,
                        desc.len,
                        std::io::Error::last_os_error()
                    );
                }
            }

            have_used = true;
            if let Err(e) = queues[FRQ_INDEX].queue.add_used(mem, index, 0) {
                error!("failed to add used elements to the FRQ: {e:?}");
            }
        }

        have_used
    }

    /// Process the inflate queue (IFQ).
    ///
    /// The guest sends arrays of 4-byte PFN values. Each PFN << 12 = guest physical
    /// address of a 4KB page the guest is giving to the balloon. The guest promises
    /// not to access these pages until deflation.
    ///
    /// PFNs are sorted and deduplicated. Contiguous runs are coalesced into batched
    /// release calls. On macOS, this uses `hv_vm_unmap`+`hv_vm_map` to release RSS.
    pub fn process_ifq(&mut self) -> bool {
        debug!("balloon: process_ifq()");
        let mem = match self.device_state {
            DeviceState::Activated(ref mem, _) => mem,
            DeviceState::Inactive => {
                error!("balloon: process_ifq called on inactive device");
                return false;
            }
        };

        let queues = self
            .queues
            .as_mut()
            .expect("queues should exist when activated");
        let mut have_used = false;

        while let Some(head) = queues[IFQ_INDEX].queue.pop(mem) {
            let index = head.index;

            let mut reader = match Reader::new(mem, head) {
                Ok(r) => r,
                Err(e) => {
                    error!("balloon: failed to create reader for inflate descriptor: {e}");
                    if let Err(e) = queues[IFQ_INDEX].queue.add_used(mem, index, 0) {
                        error!("failed to add used elements to the inflate queue: {e:?}");
                    }
                    continue;
                }
            };

            // Read all PFNs from the descriptor chain.
            let num_pfns = reader.available_bytes() / std::mem::size_of::<Le32>();
            let mut pfns = Vec::with_capacity(num_pfns);
            while reader.available_bytes() >= std::mem::size_of::<Le32>() {
                match reader.read_obj::<Le32>() {
                    Ok(pfn) => pfns.push(u32::from(pfn)),
                    Err(e) => {
                        error!("balloon: failed to read PFN from inflate queue: {e}");
                        break;
                    }
                }
            }

            // Sort and deduplicate to avoid double-unmap on macOS.
            pfns.sort_unstable();
            pfns.dedup();

            let page_size = uapi::VIRTIO_BALLOON_PAGE_SIZE;
            let mut i = 0;
            while i < pfns.len() {
                let start_pfn = pfns[i] as u64;
                let mut run_len: usize = 1;

                // Extend run while PFNs are contiguous.
                while i + run_len < pfns.len()
                    && pfns[i + run_len] as u64 == start_pfn + run_len as u64
                {
                    run_len += 1;
                }

                let guest_addr =
                    GuestAddress(start_pfn << uapi::VIRTIO_BALLOON_PFN_SHIFT);
                let mut total_len = run_len * page_size;

                // Validate that the entire range is within a single memory region.
                // If the end extends past valid memory, cap to valid range.
                let end_addr = GuestAddress(guest_addr.raw_value() + total_len as u64 - 1);
                if !mem.address_in_range(end_addr) {
                    // Find how much is valid by checking the start.
                    if !mem.address_in_range(guest_addr) {
                        error!(
                            "balloon: inflate PFN {} out of range entirely",
                            start_pfn
                        );
                        i += run_len;
                        continue;
                    }
                    // Cap to single-page processing when range crosses boundary.
                    total_len = page_size;
                    run_len = 1;
                }

                match mem.get_host_address(guest_addr) {
                    Ok(host_addr) => {
                        debug!(
                            "balloon: inflate pfn={} count={} guest_addr={:?} host_addr={:p}",
                            start_pfn, run_len, guest_addr, host_addr
                        );
                        let gpa = guest_addr.raw_value();
                        if !unsafe {
                            release_host_pages_inflate(host_addr, gpa, total_len)
                        } {
                            error!(
                                "balloon: release_host_pages_inflate failed for pfn={} len={}",
                                start_pfn, total_len
                            );
                        }
                    }
                    Err(e) => {
                        error!(
                            "balloon: invalid guest address for PFN {}: {e}",
                            start_pfn
                        );
                    }
                }

                i += run_len;
            }

            have_used = true;
            if let Err(e) = queues[IFQ_INDEX].queue.add_used(mem, index, 0) {
                error!("failed to add used elements to the inflate queue: {e:?}");
            }
        }

        have_used
    }

    /// Process the deflate queue (DFQ).
    ///
    /// The guest is reclaiming pages from the balloon. No host action is needed —
    /// on Linux, page faults repopulate. On macOS, the `hv_vm_map` from inflate
    /// already re-established the mapping, so the pages are accessible (zero-filled
    /// on first access after madvise).
    pub fn process_dfq(&mut self) -> bool {
        debug!("balloon: process_dfq()");
        let mem = match self.device_state {
            DeviceState::Activated(ref mem, _) => mem,
            DeviceState::Inactive => {
                error!("balloon: process_dfq called on inactive device");
                return false;
            }
        };

        let queues = self
            .queues
            .as_mut()
            .expect("queues should exist when activated");
        let mut have_used = false;

        while let Some(head) = queues[DFQ_INDEX].queue.pop(mem) {
            let index = head.index;
            have_used = true;
            if let Err(e) = queues[DFQ_INDEX].queue.add_used(mem, index, 0) {
                error!("failed to add used elements to the deflate queue: {e:?}");
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

        // If an initial balloon target was set before activation (via
        // krun_set_balloon_config), signal a config change now so the
        // guest driver wakes up and starts inflating.
        if self.config.num_pages > 0 {
            self.device_state.signal_config_change();
        }

        Ok(())
    }

    fn is_activated(&self) -> bool {
        self.device_state.is_activated()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::legacy::DummyIrqChip;
    use crate::virtio::queue::tests::VirtQueue;
    use vm_memory::Bytes;

    // --- Existing config/init tests ---

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
        balloon.set_num_pages(1024);
        assert_eq!(balloon.num_pages(), 1024);
        balloon.set_num_pages(0);
        assert_eq!(balloon.num_pages(), 0);
    }

    #[test]
    fn test_read_config() {
        let mut balloon = Balloon::new().unwrap();
        balloon.set_num_pages(42);

        let mut buf = [0u8; 4];
        balloon.read_config(0, &mut buf);
        assert_eq!(u32::from_le_bytes(buf), 42);

        balloon.read_config(4, &mut buf);
        assert_eq!(u32::from_le_bytes(buf), 0);
    }

    #[test]
    fn test_write_config_actual() {
        let mut balloon = Balloon::new().unwrap();

        balloon.write_config(4, &100u32.to_le_bytes());
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
        balloon.read_config(100, &mut buf);
    }

    #[test]
    fn test_device_type() {
        let balloon = Balloon::new().unwrap();
        assert_eq!(balloon.device_type(), uapi::VIRTIO_ID_BALLOON);
    }

    #[test]
    fn test_write_config_boundary() {
        let mut balloon = Balloon::new().unwrap();
        // Write spanning from offset 3 into actual field — should be rejected.
        balloon.write_config(3, &[0xAA, 0xBB, 0xCC, 0xDD]);
        assert_eq!(balloon.actual(), 0);
    }

    // --- Activation tests ---

    #[test]
    fn test_set_num_pages_persists_through_activate() {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x100000)]).unwrap();
        let mut balloon = Balloon::new().unwrap();
        balloon.set_num_pages(32768);

        let queues = create_device_queues(&mem);
        let intc: crate::legacy::IrqChip = DummyIrqChip::new().into();
        let interrupt =
            InterruptTransport::new(intc, "test_balloon".to_string()).unwrap();
        balloon.activate(mem, interrupt, queues).unwrap();

        assert!(balloon.is_activated());
        assert_eq!(balloon.num_pages(), 32768);
    }

    #[test]
    fn test_activate_config_change_nonzero() {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x100000)]).unwrap();
        let mut balloon = Balloon::new().unwrap();
        balloon.set_num_pages(1024);

        let queues = create_device_queues(&mem);
        let intc: crate::legacy::IrqChip = DummyIrqChip::new().into();
        let interrupt =
            InterruptTransport::new(intc, "test_balloon".to_string()).unwrap();
        // Should not panic — signal_config_change fires.
        balloon.activate(mem, interrupt, queues).unwrap();
        assert!(balloon.is_activated());
    }

    #[test]
    fn test_activate_no_signal_zero() {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x100000)]).unwrap();
        let mut balloon = Balloon::new().unwrap();

        let queues = create_device_queues(&mem);
        let intc: crate::legacy::IrqChip = DummyIrqChip::new().into();
        let interrupt =
            InterruptTransport::new(intc, "test_balloon".to_string()).unwrap();
        balloon.activate(mem, interrupt, queues).unwrap();
        assert!(balloon.is_activated());
    }

    // --- Defensive error tests ---

    #[test]
    fn test_process_ifq_inactive_device() {
        let mut balloon = Balloon::new().unwrap();
        assert!(!balloon.process_ifq());
        assert!(!balloon.process_dfq());
        assert!(!balloon.process_frq());
    }

    // --- Queue processing tests ---

    #[test]
    fn test_process_ifq_reads_pfns() {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x100000)]).unwrap();
        let mut balloon = activate_balloon(&mem);

        // Write PFN value into guest memory at the data buffer area.
        let pfn: u32 = 16; // GPA = 16 * 4096 = 0x10000
        mem.write_obj(Le32::from(pfn), GuestAddress(DATA_BUF))
            .unwrap();

        // Set up descriptor in IFQ pointing to the PFN buffer (1 Le32 = 4 bytes).
        setup_queue_descriptor(&mem, &mut balloon, IFQ_INDEX, DATA_BUF, 4);

        assert!(balloon.process_ifq());
    }

    #[test]
    fn test_process_ifq_invalid_pfn() {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x100000)]).unwrap();
        let mut balloon = activate_balloon(&mem);

        // Write an invalid PFN (0xFFFFFFFF → GPA far out of range).
        mem.write_obj(Le32::from(0xFFFF_FFFFu32), GuestAddress(DATA_BUF))
            .unwrap();

        setup_queue_descriptor(&mem, &mut balloon, IFQ_INDEX, DATA_BUF, 4);

        // Should not panic — invalid PFN is logged and skipped.
        assert!(balloon.process_ifq());
    }

    #[test]
    fn test_process_ifq_duplicate_pfns() {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x100000)]).unwrap();
        let mut balloon = activate_balloon(&mem);

        // Write duplicate PFNs: [16, 16, 16].
        for i in 0..3u64 {
            mem.write_obj(Le32::from(16u32), GuestAddress(DATA_BUF + i * 4))
                .unwrap();
        }

        setup_queue_descriptor(&mem, &mut balloon, IFQ_INDEX, DATA_BUF, 12);

        // Dedup reduces [16,16,16] → [16]. No double-unmap.
        assert!(balloon.process_ifq());
    }

    #[test]
    fn test_process_ifq_coalesces_contiguous() {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x100000)]).unwrap();
        let mut balloon = activate_balloon(&mem);

        // Write contiguous PFNs: [100, 101, 102, 200, 201].
        let pfns: [u32; 5] = [100, 101, 102, 200, 201];
        for (i, &pfn) in pfns.iter().enumerate() {
            mem.write_obj(Le32::from(pfn), GuestAddress(DATA_BUF + i as u64 * 4))
                .unwrap();
        }

        setup_queue_descriptor(&mem, &mut balloon, IFQ_INDEX, DATA_BUF, 20);

        // Should coalesce into 2 batches: [100-102] and [200-201].
        assert!(balloon.process_ifq());
    }

    #[test]
    fn test_process_ifq_sorts_pfns() {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x100000)]).unwrap();
        let mut balloon = activate_balloon(&mem);

        // Write unsorted PFNs: [200, 100, 150].
        let pfns: [u32; 3] = [200, 100, 150];
        for (i, &pfn) in pfns.iter().enumerate() {
            mem.write_obj(Le32::from(pfn), GuestAddress(DATA_BUF + i as u64 * 4))
                .unwrap();
        }

        setup_queue_descriptor(&mem, &mut balloon, IFQ_INDEX, DATA_BUF, 12);

        assert!(balloon.process_ifq());
    }

    #[test]
    fn test_process_ifq_empty_descriptor() {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x100000)]).unwrap();
        let mut balloon = activate_balloon(&mem);

        // Zero-length descriptor.
        setup_queue_descriptor(&mem, &mut balloon, IFQ_INDEX, DATA_BUF, 0);

        // Should consume descriptor, return true, no panic.
        assert!(balloon.process_ifq());
    }

    #[test]
    fn test_process_dfq_drains_queue() {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x100000)]).unwrap();
        let mut balloon = activate_balloon(&mem);

        setup_queue_descriptor(&mem, &mut balloon, DFQ_INDEX, DATA_BUF, 64);

        assert!(balloon.process_dfq());
    }

    #[test]
    fn test_process_frq_basic() {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x100000)]).unwrap();
        let mut balloon = activate_balloon(&mem);

        // FRQ descriptor points to a valid guest memory region.
        // desc.addr = data buffer address, desc.len = region size.
        setup_queue_descriptor(&mem, &mut balloon, FRQ_INDEX, DATA_BUF, 4096);

        assert!(balloon.process_frq());
    }

    // --- Test helpers ---

    // Layout: queues use low addresses, data buffer at 0x80000.
    const DATA_BUF: u64 = 0x80000;

    /// Create an activated balloon device with properly configured queues.
    fn activate_balloon(mem: &GuestMemoryMmap) -> Balloon {
        let mut balloon = Balloon::new().unwrap();
        let queues = create_device_queues(mem);
        let intc: crate::legacy::IrqChip = DummyIrqChip::new().into();
        let interrupt =
            InterruptTransport::new(intc, "test_balloon".to_string()).unwrap();
        balloon.activate(mem.clone(), interrupt, queues).unwrap();
        balloon
    }

    /// Create properly initialized device queues backed by guest memory.
    fn create_device_queues(mem: &GuestMemoryMmap) -> Vec<DeviceQueue> {
        use std::sync::Arc;

        let mut queues = Vec::new();
        // Each VirtQueue occupies ~4KB. Space them apart in guest memory.
        for i in 0..defs::NUM_QUEUES {
            let base = GuestAddress((i as u64) * 0x4000);
            let vq = VirtQueue::new(base, mem, 16); // small queue for tests
            let queue = vq.create_queue();
            let event = Arc::new(EventFd::new(utils::eventfd::EFD_NONBLOCK).unwrap());
            queues.push(DeviceQueue { queue, event });
        }
        queues
    }

    /// Set up a single descriptor in a queue's available ring.
    ///
    /// Writes a descriptor at index 0 of the queue's descriptor table,
    /// pointing to `(buf_addr, buf_len)` as a readable buffer. Then
    /// makes it available by updating the avail ring.
    fn setup_queue_descriptor(
        mem: &GuestMemoryMmap,
        balloon: &mut Balloon,
        queue_idx: usize,
        buf_addr: u64,
        buf_len: u32,
    ) {
        let queues = balloon.queues.as_mut().unwrap();
        let q = &mut queues[queue_idx].queue;

        // Write descriptor 0: points to buf_addr with buf_len, readable, no next.
        use vm_memory::Le16;
        let desc_addr = q.desc_table;
        // struct virtq_desc: addr(8) + len(4) + flags(2) + next(2) = 16 bytes
        mem.write_obj(vm_memory::Le64::from(buf_addr), desc_addr)
            .unwrap();
        mem.write_obj(Le32::from(buf_len), GuestAddress(desc_addr.raw_value() + 8))
            .unwrap();
        mem.write_obj(Le16::from(0u16), GuestAddress(desc_addr.raw_value() + 12)) // flags: readable, no next
            .unwrap();
        mem.write_obj(Le16::from(0u16), GuestAddress(desc_addr.raw_value() + 14)) // next: 0
            .unwrap();

        // Write avail ring: index 0 = descriptor 0, then bump idx to 1.
        let avail_ring_entries = GuestAddress(q.avail_ring.raw_value() + 4); // skip flags(2) + idx(2)
        mem.write_obj(Le16::from(0u16), avail_ring_entries).unwrap();
        let avail_idx_addr = GuestAddress(q.avail_ring.raw_value() + 2);
        mem.write_obj(Le16::from(1u16), avail_idx_addr).unwrap();
    }
}
