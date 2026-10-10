/// Synchronous IPC (message passing) for the Quark microkernel.
///
/// Tasks communicate by sending/receiving fixed-size messages.
/// Messages fit in registers for zero-copy small transfers.
///
/// A task's IPC state is changed with its record's lock held
/// (`scheduler::lock_record`) — both tasks' for a call, a send, or a receive
/// that takes a sender's message — and the watches with [`NOTICES`]'s.
/// A wait asks what it waits for and blocks in one step under the lock of
/// the record it blocks in; whoever ends the wait takes that lock too.

use crate::sync::{IrqSpinLock, IrqSpinLockGuard};
use crate::task::MAX_TASKS;
use crate::scheduler;

pub const TID_ANY: usize = usize::MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpcError {
    InvalidTid,
    DeadTask,
    WouldBlock,
    NotWaiting,
    Timeout,
    /// A sleep ended by a signal the program has a handler for.
    Interrupted,
}

/// Fixed-size IPC message: sender TID, tag, and 6 payload words.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct Message {
    pub sender: usize,
    pub tag: u64,
    pub data: [u64; 6],
}

impl Message {
    pub const fn empty() -> Self {
        Message {
            sender: 0,
            tag: 0,
            data: [0; 6],
        }
    }
}

/// Per-task IPC state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpcState {
    None,
    /// Blocked waiting to send to a specific TID.
    SendBlocked(usize),
    /// Blocked waiting to receive from a specific TID (or TID_ANY).
    RecvBlocked(usize),
    /// Blocked in sys_call send phase: message not yet picked up by receiver.
    /// When sys_recv picks this up, it transitions to CallBlocked (stays blocked).
    CallSendBlocked(usize),
    /// Blocked in sys_call (send+recv): waiting for reply from dest.
    CallBlocked(usize),
}

/// Is `tid` blocked in a `sys_call` to `dest`?
///
/// Which is to say: has it asked `dest` for something? A capability granted in
/// answer is not an imposition, and this is how the kernel tells the two apart
/// without a system call whose only purpose is to say "I am expecting one" —
/// which a task already blocked in a call could not make anyway.
///
/// Both call states count. `CallSendBlocked` is a call whose message the
/// destination has not picked up yet, and the caller is no less committed for
/// that; a server that replies out of its receive loop will see the state move
/// to `CallBlocked` underneath it, and a rule that only accepted the second
/// would depend on which side ran first.
pub fn is_calling(tid: usize, dest: usize) -> bool {
    if tid >= MAX_TASKS || dest >= MAX_TASKS {
        return false;
    }
    let held = scheduler::lock_record(tid);
    let out = unsafe {
        matches!(
            st(tid).task_ipc.state,
            IpcState::CallBlocked(d) | IpcState::CallSendBlocked(d) if d == dest
        )
    };
    drop(held);
    out
}

/// A buffer lent with a call, for the task called to use until it replies.
#[derive(Debug, Clone, Copy)]
pub struct Lent {
    pub addr: usize,
    pub len: usize,
    /// `lend::LEND_READ`, `lend::LEND_WRITE`, or both.
    pub access: u64,
    /// `addr` is a physical frame the kernel lent, not an address in the
    /// caller's space.
    pub frame: bool,
}

/// Set in `sender` on a call the kernel makes to a pager for a faulting task:
/// the TID below it is the task, and only the kernel can set it.
pub const PAGER_BIT: u64 = 1 << 62;
/// The kernel asks a pager for a page: `data` = `[cookie, page, object id]`,
/// a frame lent for writing. Sender is the faulting TID with `PAGER_BIT`.
pub const TAG_PAGE_IN: u64 = 0xFFFF_0005;
/// Nothing maps an object any more: `data` = `[cookie, object id]`, sender 0.
pub const TAG_OBJECT_IDLE: u64 = 0xFFFF_0006;
/// A task asks, through the kernel, for an object's written pages to reach
/// its file: `data` = `[cookie, object id]`, sender marked as for a page-in.
pub const TAG_OBJECT_SYNC: u64 = 0xFFFF_0007;

/// The kernel to a pager: memory is short, and pages of yours that nothing
/// maps could be given up if they were written. No object is named: the
/// pager looks at each it has. A flag and not a queue, so that it cannot be
/// full; asking twice before the pager has looked is asking once.
pub const TAG_OBJECT_CLEAN: u64 = 0xFFFF_000B;
/// What this module keeps about a task, in its record (`TaskRec::ipc`):
/// each field was an array of `MAX_TASKS`, and an id with no task reads as
/// `PerTask::new()` — what an empty slot of those arrays held.
pub struct PerTask {
    clean_wanted: bool,
    task_ipc: TaskIpc,
    /// Per-task timeout deadline, in the clock's nanoseconds. 0 = no timeout.
    timeout: u64,
    /// Set by `check_timeouts` when it abandons a task's blocking call, so the
    /// caller can tell "nobody answered in time" from "the target died".
    timed_out: bool,
    /// Where each receiver's next scan for a waiting sender begins.
    ///
    /// The scan used to start at TID 0 every time, which is not a queue but a
    /// priority order: the lowest-numbered sender blocked on a service is served,
    /// and if it blocks again before that service scans once more, it is served
    /// again. A higher-numbered sender behind it never runs. Two clients polling
    /// one server is enough to reproduce it — a compositor with two windows had
    /// the second one wait forever for a reply to its first message.
    ///
    /// Starting one past whoever was served last makes it a round robin: every
    /// waiting sender is reached within one turn of the table.
    recv_rotor: usize,
    /// Per-task notification word (seL4-style). Bits are OR'd in by sys_notify().
    /// Atomically read-and-cleared when consumed by sys_recv/sys_recv_timeout.
    notify: u64,
    /// Per-task signal kill deadline (PIT tick). 0 = no pending signal deadline.
    /// When nonzero, the task will be force-killed after the deadline expires,
    /// and it is on the list the tick looks at (`DEADLINES`), between these.
    signal_deadline: u64,
    deadline_next: u16,
    deadline_prev: u16,
    /// The watches it has made, still waiting and owed; and the watches on
    /// it (`Watch`).
    watching: u16,
    owed: u16,
    watched: u16,
}

impl PerTask {
    pub const fn new() -> Self {
        PerTask {
            clean_wanted: false,
            task_ipc: NO_IPC,
            timeout: 0,
            timed_out: false,
            recv_rotor: 0,
            notify: 0,
            signal_deadline: 0,
            deadline_next: END,
            deadline_prev: END,
            watching: END,
            owed: END,
            watched: END,
        }
    }
}

/// What this module keeps about task `tid`: its record's, or for an id with
/// no task a copy put back to `PerTask::new()` each time it is asked for, so
/// a write for a task that is not there goes nowhere. Interrupts must be off.
unsafe fn st(tid: usize) -> &'static mut PerTask {
    unsafe {
        match crate::scheduler::rec(tid) {
            Some(r) => &mut r.ipc,
            None => {
                let none = &mut *core::ptr::addr_of_mut!(NO_TASK);
                *none = PerTask::new();
                none
            }
        }
    }
}

static mut NO_TASK: PerTask = PerTask::new();


/// IPC state and pending message for each task.
struct TaskIpc {
    state: IpcState,
    pending_msg: Option<Message>,
    /// What the task lent with the call it is making, while it is making it.
    lent: Option<Lent>,
    /// The slot of the capability the task offered with the call it is
    /// making, until the call ends or the task called takes it.
    offer: Option<usize>,
}

const NO_IPC: TaskIpc = TaskIpc {
    state: IpcState::None,
    pending_msg: None,
    lent: None,
    offer: None,
};

