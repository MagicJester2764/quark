/// Object capability system for the Quark microkernel.
///
/// Each program has a CSpace of slots that grows, which its tasks share as they
/// share its descriptors: a thread holds what its program holds, and what one
/// thread is given the others have. A fork gets a copy. Capabilities are
/// typed objects with parameters (e.g., IoPort with port range, Irq with
/// specific IRQ number). Delegation with attenuation: derived caps must be
/// subsets of the source. O(1) revocation via generation counters.

use core::sync::atomic::{AtomicU64, Ordering};

use crate::task::MAX_TASKS;

/// The most slots a CSpace can have. A task holds one `Endpoint` for each
/// task it calls, and a pager one `MemObject` for each object it pages — a
/// file server, one for every file that is mapped. Sixty-four held a few
/// dozen services' and thirty files', and rustc maps more than thirty files
/// at once to build an archive: building the standard library on Quark, the
/// file server could map no more. Then 256, inline in every program's
/// holding; now room is made as it is wanted (`CSpace`), up to this.
pub const MAX_CAPS: usize = 65_536;
/// The slots a space starts with, when its first is written; and those among
/// which `init`'s are put last (`insert_last`), so that its numbers do not
/// move with the ceiling.
pub const FIRST_CAPS: usize = 256;
/// Where a capability lands when it is given without naming a slot: clear of
/// the fixed slots manifests and spawners use, which are all below 16.
pub const RECEIVED: core::ops::Range<usize> = 16..MAX_CAPS;
pub const MAX_USERS: usize = 64;

/// `CapSlot::root` value meaning "minted by the kernel, never revocable".
///
/// This must not collide with a real space. It used to be 0, but the idle
/// task's is the first — so any cap it minted was silently unrevocable, and
/// `sys_cap_grant` mistook its caps for kernel-minted ones and re-rooted them
/// at the granter. Spaces are numbered below `table::MOST`, so `u16::MAX`
/// names none.
pub const KERNEL_ROOT: u16 = u16::MAX;

/// Per-user default capability bitmask table.
/// USER_CAPS[uid] holds the default cap bits for all tasks running as that UID.
static mut USER_CAPS: [u32; MAX_USERS] = [0; MAX_USERS];

/// Get the UID for a given task TID.
///
/// Used only to look up the per-UID default capability bitmask. UID 0 no
/// longer short-circuits the capability checks themselves.
fn task_uid(tid: usize) -> u32 {
    unsafe {
        crate::scheduler::get_task_mut(tid)
            .map(|t| t.uid)
            .unwrap_or(u32::MAX)
    }
}

/// Get the per-user capability bitmask for a UID.
pub fn user_caps(uid: u32) -> u32 {
    let uid = uid as usize;
    if uid >= MAX_USERS { return 0; }
    unsafe { USER_CAPS[uid] }
}

/// Set the per-user capability bitmask for a UID.
pub fn set_user_caps(uid: u32, caps: u32) {
    let uid = uid as usize;
    if uid >= MAX_USERS { return; }
    unsafe { USER_CAPS[uid] = caps; }
}

