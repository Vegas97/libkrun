#[derive(Clone, Debug)]
pub struct FsDeviceConfig {
    pub fs_id: String,
    pub shared_dir: String,
    pub shm_size: Option<usize>,
    pub allow_root_dir_delete: bool,
    /// When true, the FUSE server rejects all mutating operations with EROFS.
    /// This enforces read-only at the VMM level, not inside the guest.
    pub read_only: bool,
}