/// What `client` lent with the call it is blocked in to `server`, and the
/// address space it lives in.
///
/// Nothing unless `server` has received that call and not yet answered it: a
/// call still waiting to be picked up has not been accepted, and one that has
/// been answered is over.
pub fn lent_to(client: usize, server: usize) -> Option<(Lent, usize)> {
    if client >= MAX_TASKS {
        return None;
    }
    let held = scheduler::lock_record(client);
    let out = unsafe {
        match (st(client).task_ipc.state, st(client).task_ipc.lent) {
            (IpcState::CallBlocked(s), Some(lent)) if s == server => {
                let cr3 = scheduler::task_cr3(client);
                if cr3 != 0 { Some((lent, cr3)) } else { None }
            }
            _ => None,
        }
    };
    drop(held);
    out
}

/// The slot `client` offered with the call it is blocked in to `server`.
///
/// On the same terms as `lent_to`: only between `server` receiving the call and
/// answering it, and only until the offer is taken.
pub fn offered_to(client: usize, server: usize) -> Option<usize> {
    if client >= MAX_TASKS {
        return None;
    }
    let held = scheduler::lock_record(client);
    let out = unsafe {
        match st(client).task_ipc.state {
            IpcState::CallBlocked(s) if s == server => st(client).task_ipc.offer,
            _ => None,
        }
    };
    drop(held);
    out
}

/// `client`'s offer has been taken, and is not there to take again.
pub fn withdraw_offer(client: usize) {
    if client < MAX_TASKS {
        let held = scheduler::lock_record(client);
        unsafe { st(client).task_ipc.offer = None };
        drop(held);
    }
}





/// A watch: `watcher` asked to be told when a task — by its number — or a
/// program — by its space id — is gone. Made when it is asked for, it is on
/// one of the watcher's two lists, of what it is still waiting for and of
/// what it is owed (`PerTask::watching`, `owed`), and a watch of a task is on
/// that task's list too (`PerTask::watched`). A death moves each watch of
/// what died to its watcher's owed list, and the watcher's next receive
/// collects it, and gives it back. Nothing is made at a death, so nothing can
/// be full then.
///
/// They were sets, a bit for each task, in words: as many tasks as a word
/// has bits, and a program's death in a list of sixty-four for each watcher.
struct Watch {
    watcher: u16,
    /// The task watched, or [`END`] for a program.
    task: u16,
    /// The program watched, or 0 for a task.
    space: u64,
    /// Its death is owed to the watcher.
    owed: bool,
    /// The watcher's other watches, on the list this one is on.
    next: u16,
    prev: u16,
    /// The watched task's other watches.
    next_on: u16,
    prev_on: u16,
}

/// The end of a list through records: of watches, or of tasks.
const END: u16 = u16::MAX;

static mut WATCHES: crate::table::Table<Watch> = crate::table::Table::new(crate::table::MOST);

/// The watches: [`WATCHES`], and each task's lists of them (`PerTask`'s
/// `watching`, `owed`, `watched`). Waking a watcher takes its record, after
/// it; what a receive collects of them is taken before its own record's.
static NOTICES: IrqSpinLock<()> = IrqSpinLock::new(crate::sync::RANK_NOTICES, "the watches", ());

/// The tasks with a signal deadline, linked through their records: what the
/// tick looks at.
static mut DEADLINES: u16 = END;

/// Tag for a program's death: `data[0]` is its space id.
pub const TAG_SPACE_DIED: u64 = 0xFFFF_0004;


/// Tag for notification messages delivered to user space.
pub const TAG_NOTIFICATION: u64 = 0xFFFF_0002;

/// Tag for a death notification: `data[0]` is the task that died.
pub const TAG_TASK_DIED: u64 = 0xFFFF_0003;

// Signal badge bits (use high bits to avoid collision with app badges)
pub const SIG_INT: u64 = 1 << 16;
pub const SIG_TERM: u64 = 1 << 17;
pub const SIG_KILL: u64 = 1 << 18;
pub const SIG_MASK: u64 = SIG_INT | SIG_TERM | SIG_KILL;

/// How long a signaled task has before it is force-killed: five seconds.
const SIGNAL_KILL_TIMEOUT: u64 = 500 * crate::clock::TICK_NS;

/// Save RFLAGS and disable interrupts. Returns saved flags.
#[inline(always)]
fn irq_save() -> u64 {
    let flags: u64;
    unsafe {
        core::arch::asm!("pushfq; pop {}; cli", out(reg) flags, options(nostack));
    }
    flags
}

/// Restore RFLAGS (re-enabling interrupts if they were enabled before).
#[inline(always)]
fn irq_restore(flags: u64) {
    unsafe {
        core::arch::asm!("push {}; popfq", in(reg) flags, options(nostack));
    }
}

/// Who is `tid` waiting on, if anyone.
///
/// The scheduler asks this to work out who is doing work on whose behalf: a
/// task blocked on another is not competing with it, it is waiting for it.
pub fn blocked_on(tid: usize) -> Option<usize> {
    if tid >= MAX_TASKS {
        return None;
    }
    unsafe {
        match st(tid).task_ipc.state {
            IpcState::SendBlocked(d) | IpcState::CallSendBlocked(d) | IpcState::CallBlocked(d) => {
                Some(d)
            }
            _ => None,
        }
    }
}

/// Ask to be told when `target` dies.
///
/// No capability is required, and deliberately: `SYS_TASK_INFO` already tells
/// anyone whether a given task is alive, so a watch reveals nothing that was
/// not already there for the asking. What it removes is the polling — and the
/// window between polls, which is where a display stays claimed by a task that
/// no longer exists.
///
/// The registration is dropped when either task dies, so a watcher is never
/// told about the next occupant of a recycled TID.
pub fn sys_task_watch(watcher: usize, target: usize) -> Result<(), IpcError> {
    if target >= MAX_TASKS || watcher >= MAX_TASKS || target == watcher {
        return Err(IpcError::InvalidTid);
    }
    if !scheduler::task_is_live(target) {
        // Already gone. Say so now rather than promising news that will never
        // come: a caller told "no such task" can reclaim immediately, and one
        // left waiting for a notification cannot.
        return Err(IpcError::DeadTask);
    }
    let held = NOTICES.lock();
    let result = unsafe {
        // A death it is owed under this number and has not collected is of
        // whoever had the number before, and it has just said it knows
        // somebody else is there now. Left, it would be taken for this
        // one's.
        let mut w = st(watcher).owed;
        while let Some(x) = watch(w) {
            let next = x.next;
            if x.task == target as u16 {
                drop_watch(w);
            }
            w = next;
        }
        if watching(watcher, |x| x.task == target as u16) {
            Ok(())
        } else {
            make_watch(watcher, target as u16, 0)
        }
    };
    drop(held);
    result
}

/// Ask to be told when program `space` has no task left.
pub fn sys_space_watch(watcher: usize, space: u64) -> Result<(), IpcError> {
    if watcher >= MAX_TASKS || space == 0 {
        return Err(IpcError::InvalidTid);
    }
    if !scheduler::space_is_live(space) {
        return Err(IpcError::DeadTask);
    }
    let held = NOTICES.lock();
    let result = unsafe {
        if watching(watcher, |x| x.task == END && x.space == space) {
            Ok(())
        } else {
            make_watch(watcher, END, space)
        }
    };
    drop(held);
    result
}

/// # Safety
/// [`NOTICES`] held.
#[inline(always)]
unsafe fn watches() -> &'static mut crate::table::Table<Watch> {
    unsafe { &mut *core::ptr::addr_of_mut!(WATCHES) }
}

