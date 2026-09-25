//! DAX window mappings for the Windows host.
//!
//! virtio-fs DAX lets the guest map a file region directly into the shared
//! memory window instead of issuing FUSE reads/writes. Windows cannot install
//! a stage-2 mapping from userspace, so it maps the file host-side with
//! `MapViewOfFile` and asks the VMM worker to install the mapping with
//! [`WorkerMessage::DaxAddMapping`], carrying whether it is writable so WHP can
//! omit `Write` for read-only mappings.

use std::os::windows::fs::FileExt;
use std::sync::Arc;

use crossbeam_channel::Sender;
use msb_krun_utils::worker_message::WorkerMessage;

use super::memory_mapping::{WindowsFileMappingAccess, WindowsFileMappingView};
use super::*;
use crate::backends::passthroughfs::window::{
    WindowMapping, remove_window_range, request_mapping, request_unmapping, window_addr,
};

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

/// `FUSE_SETUPMAPPING_FLAG_WRITE` (`linux/fuse.h`): the guest asked for a
/// writable mapping.
const SETUPMAPPING_FLAG_WRITE: u64 = 0x1;

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

/// Map `[foffset, foffset + len)` of `inode` and ask the VMM worker to install
/// the stage-2 mapping at `guest_shm_base + moffset`.
///
/// `FUSE_SETUPMAPPING` carries `fh = -1` (the kernel never fills the handle for
/// DAX), so the request cannot be authorized against an open handle. The mount
/// mode and the read-only init inode are the controls instead.
#[allow(clippy::too_many_arguments)]
pub(super) fn do_setupmapping(
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
    let write = flags & SETUPMAPPING_FLAG_WRITE != 0;
    // DAX bypasses the ordinary FUSE write/open checks, so a read-only mount
    // must refuse a writable mapping instead of relying on the guest.
    if fs.cfg.readonly && write {
        return Err(linux_error(LINUX_EROFS));
    }
    // `init.krun` is read-only even on a writable mount.
    if fs.cfg.inject_init && inode == INIT_INODE && write {
        return Err(linux_error(LINUX_EACCES));
    }
    let Some(sender) = map_sender else {
        return Err(linux_error(LINUX_ENOSYS));
    };
    let guest_addr = window_addr(moffset, len, guest_shm_base, shm_size)?;
    let len = usize::try_from(len).map_err(|_| linux_error(LINUX_EINVAL))?;
    let access = mapping_access(flags);

    // Build the host view before taking the registry lock so a slow file open
    // does not block unrelated mappings; admission below decides whether it is
    // installed.
    let view = if fs.cfg.inject_init && inode == INIT_INODE {
        init_mapping_view(foffset, len)?
    } else {
        let data = fs.inode(inode)?;
        mapping_view_for_inode(fs, data.as_ref(), foffset, len, access)?
    };
    let host_addr = view.host_addr();

    // Serialize the worker round-trip and the registry update so concurrent
    // setups cannot interleave.
    let mut windows = fs.map_windows.lock().unwrap_or_else(|p| p.into_inner());
    request_mapping(
        sender,
        host_addr,
        guest_addr,
        len as u64,
        access == WindowsFileMappingAccess::ReadWrite,
    )?;
    // The guest re-sends `FUSE_SETUPMAPPING` for the same range to upgrade a
    // read-only mapping to writable (and Linux's `MAP_FIXED` lets a request
    // replace any overlap), so drop what the new mapping supersedes now that
    // the worker has installed it. A slot that extends past the new range keeps
    // its backing.
    remove_window_range(&mut windows, guest_addr, len as u64)?;
    windows.insert(
        guest_addr,
        WindowMapping {
            guest_addr,
            host_addr,
            len: len as u64,
            backing: Arc::new(view),
        },
    );
    Ok(())
}

