/// Object capability system for the Quark microkernel.
///
/// Each task has a CSpace of MAX_CAPS slots. Capabilities are typed objects
/// with parameters (e.g., IoPort with port range, Irq with specific IRQ number).
/// Delegation with attenuation: derived caps must be subsets of the source.
/// O(1) revocation via generation counters.

use core::sync::atomic::{AtomicU64, Ordering};

use crate::task::MAX_TASKS;

/// Slots in a CSpace. A task holds one `Endpoint` for each task it calls, so
/// this is sized for a program that talks to a few dozen services, not for the
/// handful of fixed slots manifests and spawners fill.
pub const MAX_CAPS: usize = 64;
/// Where a capability lands when it is given without naming a slot: clear of
/// the fixed slots manifests and spawners use, which are all below 16.
pub const RECEIVED: core::ops::Range<usize> = 16..MAX_CAPS;
pub const MAX_USERS: usize = 64;

/// `CapSlot::root_tid` value meaning "minted by the kernel, never revocable".
///
/// This must not collide with a real TID. It used to be 0, but TID 0 is the
/// idle task — so any cap it minted was silently unrevocable, and
/// `sys_cap_grant` mistook its caps for kernel-minted ones and re-rooted them
/// at the granter. `MAX_TASKS` is 64, so 0xFF can never be a live TID.
pub const KERNEL_ROOT_TID: u8 = 0xFF;

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
}

#[derive(Debug, Clone, Copy)]
pub struct CapSlot {
    pub cap_type: CapType,
    pub generation: u32,
    pub root_slot: u8,  // slot in root_tid's CSpace
    pub root_tid: u8,   // TID that minted this cap (KERNEL_ROOT_TID = kernel)
    pub param0: u64,
    pub param1: u64,
}

impl CapSlot {
    pub const fn empty() -> Self {
        CapSlot {
            cap_type: CapType::Empty,
            generation: 0,
            root_slot: 0,
            root_tid: KERNEL_ROOT_TID,
            param0: 0,
            param1: 0,
        }
    }
}

pub type CSpace = [CapSlot; MAX_CAPS];

pub const fn empty_cspace() -> CSpace {
    [CapSlot::empty(); MAX_CAPS]
}

/// Global generation counters for O(1) revocation.
/// CAP_GENERATIONS[tid][slot] tracks the current generation for caps minted by tid at slot.
static mut CAP_GENERATIONS: [[u32; MAX_CAPS]; MAX_TASKS] = [[0; MAX_CAPS]; MAX_TASKS];