/// Watch `w`, unless it is the end of a list.
///
/// # Safety
/// [`NOTICES`] held.
unsafe fn watch(w: u16) -> Option<&'static mut Watch> {
    unsafe { if w == END { None } else { watches().get(w as usize) } }
}

/// Whether `watcher` is waiting for something `wanted` says yes to.
///
/// # Safety
/// [`NOTICES`] held.
unsafe fn watching(watcher: usize, wanted: impl Fn(&Watch) -> bool) -> bool {
    unsafe {
        let mut w = st(watcher).watching;
        while let Some(x) = watch(w) {
            if wanted(x) {
                return true;
            }
            w = x.next;
        }
        false
    }
}

/// A watch by `watcher` of `task`, or of program `space`, on its lists.
/// `WouldBlock` if there is no room for it.
///
/// # Safety
/// [`NOTICES`] held.
unsafe fn make_watch(watcher: usize, task: u16, space: u64) -> Result<(), IpcError> {
    unsafe {
        let made = Watch { watcher: watcher as u16, task, space, owed: false, next: END, prev: END, next_on: END, prev_on: END };
        let Some(w) = watches().lowest_free(0) else { return Err(IpcError::WouldBlock) };
        if watches().fill_at(w, made).is_err() {
            return Err(IpcError::WouldBlock);
        }
        let w = w as u16;
        push(&mut st(watcher).watching, w);
        if task != END {
            push_on(&mut st(task as usize).watched, w);
        }
        Ok(())
    }
}

/// Put watch `w` first on the watcher's list `head`.
///
/// # Safety
/// [`NOTICES`] held.
unsafe fn push(head: &mut u16, w: u16) {
    unsafe {
        let Some(x) = watch(w) else { return };
        x.next = *head;
        x.prev = END;
        if let Some(first) = watch(*head) {
            first.prev = w;
        }
        *head = w;
    }
}

/// Take watch `w` off the watcher's list `head`, which it is on.
///
/// # Safety
/// [`NOTICES`] held.
unsafe fn remove(head: &mut u16, w: u16) {
    unsafe {
        let Some(x) = watch(w) else { return };
        let (next, prev) = (x.next, x.prev);
        match watch(prev) {
            Some(before) => before.next = next,
            None => *head = next,
        }
        if let Some(after) = watch(next) {
            after.prev = prev;
        }
        (x.next, x.prev) = (END, END);
    }
}

/// Put watch `w` first on the watched task's list `head`.
///
/// # Safety
/// [`NOTICES`] held.
unsafe fn push_on(head: &mut u16, w: u16) {
    unsafe {
        let Some(x) = watch(w) else { return };
        x.next_on = *head;
        x.prev_on = END;
        if let Some(first) = watch(*head) {
            first.prev_on = w;
        }
        *head = w;
    }
}

/// Take watch `w` off the watched task's list `head`, which it is on.
///
/// # Safety
/// [`NOTICES`] held.
unsafe fn remove_on(head: &mut u16, w: u16) {
    unsafe {
        let Some(x) = watch(w) else { return };
        let (next, prev) = (x.next_on, x.prev_on);
        match watch(prev) {
            Some(before) => before.next_on = next,
            None => *head = next,
        }
        if let Some(after) = watch(next) {
            after.prev_on = prev;
        }
        (x.next_on, x.prev_on) = (END, END);
    }
}

/// Watch `w` is over, told or not: off every list it is on, and given back.
/// False if there is no such watch.
///
/// # Safety
/// [`NOTICES`] held.
unsafe fn drop_watch(w: u16) -> bool {
    unsafe {
        let Some(x) = watch(w) else { return false };
        let watcher = x.watcher as usize;
        if x.owed {
            remove(&mut st(watcher).owed, w);
        } else {
            remove(&mut st(watcher).watching, w);
            if x.task != END {
                remove_on(&mut st(x.task as usize).watched, w);
            }
        }
        watches().empty(w as usize);
        true
    }
}

/// What watch `w` waited for has gone: it is owed to its watcher, who is
/// woken if it is sitting in a receive that would take it. False if there
/// is no such watch.
///
/// # Safety
/// [`NOTICES`] held.
unsafe fn owe(w: u16) -> bool {
    unsafe {
        let Some(x) = watch(w) else { return false };
        let watcher = x.watcher as usize;
        remove(&mut st(watcher).watching, w);
        if x.task != END {
            remove_on(&mut st(x.task as usize).watched, w);
        }
        x.owed = true;
        push(&mut st(watcher).owed, w);
        wake_receiving(watcher);
        true
    }
}

/// Wake `t` if it is sitting in a receive that would take a notice: from
/// anybody, or from the kernel. Takes its record's lock.
///
/// # Safety
/// Interrupts are off.
unsafe fn wake_receiving(t: usize) {
    unsafe {
        let held = scheduler::lock_record(t);
        match st(t).task_ipc.state {
            IpcState::RecvBlocked(from) if from == 0 || from == TID_ANY => {
                st(t).task_ipc.state = IpcState::None;
                scheduler::unblock_task(t);
            }
            _ => {}
        }
        drop(held);
    }
}

/// Tell everyone watching program `space` that its last task has died.
///
/// Called from the scheduler as a task dies, holding no task's record. A
/// program has no record here to keep its watchers on, and a program ends
/// once: they are found among the watches.
pub fn notify_space_watchers(space: u64) {
    let held = NOTICES.lock();
    unsafe {
        let mut at = 0;
        while let Some(w) = watches().next_used(at) {
            at = w + 1;
            if let Some(x) = watches().get(w) {
                if x.task == END && x.space == space && !x.owed {
                    owe(w as u16);
                }
            }
        }
    }
    drop(held);
}

/// Tell everyone watching `dead` that it has gone.
///
/// Called when the task is marked Dead, not when it is reaped: reaping waits
/// on a parent that may never call `sys_wait`, and a compositor holding the
/// screen for a program that exited ten minutes ago is the thing this exists
/// to prevent.
pub fn notify_watchers(dead: usize) {
    if dead >= MAX_TASKS {
        return;
    }
    let held = NOTICES.lock();
    unsafe {
        while st(dead).watched != END && owe(st(dead).watched) {}
        st(dead).watched = END;
    }
    drop(held);
}

/// Take one owed notice of a kind `wanted` says yes to — a task's death or a
/// program's — as the message that says it.
///
/// # Safety
/// [`NOTICES`] held.
unsafe fn take_owed(receiver: usize, wanted: impl Fn(&Watch) -> bool) -> Option<Message> {
    unsafe {
        let mut w = st(receiver).owed;
        while let Some(x) = watch(w) {
            if wanted(x) {
                let msg = if x.task != END {
                    Message { sender: 0, tag: TAG_TASK_DIED, data: [x.task as u64, 0, 0, 0, 0, 0] }
                } else {
                    Message { sender: 0, tag: TAG_SPACE_DIED, data: [x.space, 0, 0, 0, 0, 0] }
                };
                drop_watch(w);
                return Some(msg);
            }
            w = x.next;
        }
        None
    }
}

/// Take one pending death notification, if there is one.
///
/// # Safety
/// [`NOTICES`] held.
unsafe fn take_death(receiver: usize) -> Option<Message> {
    unsafe { take_owed(receiver, |x| x.task != END) }
}

/// Wake `pager`: an object of its has nothing mapping it any more, which
/// the object says (`memobj::take_idle`) until the pager has collected it.
///
/// Interrupts are off: this is called as page tables are cleared.
pub fn notify_object_idle(pager: usize) {
    if pager >= MAX_TASKS {
        return;
    }
    unsafe { wake_receiving(pager) }
}

