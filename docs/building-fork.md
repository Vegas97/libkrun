# Building this fork on another machine

The source of truth for this fork is `main` in
[`Vegas97/libkrun`](https://github.com/Vegas97/libkrun). Clone that repository,
not the upstream `containers/libkrun`, to get the fork's networking, read-only
root filesystem, and balloon-control changes. See [FORKS.md](../FORKS.md).

## macOS requirements

Use an Apple Silicon Mac running macOS 14 or newer, with Xcode Command Line
Tools and a Rust toolchain installed. The build also needs LLVM's linker,
`xz`, and the guest firmware library:

```sh
# Run in a terminal on the new machine.
brew install lld xz
brew tap libkrun/krun
brew trust --formula libkrun/krun/libkrunfw
brew install libkrun/krun/libkrunfw
```

The firmware is supplied by the
[libkrun Homebrew tap](https://github.com/libkrun/homebrew-krun). Recent Homebrew
versions require tap formulae to be trusted explicitly; the command above
trusts only the firmware formula.

`libkrunfw` is a separate dependency. Its guest kernel must support the
devices and features that your application uses; cloning this library does
not install or replace the firmware.

## Clone and build

Run these commands in the directory where you keep source repositories:

```sh
git clone git@github.com:Vegas97/libkrun.git
cd libkrun
make NET=1 BLK=1
```

The Makefile downloads a Debian sysroot to `linux-sysroot/`, cross-compiles
`init/init.c` for the Linux guest, and builds the macOS library. Initial builds
need network access. `NET=1` and `BLK=1` enable virtio-net and virtio-block,
which Hive uses.

| Generated output | How it is recreated |
|---|---|
| `target/release/libkrun.dylib` and versioned library | `make NET=1 BLK=1` |
| `linux-sysroot/` | Downloaded by the Makefile on macOS |
| `libkrun.pc` | Generated from `libkrun.pc.in` |
| `test-prefix/` | `make NET=1 BLK=1 test-prefix` |
| `target/`, `tests/target/` | Cargo build and test caches |

These generated files should stay out of Git. Build from the checked-in
`Cargo.lock`; do not copy this machine's build cache or sysroot into another
repository.

## Install or use the local build

From the repository root, install the library and headers under `/usr/local`:

```sh
sudo make NET=1 BLK=1 install
```

For an installation inside this checkout instead, use the existing test prefix:

```sh
make NET=1 BLK=1 test-prefix
```

An existing `krunvm` executable can use the local library without a system-wide
installation. Run this from the libkrun repository root:

```sh
DYLD_LIBRARY_PATH="$PWD/target/release${DYLD_LIBRARY_PATH:+:$DYLD_LIBRARY_PATH}" krunvm --version
```

`krunvm` is a separate repository and must support the new C APIs and control
protocol. Rebuild that fork when its bindings or CLI change. Hive's installation
and VM orchestration instructions belong in `Vegas97/service-orchestrator`;
this document covers the library build.

## Verify

Run the relevant unit tests from the repository root. A dummy guest init is
acceptable for these host-side unit tests because they do not boot a guest:

```sh
KRUN_INIT_BINARY_PATH=/dev/null cargo test --locked -p devices --features net,blk -- virtio::
KRUN_INIT_BINARY_PATH=/dev/null cargo test --locked -p vmm --features net,blk -- control_socket
cargo test --locked -p kernel
```

Never use `/dev/null` as the init for a runtime build. Unset
`KRUN_INIT_BINARY_PATH` and use the Makefile to compile a real guest init.

The complete `devices` suite also includes legacy i8042 tests. On the macOS
host used to validate this merge, `test_i8042_kbd` and
`test_i8042_read_write_and_event` fail identically on the original `main`:
the assertions expect eventfd counter values that the macOS implementation
does not return. The `virtio::` filter above covers the changed devices
without those unrelated failures.

For a real guest boot and the end-to-end tests, use the Makefile and installed
firmware rather than the dummy init:

```sh
make NET=1 BLK=1 test
```

See [the test instructions](../tests/README.md) for the additional guest-side
Rust target and test harness requirements. Linux build and installation
requirements are in [README.md](../README.md#building-and-installing).

## What belongs in which repository

| Material | Home |
|---|---|
| libkrun source, tests, build instructions, fork changelog | This fork |
| Hive setup, dependency versions, orchestration, integration investigations | `Vegas97/service-orchestrator` |
| krunvm CLI and bindings | The krunvm fork |
| Build outputs, sysroots, VM runtime state, editor settings | Regenerate locally |
| `.context/` prompts and completed agent plans | Optional history; not required to build or run |

Do not commit local credentials, VM registrations, or machine-specific settings.
Preserve any useful setup knowledge as portable instructions instead.