/// Check if a task's UID grants a specific capability bit.
fn user_has_cap_bit(tid: usize, bit: u32) -> bool {
    let uid = task_uid(tid);
    if uid == u32::MAX { return false; }
    let uid_idx = uid as usize;
    if uid_idx >= MAX_USERS { return false; }
    unsafe { USER_CAPS[uid_idx] & bit != 0 }
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapType {
    Empty = 0,
    IoPort = 1,     // param0=port_start(u16), param1=port_end(u16)
    PhysRange = 2,  // param0=phys_start, param1=phys_end (page-aligned)
    Irq = 3,        // param0=irq_number (0xFF=wildcard)
    TaskMgmt = 4,   // param0=target_tid (0=any)
    PhysAlloc = 5,  // param0=max_pages (0=unlimited)
    SetUid = 6,     // no params
    // 7 was a set of destination TIDs, withdrawn at ABI 2.0 and never to be
    // reused. A TID outlives the task it named, so every CSpace had to be swept
    // each time a task was reaped, and a set could only ever be narrowed.
    /// Permission to originate IPC to one task.
    ///
    /// param0 is the number of that task's endpoint (`endpoint_of`), not its
    /// TID. Numbers are never reused, so this names the task it was minted for
    /// and nothing that takes its slot later.
    Endpoint = 8,
    /// Permission to map a memory object. param0 is the object's id, which is
    /// never reused; param1 the access, 1 read and 2 write.
    MemObject = 9,
    /// Permission to map the registers of the machine's devices: its holder
    /// may mint a `PhysRange` over any stretch of device memory
    /// (`devmem.rs`), which is where no memory is. No parameters.
    ///
    /// A kind of its own rather than a `PhysRange` over all of it, because
    /// a `PhysRange` is what a task *may map* and is kept as narrow as what
    /// it does map — a device, never a quarter of the address space. Which
    /// address a device is at is known only to whoever reads the device's
    /// configuration, and that is its driver: so the driver holds this and
    /// mints the one range it needs.
    DeviceMemory = 10,
    /// Permission to say what time it is (`SYS_CLOCK_SET`). No parameters.
    ///
    /// The clock is the machine's: every program's idea of the date, every
    /// file's, and what the machine believes when it is next started. Who
    /// may set it is nobody by default and whoever is handed this.
    Clock = 11,
    /// Permission to turn the machine off and to start it again
    /// (`SYS_POWER`). No parameters.
    Power = 12,
    /// Permission to be where memory goes when there is not enough of it:
    /// to make one's object the one programs' unused pages are written out
    /// to (`SYS_OBJECT_CTL`, op 5). No parameters.
    ///
    /// Whoever holds it is handed pages of every program's memory to keep,
    /// and hands them back: it reads all of them and could answer with
    /// anything. It is for one program the system starts, and nobody else.
    Swap = 13,
    /// One PCI device: param0 is its address, `bus << 8 | device << 3 |
    /// function`, or `pci::ANY` for every device.
    ///
    /// Everything about a device goes with it: its configuration, read and
    /// written through the kernel (`SYS_PCI_READ`, `SYS_PCI_WRITE`), what
    /// the kernel found of it (`SYS_PCI_DEVICE`), a `PhysRange` or an
    /// `IoPort` minted inside one of its BARs (`can_mint`), its claim
    /// (`SYS_DEVICE_CLAIM`) and an interrupt for it by message
    /// (`SYS_MSI_ALLOC`). Configuration space used to be reached through
    /// two ports, and whoever held them held every device; the kernel keeps
    /// those ports now (`pci::config_port`). The first task holds every
    /// device and hands each driver its own.
    PciDevice = 14,
    /// Permission to say how the network stack treats what comes in: its
    /// filter, and what else its server keeps for whoever runs the network.
    /// No parameters.
    ///
    /// The kernel acts on it nowhere. A server is offered one with a call
    /// (`SYS_CALL_OFFER`) and believes the call of a program that could:
    /// a right that is the stack's to honour, minted and handed on as the
    /// clock's is, so that who may is said where everything else a session
    /// may do is said.
    NetAdmin = 15,
}

/// A `MemObject`'s access bits.
pub const OBJECT_READ: u64 = 1;
pub const OBJECT_WRITE: u64 = 2;

#[derive(Debug, Clone, Copy)]
pub struct CapSlot {
    pub cap_type: CapType,
    pub generation: u32,
    /// The slot it was minted from, in the space that minted it.
    pub root_slot: u16,
    /// The capability space it was minted from (`root_of`), or
    /// `KERNEL_ROOT`: revoking that slot there revokes this.
    pub root: u16,
    pub param0: u64,
    pub param1: u64,
}

impl CapSlot {
    pub const fn empty() -> Self {
        CapSlot {
            cap_type: CapType::Empty,
            generation: 0,
            root_slot: 0,
            root: KERNEL_ROOT,
            param0: 0,
            param1: 0,
        }
    }
}

/// A program's capability slots, room made as they are written: 256 when
/// the first is, doubled when a slot past the end is wanted or every slot
/// there is is full, up to [`MAX_CAPS`]. A slot's number never moves, and a
/// slot past the room there is reads as empty.
pub struct CSpace {
    slots: crate::grow::Grow<CapSlot>,
}

impl CSpace {
    pub const fn new() -> Self {
        CSpace { slots: crate::grow::Grow::new(CapSlot::empty()) }
    }

    /// How many slots there is room for now.
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// What is in slot `i`: empty past the room there is.
    pub fn get(&self, i: usize) -> CapSlot {
        self.slots.get(i).copied().unwrap_or(CapSlot::empty())
    }

    /// Put `cap` in slot `i`, room made for it. False past [`MAX_CAPS`], or
    /// with no memory for the room.
    pub fn set(&mut self, i: usize, cap: CapSlot) -> bool {
        match self.slots.ensure(i, FIRST_CAPS, MAX_CAPS) {
            Ok(at) => {
                *at = cap;
                true
            }
            Err(_) => false,
        }
    }

    /// Slot `i`, to change, if there is room for it.
    pub fn get_mut(&mut self, i: usize) -> Option<&mut CapSlot> {
        self.slots.get_mut(i)
    }

    pub fn iter(&self) -> impl Iterator<Item = &CapSlot> + '_ {
        self.slots.iter()
    }

    /// The first empty slot from `from`: one there is room for, or else the
    /// first past the end, while there can be one.
    fn first_empty(&self, from: usize) -> Option<usize> {
        (from..self.len())
            .find(|&i| self.get(i).cap_type == CapType::Empty)
            .or_else(|| Some(self.len().max(from)).filter(|&i| i < MAX_CAPS))
    }
}

/// What a program holds: its capability slots, and beside them the older
/// bitmask of capability bits. One for each program, used by every task of
/// it — a thread joins its maker's (`share`), a fork gets a copy of its
/// parent's (`copy_into`), `exec` keeps it — and emptied when the last task
/// using it has gone.
///
/// A task used to hold its own, and a thread began with a copy of its
/// maker's: what one thread was given afterwards the others did not have.
/// A C library that looked a service up in one thread and called it from
/// another was refused, as a program written for this system that took a
/// capability in a worker was.
struct Holding {
    slots: CSpace,
    bits: u32,
    /// How many tasks use it. Given back at nought.
    users: u16,
}

const NO_HOLDING: u16 = u16::MAX;

/// What a holding and a number's count of revocations are made from, in
/// their room (`Table::fill_from`). A holding was six kilobytes, which built
/// on the stack were a kernel stack's; its slots are made as they are
/// written now, and it is a few words.
static EMPTY_HOLDING: crate::table::Template<Holding> =
    crate::table::Template(Some(Holding { slots: CSpace::new(), bits: 0, users: 0 }));
static NO_REVOCATIONS: crate::table::Template<crate::grow::Grow<u32>> =
    crate::table::Template(Some(crate::grow::Grow::new(0)));