/// Ask `pager` to write what it has that is dirty (`TAG_OBJECT_CLEAN`): a
/// flag in its record, under its record's lock.
pub fn notify_clean(pager: usize) {
    if pager >= MAX_TASKS {
        return;
    }
    let held = scheduler::lock_record(pager);
    unsafe {
        st(pager).clean_wanted = true;
        match st(pager).task_ipc.state {
            IpcState::RecvBlocked(from) if from == 0 || from == TID_ANY => {
                st(pager).task_ipc.state = IpcState::None;
                st(pager).timeout = 0;
                scheduler::unblock_task(pager);
            }
            _ => {}
        }
    }
    drop(held);
}

/// Stop the current task for `ns`, for a reason of the kernel's own: it is
/// waiting for memory. Nothing is received and nothing a signal is owed is
/// used up; a task in the middle of a call or a receive is not stopped at
/// all, being in no state to be woken from this.
pub fn pause(ns: u64) {
    let me = scheduler::current_tid();
    if me >= MAX_TASKS {
        return;
    }
    let held = scheduler::lock_record(me);
    let parked = unsafe {
        if matches!(st(me).task_ipc.state, IpcState::None) {
            st(me).timeout = crate::clock::after(ns);
            crate::clock::due(st(me).timeout);
            st(me).task_ipc.state = IpcState::RecvBlocked(me);
            scheduler::block_task(me);
            true
        } else {
            false
        }
    };
    drop(held);
    if !parked {
        return;
    }
    scheduler::yield_now();
    let held = scheduler::lock_record(me);
    unsafe {
        st(me).timeout = 0;
        if matches!(st(me).task_ipc.state, IpcState::RecvBlocked(t) if t == me) {
            st(me).task_ipc.state = IpcState::None;
        }
    }
    drop(held);
}

/// Wake a server that is waiting to receive, because the kernel has a notice
/// for it. The notice itself is found by its next receive.
pub fn wake_for_notice(server: usize) {
    if server >= MAX_TASKS {
        return;
    }
    let held = scheduler::lock_record(server);
    unsafe {
        match st(server).task_ipc.state {
            IpcState::RecvBlocked(from) if from == 0 || from == TID_ANY => {
                st(server).task_ipc.state = IpcState::None;
                st(server).timeout = 0;
                scheduler::unblock_task(server);
            }
            _ => {}
        }
    }
    drop(held);
}

/// Take one pending notice from the kernel: a task's death, a program's, an
/// object gone idle, or a served descriptor's last close.
///
/// Each is asked under its own lock, and the watches and a served
/// descriptor's rank before a task's record.
///
/// # Safety
/// No task's record held.
unsafe fn take_any_death(receiver: usize) -> Option<Message> {
    unsafe {
        let held = NOTICES.lock();
        let death = take_death(receiver);
        drop(held);
        if death.is_some() {
            return death;
        }
        // An object this task serves has no descriptors left. One notice
        // however many there are: the server collects until there are none.
        if crate::served::take_notice(receiver) {
            return Some(Message {
                sender: 0,
                tag: crate::served::TAG_FD_RELEASED,
                data: [0; 6],
            });
        }
        if let Some((cookie, id)) = crate::memobj::take_idle(receiver) {
            return Some(Message { sender: 0, tag: TAG_OBJECT_IDLE, data: [cookie, id, 0, 0, 0, 0] });
        }
        let held = scheduler::lock_record(receiver);
        let clean = core::mem::replace(&mut st(receiver).clean_wanted, false);
        drop(held);
        if clean {
            return Some(Message { sender: 0, tag: TAG_OBJECT_CLEAN, data: [0; 6] });
        }
        let held = NOTICES.lock();
        let gone = take_owed(receiver, |x| x.task == END);
        drop(held);
        gone
    }
}

/// Wake a task sleeping in `sys_recv_timeout` on its own TID.
///
/// That is how a task sleeps here — nobody can send to your own TID, so only
/// the deadline ends the block. `sys_notify` deliberately wakes only a task
/// waiting on `TID_ANY`, because a notification is a message and that task
/// asked for one. This is not a message: it is a sleeper being told that its
/// deadline no longer matters.
pub fn wake_sleeper(tid: usize) -> bool {
    if tid >= MAX_TASKS {
        return false;
    }
    let held = scheduler::lock_record(tid);
    let woke = unsafe {
        if matches!(st(tid).task_ipc.state, IpcState::RecvBlocked(t) if t == tid) {
            st(tid).task_ipc.state = IpcState::None;
            st(tid).timeout = 0;
            scheduler::unblock_task(tid);
            true
        } else {
            false
        }
    };
    drop(held);
    woke
}

/// Asynchronous notification: OR `badge` into dest's notification word.
/// Non-blocking. Wakes the dest task if it is RecvBlocked(0) or RecvBlocked(TID_ANY).
pub fn sys_notify(dest: usize, badge: u64) -> Result<(), IpcError> {
    if dest >= MAX_TASKS || badge == 0 {
        return Err(IpcError::InvalidTid);
    }
    // Reserved signal bits may only be raised through sys_signal, which checks
    // the caller's authority over the target. Allowing them here let any task
    // forge a signal into any other task's notification word.
    if badge & SIG_MASK != 0 {
        return Err(IpcError::InvalidTid);
    }
    if !scheduler::task_is_live(dest) {
        return Err(IpcError::DeadTask);
    }

    let held = scheduler::lock_record(dest);
    unsafe {
        st(dest).notify |= badge;

        // Wake the task if it's recv-blocked and would accept a notification
        match st(dest).task_ipc.state {
            IpcState::RecvBlocked(from) if from == 0 || from == TID_ANY => {
                st(dest).task_ipc.state = IpcState::None;
                scheduler::unblock_task(dest);
            }
            _ => {}
        }
    }
    drop(held);

    Ok(())
}

/// Raise `badge` in `dest`'s notification word without filtering reserved
/// bits. Only `sys_signal` may use this, after its own permission check.
fn notify_raw(dest: usize, badge: u64) -> Result<(), IpcError> {
    if dest >= MAX_TASKS || badge == 0 {
        return Err(IpcError::InvalidTid);
    }

    let held = scheduler::lock_record(dest);
    unsafe {
        st(dest).notify |= badge;

        match st(dest).task_ipc.state {
            IpcState::RecvBlocked(from) if from == 0 || from == TID_ANY => {
                st(dest).task_ipc.state = IpcState::None;
                scheduler::unblock_task(dest);
            }
            _ => {}
        }
    }
    drop(held);

    Ok(())
}

/// Send a signal to a task. SIG_KILL immediately kills; other signals are
/// delivered as notification badges with a force-kill deadline.
/// Permission checking is done in syscall_dispatch (same as sys_task_kill).
///
/// Unlike plain `sys_notify`, this also interrupts tasks blocked in IPC calls
/// (CallBlocked, CallSendBlocked, SendBlocked, RecvBlocked with specific sender).
/// The interrupted syscall returns an error, allowing the task to check its
/// notification word and handle the signal.
pub fn sys_signal(dest: usize, sig: u64) -> Result<(), IpcError> {
    if dest >= MAX_TASKS || dest <= 1 {
        return Err(IpcError::InvalidTid);
    }
    if sig == 0 {
        return Err(IpcError::InvalidTid);
    }

    // SIG_KILL: immediate termination, no grace period
    if sig & SIG_KILL != 0 {
        // The program, not the one task: a signal is said to a program.
        let _ = scheduler::kill_program(dest);
        return Ok(());
    }

    // Deliver signal bits via notification word and wake if RecvBlocked(0|TID_ANY).
    // Uses the privileged form: sys_notify refuses reserved signal bits.
    notify_raw(dest, sig)?;

    // Force-unblock from IPC states that sys_notify doesn't handle.
    let held = scheduler::lock_record(dest);
    unsafe {
        match st(dest).task_ipc.state {
            IpcState::CallBlocked(_) | IpcState::CallSendBlocked(_) | IpcState::SendBlocked(_) => {
                st(dest).task_ipc.pending_msg = None;
                st(dest).task_ipc.state = IpcState::None;
                scheduler::unblock_task(dest);
            }
            IpcState::RecvBlocked(from) if from != 0 && from != TID_ANY => {
                // sys_notify only handles RecvBlocked(0|TID_ANY); interrupt specific waits too
                st(dest).task_ipc.state = IpcState::None;
                scheduler::unblock_task(dest);
            }
            _ => {} // None or RecvBlocked(0|TID_ANY) already handled by sys_notify
        }
    }
    drop(held);

    // Set force-kill deadline (only if not already set — don't extend)
    let flags = irq_save();
    unsafe {
        if st(dest).signal_deadline == 0 {
            let at = crate::clock::after(SIGNAL_KILL_TIMEOUT);
            set_deadline(dest, at);
            crate::clock::due(at);
        }
    }
    irq_restore(flags);

    Ok(())
}

