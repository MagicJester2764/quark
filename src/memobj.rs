//! Memory objects: pages a pager provides, mapped into address spaces.
//!
//! An object is a numbered run of pages whose contents come from a user-space
//! pager — the VFS, for a file. A task holding a `MemObject` capability maps a
//! range of it; its entries are reservations naming the object's slot and the
//! page, and the first touch of one pages it in. A page is looked for in the
//! object's cache first. If it is not there, the task that touched it calls
//! the pager itself, lending it a fresh frame to fill, and the frame joins the
//! cache when the pager answers.
//!
//! The cache belongs to the object. A cached frame may be mapped in any number
//! of places, and nothing records where, so none is freed while the object
//! lives: the object is released only once nothing maps any of its pages, and
//! its pager has had the chance to write back what it must. The count of
//! mapped entries is kept by the page tables' own walks, which find an
//! object's slot in bits 52–62 of every entry that refers to it.
//!
//! Everything here runs with interrupts off except the pager call, and nothing
//! here allocates from the heap, which can turn them back on.

use crate::ipc;
use crate::paging::Fault;
use crate::pmm;
use core::sync::atomic::{AtomicU64, Ordering};

/// Object slots, 1 up: 0 means none, and a slot has to fit in 11 bits.
pub const MAX_OBJECTS: usize = 256;

/// `SYS_OBJECT_CTL`'s operations.
pub const CTL_RESIZE: u64 = 0;
pub const CTL_READ_PAGE: u64 = 1;
pub const CTL_WRITE_PAGE: u64 = 2;
pub const CTL_TAKE_DIRTY: u64 = 3;
pub const CTL_RELEASE: u64 = 4;

const PAGE: usize = 4096;
/// Page indices fit in the 40 bits a reservation keeps them in.
pub const MAX_PAGE: u64 = (1 << 40) - 1;

#[derive(Clone, Copy)]
struct Object {
    in_use: bool,
    /// Never reused, so a capability naming a released object names nothing.
    id: u64,
    pager: usize,
    /// The pager's endpoint number: a task that takes its TID later is not it.
    pager_number: u64,
    cookie: u64,
    bytes: u64,
    /// Page-table entries, present or not, that name this object.
    mapped: u64,
}

impl Object {
    const EMPTY: Object = Object {
        in_use: false,
        id: 0,
        pager: 0,
        pager_number: 0,
        cookie: 0,
        bytes: 0,
        mapped: 0,
    };

    fn pages(&self) -> u64 {
        self.bytes.div_ceil(PAGE as u64)
    }

    fn pager_alive(&self) -> bool {
        self.pager_number != 0 && crate::cap::endpoint_of(self.pager) == self.pager_number
    }
}

static mut OBJECTS: [Object; MAX_OBJECTS] = [Object::EMPTY; MAX_OBJECTS];
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// The page cache: open addressing on `(slot, page)`.
const CACHE_SLOTS: usize = 8192;
/// A free entry, and one whose page has gone (which a search steps past).
const EMPTY_KEY: u64 = 0;
const GONE_KEY: u64 = u64::MAX;

#[derive(Clone, Copy)]
struct Cached {
    key: u64,
    frame: usize,
    dirty: bool,
}

static mut CACHE: [Cached; CACHE_SLOTS] =
    [Cached { key: EMPTY_KEY, frame: 0, dirty: false }; CACHE_SLOTS];

#[inline(always)]
fn irq_save() -> u64 {
    let flags: u64;
    unsafe {
        core::arch::asm!("pushfq; pop {}; cli", out(reg) flags, options(nostack));
    }
    flags
}

#[inline(always)]
fn irq_restore(flags: u64) {
    unsafe {
        core::arch::asm!("push {}; popfq", in(reg) flags, options(nostack));
    }
}

fn objects() -> &'static mut [Object; MAX_OBJECTS] {
    unsafe { &mut *core::ptr::addr_of_mut!(OBJECTS) }
}

fn cache() -> &'static mut [Cached; CACHE_SLOTS] {
    unsafe { &mut *core::ptr::addr_of_mut!(CACHE) }
}

fn key(slot: usize, page: u64) -> u64 {
    ((slot as u64) << 40) | page
}

fn home(key: u64) -> usize {
    (key.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 51) as usize % CACHE_SLOTS
}