/// Every program's holding, by number (`table.rs`): made with a task, which
/// may then join its program's, and given back when the last task using it
/// has gone.
static mut HOLDINGS: crate::table::Table<Holding> = crate::table::Table::new(MAX_TASKS);

/// # Safety
/// Interrupts are off.
#[inline(always)]
unsafe fn holdings() -> &'static mut crate::table::Table<Holding> {
    unsafe { &mut *core::ptr::addr_of_mut!(HOLDINGS) }
}

/// What `tid`'s program holds.
///
/// # Safety
/// Interrupts are off for as long as the record is used.
unsafe fn holding_of(tid: usize) -> Option<&'static mut Holding> {
    unsafe {
        let i = st(tid).holding;
        if i == NO_HOLDING { None } else { holdings().get(i as usize) }
    }
}
/// What this module keeps about a task, in its record (`TaskRec::cap`):
/// each field was an array of `MAX_TASKS`, and an id with no task reads as
/// `PerTask::new()` — what an empty slot of those arrays held.
pub struct PerTask {
    /// Which holding the task uses.
    holding: u16,
    /// Each task's endpoint, as a number no endpoint has had before or will again.
    ///
    /// An `Endpoint` capability records this, not the TID, so it names the task it
    /// was minted for and nothing else. When that task is reaped its number is
    /// gone, and a capability holding it names nothing; whatever takes the slot
    /// next has a number of its own. The TID sets needed a sweep of every CSpace at
    /// reap time to approximate that.
    ///
    /// Numbers start past the last TID, so that one passed where the other belongs
    /// fails a bounds check rather than naming some task.
    endpoint: u64,
}

impl PerTask {
    pub const fn new() -> Self {
        PerTask {
            holding: NO_HOLDING,
            endpoint: 0,
        }
    }
}

/// What this module keeps about task `tid`: its record's, or for an id with
/// no task a copy put back to `PerTask::new()` each time it is asked for, so
/// a write for a task that is not there goes nowhere. Interrupts must be off.
unsafe fn st(tid: usize) -> &'static mut PerTask {
    unsafe {
        match crate::scheduler::rec(tid) {
            Some(r) => &mut r.cap,
            None => {
                let none = &mut *core::ptr::addr_of_mut!(NO_TASK);
                *none = PerTask::new();
                none
            }
        }
    }
}

static mut NO_TASK: PerTask = PerTask::new();


#[inline(always)]
fn irq_save() -> u64 {
    let flags: u64;
    unsafe { core::arch::asm!("pushfq; pop {}; cli", out(reg) flags, options(nostack)) };
    flags
}

#[inline(always)]
fn irq_restore(flags: u64) {
    unsafe { core::arch::asm!("push {}; popfq", in(reg) flags, options(nostack)) };
}

/// The space `tid` uses, as a capability's root names it: `KERNEL_ROOT` for
/// none.
pub fn root_of(tid: usize) -> u16 {
    if tid >= MAX_TASKS {
        return KERNEL_ROOT;
    }
    let flags = irq_save();
    let i = unsafe { st(tid).holding };
    irq_restore(flags);
    if i == NO_HOLDING { KERNEL_ROOT } else { i }
}

/// The capability slots `tid`'s program holds.
///
/// # Safety
/// Interrupts are off, for as long as the reference is used.
pub unsafe fn cspace_of(tid: usize) -> Option<&'static mut CSpace> {
    unsafe { holding_of(tid).map(|h| &mut h.slots) }
}

/// What is in slot `slot` of `tid`'s program's space: a copy.
pub fn slot(tid: usize, slot: usize) -> Option<CapSlot> {
    if slot >= MAX_CAPS {
        return None;
    }
    let flags = irq_save();
    let cap = unsafe { cspace_of(tid).map(|cs| cs.get(slot)) };
    irq_restore(flags);
    cap
}

/// How many slots `tid`'s program's space has room for: every slot past that
/// is empty.
pub fn room_of(tid: usize) -> Option<usize> {
    let flags = irq_save();
    let room = unsafe { cspace_of(tid).map(|cs| cs.len()) };
    irq_restore(flags);
    room
}

/// Run `f` on `tid`'s program's space, with interrupts off: what is looked
/// at and what is changed are one step.
pub fn with_cspace<R>(tid: usize, f: impl FnOnce(&mut CSpace) -> R) -> Option<R> {
    let flags = irq_save();
    let out = unsafe { cspace_of(tid).map(f) };
    irq_restore(flags);
    out
}

/// The older capability bits `tid`'s program holds.
pub fn bits_of(tid: usize) -> u32 {
    let flags = irq_save();
    let bits = unsafe { holding_of(tid).map_or(0, |h| h.bits) };
    irq_restore(flags);
    bits
}

/// Give `tid`'s program the capability bits `bits`, and the capabilities
/// they stand for.
pub fn add_bits(tid: usize, bits: u32) -> bool {
    let flags = irq_save();
    let done = unsafe {
        match holding_of(tid) {
            Some(h) => {
                h.bits |= bits;
                populate_from_bitmask(&mut h.slots, bits);
                true
            }
            None => false,
        }
    };
    irq_restore(flags);
    done
}