/// End the program of every task whose signal deadline is `now` or past.
/// From the clock (`clock::expire`), with the rest of what may not return:
/// a deadline can be the program the clock interrupted.
pub fn check_signal_deadlines(now: u64) {
    // From the clock, so interrupts are off.
    unsafe {
        let mut t = *core::ptr::addr_of!(DEADLINES);
        while t != END {
            let tid = t as usize;
            t = st(tid).deadline_next;
            if now >= st(tid).signal_deadline {
                set_deadline(tid, 0);
                let _ = scheduler::kill_program(tid);
            }
        }
    }
}

/// The earliest signal deadline after `now`, for the clock to be set by;
/// `u64::MAX` for none.
pub fn signal_deadline_after(now: u64) -> u64 {
    let mut next = u64::MAX;
    unsafe {
        let mut t = *core::ptr::addr_of!(DEADLINES);
        while t != END {
            let at = st(t as usize).signal_deadline;
            if at > now {
                next = next.min(at);
            }
            t = st(t as usize).deadline_next;
        }
    }
    next
}

/// Give `tid` a signal deadline at `at`, or none for 0: it is on the list
/// the clock looks at while it has one.
///
/// # Safety
/// Interrupts are off.
unsafe fn set_deadline(tid: usize, at: u64) {
    unsafe {
        let had = st(tid).signal_deadline != 0;
        st(tid).signal_deadline = at;
        let head = &mut *core::ptr::addr_of_mut!(DEADLINES);
        if !had && at != 0 {
            st(tid).deadline_next = *head;
            st(tid).deadline_prev = END;
            if *head != END {
                st(*head as usize).deadline_prev = tid as u16;
            }
            *head = tid as u16;
        } else if had && at == 0 {
            let (next, prev) = (st(tid).deadline_next, st(tid).deadline_prev);
            if prev != END {
                st(prev as usize).deadline_next = next;
            } else {
                *head = next;
            }
            if next != END {
                st(next as usize).deadline_prev = prev;
            }
            st(tid).deadline_next = END;
            st(tid).deadline_prev = END;
        }
    }
}

/// Clear signal deadline for a task (called when task exits or is killed).
pub fn clear_signal_deadline(tid: usize) {
    if tid < MAX_TASKS {
        let flags = irq_save();
        unsafe { set_deadline(tid, 0) };
        irq_restore(flags);
    }
}

/// Synchronous send: blocks until receiver calls recv.
pub fn sys_send(dest: usize, msg: &Message) -> Result<(), IpcError> {
    if dest >= MAX_TASKS {
        return Err(IpcError::InvalidTid);
    }
    if !scheduler::task_is_live(dest) {
        return Err(IpcError::DeadTask);
    }
    let sender = scheduler::current_tid();

    let held = scheduler::lock_records(sender, dest);
    unsafe {
        // Check if dest is blocked waiting to receive from us (or from ANY)
        let dest_state = st(dest).task_ipc.state;
        match dest_state {
            IpcState::RecvBlocked(from) if from == sender || from == TID_ANY => {
                // Receiver is waiting — deliver directly
                let mut delivered = *msg;
                delivered.sender = sender;
                st(dest).task_ipc.pending_msg = Some(delivered);
                st(dest).task_ipc.state = IpcState::None;
                scheduler::unblock_task(dest);
                drop(held);
                return Ok(());
            }
            _ => {}
        }

        // Receiver not ready — block sender
        let mut to_send = *msg;
        to_send.sender = sender;
        st(sender).task_ipc.pending_msg = Some(to_send);
        st(sender).task_ipc.state = IpcState::SendBlocked(dest);
        scheduler::block_task(sender);
    }
    drop(held);
    scheduler::yield_now();

    let held = scheduler::lock_record(sender);
    let result = unsafe {
        // Woken. If our message is still queued, nobody took it — the receiver
        // died or we were interrupted by a signal, so report failure rather
        // than pretending the send landed.
        let undelivered = st(sender).task_ipc.pending_msg.take().is_some();
        st(sender).task_ipc.state = IpcState::None;
        if undelivered {
            Err(IpcError::DeadTask)
        } else {
            Ok(())
        }
    };
    drop(held);
    result
}

/// Synchronous receive: blocks until a message arrives.
/// `from` is the expected sender TID, or TID_ANY for any sender.
pub fn sys_recv(from: usize) -> Result<Message, IpcError> {
    let receiver = scheduler::current_tid();
    let notices = from == 0 || from == TID_ANY;

    // From the first look to blocking, interrupts are off: what is looked at
    // under one lock and what under another are one step, under the one lock.
    let flags = irq_save();
    // Notices first. A notice was queued when its task died, before anything
    // could take the TID again; a call waiting behind it may be from
    // whatever did, and must not be served as the dead task's. Asked before
    // this task's record is locked: what keeps them ranks before it. Nothing
    // can be owed between this look and the block below — under the one lock
    // with interrupts off, nobody else is in the kernel — so there is no
    // second look before blocking.
    if notices {
        if let Some(msg) = unsafe { take_any_death(receiver) } {
            irq_restore(flags);
            return Ok(msg);
        }
    }
    let held = match take_sender(receiver, from) {
        Ok(msg) => {
            irq_restore(flags);
            return Ok(msg);
        }
        Err(held) => held,
    };
    unsafe {
        // Before blocking, check for pending IRQ messages
        // (from=0 means kernel, TID_ANY matches any)
        if notices {
            if let Some(msg) = crate::irq_dispatch::poll_irq_message(receiver) {
                drop(held);
                irq_restore(flags);
                return Ok(msg);
            }
        }

        // Check for pending notifications (from=0 or TID_ANY)
        if notices {
            let word = st(receiver).notify;
            if word != 0 {
                st(receiver).notify = 0;
                drop(held);
                irq_restore(flags);
                return Ok(Message {
                    sender: 0,
                    tag: TAG_NOTIFICATION,
                    data: [word, 0, 0, 0, 0, 0],
                });
            }
        }

        // No sender ready — block receiver
        st(receiver).task_ipc.state = IpcState::RecvBlocked(from);
        scheduler::block_task(receiver);
    }
    drop(held);
    irq_restore(flags);
    scheduler::yield_now();
    woken(receiver, from)
}