/// The cache entry for `key`, if there is one.
fn find(key: u64) -> Option<&'static mut Cached> {
    let c = cache();
    let mut i = home(key);
    for _ in 0..CACHE_SLOTS {
        match c[i].key {
            EMPTY_KEY => return None,
            k if k == key => return Some(&mut c[i]),
            _ => i = (i + 1) % CACHE_SLOTS,
        }
    }
    None
}

/// Add `key -> frame`. False if the cache is full.
fn insert(key: u64, frame: usize) -> bool {
    let c = cache();
    let mut i = home(key);
    for _ in 0..CACHE_SLOTS {
        if c[i].key == EMPTY_KEY || c[i].key == GONE_KEY {
            c[i] = Cached { key, frame, dirty: false };
            return true;
        }
        i = (i + 1) % CACHE_SLOTS;
    }
    false
}

/// A live object in `slot`, by id if `id` is not 0.
fn object(slot: usize, id: u64) -> Option<&'static mut Object> {
    let o = objects().get_mut(slot)?;
    (slot != 0 && o.in_use && (id == 0 || o.id == id)).then_some(o)
}

/// How many objects `pager` pages for.
pub fn objects_of(pager: usize) -> usize {
    let flags = irq_save();
    let n = objects().iter().filter(|o| o.in_use && o.pager == pager && o.pager_alive()).count();
    irq_restore(flags);
    n
}

/// The slot of the object numbered `id`.
pub fn slot_of(id: u64) -> Option<usize> {
    if id == 0 {
        return None;
    }
    let flags = irq_save();
    let slot = objects().iter().position(|o| o.in_use && o.id == id);
    irq_restore(flags);
    slot
}

/// Make an object of `bytes` bytes that `pager` will page in, and which it
/// knows as `cookie`. Returns its slot and id.
pub fn create(pager: usize, cookie: u64, bytes: u64) -> Option<(usize, u64)> {
    let flags = irq_save();
    let found = objects().iter().skip(1).position(|o| !o.in_use).map(|i| i + 1);
    let result = found.map(|slot| {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        objects()[slot] = Object {
            in_use: true,
            id,
            pager,
            pager_number: crate::cap::endpoint_of(pager),
            cookie,
            bytes,
            mapped: 0,
        };
        (slot, id)
    });
    irq_restore(flags);
    result
}

/// Free everything the object in `slot` holds, and the slot.
fn release(slot: usize) {
    let c = cache();
    for e in c.iter_mut() {
        if e.key != EMPTY_KEY && e.key != GONE_KEY && (e.key >> 40) as usize == slot {
            pmm::free(pmm::PhysFrame::from_address(e.frame));
            *e = Cached { key: GONE_KEY, frame: 0, dirty: false };
        }
    }
    objects()[slot] = Object::EMPTY;
}

/// `n` more page-table entries name the object in `slot`.
pub fn map_ref(slot: usize, n: u64) {
    let flags = irq_save();
    if let Some(o) = object(slot, 0) {
        o.mapped += n;
    }
    irq_restore(flags);
}

/// `n` fewer entries name the object in `slot`. When none is left, its pager
/// is told, so that it can write back and release it — or, with the pager
/// gone, it is released here.
pub fn unmap_ref(slot: usize, n: u64) {
    let flags = irq_save();
    if let Some(o) = object(slot, 0) {
        o.mapped = o.mapped.saturating_sub(n);
        if o.mapped == 0 {
            if o.pager_alive() {
                ipc::notify_object_idle(o.pager, o.cookie, o.id);
            } else {
                release(slot);
            }
        }
    }
    irq_restore(flags);
}

/// Task `tid` is being reaped. Objects it paged for cannot be paged in any
/// more; those nothing maps go now, and the rest when their last page does.
pub fn task_gone(tid: usize) {
    let flags = irq_save();
    let number = crate::cap::endpoint_of(tid);
    for slot in 1..MAX_OBJECTS {
        let o = &mut objects()[slot];
        if o.in_use && o.pager == tid && o.pager_number == number {
            o.pager_number = 0;
            if o.mapped == 0 {
                release(slot);
            }
        }
    }
    irq_restore(flags);
}