/// A task has been made: a space of its own, empty, until it is given
/// things or joins its program's.
pub fn task_made(tid: usize) {
    if tid >= MAX_TASKS {
        return;
    }
    let flags = irq_save();
    unsafe {
        leave(tid);
        // Its own number first: nothing else is using it, unless a program
        // whose first task had it is still running. The number's count of
        // revocations is made with its first holding and kept for good
        // (`GENERATIONS`); with no memory for either, it holds nothing.
        let free = if holdings().used(tid) { holdings().lowest_free(0) } else { Some(tid) };
        if let Some(i) = free {
            let counted = generations().used(i) || generations().fill_from(i, &NO_REVOCATIONS, |_| {}).is_ok();
            if counted && holdings().fill_from(i, &EMPTY_HOLDING, |h| h.users = 1).is_ok() {
                st(tid).holding = i as u16;
            }
        }
    }
    irq_restore(flags);
}

/// `tid` has gone: it uses no space now, and one nobody uses is emptied.
pub fn task_gone(tid: usize) {
    if tid >= MAX_TASKS {
        return;
    }
    let flags = irq_save();
    unsafe { leave(tid) };
    irq_restore(flags);
}

/// # Safety
/// Interrupts are off.
unsafe fn leave(tid: usize) {
    unsafe {
        let i = st(tid).holding;
        if i == NO_HOLDING {
            return;
        }
        st(tid).holding = NO_HOLDING;
        if let Some(h) = holdings().get(i as usize) {
            h.users = h.users.saturating_sub(1);
            if h.users == 0 {
                holdings().empty(i as usize);
            }
        }
    }
}

/// `tid` is a thread of `with`'s program from now on, and holds what the
/// program holds. Anything it was given before it started is the program's
/// too, in a slot free there.
pub fn share(tid: usize, with: usize) -> bool {
    if tid >= MAX_TASKS || with >= MAX_TASKS || tid == with {
        return false;
    }
    let flags = irq_save();
    let done = unsafe {
        let target = st(with).holding;
        if target == NO_HOLDING {
            false
        } else {
            let own = st(tid).holding;
            if own != target {
                if let Some(mine) = holdings().get(own as usize) {
                    if let Some(h) = holdings().get(target as usize) {
                        h.bits |= mine.bits;
                        for cap in mine.slots.iter().filter(|c| c.cap_type != CapType::Empty) {
                            if let Some(slot) = find_empty_slot(&h.slots) {
                                h.slots.set(slot, *cap);
                            }
                        }
                    }
                    leave(tid);
                }
                if let Some(h) = holdings().get(target as usize) {
                    h.users += 1;
                }
                st(tid).holding = target;
            }
            true
        }
    };
    irq_restore(flags);
    done
}

/// `tid` holds a copy of what `from`'s program holds, in the slots its own
/// has free: a forked child's. False if there was no memory for all of it —
/// a child that could not have everything its parent holds is not made.
pub fn copy_into(tid: usize, from: usize) -> bool {
    if tid >= MAX_TASKS || from >= MAX_TASKS || tid == from {
        return false;
    }
    let flags = irq_save();
    let whole = unsafe {
        let (src, dst) = (st(from).holding, st(tid).holding);
        match (holdings().get(src as usize), holdings().get(dst as usize)) {
            (Some(copy), Some(h)) if src != dst => {
                let mut whole = true;
                for (slot, cap) in copy.slots.iter().enumerate() {
                    if cap.cap_type != CapType::Empty && h.slots.get(slot).cap_type == CapType::Empty {
                        whole &= h.slots.set(slot, *cap);
                    }
                }
                h.bits |= copy.bits;
                whole
            }
            _ => false,
        }
    };
    irq_restore(flags);
    whole
}

/// How many times each slot of each space has been revoked, by the space's
/// number: what a capability minted from a slot carries (`generation`), and
/// what revoking the slot moves on, so that everything minted from it before
/// is no longer valid (`is_valid`). O(1) revocation.
///
/// Made the first time a holding has the number, and kept for good. A
/// holding is given back when its program goes; counts made afresh with the
/// next holding there would start at nought again, and every capability
/// revoked at a count the new one passes through would be valid once more.
/// A number's counts are room made as its slots are revoked, as a space's
/// slots are as they are written: a slot past the room there is has been
/// revoked no times.
static mut GENERATIONS: crate::table::Table<crate::grow::Grow<u32>> = crate::table::Table::new(MAX_TASKS);

/// # Safety
/// Interrupts are off.
#[inline(always)]
unsafe fn generations() -> &'static mut crate::table::Table<crate::grow::Grow<u32>> {
    unsafe { &mut *core::ptr::addr_of_mut!(GENERATIONS) }
}

/// Validate that a cap slot is still valid (not revoked).
fn is_valid(cap: &CapSlot) -> bool {
    if cap.cap_type as u8 == CapType::Empty as u8 {
        return false;
    }
    // Kernel-minted caps are always valid — nothing can revoke them.
    if cap.root == KERNEL_ROOT {
        return true;
    }
    let slot = cap.root_slot as usize;
    if cap.root as usize >= MAX_TASKS || slot >= MAX_CAPS {
        return false;
    }
    cap.generation == generation_at(cap.root, slot)
}

/// Check if a task has IoPort capability covering the given port.
pub fn task_has_ioport(tid: usize, port: u16) -> bool {
    if tid >= MAX_TASKS { return false; }
    if user_has_cap_bit(tid, crate::task::CAP_IOPORT) { return true; }
    unsafe {
        let cspace = task_cspace(tid);
        match cspace {
            Some(cs) => cs.iter().any(|cap| {
                cap.cap_type as u8 == CapType::IoPort as u8
                    && is_valid(cap)
                    && port >= cap.param0 as u16
                    && port <= cap.param1 as u16
            }),
            None => false,
        }
    }
}