/// What a receive that blocked was woken by: a message delivered, first —
/// otherwise an IRQ arriving between the delivery and the resume would be
/// returned and the message orphaned — then an IRQ, a notice, and the
/// notification word. Whoever woke it set its state to `None`, so nothing is
/// delivered to it from here on. Its record is held for what is its own, and
/// not for the notices, which rank before it.
fn woken(receiver: usize, from: usize) -> Result<Message, IpcError> {
    let notices = from == 0 || from == TID_ANY;
    let held = scheduler::lock_record(receiver);
    unsafe {
        if let Some(msg) = st(receiver).task_ipc.pending_msg.take() {
            st(receiver).task_ipc.state = IpcState::None;
            drop(held);
            return Ok(msg);
        }
        if notices {
            if let Some(msg) = crate::irq_dispatch::poll_irq_message(receiver) {
                st(receiver).task_ipc.state = IpcState::None;
                drop(held);
                return Ok(msg);
            }
        }
    }
    drop(held);
    // A task this one was watching has died.
    if notices {
        if let Some(msg) = unsafe { take_any_death(receiver) } {
            let held = scheduler::lock_record(receiver);
            unsafe { st(receiver).task_ipc.state = IpcState::None };
            drop(held);
            return Ok(msg);
        }
    }
    let held = scheduler::lock_record(receiver);
    let result = unsafe {
        let word = if notices { core::mem::replace(&mut st(receiver).notify, 0) } else { 0 };
        st(receiver).task_ipc.state = IpcState::None;
        if word != 0 {
            Ok(Message { sender: 0, tag: TAG_NOTIFICATION, data: [word, 0, 0, 0, 0, 0] })
        } else {
            Err(IpcError::WouldBlock)
        }
    };
    drop(held);
    result
}

/// Take a message from a task blocked sending to `receiver` — from `from`,
/// or from anybody for `TID_ANY` — beginning one past whoever was served
/// last, so that no sender can monopolise it. A caller is left waiting for
/// its answer, and a sender is woken. With nobody there, `Err` with
/// `receiver`'s record's lock held: what it asks next, and its blocking, are
/// one step with this look.
///
/// Who is blocked sending here is read without the senders' locks: a task
/// becomes so with this one's lock held as well as its own, so one that is
/// there now is found, and one found is looked at again with its own lock
/// held, which whatever else ends its wait holds too.
fn take_sender(receiver: usize, from: usize) -> Result<Message, Option<IrqSpinLockGuard<'static, ()>>> {
    loop {
        let held = scheduler::lock_record(receiver);
        let found = unsafe {
            scheduler::tids_from(st(receiver).recv_rotor).find(|&tid| {
                tid != receiver
                    && (from == TID_ANY || from == tid)
                    && matches!(
                        st(tid).task_ipc.state,
                        IpcState::SendBlocked(d) | IpcState::CallSendBlocked(d) if d == receiver
                    )
            })
        };
        let Some(tid) = found else { return Err(held) };
        drop(held);
        let both = scheduler::lock_records(receiver, tid);
        unsafe {
            let was_call = match st(tid).task_ipc.state {
                IpcState::CallSendBlocked(d) if d == receiver => true,
                IpcState::SendBlocked(d) if d == receiver => false,
                // Its wait ended in between: look again.
                _ => continue,
            };
            st(receiver).recv_rotor = (tid + 1) % MAX_TASKS;
            match st(tid).task_ipc.pending_msg.take() {
                Some(msg) => {
                    if was_call {
                        // Transition to CallBlocked — keep blocked, waiting for reply
                        st(tid).task_ipc.state = IpcState::CallBlocked(receiver);
                    } else {
                        // Plain send — unblock sender
                        st(tid).task_ipc.state = IpcState::None;
                        scheduler::unblock_task(tid);
                    }
                    drop(both);
                    return Ok(msg);
                }
                None => {
                    // Inconsistent state: reset sender and skip
                    st(tid).task_ipc.state = IpcState::None;
                    scheduler::unblock_task(tid);
                }
            }
        }
        drop(both);
    }
}

/// Synchronous RPC: send a message and wait for a reply.
pub fn sys_call(dest: usize, msg: &Message) -> Result<Message, IpcError> {
    call_inner(dest, msg, 0, None, None)
}

/// A call that lends `dest` a buffer until it replies.
pub fn sys_call_lend(dest: usize, msg: &Message, lent: Lent) -> Result<Message, IpcError> {
    call_inner(dest, msg, 0, Some(lent), None)
}

/// A call that offers `dest` a copy of the capability in the caller's `slot`,
/// for it to take before it replies or leave.
pub fn sys_call_offer(dest: usize, msg: &Message, slot: usize) -> Result<Message, IpcError> {
    call_inner(dest, msg, 0, None, Some(slot))
}

/// Synchronous call that gives up after `timeout_ns` nanoseconds.
///
/// A call has two blocking points and a target that is alive but not serving
/// can hang either one: it may never reach sys_recv, leaving us in
/// CallSendBlocked with the message still undelivered, or it may receive and
/// never reply, leaving us in CallBlocked. Both are abandoned on expiry.
///
/// Dropping out of CallBlocked is safe because sys_reply only delivers to a
/// task still in CallBlocked on that replier; a reply that lands after we have
/// given up is discarded rather than written into a caller that moved on.
pub fn sys_call_timeout(
    dest: usize,
    msg: &Message,
    timeout_ns: u64,
) -> Result<Message, IpcError> {
    call_inner(dest, msg, timeout_ns, None, None)
}

/// A call with any of a buffer lent, a capability offered and a deadline.
pub fn sys_call_with(
    dest: usize,
    msg: &Message,
    timeout_ns: u64,
    lent: Option<Lent>,
    offer: Option<usize>,
) -> Result<Message, IpcError> {
    call_inner(dest, msg, timeout_ns, lent, offer)
}

/// `timeout_ns` of 0 means block indefinitely.
fn call_inner(
    dest: usize,
    msg: &Message,
    timeout_ns: u64,
    lent: Option<Lent>,
    offer: Option<usize>,
) -> Result<Message, IpcError> {
    call_as(dest, msg, timeout_ns, lent, offer, 0)
}

/// A call from the kernel, on behalf of the current task, to the server
/// behind a descriptor it holds, lending the task's own buffer. The descriptor
/// is the authorisation: the task needs no capability for the server.
pub fn served_call(server: usize, msg: &Message, lent: Lent) -> Result<Message, IpcError> {
    call_as(server, msg, 0, Some(lent), None, 0)
}

/// A call from the kernel, on behalf of the current task, to the pager of an
/// object it touched: the pager fills `frame`, lent for writing, and replies.
/// The task's own capabilities have nothing to do with it.
pub fn pager_call(pager: usize, msg: &Message, frame: Option<usize>) -> Result<Message, IpcError> {
    let lent = frame.map(|addr| Lent {
        addr,
        len: 4096,
        access: crate::lend::LEND_WRITE,
        frame: true,
    });
    call_as(pager, msg, 0, lent, None, PAGER_BIT)
}