/// Validate that a cap slot is still valid (not revoked).
fn is_valid(cap: &CapSlot) -> bool {
    if cap.cap_type as u8 == CapType::Empty as u8 {
        return false;
    }
    // Kernel-minted caps are always valid — nothing can revoke them.
    if cap.root_tid == KERNEL_ROOT_TID {
        return true;
    }
    let tid = cap.root_tid as usize;
    let slot = cap.root_slot as usize;
    if tid >= MAX_TASKS || slot >= MAX_CAPS {
        return false;
    }
    unsafe { cap.generation == CAP_GENERATIONS[tid][slot] }
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
static mut ENDPOINTS: [u64; MAX_TASKS] = [0; MAX_TASKS];
static NEXT_ENDPOINT: AtomicU64 = AtomicU64::new(MAX_TASKS as u64);

/// Give `tid` a fresh endpoint. Called when a task slot is filled.
pub fn open_endpoint(tid: usize) {
    if tid < MAX_TASKS {
        let number = NEXT_ENDPOINT.fetch_add(1, Ordering::Relaxed);
        unsafe { ENDPOINTS[tid] = number };
    }
}

/// `tid`'s endpoint is gone, and its number with it. Called when it is reaped.
pub fn close_endpoint(tid: usize) {
    if tid < MAX_TASKS {
        unsafe { ENDPOINTS[tid] = 0 };
    }
}

/// The number of `tid`'s endpoint, or 0 if there is no such task.
pub fn endpoint_of(tid: usize) -> u64 {
    if tid < MAX_TASKS { unsafe { ENDPOINTS[tid] } } else { 0 }
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

/// Public wrapper over the revocation check, for callers outside this module.
pub fn slot_is_valid(cap: &CapSlot) -> bool {
    is_valid(cap)
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
    unsafe {
        let task = match crate::scheduler::get_task_mut(tid) {
            Some(t) => t,
            None => return false,
        };
        let slot = match find_empty_slot(&task.cspace) {
            Some(s) => s,
            None => return false,
        };
        task.cspace[slot] = CapSlot {
            cap_type,
            generation: current_generation(granter, slot),
            root_slot: slot as u8,
            root_tid: granter as u8,
            param0,
            param1,
        };
    }
    true
}

/// Find an empty slot in a task's CSpace. Returns slot index or None.
pub fn find_empty_slot(cspace: &CSpace) -> Option<usize> {
    cspace.iter().position(|cap| cap.cap_type as u8 == CapType::Empty as u8)
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
    RECEIVED.clone().find(|&i| cspace[i].cap_type == CapType::Empty)
}

/// The copy another task receives of `src`, which `granter` holds in `slot`.
///
/// It keeps the provenance of what it was copied from, so revoking that
/// revokes this too. A capability the kernel minted has no root to be revoked
/// through, so the granter becomes its root.
pub fn derive(granter: usize, slot: usize, src: &CapSlot) -> CapSlot {
    let (root_tid, root_slot) = if src.root_tid == KERNEL_ROOT_TID {
        (granter as u8, slot as u8)
    } else {
        (src.root_tid, src.root_slot)
    };
    CapSlot {
        cap_type: src.cap_type,
        generation: current_generation(root_tid as usize, root_slot as usize),
        root_slot,
        root_tid,
        param0: src.param0,
        param1: src.param1,
    }
}

/// Insert a cap into a specific slot.
pub fn insert_cap(cspace: &mut CSpace, slot: usize, cap: CapSlot) {
    if slot < MAX_CAPS {
        cspace[slot] = cap;
    }
}

/// Populate CSpace from old-style bitmask caps (for backward compatibility).
/// Inserts wildcard/full-range caps matching the bitmask bits.
pub fn populate_from_bitmask(cspace: &mut CSpace, caps: u32) {
    if caps & crate::task::CAP_IOPORT != 0 {
        if let Some(slot) = find_empty_slot(cspace) {
            cspace[slot] = CapSlot {
                cap_type: CapType::IoPort,
                generation: 0,
                root_slot: 0,
                root_tid: KERNEL_ROOT_TID,
                param0: 0,        // port_start
                param1: 0xFFFF,   // port_end
            };
        }
    }
    // `CAP_MAP_PHYS` expands to nothing. It used to be a PhysRange over all
    // four gigabytes, which is how a bit handed over with SYS_GRANT_CAP or
    // SYS_CAP_TRANSFER undid every narrow grant made alongside it. Physical
    // memory is granted as the range it is; see `insert_kernel_range`.
    if caps & crate::task::CAP_IRQ != 0 {
        if let Some(slot) = find_empty_slot(cspace) {
            cspace[slot] = CapSlot {
                cap_type: CapType::Irq,
                generation: 0,
                root_slot: 0,
                root_tid: KERNEL_ROOT_TID,
                param0: 0xFF, // wildcard
                param1: 0,
            };
        }
    }
    if caps & crate::task::CAP_TASK_MGMT != 0 {
        if let Some(slot) = find_empty_slot(cspace) {
            cspace[slot] = CapSlot {
                cap_type: CapType::TaskMgmt,
                generation: 0,
                root_slot: 0,
                root_tid: KERNEL_ROOT_TID,
                param0: 0, // any target
                param1: 0,
            };
        }
    }
    if caps & crate::task::CAP_PHYS_ALLOC != 0 {
        if let Some(slot) = find_empty_slot(cspace) {
            cspace[slot] = CapSlot {
                cap_type: CapType::PhysAlloc,
                generation: 0,
                root_slot: 0,
                root_tid: KERNEL_ROOT_TID,
                param0: 0, // unlimited
                param1: 0,
            };
        }
    }
    // `CAP_ENDPOINT` expands to nothing either. It was a set naming every
    // task; an endpoint is minted for the task it names, by that task, its
    // creator, or a holder.
    if caps & crate::task::CAP_SET_UID != 0 {
        if let Some(slot) = find_empty_slot(cspace) {
            cspace[slot] = CapSlot {
                cap_type: CapType::SetUid,
                generation: 0,
                root_slot: 0,
                root_tid: KERNEL_ROOT_TID,
                param0: 0,
                param1: 0,
            };
        }
    }
}

/// Give `cspace` an unrevocable capability over the pages covering
/// `[base, base + len)`, in its first free slot. False if none is free.
pub fn insert_kernel_range(cspace: &mut CSpace, base: usize, len: usize) -> bool {
    let start = base & !0xFFF;
    let end = (base + len + 0xFFF) & !0xFFF;
    match find_empty_slot(cspace) {
        Some(slot) => {
            cspace[slot] = CapSlot {
                cap_type: CapType::PhysRange,
                generation: 0,
                root_slot: 0,
                root_tid: KERNEL_ROOT_TID,
                param0: start as u64,
                param1: end as u64,
            };
            true
        }
        None => false,
    }
}

/// Get a reference to a task's CSpace via the scheduler.
///
/// # Safety
/// Must be called with the task table accessible.
unsafe fn task_cspace(tid: usize) -> Option<&'static CSpace> { unsafe {
    crate::scheduler::get_task_mut(tid).map(|t| &t.cspace)
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
    }
}

/// Revoke a cap slot: bump the generation counter so all derived caps become invalid.
pub fn revoke(tid: usize, slot: usize) {
    if tid >= MAX_TASKS || slot >= MAX_CAPS {
        return;
    }
    unsafe {
        CAP_GENERATIONS[tid][slot] = CAP_GENERATIONS[tid][slot].wrapping_add(1);
    }
}

/// Mint a new cap: find a source cap of the same type in the caller's CSpace
/// that is a superset of the requested params. An `Endpoint` is minted on
/// ownership instead; see `endpoint_to_mint`.
pub fn can_mint(cspace: &CSpace, cap_type: CapType, param0: u64, param1: u64) -> bool {
    cspace.iter().any(|cap| validate_attenuation(cap, cap_type, param0, param1))
}

/// Get the current generation for a given tid/slot pair (for creating derived caps).
pub fn current_generation(tid: usize, slot: usize) -> u32 {
    if tid >= MAX_TASKS || slot >= MAX_CAPS {
        return 0;
    }
    unsafe { CAP_GENERATIONS[tid][slot] }
}
