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
//! of places, and nothing records where — only how many (`pmm::mapped`, the
//! count a frame has): a frame nothing maps, holding nothing its pager has
//! still to write, can be given up when memory is short ([`evict`]) and
//! asked for again when it is wanted. The object itself is released only
//! once nothing maps any of its pages, and its pager has had the chance to
//! write back what it must. The count of mapped entries is kept by the page
//! tables' own walks, which find an object's slot in bits 52–62 of every
//! entry that refers to it.
//!
//! **One object is where memory that is nobody's file goes** ([`swap_slot`]).
//! A page of a program's own that has not been used for a while is moved
//! into that object's cache — the same frame, under a page number the
//! kernel picks — and the program's entry becomes a reservation for it, as
//! if it had mapped a file there and not yet touched the page. From then on
//! it is a dirty page like any other: its pager writes it out when asked,
//! the frame is given up, and the first touch pages it back in. What is
//! different is that the page has one owner, or a few after a `fork`, and
//! is theirs alone again when they touch it: it leaves the cache, and its
//! number is free (`SWAP_REFS`).
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
/// This object is where anonymous memory goes when memory is short.
pub const CTL_SWAP: u64 = 5;
/// As `CTL_TAKE_DIRTY`, but the page is neither clean nor dirty until its
/// pager says how the writing went.
pub const CTL_TAKE_OUT: u64 = 6;
/// A page taken with `CTL_TAKE_OUT` has been written (arg 1) or could not
/// be (0).
pub const CTL_WRITTEN: u64 = 7;
/// `CTL_RELEASE`'s answer while a task other than the pager holds a
/// capability for the object: not now, and nothing will say when.
pub const RELEASE_LATER: u64 = 1;

const PAGE: usize = 4096;
/// Where a present entry keeps its frame's address.
const FRAME_BITS: u64 = 0x000F_FFFF_FFFF_F000;
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
    /// Of those, the ones mapping a cached frame writable. While any does, a
    /// dirty page stays dirty after it is taken: it can change again unseen.
    writable: u64,
    /// One past the highest page of it that has been in the cache: how far
    /// a walk of its pages has to go to have seen every one that is there.
    end: u64,
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
        writable: 0,
        end: 0,
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

/// The page cache: open addressing on `(slot, page)`, in a table made at
/// boot as big as the machine is (`init`). It was 8192 pages whatever the
/// machine, and a program that mapped more than that of files — rustc maps
/// its own code, two hundred megabytes of it — had each page it touched
/// past them take back one it was using: building the kernel on Quark read
/// six gigabytes from the disk in twenty minutes and did not finish.
static mut CACHE: *mut Cached = core::ptr::NonNull::dangling().as_ptr();
/// How many places the table has, a power of two; and how many of them may
/// hold a page at once, three quarters at most, so that a search for a page
/// that is not there ends soon.
static mut SLOTS: usize = 0;
static mut ROOM: usize = 0;
/// Where in the table each frame's entry is, a word for every frame of the
/// machine: what a mapped page of a file is found by when the mapping is
/// taken away (`page_of`), where it used to be a search of the whole table.
static mut PLACES: *mut u32 = core::ptr::NonNull::dangling().as_ptr();
static mut FRAMES: usize = 0;
/// A free entry, and one whose page has gone (which a search steps past).
const EMPTY_KEY: u64 = 0;
const GONE_KEY: u64 = u64::MAX;

#[derive(Clone, Copy)]
struct Cached {
    key: u64,
    frame: usize,
    dirty: bool,
    /// Its pager has a copy and is writing it (`CTL_TAKE_OUT`): not to be
    /// given up until that is known to have worked, and not to be taken
    /// again meanwhile.
    writing: bool,
}

