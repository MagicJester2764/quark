//! Client memory, arriving as a descriptor.
//!
//! `wl_shm.create_pool` is the one request in the MVP that carries a
//! descriptor, and it is what all of Phase 10's descriptor passing was for: the
//! client makes memory with `memfd_create`, maps it, draws into it, and hands
//! the compositor a handle to the same physical pages. Nothing is copied and
//! nobody is trusted — receiving the descriptor is what admits this task to the
//! region, and the mapping says how big it really is.
//!
//! A pool is a span of memory; a buffer is a rectangle described *within* it. A
//! client picks both, which is why every buffer is checked against its pool
//! before anything reads a pixel: the numbers in `create_buffer` are the
//! client's arithmetic, and the compositor is the thing that would fault.

use quark_rt::syscall;

pub const MAX_POOLS: usize = 8;
pub const MAX_BUFFERS: usize = 32;

/// Where pools are mapped, one 16 MiB slot each — which is `MAX_PAGES_PER_REGION`,
/// so a slot can always hold the largest region the kernel will make.
const POOL_BASE: usize = 0x88_0000_0000;
const POOL_STRIDE: usize = 0x100_0000;

/// The pixel formats this compositor can composite, in `wl_shm.format` numbers.
///
/// Both are 32 bits per pixel, little-endian, with blue in the low byte — the
/// same thing the framebuffer holds, so compositing is a copy rather than a
/// conversion. `XRGB8888` ignores the top byte; `ARGB8888` would carry alpha,
/// which nothing here blends yet, so it is accepted and treated as opaque.
pub const FORMAT_ARGB8888: u32 = 0;
pub const FORMAT_XRGB8888: u32 = 1;

pub fn format_supported(f: u32) -> bool {
    f == FORMAT_ARGB8888 || f == FORMAT_XRGB8888
}

#[derive(Clone, Copy)]
pub struct Pool {
    pub used: bool,
    /// The client this belongs to. Pools are per client, and a slot freed by
    /// one client must not be visible to the next.
    pub owner: usize,
    pub vaddr: usize,
    /// What the mapping actually turned out to be, not what the client said.
    pub size: usize,
    /// Buffers still referring to this pool. A client may destroy the pool
    /// while buffers carved from it are still in use, and the memory must stay
    /// until the last of them goes.
    pub buffers: usize,
    /// The client asked for it to go, and it will once `buffers` reaches zero.
    pub zombie: bool,
}

#[derive(Clone, Copy)]
pub struct Buffer {
    pub used: bool,
    pub pool: usize,
    pub offset: usize,
    pub width: usize,
    pub height: usize,
    pub stride: usize,
    pub format: u32,
    /// The compositor is reading these pixels right now. Releasing a buffer it
    /// is still reading is what tears a frame in half.
    pub in_use: bool,
}

const NO_POOL: Pool =
    Pool { used: false, owner: 0, vaddr: 0, size: 0, buffers: 0, zombie: false };
const NO_BUFFER: Buffer = Buffer {
    used: false,
    pool: 0,
    offset: 0,
    width: 0,
    height: 0,
    stride: 0,
    format: 0,
    in_use: false,
};

static mut POOLS: [Pool; MAX_POOLS] = [NO_POOL; MAX_POOLS];
static mut BUFFERS: [Buffer; MAX_BUFFERS] = [NO_BUFFER; MAX_BUFFERS];

pub fn pool(idx: usize) -> Option<Pool> {
    unsafe { POOLS.get(idx).copied().filter(|p| p.used) }
}

pub fn buffer(idx: usize) -> Option<Buffer> {
    unsafe { BUFFERS.get(idx).copied().filter(|b| b.used) }
}

/// Take a descriptor a client sent and map what it names.
///
/// `claimed` is the size the client says the pool is. It is checked against the
/// mapping rather than believed: a client that overstates it would otherwise
/// have the compositor reading past the end of the region on its behalf.
pub fn create_pool(owner: usize, fd: usize, claimed: usize) -> Option<usize> {
    let idx = unsafe { POOLS.iter().position(|p| !p.used) }?;
    let vaddr = POOL_BASE + idx * POOL_STRIDE;
    let size = match syscall::sys_mmap_fd(fd, vaddr) {
        Ok(n) => n,
        Err(()) => {
            let _ = syscall::sys_fd_close(fd);
            return None;
        }
    };
    // The descriptor has done its work: the mapping is what this task keeps,
    // and holding the descriptor as well would keep the region alive after the
    // client has finished with it.
    let _ = syscall::sys_fd_close(fd);
    if claimed == 0 || claimed > size {
        unmap_range(vaddr, size / 4096);
        return None;
    }
    unsafe {
        POOLS[idx] = Pool {
            used: true,
            owner,
            vaddr,
            // The client's figure, since it is the smaller: the rest of the
            // region is real memory but the client has not said it is a pool.
            size: claimed,
            buffers: 0,
            zombie: false,
        };
    }
    Some(idx)
}