/// Check if a task has Irq capability for the given IRQ number.
pub fn task_has_irq(tid: usize, irq: u8) -> bool {
    if tid >= MAX_TASKS { return false; }
    if user_has_cap_bit(tid, crate::task::CAP_IRQ) { return true; }
    unsafe {
        let cspace = task_cspace(tid);
        match cspace {
            Some(cs) => cs.iter().any(|cap| {
                cap.cap_type as u8 == CapType::Irq as u8
                    && is_valid(cap)
                    && (cap.param0 as u8 == 0xFF || cap.param0 as u8 == irq)
            }),
            None => false,
        }
    }
}

/// Check if a task has PhysRange capability covering [phys, phys + pages*4096).
///
/// Only a capability counts. The per-UID `CAP_MAP_PHYS` bit used to as well,
/// and meant all of memory to every task of that user.
pub fn task_has_phys_range(tid: usize, phys: usize, pages: usize) -> bool {
    if tid >= MAX_TASKS { return false; }
    // Checked: a wrapped `phys_end` would compare below `cap.param1` and let
    // an arbitrary physical range through.
    let phys_end = match pages
        .checked_mul(4096)
        .and_then(|len| phys.checked_add(len))
    {
        Some(e) => e as u64,
        None => return false,
    };
    unsafe {
        let cspace = task_cspace(tid);
        match cspace {
            Some(cs) => cs.iter().any(|cap| {
                cap.cap_type as u8 == CapType::PhysRange as u8
                    && is_valid(cap)
                    && phys as u64 >= cap.param0
                    && phys_end <= cap.param1
            }),
            None => false,
        }
    }
}

/// Check if a task has TaskMgmt capability for the given target TID.
/// target=0 means "any task" (for create/generic operations).
pub fn task_has_task_mgmt(tid: usize, target: usize) -> bool {
    if tid >= MAX_TASKS { return false; }
    if user_has_cap_bit(tid, crate::task::CAP_TASK_MGMT) { return true; }
    unsafe {
        let cspace = task_cspace(tid);
        match cspace {
            Some(cs) => cs.iter().any(|cap| {
                cap.cap_type as u8 == CapType::TaskMgmt as u8
                    && is_valid(cap)
                    && (cap.param0 == 0 || cap.param0 == target as u64)
            }),
            None => false,
        }
    }
}

/// Check if a task has PhysAlloc capability.
pub fn task_has_phys_alloc(tid: usize) -> bool {
    if tid >= MAX_TASKS { return false; }
    if user_has_cap_bit(tid, crate::task::CAP_PHYS_ALLOC) { return true; }
    unsafe {
        let cspace = task_cspace(tid);
        match cspace {
            Some(cs) => cs.iter().any(|cap| {
                cap.cap_type as u8 == CapType::PhysAlloc as u8
                    && is_valid(cap)
            }),
            None => false,
        }
    }
}

static NEXT_ENDPOINT: AtomicU64 = AtomicU64::new(MAX_TASKS as u64);

/// Give `tid` a fresh endpoint. Called when a task slot is filled.
pub fn open_endpoint(tid: usize) {
    if tid < MAX_TASKS {
        let number = NEXT_ENDPOINT.fetch_add(1, Ordering::Relaxed);
        unsafe { st(tid).endpoint = number };
    }
}

/// `tid`'s endpoint is gone, and its number with it. Called when it is reaped.
pub fn close_endpoint(tid: usize) {
    if tid < MAX_TASKS {
        unsafe { st(tid).endpoint = 0 };
    }
}

/// The number of `tid`'s endpoint, or 0 if there is no such task.
pub fn endpoint_of(tid: usize) -> u64 {
    if tid < MAX_TASKS { unsafe { st(tid).endpoint } } else { 0 }
}

/// Check if `tid` may originate IPC to `dest`.
///
/// Only gates messages where the sender names the destination itself
/// (sys_send/sys_call/sys_notify). IPC the kernel performs on a task's behalf
/// through an installed file descriptor is authorised by the fd itself, which
/// only a CAP_TASK_MGMT holder can install.
pub fn task_has_endpoint(tid: usize, dest: usize) -> bool {
    if tid >= MAX_TASKS || dest >= MAX_TASKS {
        return false;
    }
    let number = endpoint_of(dest);
    if number == 0 {
        return false;
    }
    unsafe {
        match task_cspace(tid) {
            Some(cs) => cs.iter().any(|cap| {
                cap.cap_type == CapType::Endpoint && cap.param0 == number && is_valid(cap)
            }),
            None => false,
        }
    }
}

/// The number an `Endpoint` to `target` would record, if `caller` may mint
/// one: `caller` is `target`, created it, or already holds a capability to it.
///
/// That is ownership, not a special case. The TID sets this replaced needed
/// one — any task could add its own TID to a set — because a set could only be
/// narrowed, and a server started at run time was in nobody's.
pub fn endpoint_to_mint(cspace: &CSpace, caller: usize, target: usize) -> Option<u64> {
    let number = endpoint_of(target);
    if number == 0 {
        return None;
    }
    let holds = cspace
        .iter()
        .any(|cap| cap.cap_type == CapType::Endpoint && cap.param0 == number && is_valid(cap));
    if caller == target || crate::scheduler::parent_of(target) == Some(caller) || holds {
        Some(number)
    } else {
        None
    }
}