/// Make the cache as big as the machine: room for a quarter of its memory,
/// at least the 8192 pages it had before and at most a million, in a table
/// no more than three quarters full, and the word for each frame saying
/// where in it the frame is. Both are taken from the allocator, before
/// there is an object, and are the kernel's for good; an empty entry and a
/// table of places are both noughts.
pub fn init() {
    let room = (pmm::total_count() / 4).clamp(8192, 1 << 20);
    let slots = (room * 4 / 3 + 1).next_power_of_two();
    let frames = pmm::top_of_memory() / PAGE;
    let table = slots * core::mem::size_of::<Cached>();
    let pages = (table + frames * core::mem::size_of::<u32>()).div_ceil(PAGE);
    let Some(run) = pmm::alloc_contiguous(pages, false) else {
        panic!("no room for the page cache: {} pages", pages);
    };
    let at = run.address();
    unsafe {
        core::ptr::write_bytes(at as *mut u8, 0, pages * PAGE);
        CACHE = at as *mut Cached;
        PLACES = (at + table) as *mut u32;
        SLOTS = slots;
        ROOM = room;
        FRAMES = frames;
    }
}

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

fn cache() -> &'static mut [Cached] {
    unsafe { core::slice::from_raw_parts_mut(CACHE, SLOTS) }
}

fn slots() -> usize {
    unsafe { SLOTS }
}

/// How many pages the cache may hold at once.
pub fn room() -> usize {
    unsafe { ROOM }
}

fn key(slot: usize, page: u64) -> u64 {
    ((slot as u64) << 40) | page
}

/// The top bits of the key's product with the golden ratio, as many as
/// number the table's places.
fn home(key: u64) -> usize {
    (key.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> (64 - slots().trailing_zeros())) as usize
}

/// How many entries of the cache hold a page.
static mut LIVE: usize = 0;

/// Where in the cache `key`'s entry is, if it has one.
fn place(key: u64) -> Option<usize> {
    let c = cache();
    let n = c.len();
    let mut i = home(key);
    for _ in 0..n {
        match c[i].key {
            EMPTY_KEY => return None,
            k if k == key => return Some(i),
            _ => i = (i + 1) % n,
        }
    }
    None
}

/// The cache entry for `key`, if there is one.
fn find(key: u64) -> Option<&'static mut Cached> {
    place(key).map(|i| &mut cache()[i])
}

/// Add `key -> frame`. False if the cache is full, which is asked first:
/// the table is never more than three quarters full, so that a search for
/// a place, or for a page that is not there, ends soon.
fn insert(key: u64, frame: usize) -> bool {
    if unsafe { *core::ptr::addr_of!(LIVE) } >= unsafe { ROOM } {
        return false;
    }
    let c = cache();
    let n = c.len();
    let mut i = home(key);
    for _ in 0..n {
        if c[i].key == EMPTY_KEY || c[i].key == GONE_KEY {
            c[i] = Cached { key, frame, dirty: false, writing: false };
            unsafe { *core::ptr::addr_of_mut!(LIVE) += 1 };
            if let Some(at) = (frame / PAGE < unsafe { FRAMES }).then(|| unsafe { PLACES.add(frame / PAGE) }) {
                unsafe { *at = i as u32 };
            }
            let o = &mut objects()[(key >> 40) as usize];
            o.end = o.end.max((key & MAX_PAGE) + 1);
            return true;
        }
        i = (i + 1) % n;
    }
    false
}

/// Take the entry in place `i` out of the cache.
///
/// It leaves a gap that a search steps past, because the entry it is
/// looking for may have been put beyond this one. Where the next place is
/// empty nothing was, and the gap — with any gaps before it — is empty
/// too. Pages come and go through here all the time now, and gaps that
/// were only ever left would in the end be the whole table: every search
/// for a page that is not there would walk all of it.
fn forget(i: usize) {
    let c = cache();
    let n = c.len();
    c[i] = Cached { key: GONE_KEY, frame: 0, dirty: false, writing: false };
    unsafe { *core::ptr::addr_of_mut!(LIVE) -= 1 };
    if c[(i + 1) % n].key == EMPTY_KEY {
        let mut j = i;
        while c[j].key == GONE_KEY {
            c[j].key = EMPTY_KEY;
            j = (j + n - 1) % n;
        }
    }
}