/// Tear down mappings the VMM worker installed and release the host views.
///
/// A removal can cover only part of an installed window; the untouched slots
/// (and their host backing) stay alive.
pub(super) fn do_removemapping(
    fs: &PassthroughFs,
    requests: &[crate::RemovemappingOne],
    guest_shm_base: u64,
    shm_size: u64,
    map_sender: &Option<Sender<WorkerMessage>>,
) -> io::Result<()> {
    let Some(sender) = map_sender else {
        return Err(linux_error(LINUX_ENOSYS));
    };
    for request in requests {
        let guest_addr = window_addr(request.moffset, request.len, guest_shm_base, shm_size)?;

        // Hold the registry lock across the worker round-trip and the update so a
        // concurrent setup cannot install a replacement between them that this
        // stale removal would then drop. The stage-2 mapping is torn down first
        // and the registry is only touched after the worker acknowledges, so a
        // failed removal leaves it intact for a retry.
        let mut windows = fs.map_windows.lock().unwrap_or_else(|p| p.into_inner());
        request_unmapping(sender, guest_addr, request.len)?;
        // Windows cannot unmap part of a `MapViewOfFile`; dropping the removed
        // slot releases the whole view only once the last slot referencing it
        // is removed.
        remove_window_range(&mut windows, guest_addr, request.len)?;
    }
    Ok(())
}

/// Access mode implied by the FUSE mapping flags.
fn mapping_access(flags: u64) -> WindowsFileMappingAccess {
    if flags & SETUPMAPPING_FLAG_WRITE != 0 {
        WindowsFileMappingAccess::ReadWrite
    } else {
        WindowsFileMappingAccess::ReadOnly
    }
}

/// Map a host file region, filling a read-only mapping that runs past EOF with
/// zeros (matching the non-DAX read path) instead of failing `MapViewOfFile`.
fn mapping_view_for_inode(
    fs: &PassthroughFs,
    data: &InodeData,
    foffset: u64,
    len: usize,
    access: WindowsFileMappingAccess,
) -> io::Result<WindowsFileMappingView> {
    // Reuse the backend's open path so a reparse point or a replaced path
    // cannot redirect the mapping outside the exported root.
    let flags: u32 = if access == WindowsFileMappingAccess::ReadWrite {
        LINUX_O_RDWR as u32
    } else {
        0
    };
    let file = fs.open_inode_file(data, flags)?;
    reject_reparse_metadata(&file.metadata().map_err(host_error)?)?;
    if let Some(expected) = data.identity
        && mobility::file_identity(&file).map_err(host_error)? != expected
    {
        return Err(linux_error(LINUX_ESTALE));
    }

    let end = foffset
        .checked_add(len as u64)
        .ok_or_else(|| linux_error(LINUX_EINVAL))?;
    if access == WindowsFileMappingAccess::ReadOnly
        && end > file.metadata().map_err(host_error)?.len()
    {
        return read_only_oversized_mapping_view(&file, foffset, len);
    }

    WindowsFileMappingView::map_file(&file, foffset, len, access).map_err(host_error)
}

/// Back a read-only view that extends past EOF with an anonymous, zero-filled
/// view containing the available bytes.
fn read_only_oversized_mapping_view(
    file: &File,
    foffset: u64,
    len: usize,
) -> io::Result<WindowsFileMappingView> {
    let mut view = WindowsFileMappingView::map_anonymous(len, WindowsFileMappingAccess::ReadWrite)
        .map_err(host_error)?;
    let file_len = file.metadata().map_err(host_error)?.len();
    if foffset < file_len {
        let to_copy: usize = len.min((file_len - foffset).try_into().unwrap_or(usize::MAX));
        let mut bytes = vec![0u8; to_copy];
        let read = file.seek_read(&mut bytes, foffset).map_err(host_error)?;
        view.copy_from_slice(&bytes[..read]).map_err(host_error)?;
    }
    // The guest mapping is read-only; drop the host view's write access once
    // the payload has been copied.
    view.make_read_only().map_err(host_error)?;
    Ok(view)
}

/// Map the synthetic init binary anonymously and copy the requested region of
/// its payload in.
fn init_mapping_view(foffset: u64, len: usize) -> io::Result<WindowsFileMappingView> {
    // Map read/write so the payload can be copied in; a write mapping of
    // `init.krun` was rejected above, so the guest mapping is read-only.
    let mut view = WindowsFileMappingView::map_anonymous(len, WindowsFileMappingAccess::ReadWrite)
        .map_err(host_error)?;
    if let Ok(start) = usize::try_from(foffset)
        && let Some(tail) = crate::agentd::agentd_bytes().get(start..)
    {
        let to_copy = len.min(tail.len());
        view.copy_from_slice(&tail[..to_copy]).map_err(host_error)?;
    }
    view.make_read_only().map_err(host_error)?;
    Ok(view)
}