/// Check if a task has SetUid capability.
pub fn task_has_set_uid(tid: usize) -> bool {
    if tid >= MAX_TASKS { return false; }
    if user_has_cap_bit(tid, crate::task::CAP_SET_UID) { return true; }
    unsafe {
        let cspace = task_cspace(tid);
        match cspace {
            Some(cs) => cs.iter().any(|cap| {
                cap.cap_type as u8 == CapType::SetUid as u8
                    && is_valid(cap)
            }),
            None => false,
        }
    }
}

/// Check if a task has the Clock capability.
pub fn task_has_clock(tid: usize) -> bool {
    if tid >= MAX_TASKS { return false; }
    unsafe {
        match task_cspace(tid) {
            Some(cs) => cs.iter().any(|cap| cap.cap_type as u8 == CapType::Clock as u8 && is_valid(cap)),
            None => false,
        }
    }
}

/// Check if a task has the Swap capability.
pub fn task_has_swap(tid: usize) -> bool {
    if tid >= MAX_TASKS { return false; }
    unsafe {
        match task_cspace(tid) {
            Some(cs) => cs.iter().any(|cap| cap.cap_type as u8 == CapType::Swap as u8 && is_valid(cap)),
            None => false,
        }
    }
}

/// Check if a task has the Power capability.
pub fn task_has_power(tid: usize) -> bool {
    if tid >= MAX_TASKS { return false; }
    unsafe {
        match task_cspace(tid) {
            Some(cs) => cs.iter().any(|cap| cap.cap_type as u8 == CapType::Power as u8 && is_valid(cap)),
            None => false,
        }
    }
}

/// Public wrapper over the revocation check, for callers outside this module.
pub fn slot_is_valid(cap: &CapSlot) -> bool {
    is_valid(cap)
}

/// Whether a task that is alive, and not of `pager`'s program, holds a
/// capability for memory object `id`: somebody who may be about to map it.
/// The pager's own threads hold copies of whatever it held when they were
/// made, and are the pager.
///
/// Looked for, not counted. Capabilities are plain words in each task's
/// table, deleted and overwritten in half a dozen places; a count kept
/// beside them would be one more thing for each of those to get wrong, and
/// this is asked only when a pager lets go of an object.
pub fn memobject_held_elsewhere(id: u64, pager: usize) -> bool {
    let own = crate::scheduler::space_of_task(pager);
    let flags = irq_save();
    let held = crate::scheduler::tids().filter(|&tid| tid >= 1).any(|tid| {
        tid != pager
            && crate::scheduler::task_is_live(tid)
            && (own == 0 || crate::scheduler::space_of_task(tid) != own)
            && {
                let flags = irq_save();
                let held = unsafe {
                    cspace_of(tid).is_some_and(|cs| {
                        cs.iter().any(|c| {
                            c.cap_type as u8 == CapType::MemObject as u8 && c.param0 == id && is_valid(c)
                        })
                    })
                };
                irq_restore(flags);
                held
            }
    });
    irq_restore(flags);
    held
}

/// Insert a typed capability into the first free slot of `tid`'s CSpace,
/// rooted at `granter` so it can be revoked later.
///
/// Returns false if the task does not exist or has no free slot.
pub fn grant_slot(
    tid: usize,
    cap_type: CapType,
    param0: u64,
    param1: u64,
    granter: usize,
) -> bool {
    if tid >= MAX_TASKS || granter >= MAX_TASKS {
        return false;
    }
    let root = root_of(granter);
    let flags = irq_save();
    let done = unsafe {
        match cspace_of(tid) {
            Some(cs) => match find_empty_slot(cs).and_then(|slot| Some((slot, generation_for(root, slot)?))) {
                Some((slot, generation)) => cs.set(
                    slot,
                    CapSlot { cap_type, generation, root_slot: slot as u16, root, param0, param1 },
                ),
                None => false,
            },
            None => false,
        }
    };
    irq_restore(flags);
    done
}

/// Find an empty slot in a task's CSpace: one it has room for, or else the
/// first past its room, which writing it makes. `None` once it is as big as
/// a space can be and full.
pub fn find_empty_slot(cspace: &CSpace) -> Option<usize> {
    cspace.first_empty(0)
}

/// Where `cap` lands when it is given without naming a slot: wherever `cspace`
/// already holds the same endpoint, so asking for a service twice costs
/// nothing, or else the first empty slot in `RECEIVED`.
pub fn receive_slot(cspace: &CSpace, cap: &CapSlot) -> Option<usize> {
    if cap.cap_type == CapType::Endpoint {
        let held = cspace.iter().position(|c| {
            c.cap_type == CapType::Endpoint && c.param0 == cap.param0 && is_valid(c)
        });
        if held.is_some() {
            return held;
        }
    }
    cspace.first_empty(RECEIVED.start)
}

/// The copy another task receives of `src`, which `granter` holds in `slot`.
///
/// It keeps the provenance of what it was copied from, so revoking that
/// revokes this too. A capability the kernel minted has no root to be revoked
/// through, so the granter becomes its root — and `None` with no memory for
/// that slot's count, since a copy that could not be revoked is not made.
pub fn derive(granter: usize, slot: usize, src: &CapSlot) -> Option<CapSlot> {
    let (root, root_slot, generation) = if src.root == KERNEL_ROOT {
        let root = root_of(granter);
        (root, slot as u16, generation_for(root, slot)?)
    } else {
        (src.root, src.root_slot, generation_at(src.root, src.root_slot as usize))
    };
    Some(CapSlot {
        cap_type: src.cap_type,
        generation,
        root_slot,
        root,
        param0: src.param0,
        param1: src.param1,
    })
}