/// Carve a rectangle out of a pool.
///
/// Every one of these numbers came from the client. The check is that the last
/// byte the compositor would read stays inside the pool — computed without
/// overflowing, because a client that picks `usize::MAX` for a stride is
/// picking it precisely so the arithmetic wraps and the check passes.
pub fn create_buffer(
    pool_idx: usize,
    offset: usize,
    width: usize,
    height: usize,
    stride: usize,
    format: u32,
) -> Option<usize> {
    let p = pool(pool_idx)?;
    if p.zombie || width == 0 || height == 0 || !format_supported(format) {
        return None;
    }
    if stride < width.checked_mul(4)? {
        return None;
    }
    // The last row starts at offset + (height - 1) * stride and runs width * 4.
    let last_row = stride.checked_mul(height - 1)?;
    let extent = offset
        .checked_add(last_row)?
        .checked_add(width.checked_mul(4)?)?;
    if extent > p.size {
        return None;
    }
    let idx = unsafe { BUFFERS.iter().position(|b| !b.used) }?;
    unsafe {
        BUFFERS[idx] = Buffer {
            used: true,
            pool: pool_idx,
            offset,
            width,
            height,
            stride,
            format,
            in_use: false,
        };
        POOLS[pool_idx].buffers += 1;
    }
    Some(idx)
}

/// Where a buffer's pixels are.
pub fn pixels(idx: usize) -> Option<(*const u8, usize, usize, usize)> {
    let b = buffer(idx)?;
    let p = pool(b.pool)?;
    Some(((p.vaddr + b.offset) as *const u8, b.width, b.height, b.stride))
}

pub fn set_in_use(idx: usize, yes: bool) {
    unsafe {
        if let Some(b) = BUFFERS.get_mut(idx) {
            if b.used {
                b.in_use = yes;
            }
        }
    }
}

/// A client is finished with a buffer.
pub fn destroy_buffer(idx: usize) {
    unsafe {
        let Some(b) = BUFFERS.get_mut(idx) else { return };
        if !b.used {
            return;
        }
        let pool_idx = b.pool;
        *b = NO_BUFFER;
        release_pool_ref(pool_idx);
    }
}

/// A client is finished with a pool. Its memory stays until its buffers do.
pub fn destroy_pool(idx: usize) {
    unsafe {
        let Some(p) = POOLS.get_mut(idx) else { return };
        if !p.used {
            return;
        }
        p.zombie = true;
        if p.buffers == 0 {
            unmap(idx);
        }
    }
}

fn release_pool_ref(idx: usize) {
    unsafe {
        let Some(p) = POOLS.get_mut(idx) else { return };
        if p.used && p.buffers > 0 {
            p.buffers -= 1;
            if p.buffers == 0 && p.zombie {
                unmap(idx);
            }
        }
    }
}

fn unmap(idx: usize) {
    unsafe {
        let p = &mut POOLS[idx];
        // Round up: the mapping is whole pages even when the client's size is
        // not, and leaving the tail mapped leaks an address slot for good.
        unmap_range(p.vaddr, (p.size + 4095) / 4096);
        *p = NO_POOL;
    }
}

/// `sys_munmap` takes at most 256 pages, and a pool can be four thousand.
fn unmap_range(vaddr: usize, pages: usize) {
    let mut done = 0;
    while done < pages {
        let n = (pages - done).min(256);
        let _ = syscall::sys_munmap(vaddr + done * 4096, n);
        done += n;
    }
}

/// Drop everything a client had. Called when it disconnects, which it may do
/// without destroying anything first — a crash is a disconnection too.
pub fn forget_client(owner: usize) {
    unsafe {
        for i in 0..MAX_BUFFERS {
            if BUFFERS[i].used && POOLS[BUFFERS[i].pool].owner == owner {
                BUFFERS[i] = NO_BUFFER;
            }
        }
        for i in 0..MAX_POOLS {
            if POOLS[i].used && POOLS[i].owner == owner {
                POOLS[i].buffers = 0;
                unmap(i);
            }
        }
    }
}