/// The frame holding page `page` of the object in `slot`, paging it in if it
/// is not cached. May block, calling the pager, unless `may_block` is false.
pub fn page_in(slot: usize, page: u64, may_block: bool) -> Result<usize, Fault> {
    let flags = irq_save();
    let Some(o) = object(slot, 0).copied() else {
        irq_restore(flags);
        return Err(Fault::Bus);
    };
    if let Some(e) = find(key(slot, page)) {
        let frame = e.frame;
        irq_restore(flags);
        return Ok(frame);
    }
    irq_restore(flags);
    // Past the end of the file is SIGBUS, as on Linux.
    if page >= o.pages() || !o.pager_alive() {
        return Err(Fault::Bus);
    }
    if !may_block {
        return Err(Fault::Invalid);
    }

    let frame = pmm::alloc().ok_or(Fault::NoMemory)?.address();
    unsafe { core::ptr::write_bytes(frame as *mut u8, 0, PAGE) };
    let msg = ipc::Message {
        sender: 0,
        tag: ipc::TAG_PAGE_IN,
        data: [o.cookie, page, o.id, 0, 0, 0],
    };
    let answered = ipc::pager_call(o.pager, &msg, frame);
    let filled = matches!(answered, Ok(ref reply) if reply.tag == 0);

    let flags = irq_save();
    let result = match object(slot, o.id) {
        Some(_) if filled => match find(key(slot, page)) {
            // Somebody else paged it in while we waited; theirs is kept.
            Some(e) => {
                pmm::free(pmm::PhysFrame::from_address(frame));
                Ok(e.frame)
            }
            None if insert(key(slot, page), frame) => Ok(frame),
            None => {
                pmm::free(pmm::PhysFrame::from_address(frame));
                Err(Fault::NoMemory)
            }
        },
        _ => {
            pmm::free(pmm::PhysFrame::from_address(frame));
            Err(Fault::Bus)
        }
    };
    irq_restore(flags);
    result
}

/// Page `page` of the object in `slot` has been written through a shared
/// mapping.
pub fn mark_dirty(slot: usize, page: u64) {
    let flags = irq_save();
    if let Some(e) = find(key(slot, page)) {
        e.dirty = true;
    }
    irq_restore(flags);
}

/// `SYS_OBJECT_CTL`: the pager's operations on its own object. `buf` has been
/// validated as a page the caller may read and write.
pub fn ctl(caller: usize, id: u64, op: u64, a: u64, b: u64) -> u64 {
    let flags = irq_save();
    let result = ctl_locked(caller, id, op, a, b);
    irq_restore(flags);
    result
}

fn ctl_locked(caller: usize, id: u64, op: u64, a: u64, b: u64) -> u64 {
    let Some(slot) = objects().iter().position(|o| o.in_use && o.id == id) else {
        return u64::MAX;
    };
    let o = &mut objects()[slot];
    if o.pager != caller || !o.pager_alive() {
        return u64::MAX;
    }
    match op {
        CTL_RESIZE => {
            // Frames past the new end stay cached: nothing says where they
            // are mapped. Paging past the end is refused from now on.
            o.bytes = a;
            0
        }
        CTL_READ_PAGE | CTL_WRITE_PAGE => {
            let Some(e) = find(key(slot, b)) else { return 0 };
            let _ua = crate::cpu::UserAccess::begin();
            unsafe {
                if op == CTL_READ_PAGE {
                    core::ptr::copy_nonoverlapping(e.frame as *const u8, a as *mut u8, PAGE);
                } else {
                    core::ptr::copy_nonoverlapping(a as *const u8, e.frame as *mut u8, PAGE);
                }
            }
            1
        }
        CTL_TAKE_DIRTY => {
            for e in cache().iter_mut() {
                if e.key != EMPTY_KEY && e.key != GONE_KEY && (e.key >> 40) as usize == slot && e.dirty {
                    e.dirty = false;
                    let _ua = crate::cpu::UserAccess::begin();
                    unsafe {
                        core::ptr::copy_nonoverlapping(e.frame as *const u8, a as *mut u8, PAGE);
                    }
                    return e.key & MAX_PAGE;
                }
            }
            u64::MAX
        }
        CTL_RELEASE => {
            if o.mapped != 0 {
                return u64::MAX;
            }
            release(slot);
            0
        }
        _ => u64::MAX,
    }
}