/// Insert a cap into a specific slot. False if there is no room for it.
pub fn insert_cap(cspace: &mut CSpace, slot: usize, cap: CapSlot) -> bool {
    slot < MAX_CAPS && cspace.set(slot, cap)
}

/// Populate CSpace from old-style bitmask caps (for backward compatibility).
/// Inserts wildcard/full-range caps matching the bitmask bits.
pub fn populate_from_bitmask(cspace: &mut CSpace, caps: u32) {
    if caps & crate::task::CAP_IOPORT != 0 {
        if let Some(slot) = find_empty_slot(cspace) {
            cspace.set(
                slot,
                CapSlot {
                    cap_type: CapType::IoPort,
                    generation: 0,
                    root_slot: 0,
                    root: KERNEL_ROOT,
                    param0: 0,        // port_start
                    param1: 0xFFFF,   // port_end
                },
            );
        }
    }
    // `CAP_MAP_PHYS` expands to nothing. It used to be a PhysRange over all
    // four gigabytes, which is how a bit handed over with SYS_GRANT_CAP or
    // SYS_CAP_TRANSFER undid every narrow grant made alongside it. Physical
    // memory is granted as the range it is; see `insert_kernel_range`.
    if caps & crate::task::CAP_IRQ != 0 {
        if let Some(slot) = find_empty_slot(cspace) {
            cspace.set(
                slot,
                CapSlot {
                    cap_type: CapType::Irq,
                    generation: 0,
                    root_slot: 0,
                    root: KERNEL_ROOT,
                    param0: 0xFF, // wildcard
                    param1: 0,
                },
            );
        }
    }
    if caps & crate::task::CAP_TASK_MGMT != 0 {
        if let Some(slot) = find_empty_slot(cspace) {
            cspace.set(
                slot,
                CapSlot {
                    cap_type: CapType::TaskMgmt,
                    generation: 0,
                    root_slot: 0,
                    root: KERNEL_ROOT,
                    param0: 0, // any target
                    param1: 0,
                },
            );
        }
    }
    if caps & crate::task::CAP_PHYS_ALLOC != 0 {
        if let Some(slot) = find_empty_slot(cspace) {
            cspace.set(
                slot,
                CapSlot {
                    cap_type: CapType::PhysAlloc,
                    generation: 0,
                    root_slot: 0,
                    root: KERNEL_ROOT,
                    param0: 0, // unlimited
                    param1: 0,
                },
            );
        }
    }
    // `CAP_ENDPOINT` expands to nothing either. It was a set naming every
    // task; an endpoint is minted for the task it names, by that task, its
    // creator, or a holder.
    if caps & crate::task::CAP_SET_UID != 0 {
        if let Some(slot) = find_empty_slot(cspace) {
            cspace.set(
                slot,
                CapSlot {
                    cap_type: CapType::SetUid,
                    generation: 0,
                    root_slot: 0,
                    root: KERNEL_ROOT,
                    param0: 0,
                    param1: 0,
                },
            );
        }
    }
}

/// Give `cspace` an unrevocable capability over the pages covering
/// `[base, base + len)`, in its first free slot. False if none is free.
pub fn insert_kernel_range(cspace: &mut CSpace, base: usize, len: usize) -> bool {
    let start = base & !0xFFF;
    let end = (base + len + 0xFFF) & !0xFFF;
    match find_empty_slot(cspace) {
        Some(slot) => cspace.set(
            slot,
            CapSlot {
                cap_type: CapType::PhysRange,
                generation: 0,
                root_slot: 0,
                root: KERNEL_ROOT,
                param0: start as u64,
                param1: end as u64,
            },
        ),
        None => false,
    }
}

/// Give `cspace` an unrevocable capability of a kind that takes one
/// parameter or none — the right to map the machine's devices' registers
/// ([`CapType::DeviceMemory`]), the right to set its clock
/// ([`CapType::Clock`]), the right to turn it off ([`CapType::Power`]), the
/// right to keep what is written out of memory ([`CapType::Swap`]), the
/// right to run its network ([`CapType::NetAdmin`]), every
/// PCI device ([`CapType::PciDevice`], `pci::ANY`) — in its last free
/// slot of the first [`FIRST_CAPS`]. The first task names its
/// low slots itself — where it keeps the nameserver's endpoint, where it
/// mints what it hands on — and counts on the ones it has not filled being
/// empty; what the kernel adds to what it starts with goes where the task
/// will not look for room. Of the first 256 and not of the space's room, so
/// that the numbers `init` was given stay where they were when the space
/// could grow.
pub fn insert_last(cspace: &mut CSpace, cap_type: CapType, param0: u64) -> bool {
    match (0..FIRST_CAPS).rev().find(|&i| cspace.get(i).cap_type == CapType::Empty) {
        Some(slot) => cspace.set(
            slot,
            CapSlot {
                cap_type,
                generation: 0,
                root_slot: 0,
                root: KERNEL_ROOT,
                param0,
                param1: 0,
            },
        ),
        None => false,
    }
}

/// Get a reference to a task's CSpace via the scheduler.
///
/// # Safety
/// Must be called with the task table accessible.
unsafe fn task_cspace(tid: usize) -> Option<&'static CSpace> { unsafe {
    cspace_of(tid).map(|c| &*c)
}}