/// The place of the cached page of the object in `slot` with the lowest
/// number at or after `from` that `wanted` says yes to.
///
/// Its pages are looked up one by one from `from`, for as many as an
/// eighth of the table's places: a pager walking an object's dirty pages
/// asks for the next each time, and finds it soon. Past that the whole
/// table is looked through for the rest, which with a table as big as a
/// quarter of memory is to be done once in a while and not once a page.
fn lowest(slot: usize, from: u64, wanted: impl Fn(&Cached) -> bool) -> Option<usize> {
    let end = objects()[slot].end;
    let walked = end.min(from.saturating_add((slots() / 8) as u64));
    for page in from..walked {
        if let Some(i) = place(key(slot, page)).filter(|&i| wanted(&cache()[i])) {
            return Some(i);
        }
    }
    if walked >= end {
        return None;
    }
    let mut best: Option<usize> = None;
    for (i, e) in cache().iter().enumerate() {
        let live = e.key != EMPTY_KEY && e.key != GONE_KEY;
        if live && (e.key >> 40) as usize == slot && e.key & MAX_PAGE >= walked && wanted(e) {
            let page = e.key & MAX_PAGE;
            if best.is_none_or(|j| cache()[j].key & MAX_PAGE > page) {
                best = Some(i);
            }
        }
    }
    best
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
            writable: 0,
            end: 0,
        };
        (slot, id)
    });
    irq_restore(flags);
    result
}

/// Free everything the object in `slot` holds, and the slot.
fn release(slot: usize) {
    // Nothing maps the object, so nothing maps what it has cached: a count
    // that had stuck at the most it can say is not one to keep a frame for.
    let free = |i: usize| {
        let frame = cache()[i].frame;
        pmm::unmapped_everywhere(frame);
        pmm::free(pmm::PhysFrame::from_address(frame));
        forget(i);
    };
    // By its pages, where it has had fewer than an eighth of the table's
    // places: it nearly always has.
    let end = objects()[slot].end;
    if end <= (slots() / 8) as u64 {
        for page in 0..end {
            if let Some(i) = place(key(slot, page)) {
                free(i);
            }
        }
    } else {
        for i in 0..slots() {
            let e = cache()[i];
            if e.key != EMPTY_KEY && e.key != GONE_KEY && (e.key >> 40) as usize == slot {
                free(i);
            }
        }
    }
    objects()[slot] = Object::EMPTY;
    swap_gone(slot);
}

/// A page-table entry that named an object has been cleared or replaced.
/// `raw` is what it held.
pub fn drop_entry(raw: u64) {
    let slot = crate::paging::object_slot(raw);
    if slot == 0 {
        return;
    }
    let present = raw & crate::paging::PRESENT != 0;
    if present && raw & crate::paging::OWNED == 0 {
        // It mapped a frame of the cache, which has one mapping fewer.
        pmm::unmapped((raw & FRAME_BITS) as usize);
    }
    if !present {
        // A reservation for a page that was written out names its number.
        swap_unref(slot, (raw >> 12) & MAX_PAGE);
    }
    let shared_writable = present
        && raw & crate::paging::WRITABLE != 0
        && raw & crate::paging::OWNED == 0;
    if shared_writable {
        let flags = irq_save();
        if let Some(o) = object(slot, 0) {
            o.writable = o.writable.saturating_sub(1);
        }
        irq_restore(flags);
    }
    unmap_ref(slot, 1);
}

/// A page of the object in `slot` has been mapped writable, straight from the
/// cache: it is dirty, and stays so while mapped that way.
pub fn mapped_writable(slot: usize, page: u64) {
    let flags = irq_save();
    if let Some(o) = object(slot, 0) {
        o.writable += 1;
    }
    if let Some(e) = find(key(slot, page)) {
        e.dirty = true;
    }
    irq_restore(flags);
}