fn call_as(
    dest: usize,
    msg: &Message,
    timeout_ns: u64,
    lent: Option<Lent>,
    offer: Option<usize>,
    sender_bits: u64,
) -> Result<Message, IpcError> {
    if dest >= MAX_TASKS {
        return Err(IpcError::InvalidTid);
    }
    if !scheduler::task_is_live(dest) {
        return Err(IpcError::DeadTask);
    }
    let caller = scheduler::current_tid();

    // Set when the receiver was waiting for this and can take over directly.
    let mut hand_over_to: Option<usize> = None;

    let flags = irq_save();
    let held = scheduler::lock_records(caller, dest);
    unsafe {
        let mut to_send = *msg;
        to_send.sender = caller | sender_bits as usize;
        // Lent and offered before either path can hand the message over: a
        // server woken by it may look for them before this task runs again.
        st(caller).task_ipc.lent = lent;
        st(caller).task_ipc.offer = offer;

        // Check if dest is recv-blocked
        let dest_state = st(dest).task_ipc.state;
        match dest_state {
            IpcState::RecvBlocked(from) if from == caller || from == TID_ANY => {
                // Fast path: deliver message directly to receiver.
                st(caller).task_ipc.state = IpcState::CallBlocked(dest);
                st(caller).task_ipc.pending_msg = None;
                st(dest).task_ipc.pending_msg = Some(to_send);
                st(dest).task_ipc.state = IpcState::None;
                // Runnable, but deliberately not queued: it is about to be
                // switched to, and an entry left behind is a turn it has
                // already had.
                scheduler::make_ready(dest);
                scheduler::block_task(caller);
                hand_over_to = Some(dest);
            }
            _ => {
                // Slow path: receiver not ready, block as CallSendBlocked.
                st(caller).task_ipc.pending_msg = Some(to_send);
                st(caller).task_ipc.state = IpcState::CallSendBlocked(dest);
                scheduler::block_task(caller);
            }
        };

        st(caller).timed_out = false;
        st(caller).timeout = if timeout_ns == 0 {
            0
        } else {
            crate::clock::after(timeout_ns)
        };
        if st(caller).timeout != 0 {
            crate::clock::due(st(caller).timeout);
        }
    }
    drop(held);
    // The caller is now waiting on `dest`, so `dest` is working on its
    // behalf and runs at its urgency until it answers. Done before anything
    // decides who runs next, so the decision sees the raised band rather than
    // the one it will have a moment later — and with no record held, since it
    // takes each one it changes.
    scheduler::refresh_priority(dest);
    match hand_over_to {
        // Straight across, on what is left of this task's slice, with
        // interrupts still off. `dest` is runnable but in no queue, so a tick
        // that preempted this task first would run something else, and then
        // nothing would ever run `dest` -- which this task is blocked on.
        Some(dest) => scheduler::donate_to(dest, flags),
        // Nobody was waiting; the message is queued and somebody will come
        // for it. Ordinary scheduling.
        None => {
            irq_restore(flags);
            scheduler::yield_now();
        }
    }

    // Reply arrived
    let held = scheduler::lock_record(caller);
    let result = unsafe {
        st(caller).timeout = 0;
        // However the call ended, nothing is lent or offered any more.
        st(caller).task_ipc.lent = None;
        st(caller).task_ipc.offer = None;
        let reply = match st(caller).task_ipc.pending_msg.take() {
            Some(m) => m,
            None => {
                st(caller).task_ipc.state = IpcState::None;
                let timed_out = st(caller).timed_out;
                st(caller).timed_out = false;
                drop(held);
                return Err(if timed_out { IpcError::Timeout } else { IpcError::DeadTask });
            }
        };
        st(caller).task_ipc.state = IpcState::None;
        Ok(reply)
    };
    drop(held);
    result
}

/// Reply to a caller that is blocked in sys_call.
pub fn sys_reply(dest: usize, msg: &Message) -> Result<(), IpcError> {
    // A pager answers the task the kernel called for.
    let dest = dest & !(PAGER_BIT as usize);
    if dest >= MAX_TASKS {
        return Err(IpcError::InvalidTid);
    }
    let replier = scheduler::current_tid();

    let flags = irq_save();
    let held = scheduler::lock_record(dest);
    let result = unsafe {
        match st(dest).task_ipc.state {
            IpcState::CallBlocked(expected_replier) if expected_replier == replier => {
                let mut reply = *msg;
                reply.sender = replier;
                st(dest).task_ipc.pending_msg = Some(reply);
                st(dest).task_ipc.state = IpcState::None;
                // The caller has been stopped waiting for exactly this. It
                // runs as soon as this server blocks again, rather than after
                // everything else that became ready in the meantime.
                scheduler::unblock_task_next(dest);
                Ok(())
            }
            _ => {
                Err(IpcError::NotWaiting)
            }
        }
    };
    drop(held);
    // It is no longer waiting on us, so whatever urgency it lent goes back
    // with it.
    if result.is_ok() {
        scheduler::refresh_priority(replier);
    }
    irq_restore(flags);
    result
}

/// Synchronous receive with timeout: blocks until a message arrives or deadline expires.
/// `from` is the expected sender TID, or TID_ANY for any sender.
/// `timeout_ns` is how long to wait, in nanoseconds (0 = non-blocking poll).
pub fn sys_recv_timeout(from: usize, timeout_ns: u64) -> Result<Message, IpcError> {
    let receiver = scheduler::current_tid();
    let notices = from == 0 || from == TID_ANY;

    // One step from the first look to blocking, as in `sys_recv`.
    let flags = irq_save();
    // Notices first, as in `sys_recv`.
    if notices {
        if let Some(msg) = unsafe { take_any_death(receiver) } {
            irq_restore(flags);
            return Ok(msg);
        }
    }
    let held = match take_sender(receiver, from) {
        Ok(msg) => {
            irq_restore(flags);
            return Ok(msg);
        }
        Err(held) => held,
    };
    unsafe {
        // Check for pending IRQ messages
        if notices {
            if let Some(msg) = crate::irq_dispatch::poll_irq_message(receiver) {
                drop(held);
                irq_restore(flags);
                return Ok(msg);
            }
        }

        // Check for pending notifications
        if notices {
            let word = st(receiver).notify;
            if word != 0 {
                st(receiver).notify = 0;
                drop(held);
                irq_restore(flags);
                return Ok(Message {
                    sender: 0,
                    tag: TAG_NOTIFICATION,
                    data: [word, 0, 0, 0, 0, 0],
                });
            }
        }
    }
    drop(held);

    // Non-blocking poll: return immediately if timeout is 0
    if timeout_ns == 0 {
        irq_restore(flags);
        return Err(IpcError::Timeout);
    }

    // A receive from oneself is a sleep, and a sleep is one of the waits a
    // signal ends. One already waiting for a handler means there is no sleep
    // to begin: looked at here, with interrupts off, so that one raised a
    // moment from now finds a sleeper to wake — and before the record is
    // held again, since a program's signals rank before it.
    if from == receiver && crate::signal::interrupted(receiver) {
        irq_restore(flags);
        return Err(IpcError::Interrupted);
    }

    // Set deadline and block
    let held = scheduler::lock_record(receiver);
    unsafe {
        st(receiver).timeout = crate::clock::after(timeout_ns);
        crate::clock::due(st(receiver).timeout);
        st(receiver).task_ipc.state = IpcState::RecvBlocked(from);
        scheduler::block_task(receiver);
    }
    drop(held);
    irq_restore(flags);
    scheduler::yield_now();

    // Clear timeout (may already be 0 if expired)
    let held = scheduler::lock_record(receiver);
    unsafe { st(receiver).timeout = 0 };
    drop(held);
    let result = woken(receiver, from);
    if result.is_ok() {
        return result;
    }
    // No message — a timeout, or a sleeper woken for a signal.
    if from == receiver && crate::signal::interrupted(receiver) {
        return Err(IpcError::Interrupted);
    }
    Err(IpcError::Timeout)
}