/// Validate attenuation: new cap must be a subset of source cap.
pub fn validate_attenuation(source: &CapSlot, new_type: CapType, new_p0: u64, new_p1: u64) -> bool {
    if source.cap_type as u8 != new_type as u8 {
        return false;
    }
    if !is_valid(source) {
        return false;
    }
    match new_type {
        CapType::Empty => false,
        CapType::IoPort => {
            // new range must be non-inverted and within the source range
            new_p0 <= new_p1 && new_p0 >= source.param0 && new_p1 <= source.param1
        }
        CapType::PhysRange => {
            new_p0 <= new_p1 && new_p0 >= source.param0 && new_p1 <= source.param1
        }
        CapType::Irq => {
            // wildcard can narrow to specific; specific must match
            if source.param0 as u8 == 0xFF {
                // Wildcard source covers every IRQ, so any request is a subset.
                true
            } else {
                new_p0 == source.param0
            }
        }
        CapType::TaskMgmt => {
            // any (0) covers every target, so any request is a subset
            if source.param0 == 0 {
                true
            } else {
                new_p0 == source.param0
            }
        }
        CapType::PhysAlloc => {
            // 0 = unlimited (largest). If source is unlimited, anything goes.
            // If source has a limit, new must be <= source limit.
            if source.param0 == 0 {
                true
            } else if new_p0 == 0 {
                false // can't escalate to unlimited
            } else {
                new_p0 <= source.param0
            }
        }
        CapType::SetUid => true,
        // One endpoint: the same one, or nothing.
        CapType::Endpoint => new_p0 == source.param0,
        CapType::DeviceMemory => true,
        CapType::Clock => true,
        CapType::Power => true,
        CapType::Swap => true,
        CapType::NetAdmin => true,
        // Every device covers each one; one covers itself.
        CapType::PciDevice => {
            (new_p0 <= 0xFFFF || new_p0 == crate::pci::ANY)
                && (source.param0 == crate::pci::ANY || new_p0 == source.param0)
        }
        // The same object, with no access the source lacks.
        CapType::MemObject => {
            new_p0 == source.param0
                && new_p1 & !(OBJECT_READ | OBJECT_WRITE) == 0
                && new_p1 & !source.param1 == 0
        }
    }
}

/// Revoke a cap slot of `tid`'s program: bump the generation counter so all
/// derived caps become invalid.
pub fn revoke(tid: usize, slot: usize) {
    let root = root_of(tid) as usize;
    if root >= MAX_TASKS || slot >= MAX_CAPS {
        return;
    }
    let flags = irq_save();
    unsafe {
        // A slot's count is made when a capability is first minted from it
        // (`generation_for`): one with none has had nothing derived from it,
        // and has nothing to take back.
        if let Some(g) = generations().get(root).and_then(|g| g.get_mut(slot)) {
            *g = g.wrapping_add(1);
        }
    }
    irq_restore(flags);
}

/// Mint a new cap: find a source cap of the same type in the caller's CSpace
/// that is a superset of the requested params. An `Endpoint` is minted on
/// ownership instead; see `endpoint_to_mint`.
pub fn can_mint(cspace: &CSpace, cap_type: CapType, param0: u64, param1: u64) -> bool {
    cspace.iter().any(|cap| validate_attenuation(cap, cap_type, param0, param1))
        // Or a range of physical memory from the right to device memory:
        // one that lies in it, whole.
        || (cap_type as u8 == CapType::PhysRange as u8
            && cspace.iter().any(|cap| cap.cap_type as u8 == CapType::DeviceMemory as u8 && is_valid(cap))
            && crate::devmem::covers(param0, param1))
        // Or from a device: a range inside one of its BARs, of memory or of
        // ports.
        || (cap_type as u8 == CapType::PhysRange as u8
            && crate::pci::memory_covers(param0, param1, |bdf| holds_device(cspace, bdf as u64)))
        || (cap_type as u8 == CapType::IoPort as u8
            && crate::pci::ports_cover(param0, param1, |bdf| holds_device(cspace, bdf as u64)))
}

/// Whether `cspace` holds PCI device `bdf`, or every device.
fn holds_device(cspace: &CSpace, bdf: u64) -> bool {
    cspace.iter().any(|cap| {
        cap.cap_type as u8 == CapType::PciDevice as u8
            && is_valid(cap)
            && (cap.param0 == crate::pci::ANY || cap.param0 == bdf)
    })
}

/// Whether task `tid`'s program holds PCI device `bdf`, or every device.
pub fn task_has_pci_device(tid: usize, bdf: u64) -> bool {
    if tid >= MAX_TASKS || bdf > 0xFFFF {
        return false;
    }
    unsafe { task_cspace(tid).is_some_and(|cs| holds_device(cs, bdf)) }
}

/// The count of revocations of slot `slot` of space `root`, made if it has
/// none yet: what a capability minted from that slot carries. `None` with no
/// memory for it — a capability that could not be revoked is not made.
pub fn generation_for(root: u16, slot: usize) -> Option<u32> {
    if root as usize >= MAX_TASKS || slot >= MAX_CAPS {
        return None;
    }
    let flags = irq_save();
    let g = unsafe {
        generations().get(root as usize).and_then(|g| g.ensure(slot, FIRST_CAPS, MAX_CAPS).ok()).map(|g| *g)
    };
    irq_restore(flags);
    g
}

/// The current generation of slot `slot` of space `root`.
fn generation_at(root: u16, slot: usize) -> u32 {
    if root as usize >= MAX_TASKS || slot >= MAX_CAPS {
        return 0;
    }
    let flags = irq_save();
    let g = unsafe { generations().get(root as usize).and_then(|g| g.get(slot).copied()).unwrap_or(0) };
    irq_restore(flags);
    g
}