/// The pager and cookie of the object in `slot`, for a sync.
pub fn pager_of(slot: usize) -> Option<(usize, u64, u64)> {
    let flags = irq_save();
    let found = object(slot, 0).filter(|o| o.pager_alive()).map(|o| (o.pager, o.cookie, o.id));
    irq_restore(flags);
    found
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

    let frame = crate::reclaim::frame().ok_or(Fault::NoMemory)?;
    unsafe { core::ptr::write_bytes(frame as *mut u8, 0, PAGE) };
    let msg = ipc::Message {
        sender: 0,
        tag: ipc::TAG_PAGE_IN,
        data: [o.cookie, page, o.id, 0, 0, 0],
    };
    let answered = ipc::pager_call(o.pager, &msg, Some(frame));
    let filled = matches!(answered, Ok(ref reply) if reply.tag == 0);

    let flags = irq_save();
    let result = match object(slot, o.id) {
        Some(_) if filled => match find(key(slot, page)) {
            // Somebody else paged it in while we waited; theirs is kept.
            Some(e) => {
                pmm::free(pmm::PhysFrame::from_address(frame));
                Ok(e.frame)
            }
            None if insert(key(slot, page), frame) => {
                if slot == swap().slot {
                    swap().back += 1;
                }
                Ok(frame)
            }
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
            // The dirty page with the lowest index at or after `b`. A page
            // still mapped writable somewhere stays dirty, so a pager walks
            // on from the page it was given rather than asking again.
            let keep = o.writable != 0;
            let Some(i) = lowest(slot, b, |e| e.dirty) else { return u64::MAX };
            let e = &mut cache()[i];
            if !keep {
                e.dirty = false;
            }
            let _ua = crate::cpu::UserAccess::begin();
            unsafe {
                core::ptr::copy_nonoverlapping(e.frame as *const u8, a as *mut u8, PAGE);
            }
            e.key & MAX_PAGE
        }
        CTL_TAKE_OUT => {
            // The dirty page with the lowest number at or after `b` that is
            // not being written already: copied out, and neither clean nor
            // to be taken again until its pager says how the writing went.
            let Some(i) = lowest(slot, b, |e| e.dirty && !e.writing) else { return u64::MAX };
            let e = &mut cache()[i];
            e.writing = true;
            let _ua = crate::cpu::UserAccess::begin();
            unsafe {
                core::ptr::copy_nonoverlapping(e.frame as *const u8, a as *mut u8, PAGE);
            }
            e.key & MAX_PAGE
        }
        CTL_WRITTEN => {
            // Page `b` was written (`a` = 1) or could not be (0). A page
            // that has gone from the cache meanwhile — its owner touched it
            // again and has it back — is nobody's business any more.
            let keep = o.writable != 0;
            if let Some(e) = find(key(slot, b)) {
                if e.writing {
                    e.writing = false;
                    if a == 1 && !keep {
                        e.dirty = false;
                    }
                }
            }
            0
        }
        CTL_SWAP => {
            if swap_on(slot) { 0 } else { u64::MAX }
        }
        CTL_RELEASE => {
            if o.mapped != 0 {
                return u64::MAX;
            }
            // Somebody has been given a capability for it and has not
            // mapped it yet: the pager answered one program's request to
            // map a file as another unmapped the last of it. Released now,
            // the capability names nothing and the first program is told
            // there is no memory — which two threads mapping one file did
            // to each other, on two processors, a few times in a hundred.
            // The pager asks again; nothing tells it when the capability
            // has been used or given up.
            if crate::cap::memobject_held_elsewhere(id, caller) {
                return RELEASE_LATER;
            }
            release(slot);
            0
        }
        _ => u64::MAX,
    }
}

/// The object memory that is nobody's file is written out to, and what the
/// kernel keeps about its pages: for each page number, how many page-table
/// entries are reservations for it. None is a number that is free.
struct Swap {
    /// The object's slot; 0 while there is none.
    slot: usize,
    /// How many page numbers it has.
    pages: usize,
    /// The count for each, a byte a page, in frames of its own.
    refs: *mut u8,
    /// How many frames that table takes.
    frames: usize,
    /// How many numbers are in use.
    used: usize,
    /// No number below this one is free: where to begin looking. The
    /// lowest free number is the one given, always, so that a pager whose
    /// object is a file has a file as long as the most that was ever out
    /// at once and no longer — given out in turn, every number is used
    /// before any is used twice, and a file asked to be thirty-two
    /// megabytes took all thirty-two from a disk with twenty-four.
    next: usize,
    /// Pages written out, and pages read back from the pager, since the
    /// machine started.
    out: u64,
    back: u64,
}

const NO_SWAP: Swap =
    Swap { slot: 0, pages: 0, refs: core::ptr::null_mut(), frames: 0, used: 0, next: 0, out: 0, back: 0 };
static mut SWAP: Swap = NO_SWAP;

