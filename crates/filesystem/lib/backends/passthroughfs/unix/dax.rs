//! DAX window mappings.
//!
//! virtio-fs DAX lets the guest map a file region directly into the shared
//! memory window instead of issuing FUSE reads/writes. The VMM forwards
//! `FUSE_SETUPMAPPING`/`FUSE_REMOVEMAPPING` here.
//!
//! Linux maps the file region into the process's own window with
//! `mmap(MAP_SHARED|MAP_FIXED)`. macOS cannot install a stage-2 mapping from
//! userspace, so it maps the file host-side and asks the VMM worker to install
//! it with [`WorkerMessage::DaxAddMapping`], carrying whether the mapping is
//! writable so HVF can omit `HV_MEMORY_WRITE` for read-only mappings.

use std::io;

use crate::backends::shared::platform;

#[cfg(target_os = "linux")]
mod linux {
    use super::super::{PassthroughFs, inode};
    use super::*;
    use crate::backends::passthroughfs::window::window_addr;

    /// Map `[foffset, foffset + len)` of `inode` into the DAX window at
    /// `host_shm_base + moffset`.
    ///
    /// `FUSE_SETUPMAPPING` carries `fh = -1` (the kernel never fills the handle
    /// for DAX), so the request cannot be authorized against an open handle.
    /// The mount mode and the read-only init inode are the controls instead.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn do_setupmapping(
        fs: &PassthroughFs,
        inode: u64,
        foffset: u64,
        len: u64,
        flags: u64,
        moffset: u64,
        host_shm_base: u64,
        shm_size: u64,
    ) -> io::Result<()> {
        // DAX bypasses the ordinary FUSE write/open checks, so a read-only mount
        // must refuse a writable mapping instead of relying on the guest.
        if fs.cfg.readonly() && flags & SETUPMAPPING_FLAG_WRITE != 0 {
            return Err(platform::erofs());
        }
        // `init.krun` is read-only even on a writable mount.
        if fs.is_virtual_init_inode(inode) && flags & SETUPMAPPING_FLAG_WRITE != 0 {
            return Err(platform::eacces());
        }
        let addr = window_addr(moffset, len, host_shm_base, shm_size)?;

        // The synthetic init binary has no backing host file; map anonymous
        // memory and copy the requested region of its payload in, matching the
        // non-DAX read path.
        if fs.is_virtual_init_inode(inode) {
            return map_init(addr, len, foffset);
        }

        let write = flags & SETUPMAPPING_FLAG_WRITE != 0;
        let open_flags = if write { libc::O_RDWR } else { libc::O_RDONLY };
        let prot = if write {
            libc::PROT_READ | libc::PROT_WRITE
        } else {
            libc::PROT_READ
        };

        let fd = inode::open_inode_fd(fs, inode, open_flags)?;
        let ret = unsafe {
            libc::mmap(
                addr as *mut libc::c_void,
                len as usize,
                prot,
                libc::MAP_SHARED | libc::MAP_FIXED,
                fd,
                foffset as libc::off_t,
            )
        };
        let error = (ret == libc::MAP_FAILED).then(io::Error::last_os_error);
        // The mapping keeps the file alive after the fd is closed.
        unsafe { libc::close(fd) };
        match error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// Tear down previously established mappings by replacing them with a
    /// `PROT_NONE` anonymous mapping.
    pub(crate) fn do_removemapping(
        requests: &[crate::RemovemappingOne],
        host_shm_base: u64,
        shm_size: u64,
    ) -> io::Result<()> {
        for request in requests {
            let addr = window_addr(request.moffset, request.len, host_shm_base, shm_size)?;
            let ret = unsafe {
                libc::mmap(
                    addr as *mut libc::c_void,
                    request.len as usize,
                    libc::PROT_NONE,
                    libc::MAP_ANONYMOUS | libc::MAP_PRIVATE | libc::MAP_FIXED,
                    -1,
                    0,
                )
            };
            if ret == libc::MAP_FAILED {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    /// Map the synthetic init binary into the window, copying the requested
    /// region of its payload.
    fn map_init(addr: u64, len: u64, foffset: u64) -> io::Result<()> {
        let ret = unsafe {
            libc::mmap(
                addr as *mut libc::c_void,
                len as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_FIXED,
                -1,
                0,
            )
        };
        if ret == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        copy_init(addr as *mut libc::c_void, len, foffset);
        // The init binary is read-only; drop the mapping's write access once the
        // payload has been copied.
        if unsafe { libc::mprotect(addr as *mut libc::c_void, len as usize, libc::PROT_READ) } == -1
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use std::ptr::null_mut;
    use std::sync::Arc;

    use crossbeam_channel::Sender;
    use msb_krun_utils::worker_message::WorkerMessage;

    use super::super::{PassthroughFs, inode};
    use super::*;
    use crate::backends::passthroughfs::window::{
        WindowMapping, remove_window_range, request_mapping, request_unmapping, window_addr,
    };

    /// Map `[foffset, foffset + len)` of `inode` host-side and ask the VMM
    /// worker to install the stage-2 mapping at `guest_shm_base + moffset`.
    ///
    /// `FUSE_SETUPMAPPING` carries `fh = -1` (the kernel never fills the handle
    /// for DAX), so the request cannot be authorized against an open handle.
    /// The mount mode and the read-only init inode are the controls instead.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn do_setupmapping(
        fs: &PassthroughFs,
        inode: u64,
        foffset: u64,
        len: u64,
        flags: u64,
        moffset: u64,
        guest_shm_base: u64,
        shm_size: u64,
        map_sender: &Option<Sender<WorkerMessage>>,
    ) -> io::Result<()> {
        // DAX bypasses the ordinary FUSE write/open checks, so a read-only mount
        // must refuse a writable mapping instead of relying on the guest.
        if fs.cfg.readonly() && flags & SETUPMAPPING_FLAG_WRITE != 0 {
            return Err(platform::erofs());
        }
        // `init.krun` is read-only even on a writable mount.
        if fs.is_virtual_init_inode(inode) && flags & SETUPMAPPING_FLAG_WRITE != 0 {
            return Err(platform::eacces());
        }
        let Some(sender) = map_sender else {
            return Err(platform::enosys());
        };
        let guest_addr = window_addr(moffset, len, guest_shm_base, shm_size)?;

        let write = flags & SETUPMAPPING_FLAG_WRITE != 0;
        let prot = if write {
            libc::PROT_READ | libc::PROT_WRITE
        } else {
            libc::PROT_READ
        };

        let backing = if fs.is_virtual_init_inode(inode) {
            map_init(len, foffset)?
        } else {
            let open_flags = if write { libc::O_RDWR } else { libc::O_RDONLY };
            let fd = inode::open_inode_fd(fs, inode, open_flags)?;
            let addr = unsafe {
                libc::mmap(
                    null_mut(),
                    len as usize,
                    prot,
                    libc::MAP_SHARED,
                    fd,
                    foffset as libc::off_t,
                )
            };
            let error = (addr == libc::MAP_FAILED).then(io::Error::last_os_error);
            unsafe { libc::close(fd) };
            match error {
                Some(error) => return Err(error),
                None => Arc::new(MmapView {
                    addr: addr as u64,
                    len: len as usize,
                }),
            }
        };
        let host_addr = backing.addr;

        // Serialize the worker round-trip and the registry update so concurrent
        // setups cannot interleave. A failed send or rejected reply drops
        // `backing`, releasing the host mapping we just established.
        let mut windows = fs.map_windows.lock().unwrap_or_else(|p| p.into_inner());
        request_mapping(sender, host_addr, guest_addr, len, write)?;
        // The guest re-sends `FUSE_SETUPMAPPING` for the same range to upgrade a
        // read-only mapping to writable (and Linux's `MAP_FIXED` lets a request
        // replace any overlap), so drop what the new mapping supersedes now that
        // the worker has installed it. A slot that extends past the new range
        // keeps its backing.
        remove_window_range(&mut windows, guest_addr, len)?;
        windows.insert(
            guest_addr,
            WindowMapping {
                guest_addr,
                host_addr,
                len,
                backing,
            },
        );
        Ok(())
    }

    /// Tear down mappings the VMM worker installed, then release the host
    /// mappings.
    pub(crate) fn do_removemapping(
        fs: &PassthroughFs,
        requests: &[crate::RemovemappingOne],
        guest_shm_base: u64,
        shm_size: u64,
        map_sender: &Option<Sender<WorkerMessage>>,
    ) -> io::Result<()> {
        let Some(sender) = map_sender else {
            return Err(platform::enosys());
        };
        for request in requests {
            let guest_addr = window_addr(request.moffset, request.len, guest_shm_base, shm_size)?;

            // Hold the registry lock across the worker round-trip and the update
            // so a concurrent setup cannot install a replacement between them
            // that this stale removal would then drop. The stage-2 mapping is
            // torn down first and the registry is only touched after the worker
            // acknowledges, so a failed removal leaves it intact for a retry.
            let mut windows = fs.map_windows.lock().unwrap_or_else(|p| p.into_inner());
            request_unmapping(sender, guest_addr, request.len)?;
            remove_window_range(&mut windows, guest_addr, request.len)?;
        }
        Ok(())
    }

    /// Map the synthetic init binary anonymously and copy the requested region
    /// of its payload.
    fn map_init(len: u64, foffset: u64) -> io::Result<Arc<MmapView>> {
        let addr = unsafe {
            libc::mmap(
                null_mut(),
                len as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if addr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let view = Arc::new(MmapView {
            addr: addr as u64,
            len: len as usize,
        });
        copy_init(addr, len, foffset);
        // The init binary is read-only; drop the mapping's write access once the
        // payload has been copied.
        if unsafe { libc::mprotect(addr, len as usize, libc::PROT_READ) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(view)
    }
}

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

/// `FUSE_SETUPMAPPING_FLAG_WRITE` (`linux/fuse.h`): the guest asked for a
/// writable mapping.
const SETUPMAPPING_FLAG_WRITE: u64 = 0x1;

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

/// Owns a host `mmap` region backing one or more macOS DAX window slots.
///
/// The guest removes DAX ranges at 4 KiB granularity, but `munmap` requires
/// host page alignment, so a partial removal must not unmap eagerly. The region
/// is released only when the last slot referencing it is removed.
#[cfg(target_os = "macos")]
pub(crate) struct MmapView {
    addr: u64,
    len: usize,
}

/// Shared backing for installed macOS DAX window slots.
#[cfg(target_os = "macos")]
pub(crate) type MmapBacking = std::sync::Arc<MmapView>;

//--------------------------------------------------------------------------------------------------
// Trait Implementations
//--------------------------------------------------------------------------------------------------

#[cfg(target_os = "macos")]
impl Drop for MmapView {
    fn drop(&mut self) {
        if unsafe { libc::munmap(self.addr as *mut libc::c_void, self.len) } == -1 {
            tracing::error!("DAX munmap failed: {}", io::Error::last_os_error());
        }
    }
}

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

/// Copy `[foffset, foffset + len)` of the init-binary payload into an anonymous
/// mapping at `addr`.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn copy_init(addr: *mut libc::c_void, len: u64, foffset: u64) {
    let Ok(start) = usize::try_from(foffset) else {
        return;
    };
    let data = crate::agentd::agentd_bytes();
    let Some(tail) = data.get(start..) else {
        return;
    };
    let to_copy = std::cmp::min(len as usize, tail.len());
    if to_copy > 0 {
        // SAFETY: `addr` is a writable mapping of at least `len` bytes and
        // `tail` is a valid slice of at least `to_copy` bytes.
        unsafe {
            libc::memcpy(addr, tail.as_ptr() as *const _, to_copy);
        }
    }
}

//--------------------------------------------------------------------------------------------------
// Re-Exports
//--------------------------------------------------------------------------------------------------

#[cfg(target_os = "linux")]
pub(crate) use linux::{do_removemapping, do_setupmapping};

#[cfg(target_os = "macos")]
pub(crate) use macos::{do_removemapping, do_setupmapping};
