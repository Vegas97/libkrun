# Fork Changes

This document tracks changes in this fork relative to upstream libkrun.
These changes are candidates for upstream submission.

---

## virtio-net: Worker lifecycle and graceful shutdown

**Commits:** `ec8b33c`, `07439a6`, `ea00462`, `f0266d9`

**Problem:** After ~38 VM create/delete cycles, libkrun panics with
`BadActivate` in the virtio-net device. Worker threads and EventFds leak
across device resets because there is no shutdown mechanism.

**Changes:**
- Added `worker_stopfd: EventFd` and `worker_thread: Option<JoinHandle<()>>`
  to the `Net` device struct
- Implemented `Net::reset()` — signals the worker via `stop_fd`, joins the
  thread, and transitions to `Inactive`
- Worker epoll loop listens on `stop_fd` for graceful exit
- `activate()` returns `Err(BadActivate)` instead of panicking on spawn failure
- Added stress test: 80 activate/reset cycles without panic

**Files:**
- `src/devices/src/virtio/net/device.rs`
- `src/devices/src/virtio/net/worker.rs`

**Upstream status:** Not yet submitted

---

## virtio-net: TX checksum offload finalization

**Commit:** `ec8b33c`

**Problem:** Unix socket backends (gvproxy, passt) don't have a kernel
networking stack to complete `VIRTIO_NET_HDR_F_NEEDS_CSUM` offloaded
checksums. The guest sets only a pseudo-header checksum and expects the
host to finish it. With these backends, packets arrive with incorrect
checksums and are dropped.

**Changes:**
- Added `finalize_checksum()` in the TX path — computes the ones-complement
  checksum over the segment and stores it at the offset specified by the
  virtio-net header before the backend strips the header
- Only activates when `VIRTIO_NET_HDR_F_NEEDS_CSUM` is set; no-op otherwise
- Unit tests for basic, pseudo-header, no-flag, and short-buffer cases

**Files:**
- `src/devices/src/virtio/net/mod.rs` (checksum logic + tests)
- `src/devices/src/virtio/net/worker.rs` (TX path integration)

**Upstream status:** Tracked as BKL-019, not yet submitted

---

## virtio-net: Notification ordering fix

**Commit:** `ec8b33c`

**Problem:** `process_backend_socket_readable()` had inverted
disable/enable notification calls. After `process_rx()` deferred a frame,
the guest could not wake the worker to retry — notifications were
permanently disabled.

**Changes:**
- Corrected the order to: disable → process → enable (matching the
  standard virtio pattern used everywhere else)
- Added unit tests proving correct vs. inverted ordering behavior

**Files:**
- `src/devices/src/virtio/net/worker.rs`

**Upstream status:** Not yet submitted

---

## virtio-net: PID-scoped Unix socket paths

**Commit:** `ec8b33c`

**Problem:** Concurrent krunvm instances using `UnixgramPath` backends
collide on the client socket path, causing connection failures.

**Changes:**
- Client socket path now includes the process PID
- Unit tests verify PID inclusion and deterministic naming

**Files:**
- `src/devices/src/virtio/net/unixgram.rs`

**Upstream status:** Not yet submitted

---

## virtiofs: Read-only rootfs API (`krun_set_root_ro`)

**Commit:** (this branch, not yet committed)

**Problem:** virtiofs rejects `mount -o remount,ro /` from inside the VM.
Without VMM-level enforcement, an attacker with code execution can modify
files on the rootfs. The Hive Shield threat model requires read-only rootfs.

**Changes:**
- Added `read_only: bool` to `FsDeviceConfig` and passthrough `Config`
- FUSE server intercepts all mutating opcodes (write, create, mkdir, unlink,
  rename, link, symlink, mknod, setattr, setxattr, removexattr, fallocate,
  copyfilerange) and returns `EROFS`
- `open()` with `O_WRONLY`/`O_RDWR` also returns `EROFS`
- Kernel cmdline switches from `rw` to `ro` when root fs is read-only
- New C API: `krun_set_root_ro(ctx_id, root_path)`

**Files:**
- `src/vmm/src/vmm_config/fs.rs`
- `src/devices/src/virtio/fs/device.rs`
- `src/devices/src/virtio/fs/server.rs`
- `src/devices/src/virtio/fs/worker.rs`
- `src/devices/src/virtio/fs/{linux,macos}/passthrough.rs`
- `src/vmm/src/builder.rs`
- `src/libkrun/src/lib.rs`
- `include/libkrun.h`

**Upstream status:** Not applicable (Hive-specific feature)

---

## Other fork-only changes on main

| Commit | Description |
|--------|-------------|
| `7c5292c` | Fix cross-compilation of build.rs for aarch64 target |
| `3fc7d1c` | Fix fence completion race in virtio-gpu worker |
| `c58f00a` | init: Remove MAX_ARGS limit |
| `b7ecf07` | init: Fix out of argv array bounds accesses |
| `dbc7fef` | init: Handle failed memory allocations in argv processing |