fn swap() -> &'static mut Swap {
    unsafe { &mut *core::ptr::addr_of_mut!(SWAP) }
}

fn swap_refs() -> &'static mut [u8] {
    let s = swap();
    if s.refs.is_null() {
        return &mut [];
    }
    unsafe { core::slice::from_raw_parts_mut(s.refs, s.pages) }
}

/// The slot of the object memory is written out to, or 0 if there is none.
/// It is what a reservation for a written-out page names.
pub fn swap_slot() -> usize {
    swap().slot
}

/// How much there is to write memory out to, and how much of it is in use,
/// in pages.
pub fn swap_room() -> (usize, usize) {
    let flags = irq_save();
    let s = swap();
    let out = if s.slot == 0 { (0, 0) } else { (s.pages, s.used) };
    irq_restore(flags);
    out
}

/// How many pages have been written out, and how many read back from the
/// pager, since the machine started.
pub fn swap_traffic() -> (u64, u64) {
    let flags = irq_save();
    let s = swap();
    let out = (s.out, s.back);
    irq_restore(flags);
    out
}

/// Make the object in `slot` the one memory is written out to. There is one
/// at a time: a second is refused while the first is there.
fn swap_on(slot: usize) -> bool {
    let s = swap();
    if s.slot != 0 {
        return false;
    }
    let pages = objects()[slot].pages().min(MAX_PAGE) as usize;
    if pages == 0 {
        return false;
    }
    let frames = pages.div_ceil(PAGE);
    let Some(table) = pmm::alloc_contiguous(frames, false) else {
        return false;
    };
    unsafe { core::ptr::write_bytes(table.address() as *mut u8, 0, frames * PAGE) };
    *s = Swap { slot, pages, refs: table.address() as *mut u8, frames, ..NO_SWAP };
    true
}

/// The object in `slot` has been released. If it was where memory went,
/// nothing is any more.
fn swap_gone(slot: usize) {
    let s = swap();
    if s.slot != slot || slot == 0 {
        return;
    }
    for i in 0..s.frames {
        pmm::free(pmm::PhysFrame::from_address(s.refs as usize + i * PAGE));
    }
    *s = NO_SWAP;
}

/// Whether memory can be written out at all just now: there is somewhere
/// for it, with a pager to write it and a number to give it.
pub fn swap_ready() -> bool {
    let s = swap();
    s.slot != 0 && s.used < s.pages && object(s.slot, 0).is_some_and(|o| o.pager_alive())
}

/// Move `frame`, a page of some program's own, into the cache of the object
/// memory is written out to. Answers with the page number it was given, for
/// the reservation that replaces the program's entry; `None` if there is no
/// number to give or no room in the cache, and the page stays where it is.
///
/// Interrupts must be off. The caller counts the reservation it writes
/// ([`map_ref`]); the number is counted here.
pub fn swap_out(frame: usize) -> Option<u64> {
    if !swap_ready() {
        return None;
    }
    let s = swap();
    let refs = swap_refs();
    let page = (s.next..s.pages).find(|&i| refs[i] == 0)?;
    if !insert(key(s.slot, page as u64), frame) {
        return None;
    }
    if let Some(e) = find(key(s.slot, page as u64)) {
        e.dirty = true;
    }
    refs[page] = 1;
    s.used += 1;
    s.next = page + 1;
    s.out += 1;
    Some(page as u64)
}

/// One more reservation names written-out page `page` of the object in
/// `slot`: a `fork` copied one.
pub fn swap_ref(slot: usize, page: u64) {
    let s = swap();
    if slot == 0 || slot != s.slot {
        return;
    }
    if let Some(count) = swap_refs().get_mut(page as usize) {
        *count = count.saturating_add(1);
    }
}

/// One fewer does. When none is left the page is nobody's: its number is
/// free, and what the cache holds of it goes.
pub fn swap_unref(slot: usize, page: u64) {
    let flags = irq_save();
    let s = swap();
    if slot != 0 && slot == s.slot {
        if let Some(count) = swap_refs().get_mut(page as usize) {
            if *count != 0 && *count != u8::MAX {
                *count -= 1;
                if *count == 0 {
                    s.used = s.used.saturating_sub(1);
                    s.next = s.next.min(page as usize);
                    if let Some(i) = place(key(slot, page)) {
                        pmm::free(pmm::PhysFrame::from_address(cache()[i].frame));
                        forget(i);
                    }
                }
            }
        }
    }
    irq_restore(flags);
}