/// Kernel-initiated IPC call on behalf of a faulting task.
/// Used by the exception handler to forward page faults to a pager task.
/// The faulting task is blocked until the pager replies via sys_reply.
///
/// Must be called with the faulting task as the current task.
/// After this returns, the pager has replied and the faulting task can resume.
pub fn fault_call(faulting_tid: usize, pager_tid: usize, msg: Message) {
    if faulting_tid >= MAX_TASKS || pager_tid >= MAX_TASKS {
        return;
    }
    let held = scheduler::lock_records(faulting_tid, pager_tid);
    unsafe {
        // Check if pager is recv-blocked waiting for us (or TID_ANY)
        let pager_state = st(pager_tid).task_ipc.state;
        match pager_state {
            IpcState::RecvBlocked(from) if from == faulting_tid || from == TID_ANY => {
                // Fast path: deliver directly to pager.
                // Set CallBlocked BEFORE unblocking pager to prevent race.
                st(faulting_tid).task_ipc.state = IpcState::CallBlocked(pager_tid);
                st(faulting_tid).task_ipc.pending_msg = None;
                st(pager_tid).task_ipc.pending_msg = Some(msg);
                st(pager_tid).task_ipc.state = IpcState::None;
                scheduler::unblock_task(pager_tid);
            }
            _ => {
                // Slow path: pager not waiting — queue as CallSendBlocked.
                // When pager calls sys_recv, it picks this up.
                st(faulting_tid).task_ipc.pending_msg = Some(msg);
                st(faulting_tid).task_ipc.state = IpcState::CallSendBlocked(pager_tid);
            }
        }
        scheduler::block_task(faulting_tid);
    }
    drop(held);
    scheduler::yield_now();

    // Resumed — pager replied. Clean up.
    let held = scheduler::lock_record(faulting_tid);
    unsafe {
        st(faulting_tid).task_ipc.pending_msg = None;
        st(faulting_tid).task_ipc.state = IpcState::None;
    }
    drop(held);
}

/// Check all task timeouts at `now` and unblock expired ones; say when the
/// next one is, or `u64::MAX` if nobody is waiting on a time.
/// Called from the clock (`clock::expire`).
pub fn check_timeouts(now: u64) -> u64 {
    // Already in interrupt context (IRQ handler), interrupts are implicitly off.
    let mut next = u64::MAX;
    for tid in scheduler::tids() {
        // Most have no deadline, which is looked at first without the lock:
        // a task writes its own and then says so to the clock, which comes
        // back here. One that has is looked at under its record's lock, and
        // the urgency it lent given back with none held.
        if unsafe { st(tid).timeout } == 0 {
            continue;
        }
        let held = scheduler::lock_record(tid);
        let mut lent_to = None;
        unsafe {
            let deadline = st(tid).timeout;
            if deadline != 0 && now < deadline {
                next = next.min(deadline);
            }
            if deadline != 0 && now >= deadline {
                st(tid).timeout = 0;
                // Only unblock if still blocked on the thing we timed (it could
                // have been woken by IPC already, between deadline and now).
                match st(tid).task_ipc.state {
                    IpcState::RecvBlocked(_) => {
                        st(tid).task_ipc.state = IpcState::None;
                        scheduler::unblock_task(tid);
                    }
                    IpcState::CallSendBlocked(dest) | IpcState::CallBlocked(dest) => {
                        // Drop the undelivered message so no receiver can pick
                        // it up after we have stopped waiting for the reply.
                        st(tid).task_ipc.pending_msg = None;
                        st(tid).task_ipc.state = IpcState::None;
                        st(tid).timed_out = true;
                        scheduler::unblock_task(tid);
                        lent_to = Some(dest);
                    }
                    _ => {}
                }
            }
        }
        drop(held);
        // Giving up on the reply takes back the urgency lent to whoever was
        // going to send it.
        if let Some(dest) = lent_to {
            scheduler::refresh_priority(dest);
        }
    }
    next
}

/// Fail every task that is blocked on `dead_tid`: sending to it, in a call to
/// it, or receiving from it alone. Each is answered with an error and runs.
///
/// Done when a task *dies*, not when it is reaped — for the reason its
/// descriptors are let go then. A dead task waits to be collected by its
/// parent, and a parent that is in a call to it is not going to collect
/// anything: `mount` starts a file server, calls it to see whether it found
/// a filesystem, and the server that found none exits without answering.
/// Left to reaping, the two waited for each other for ever — and whether
/// they did was a race, lost only when the child was slow enough to still
/// be alive when its parent called. Anybody else's caller waited on a
/// parent that might never look.
///
/// The caller has interrupts off, and holds no task's record: each is taken
/// in turn.
pub fn fail_waiters(dead_tid: usize) {
    if dead_tid >= MAX_TASKS {
        return;
    }
    let error_msg = Message { sender: dead_tid, tag: u64::MAX, data: [0; 6] };
    for tid in scheduler::tids() {
        // Who is blocked on it is looked for without the locks — becoming so
        // takes its record's lock and the dead task's — and whoever is, is
        // looked at again under its own.
        let on_it = unsafe {
            matches!(
                st(tid).task_ipc.state,
                IpcState::SendBlocked(d) | IpcState::CallSendBlocked(d) | IpcState::CallBlocked(d) | IpcState::RecvBlocked(d)
                    if d == dead_tid
            )
        };
        if tid == dead_tid || !on_it {
            continue;
        }
        let held = scheduler::lock_record(tid);
        unsafe {
            match st(tid).task_ipc.state {
                IpcState::SendBlocked(dest) if dest == dead_tid => {
                    st(tid).task_ipc.state = IpcState::None;
                    scheduler::unblock_task(tid);
                }
                IpcState::CallSendBlocked(dest) | IpcState::CallBlocked(dest) if dest == dead_tid => {
                    st(tid).task_ipc.pending_msg = Some(error_msg);
                    st(tid).task_ipc.state = IpcState::None;
                    scheduler::unblock_task(tid);
                }
                IpcState::RecvBlocked(from) if from == dead_tid => {
                    st(tid).task_ipc.pending_msg = Some(error_msg);
                    st(tid).task_ipc.state = IpcState::None;
                    scheduler::unblock_task(tid);
                }
                _ => {}
            }
        }
        drop(held);
    }
}

/// Clean up IPC state when a task is reaped.
/// Whoever was blocked on it was failed when it died ([`fail_waiters`]);
/// this is done again here for a task that died some other way.
pub fn cleanup_task_ipc(dead_tid: usize) {
    if dead_tid >= MAX_TASKS {
        return;
    }

    // Whatever it was waiting on is no longer working on its behalf.
    let waiting_on = blocked_on(dead_tid);

    let flags = irq_save();
    let held = NOTICES.lock();
    unsafe {
        // Withdraw its watches and drop what it never collected, and the
        // watches of it its death did not already tell of. TIDs are
        // recycled, so a registration left behind would fire for whoever
        // lands in the slot next.
        while st(dead_tid).watching != END && drop_watch(st(dead_tid).watching) {}
        while st(dead_tid).owed != END && drop_watch(st(dead_tid).owed) {}
        while st(dead_tid).watched != END && drop_watch(st(dead_tid).watched) {}
        st(dead_tid).watching = END;
        st(dead_tid).owed = END;
        st(dead_tid).watched = END;
    }
    drop(held);
    let held = scheduler::lock_record(dead_tid);
    unsafe {
        st(dead_tid).clean_wanted = false;
        // Clear the dead task's own IPC state, timeout, notifications, and signal deadline
        st(dead_tid).task_ipc.state = IpcState::None;
        st(dead_tid).task_ipc.pending_msg = None;
        st(dead_tid).task_ipc.lent = None;
        st(dead_tid).task_ipc.offer = None;
        st(dead_tid).timeout = 0;
        st(dead_tid).timed_out = false;
        st(dead_tid).notify = 0;
    }
    drop(held);
    // The deadlines' list is the one lock's, with interrupts off.
    unsafe { set_deadline(dead_tid, 0) };
    // And whoever is blocked on it, if its death did not already.
    fail_waiters(dead_tid);
    irq_restore(flags);
    if let Some(target) = waiting_on {
        scheduler::refresh_priority(target);
    }
}