/// Take written-out page `page` back out of the cache for the one entry
/// that names it: the frame is that program's own again, and the number is
/// free. `None` if more than one entry names the page — after a `fork` —
/// and each has to be given a copy instead, or if the page is not in the
/// cache (it is asked for first, [`page_in`]).
///
/// Interrupts must be off. The caller gives the reservation's reference
/// back ([`unmap_ref`]).
pub fn swap_take(page: u64) -> Option<usize> {
    let s = swap();
    let refs = swap_refs();
    if s.slot == 0 || refs.get(page as usize).copied() != Some(1) {
        return None;
    }
    let i = place(key(s.slot, page))?;
    let frame = cache()[i].frame;
    forget(i);
    refs[page as usize] = 0;
    s.used = s.used.saturating_sub(1);
    s.next = s.next.min(page as usize);
    Some(frame)
}

/// Where [`evict`] looks next.
static mut HAND: usize = 0;

/// Give up to `want` frames of the cache back to the allocator: pages that
/// nothing maps, that hold nothing still to be written, and whose pager is
/// there to give them again. Answers with how many went.
pub fn evict(want: usize) -> usize {
    let flags = irq_save();
    let c = cache();
    let mut gone = 0;
    let n = c.len();
    let start = unsafe { *core::ptr::addr_of!(HAND) };
    for step in 0..n {
        if gone >= want {
            break;
        }
        let i = (start + step) % n;
        let e = c[i];
        if e.key == EMPTY_KEY || e.key == GONE_KEY || e.dirty || e.writing {
            continue;
        }
        if pmm::shared(e.frame) != 0 {
            continue;
        }
        // A page whose pager has gone cannot be asked for again: what the
        // cache holds is all there is of it.
        let slot = (e.key >> 40) as usize;
        if !object(slot, 0).is_some_and(|o| o.pager_alive()) {
            continue;
        }
        pmm::free(pmm::PhysFrame::from_address(e.frame));
        forget(i);
        gone += 1;
        unsafe { *core::ptr::addr_of_mut!(HAND) = (i + 1) % n };
    }
    irq_restore(flags);
    gone
}

/// Ask every pager that has pages which could be given up if only they were
/// written to write them, and say how many such pages there are — being
/// written already or still to be asked for. Nought is nothing to wait for.
pub fn ask_to_clean() -> usize {
    let flags = irq_save();
    let mut waiting = 0;
    let mut asked = [0usize; 8];
    let mut nasked = 0;
    for e in cache().iter() {
        if e.key == EMPTY_KEY || e.key == GONE_KEY || !e.dirty {
            continue;
        }
        let slot = (e.key >> 40) as usize;
        let Some(o) = object(slot, 0) else { continue };
        // Mapped writable somewhere, it stays dirty whatever is written.
        if !o.pager_alive() || o.writable != 0 || pmm::shared(e.frame) != 0 {
            continue;
        }
        waiting += 1;
        if !e.writing && !asked[..nasked].contains(&o.pager) && nasked < asked.len() {
            asked[nasked] = o.pager;
            nasked += 1;
        }
    }
    for &pager in &asked[..nasked] {
        ipc::notify_clean(pager);
    }
    irq_restore(flags);
    waiting
}

/// The page number of the cache entry holding `frame` for the object in
/// `slot`: what a mapped page of a file is, said the other way round, for
/// whoever is taking a mapping away and needs to leave a reservation that
/// names the page. The frame's place says where to look, and the entry
/// there says whether it is still the frame's.
pub fn page_of(slot: usize, frame: usize) -> Option<u64> {
    let index = frame / PAGE;
    if index >= unsafe { FRAMES } {
        return None;
    }
    let e = cache().get(unsafe { *PLACES.add(index) } as usize)?;
    (e.key != EMPTY_KEY && e.key != GONE_KEY && e.frame == frame && (e.key >> 40) as usize == slot)
        .then_some(e.key & MAX_PAGE)
}
