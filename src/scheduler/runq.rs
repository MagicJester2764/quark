//! The run queues: what each processor has ready to run, a queue for each
//! band.
//!
//! A band's ready tasks were one list for the machine, and every choice
//! looked at all of them — for whoever had run least, and again to say that
//! each task that had yielded had had its turn passed over — as did asking,
//! at every wake between ticks, every call's hand-over and every tick,
//! whether a real-time task was among them. With a few ready that was
//! nothing. With thousands it was most of what the kernel did: a program of
//! 4,095 threads that each woke ten times a second kept the list thousands
//! long, and the thread that had made them, having run longest, was chosen
//! last, minutes later. And a thread whose turn ended went back into the one
//! list for whichever processor looked next, so that as many computing
//! threads as processors went round them.
//!
//! Each processor has a queue of each band now, of three parts, each found
//! without looking at the others or at what is in them:
//!
//! - its real-time tasks, the best priority first and of those the first
//!   queued;
//! - the ordinary tasks put at the front — a caller its reply woke — the
//!   first put there first;
//! - the other ordinary tasks, whoever has run least first, as the band
//!   counts it on that processor (`PerTask::vrun`, the queue's `floor`).
//!
//! The first and the last are pairing heaps linked through the tasks' own
//! records (`PerTask::heap_*`): a queue takes no room of its own and is as
//! long as there are tasks, a task is put in in one step, and the first or
//! any other is taken out in about as many as a task's number has bits. A
//! task is in one queue at most, and its record says which and where.
//!
//! Where a task made ready waits is [`place_for`]'s to say; a processor with
//! nothing of its own takes the best of the busiest's ([`pull`]), and every
//! fourth tick one with less to run than the busiest takes a task light
//! enough that the two are nearer even afterwards ([`balance`]). What a
//! processor has to run is weighed, not counted (`usage::weight`): a nicer
//! task weighs less, and two processors each running one task at nought
//! and one at ten give the nicer a tenth of each, where one running both at
//! nought and the other both at ten gave the nicer half of the machine.
//! Each processor's queues are its own to choose from, but any processor
//! may put a task in them or take one out.
//!
//! **A processor's queues have a lock** (`sync::RANK_RUNQ`), and so do the
//! links of the tasks waiting in them — which queue and part a task is in,
//! its heap's and its list's links, the order it was queued in, what it
//! weighed as it went in — and what a heap orders its tasks by, how far
//! they have run and their real-time priority, which changes only while a
//! task is out of its queue ([`recount`], [`replace`]). A task moves
//! between processors in two steps, never holding two of these locks: out
//! of one queue under its lock, and into the other under its own, belonging
//! in between to whoever moves it. What a queue reads of a task that is
//! not its own — whether it is still ready, where it may run, how nice it
//! is — is read without the task's lock, which comes before this one in
//! the order; and where a task should wait, and which processor is
//! busiest, are worked out from other processors' queues without their
//! locks, as hints: their lengths, loads and floors are words, and one a
//! moment out of date places a task a little worse, never wrongly.

use super::{better, place_of, slot, st, TaskState, END, NOT_QUEUED, NUM_PRIORITIES, SLEEPER_LEAD};
use crate::percpu::MAX_CPUS;
use crate::sync::IrqSpinLock;
use core::sync::atomic::{AtomicU64, Ordering};


/// Which part of its queue a task is in.
pub(super) const IN_RT: u8 = 0;
pub(super) const IN_FRONT: u8 = 1;
pub(super) const IN_FAIR: u8 = 2;

/// One processor's ready tasks of one band.
#[derive(Clone, Copy)]
pub(super) struct Queue {
    /// The real-time tasks: a heap's root.
    rt: u16,
    /// The ordinary tasks put at the front: the first and the last, linked
    /// through `PerTask::run_next` and `run_prev`.
    front: (u16, u16),
    /// The other ordinary tasks: a heap's root.
    fair: u16,
    /// How many tasks are in it — one that is no longer ready included,
    /// until somebody comes to it — and what they weigh together.
    len: u32,
    load: u64,
    /// Where the band has got to on this processor: the least that the
    /// ordinary task it runs and those waiting by run time have run, never
    /// going back ([`settle`]). A task that was not ready joins no further
    /// back than a little behind it ([`join`]), and one that moves keeps its
    /// distance from it ([`moved`]). It was the most any task had run when
    /// it was chosen, which only rises and which a nice task's long strides,
    /// a call handed over and a server lent another band's place each drove
    /// up: moved by their distance from it, tasks' run times grew without
    /// end, until in a full dtest they were all the largest a run time can
    /// be, and two tasks of a processor took turns whatever their weights.
    floor: u64,
    /// Choices made from it. A task that yielded is passed over while this
    /// is what it was when the task was queued: one turn.
    turns: u64,
}

impl Queue {
    const EMPTY: Queue = Queue { rt: END, front: (END, END), fair: END, len: 0, load: 0, floor: 0, turns: 0 };
}

/// Every processor's, empty from [`init`]: noughts until then, rather than
/// forty kilobytes of empty queues in the kernel's image.
static mut QUEUES: [crate::sync::Padded<[Queue; NUM_PRIORITIES]>; MAX_CPUS] = unsafe { core::mem::zeroed() };

/// Make every processor's queues empty: before the first task is made.
///
/// # Safety
/// Once, on the first processor, before anything is queued.
pub(super) unsafe fn init() {
    unsafe {
        for queues in (*(&raw mut QUEUES)).iter_mut() {
            queues.0 = [Queue::EMPTY; NUM_PRIORITIES];
        }
    }
}
/// Each processor's queues' lock (above).
static LOCKS: [crate::sync::Padded<IrqSpinLock<()>>; MAX_CPUS] =
    [const { crate::sync::Padded(IrqSpinLock::new(crate::sync::RANK_RUNQ, "a processor's run queues", ())) }; MAX_CPUS];

/// Processor `cpu`'s queues, held.
fn held(cpu: usize) -> crate::sync::IrqSpinLockGuard<'static, ()> {
    LOCKS[cpu].lock()
}

/// The order tasks were queued in, for those otherwise equal: any
/// processor's queues count from it.
static SEQ: AtomicU64 = AtomicU64::new(0);
/// A task that ran on a processor less than this long ago is warm there:
/// what it uses is still in that processor's cache, and another processor
/// takes it last.
const WARM_NS: u64 = 2_000_000;

/// Processor `cpu`'s queue of band `p`.
///
/// # Safety
/// Interrupts off, and its lock held — or, for a hint, read and not
/// changed.
unsafe fn queue(cpu: usize, p: usize) -> &'static mut Queue {
    unsafe { &mut (*(&raw mut QUEUES))[cpu].0[p] }
}

/// How the tasks of a heap are ordered: whether the first goes before the
/// second.
type First = unsafe fn(usize, usize) -> bool;

/// Real-time tasks: the better priority first, and of the same the first
/// queued.
unsafe fn rt_first(a: usize, b: usize) -> bool {
    unsafe {
        let (ra, qa) = (st(a).rt, st(a).seq);
        let (rb, qb) = (st(b).rt, st(b).seq);
        ra > rb || (ra == rb && qa < qb)
    }
}

/// Ordinary tasks: whoever has run least first, and of the same the first
/// queued.
unsafe fn fair_first(a: usize, b: usize) -> bool {
    unsafe {
        let (va, qa) = (st(a).vrun, st(a).seq);
        let (vb, qb) = (st(b).vrun, st(b).seq);
        va < vb || (va == vb && qa < qb)
    }
}

/// `x` with neither siblings nor a parent: a root.
unsafe fn h_alone(x: u16) {
    unsafe {
        st(x as usize).heap_next = END;
        st(x as usize).heap_prev = END;
    }
}

/// Two heaps as one: the root that goes first stays the root, and the other
/// becomes its first child. A child's `heap_prev` is its parent if it is the
/// first, and its elder sibling if not.
unsafe fn h_meld(a: u16, b: u16, first: First) -> u16 {
    unsafe {
        if a == END {
            return b;
        }
        if b == END {
            return a;
        }
        let (root, child) = if first(a as usize, b as usize) { (a, b) } else { (b, a) };
        let eldest = st(root as usize).heap_child;
        st(child as usize).heap_next = eldest;
        st(child as usize).heap_prev = root;
        if eldest != END {
            st(eldest as usize).heap_prev = child;
        }
        st(root as usize).heap_child = child;
        root
    }
}

/// The siblings from `head` on, made one heap in two passes: each pair
/// melded, from the first, and then the pairs melded from the last. Taking
/// the first out is cheap over many takings for that.
unsafe fn h_pairs(head: u16, first: First) -> u16 {
    unsafe {
        // The pairs, each a root, kept on a stack through their `heap_next`.
        let mut stack = END;
        let mut x = head;
        while x != END {
            let a = x;
            let b = st(a as usize).heap_next;
            x = if b == END { END } else { st(b as usize).heap_next };
            h_alone(a);
            let pair = if b == END {
                a
            } else {
                h_alone(b);
                h_meld(a, b, first)
            };
            st(pair as usize).heap_next = stack;
            stack = pair;
        }
        let mut root = END;
        while stack != END {
            let pair = stack;
            stack = st(pair as usize).heap_next;
            st(pair as usize).heap_next = END;
            root = h_meld(root, pair, first);
        }
        root
    }
}

/// The heap at `root` with `x`, which is in no heap, in it.
unsafe fn h_push(root: u16, x: u16, first: First) -> u16 {
    unsafe {
        st(x as usize).heap_child = END;
        h_alone(x);
        h_meld(root, x, first)
    }
}

/// The heap at `root` without its root.
unsafe fn h_pop(root: u16, first: First) -> u16 {
    unsafe {
        let rest = h_pairs(st(root as usize).heap_child, first);
        st(root as usize).heap_child = END;
        h_alone(root);
        rest
    }
}

/// The heap at `root` without `x`, which is in it: `x` is cut from where it
/// is, its children made a heap, and that melded with the rest.
unsafe fn h_remove(root: u16, x: u16, first: First) -> u16 {
    unsafe {
        if x == root {
            return h_pop(root, first);
        }
        let (prev, next) = (st(x as usize).heap_prev, st(x as usize).heap_next);
        if st(prev as usize).heap_child == x {
            st(prev as usize).heap_child = next;
        } else {
            st(prev as usize).heap_next = next;
        }
        if next != END {
            st(next as usize).heap_prev = prev;
        }
        h_alone(x);
        let below = h_pairs(st(x as usize).heap_child, first);
        st(x as usize).heap_child = END;
        h_meld(root, below, first)
    }
}

/// Whether `tid` is ready to run: a task in a queue that has since died, or
/// blocked, is not.
unsafe fn ready(tid: usize) -> bool {
    unsafe { matches!(*slot(tid), Some(ref t) if t.state == TaskState::Ready) }
}

/// `tid` is out of its queue's count and its weight, and in no queue.
unsafe fn out(q: &mut Queue, tid: usize) {
    unsafe {
        q.len -= 1;
        q.load -= st(tid).weight as u64;
        st(tid).queued = NOT_QUEUED;
    }
}

/// Put `tid`, which is in no queue, in processor `cpu`'s queue of band `p`:
/// a real-time task among the real-time ones, an ordinary one at the front
/// if `front` says so, and by what it has run if not.
///
/// # Safety
/// Interrupts off, the kernel lock held.
pub(super) unsafe fn link(tid: usize, cpu: usize, p: usize, front: bool) {
    let _queues = held(cpu);
    unsafe { link_held(tid, cpu, p, front) }
}

/// [`link`], with `cpu`'s queues held.
unsafe fn link_held(tid: usize, cpu: usize, p: usize, front: bool) {
    unsafe {
        st(tid).seq = SEQ.fetch_add(1, Ordering::Relaxed) + 1;
        st(tid).queued = p as u8;
        st(tid).queued_on = cpu as u16;
        st(tid).measured_on = cpu as u16;
        st(tid).measured_in = p as u8;
        // What it weighs as it is put in is what it takes out with it: how
        // nice it is can change while it waits.
        st(tid).weight = crate::usage::weight(st(tid).nice) as u32;
        let q = queue(cpu, p);
        q.len += 1;
        q.load += st(tid).weight as u64;
        let t = tid as u16;
        if st(tid).rt > 0 {
            st(tid).heap_in = IN_RT;
            q.rt = h_push(q.rt, t, rt_first);
        } else if front {
            st(tid).heap_in = IN_FRONT;
            st(tid).run_next = END;
            st(tid).run_prev = q.front.1;
            if q.front.1 == END {
                q.front.0 = t;
            } else {
                st(q.front.1 as usize).run_next = t;
            }
            q.front.1 = t;
        } else {
            st(tid).heap_in = IN_FAIR;
            st(tid).yield_turn = q.turns;
            q.fair = h_push(q.fair, t, fair_first);
        }
    }
}

/// Take `tid` out of the queue it is in, if it is in one: that processor's,
/// held, and looked at again once it is, since the task may have been
/// moved meanwhile.
///
/// # Safety
/// Interrupts off, the kernel lock held.
pub(super) unsafe fn unlink(tid: usize) {
    unsafe {
        loop {
            if st(tid).queued == NOT_QUEUED {
                return;
            }
            let cpu = st(tid).queued_on as usize;
            let _queues = held(cpu);
            if st(tid).queued != NOT_QUEUED && st(tid).queued_on as usize == cpu {
                unlink_held(tid);
                return;
            }
        }
    }
}

/// [`unlink`], with the queues `tid` is in held.
unsafe fn unlink_held(tid: usize) {
    unsafe {
        let p = st(tid).queued;
        if p == NOT_QUEUED {
            return;
        }
        let q = queue(st(tid).queued_on as usize, p as usize);
        let t = tid as u16;
        match st(tid).heap_in {
            IN_RT => q.rt = h_remove(q.rt, t, rt_first),
            IN_FRONT => {
                let (next, prev) = (st(tid).run_next, st(tid).run_prev);
                if prev == END {
                    q.front.0 = next;
                } else {
                    st(prev as usize).run_next = next;
                }
                if next == END {
                    q.front.1 = prev;
                } else {
                    st(next as usize).run_prev = prev;
                }
                (st(tid).run_next, st(tid).run_prev) = (END, END);
            }
            _ => q.fair = h_remove(q.fair, t, fair_first),
        }
        out(q, tid);
    }
}

/// The best real-time task ready in processor `cpu`'s queue of band `p`
/// that may run on processor `for_cpu`, taken out of it. Those that may
/// not — eight at most are looked past — stay where they were.
unsafe fn take_rt(cpu: usize, p: usize, for_cpu: usize) -> Option<usize> {
    unsafe {
        let q = queue(cpu, p);
        let mut aside = [END; 8];
        let mut set = 0;
        let mut found = None;
        while q.rt != END && set < aside.len() {
            let t = q.rt as usize;
            q.rt = h_pop(q.rt, rt_first);
            if !ready(t) {
                out(q, t);
                continue;
            }
            if !super::may_run_on(t, for_cpu) {
                aside[set] = t as u16;
                set += 1;
                continue;
            }
            out(q, t);
            found = Some(t);
            break;
        }
        for &t in &aside[..set] {
            q.rt = h_push(q.rt, t, rt_first);
        }
        found
    }
}

/// The ordinary task to go next of processor `cpu`'s queue of band `p` that
/// may run on processor `for_cpu`, taken out of it: the first put at the
/// front, or else whoever has run least — passing over one that yielded
/// this turn, and with `cold` one that ran within [`WARM_NS`], while another
/// is ready; with nothing else ready, the first of those goes after all. A
/// task that may not run on `for_cpu` never goes, and stays where it was;
/// eight at most of all these are looked past.
unsafe fn take_ordinary(cpu: usize, p: usize, cold: bool, for_cpu: usize) -> Option<usize> {
    unsafe {
        let q = queue(cpu, p);
        let mut t = q.front.0;
        while t != END {
            let next = st(t as usize).run_next;
            if !ready(t as usize) {
                unlink_held(t as usize);
            } else if super::may_run_on(t as usize, for_cpu) {
                unlink_held(t as usize);
                return Some(t as usize);
            }
            t = next;
        }
        let now = if cold { crate::clock::now() } else { 0 };
        // Passed over, which may still go; and kept off this processor,
        // which may not. Both go back.
        let mut passed = [END; 4];
        let mut npassed = 0;
        let mut barred = [END; 8];
        let mut nbarred = 0;
        let mut found = None;
        while q.fair != END && npassed < passed.len() && nbarred < barred.len() {
            let t = q.fair as usize;
            q.fair = h_pop(q.fair, fair_first);
            if !ready(t) {
                out(q, t);
                continue;
            }
            if !super::may_run_on(t, for_cpu) {
                barred[nbarred] = t as u16;
                nbarred += 1;
                continue;
            }
            let yielded = st(t).yielded && st(t).yield_turn == q.turns;
            let warm = cold && now.saturating_sub(st(t).ran_at) < WARM_NS;
            if yielded || warm {
                passed[npassed] = t as u16;
                npassed += 1;
                continue;
            }
            out(q, t);
            found = Some(t);
            break;
        }
        for (i, &t) in passed[..npassed].iter().enumerate() {
            if found.is_none() && i == 0 {
                out(q, t as usize);
                found = Some(t as usize);
            } else {
                q.fair = h_push(q.fair, t, fair_first);
            }
        }
        for &t in &barred[..nbarred] {
            q.fair = h_push(q.fair, t, fair_first);
        }
        found
    }
}

/// `tid` has been chosen from processor `cpu`'s queue of band `p`: a turn
/// of the queue, and where its band has got to there. `cpu`'s queues held.
unsafe fn chosen_from(cpu: usize, p: usize, tid: usize) {
    unsafe {
        queue(cpu, p).turns += 1;
        st(tid).yielded = false;
        settle_held(cpu, p, tid);
    }
}

/// Where band `p` has got to on processor `cpu`, which runs `running`: the
/// least that it and the tasks waiting by run time have run, if that is
/// further than the floor was. A real-time task is chosen by priority, and
/// what it has run is not counted in it.
///
/// # Safety
/// Interrupts off, the kernel lock held.
pub(super) unsafe fn settle(cpu: usize, p: usize, running: usize) {
    let _queues = held(cpu);
    unsafe { settle_held(cpu, p, running) }
}

/// [`settle`], with `cpu`'s queues held.
unsafe fn settle_held(cpu: usize, p: usize, running: usize) {
    unsafe {
        let q = queue(cpu, p);
        let mut least = u64::MAX;
        if running != 0 && st(running).rt == 0 {
            least = st(running).vrun;
        }
        if q.fair != END {
            least = least.min(st(q.fair as usize).vrun);
        }
        if least != u64::MAX && least > q.floor {
            q.floor = least;
        }
    }
}

/// What is to run next of processor `cpu`'s queue of band `p`, taken out of
/// it: a real-time task first — or last, when the processor's real-time
/// tasks have had their share (`throttled`) — then one put at the front,
/// then whoever has run least.
///
/// # Safety
/// Interrupts off, the kernel lock held.
pub(super) unsafe fn take(cpu: usize, p: usize, throttled: bool) -> Option<usize> {
    let _queues = held(cpu);
    unsafe {
        let chosen = if throttled {
            take_ordinary(cpu, p, false, cpu).or_else(|| take_rt(cpu, p, cpu))
        } else {
            take_rt(cpu, p, cpu).or_else(|| take_ordinary(cpu, p, false, cpu))
        }?;
        chosen_from(cpu, p, chosen);
        Some(chosen)
    }
}

/// The best band with anything waiting in processor `cpu`'s queues.
///
/// # Safety
/// Interrupts off, the kernel lock held.
pub(super) unsafe fn best_band(cpu: usize) -> Option<usize> {
    let _queues = held(cpu);
    unsafe { (0..NUM_PRIORITIES).find(|&p| queue(cpu, p).len > 0) }
}

/// How many tasks wait in processor `cpu`'s queues: as a hint, read
/// without their lock.
///
/// # Safety
/// Interrupts off, the kernel lock held.
pub(super) unsafe fn waiting(cpu: usize) -> u32 {
    unsafe { (0..NUM_PRIORITIES).map(|p| queue(cpu, p).len).sum() }
}

/// Whether something waits on another processor than `me` for `me` to
/// take (`pull`).
///
/// # Safety
/// Interrupts off, the kernel lock held.
pub(super) unsafe fn takeable(me: usize) -> bool {
    unsafe { busiest(me, None).is_some() }
}

/// Everything waiting in processor `cpu`'s queues, put to wait where it may
/// run: for a processor taken offline, which may run nothing.
///
/// # Safety
/// Interrupts off, the kernel lock held.
pub(super) unsafe fn give_away(cpu: usize) {
    unsafe {
        for p in 0..NUM_PRIORITIES {
            loop {
                // Out under this processor's lock, and to wherever it goes
                // under that one's: one at a time.
                let t = {
                    let _queues = held(cpu);
                    let q = queue(cpu, p);
                    let Some(t) = [q.rt, q.front.0, q.fair].into_iter().find(|&t| t != END) else { break };
                    unlink_held(t as usize);
                    t as usize
                };
                if ready(t) {
                    super::enqueue(t);
                }
            }
        }
    }
}

/// What waits first in processor `cpu`'s queues — the first of each band's
/// real-time tasks, its front and its fair queue, looked at in that order —
/// that a processor asleep may run, put to wait there instead: which
/// processor, for it to be woken. Moved rather than left to be taken: a
/// processor that wakes takes from the busiest (`pull`), which need not be
/// this one, and what waits there need not be anything it may run.
///
/// # Safety
/// Interrupts off, the kernel lock held.
pub(super) unsafe fn hand_to_sleeper(cpu: usize) -> Option<usize> {
    unsafe {
        let count = crate::percpu::count();
        if waiting(cpu) == 0 || !(0..count).any(|n| n != cpu && crate::percpu::napping(n)) {
            return None;
        }
        // Out of this processor's queue under its lock; into the sleeper's
        // under that one's.
        let found = {
            let _queues = held(cpu);
            let mut found = None;
            'bands: for p in 0..NUM_PRIORITIES {
                let q = queue(cpu, p);
                if q.len == 0 {
                    continue;
                }
                for t in [q.rt, q.front.0, q.fair] {
                    if t == END || !ready(t as usize) {
                        continue;
                    }
                    let t = t as usize;
                    if let Some(n) = (0..count).find(|&n| n != cpu && crate::percpu::napping(n) && super::may_run_on(t, n)) {
                        let front = st(t).heap_in == IN_FRONT;
                        unlink_held(t);
                        found = Some((t, n, p, front));
                        break 'bands;
                    }
                }
            }
            found
        };
        let (t, n, p, front) = found?;
        moved(t, n, p);
        link(t, n, p, front);
        Some(n)
    }
}

/// The best real-time priority waiting in processor `cpu`'s queue of band
/// `p`; 0 for none.
///
/// # Safety
/// Interrupts off, the kernel lock held.
pub(super) unsafe fn best_rt(cpu: usize, p: usize) -> u8 {
    let _queues = held(cpu);
    unsafe {
        let q = queue(cpu, p);
        if q.rt == END { 0 } else { st(q.rt as usize).rt }
    }
}

/// Whether an ordinary task waits in processor `cpu`'s queue of band `p`.
///
/// # Safety
/// Interrupts off, the kernel lock held.
pub(super) unsafe fn has_ordinary(cpu: usize, p: usize) -> bool {
    let _queues = held(cpu);
    unsafe {
        let q = queue(cpu, p);
        q.front.0 != END || q.fair != END
    }
}

/// A task joining processor `cpu`'s queue of band `p` that may have been
/// away from it: no further back than a little behind where the band has
/// got to there. A program that slept for a minute is not owed the minute.
/// The floor is read without the queue's lock: a word that only grows,
/// a moment out of date at worst. The task is in no queue, and its run
/// time is whoever is putting it in one's.
///
/// # Safety
/// Interrupts off, the kernel lock held.
pub(super) unsafe fn join(tid: usize, cpu: usize, p: usize) {
    unsafe {
        let floor = queue(cpu, p).floor;
        st(tid).vrun = st(tid).vrun.max(floor.saturating_sub(SLEEPER_LEAD));
    }
}

/// `tid`, whose run time is counted as some processor's band counts it
/// (`PerTask::measured_on`, `measured_in`), is to wait or run on processor
/// `to` in band `p`: the same distance from where that band has got to
/// there, and counted as it counts it from now. Each processor's band gets
/// as far as its own tasks have run, and a task from a busy one would
/// otherwise wait behind everything on a quiet one, or go before it all.
/// Every way a task comes to a processor or a band says so: a call handed
/// over to a server on the caller's processor, a server lent its caller's
/// band. The floors are read as [`join`] reads one.
///
/// # Safety
/// Interrupts off, the kernel lock held.
pub(super) unsafe fn moved(tid: usize, to: usize, p: usize) {
    unsafe {
        let (from, q) = (st(tid).measured_on as usize, st(tid).measured_in as usize);
        st(tid).measured_on = to as u16;
        st(tid).measured_in = p as u8;
        if from == to && q == p {
            return;
        }
        let (was, is) = (queue(from.min(MAX_CPUS - 1), q.min(NUM_PRIORITIES - 1)).floor, queue(to, p).floor);
        let vrun = st(tid).vrun;
        st(tid).vrun = if is >= was { vrun.saturating_add(is - was) } else { vrun.saturating_sub(was - is) };
    }
}

/// Where `tid`, made ready, waits for its turn: the processor it last ran
/// on, if it would run there at once — that one has nothing to do, or runs
/// something `tid` outranks; else one that is asleep — one whose core has
/// nothing else to run first, then one of the same package as its last;
/// else the one running the worst, if `tid` outranks what that runs; else,
/// for a task that has never run, the one with least to run, and for any
/// other its last. Each of them one it may run on (`may_run_on`, its
/// affinity). Waiting on its last processor behind something no better
/// while another slept, a thread a futex woke waited out a turn for
/// nothing.
///
/// # Safety
/// Interrupts off, the kernel lock held.
pub(super) unsafe fn place_for(tid: usize) -> usize {
    unsafe {
        let count = crate::percpu::count();
        // Kept to processors none of which is online — taken offline since
        // — it is let run on any, as Linux lets it: a task that may run
        // nowhere is a task that never runs again.
        if !(0..count).any(|cpu| super::may_run_on(tid, cpu)) {
            st(tid).allowed = [u64::MAX; 4];
        }
        let may = |cpu: usize| super::may_run_on(tid, cpu);
        let last = (st(tid).last_cpu as usize).min(count - 1);
        // Where it may run at all, if not its last.
        let anywhere = if may(last) { last } else { (0..count).find(|&cpu| may(cpu)).unwrap_or(last) };
        if count == 1 {
            return 0;
        }
        let mine = place_of(tid);
        let running = crate::percpu::current_of;
        if may(last) && (running(last) == 0 || better(mine, place_of(running(last)))) {
            return last;
        }
        let near = crate::percpu::place(anywhere);
        let mut asleep: Option<(u8, usize)> = None;
        for cpu in (0..count).filter(|&cpu| may(cpu) && crate::percpu::napping(cpu)) {
            let here = crate::percpu::place(cpu);
            let mate_busy = (0..count).any(|other| {
                let there = crate::percpu::place(other);
                other != cpu && there.package == here.package && there.core == here.core && running(other) != 0
            });
            let score = (mate_busy as u8) << 1 | (here.package != near.package) as u8;
            if asleep.is_none_or(|(best, _)| score < best) {
                asleep = Some((score, cpu));
            }
        }
        if let Some((_, cpu)) = asleep {
            return cpu;
        }
        let mut worst = anywhere;
        for cpu in (0..count).filter(|&cpu| may(cpu)) {
            if better(place_of(running(worst)), place_of(running(cpu))) {
                worst = cpu;
            }
        }
        if better(mine, place_of(running(worst))) {
            return worst;
        }
        // A task that has never run has nothing in any cache: it goes where
        // there is least to run.
        if st(tid).ran_at == 0 {
            let p = super::priority_of(tid);
            return (0..count).filter(|&cpu| may(cpu)).min_by_key(|&cpu| load(cpu, Some(p))).unwrap_or(anywhere);
        }
        anywhere
    }
}

/// What processor `cpu` has to run of band `p` — or of every band, for
/// `None` — weighed: what waits in its queues and what it is running. A
/// hint, read without the queues' lock.
unsafe fn load(cpu: usize, p: Option<usize>) -> u64 {
    unsafe {
        let running = crate::percpu::current_of(cpu);
        let busy = if running != 0 && p.is_none_or(|p| super::priority_of(running) == p) {
            crate::usage::weight(st(running).nice)
        } else {
            0
        };
        let waiting = match p {
            Some(p) => queue(cpu, p).load,
            None => (0..NUM_PRIORITIES).map(|p| queue(cpu, p).load).sum(),
        };
        waiting + busy
    }
}

/// The processor other than `me` with the most to run of band `p` — or of
/// every band, for `None` — of those with any of it waiting: of `me`'s
/// package if one there has; `None` if none has. Not one running nothing:
/// what waits there was put there for it, and it has been woken to run it
/// (`enqueue`) — taken from under it, it woke to nothing.
unsafe fn busiest(me: usize, p: Option<usize>) -> Option<usize> {
    unsafe {
        let count = crate::percpu::count();
        let package = crate::percpu::place(me).package;
        let mut best: Option<((bool, u64), usize)> = None;
        for cpu in (0..count).filter(|&cpu| cpu != me && crate::percpu::current_of(cpu) != 0) {
            let any = match p {
                Some(p) => queue(cpu, p).len > 0,
                None => waiting(cpu) > 0,
            };
            if !any {
                continue;
            }
            let key = (crate::percpu::place(cpu).package == package, load(cpu, p));
            if best.is_none_or(|(b, _)| key > b) {
                best = Some((key, cpu));
            }
        }
        best.map(|(_, cpu)| cpu)
    }
}

/// For processor `me`, which has nothing waiting of its own: the best task
/// waiting on the busiest — a cold one first, of those as good — taken
/// from there to run here, its run time moved to this processor's measure.
///
/// # Safety
/// Interrupts off, the kernel lock held, on processor `me`.
pub(super) unsafe fn pull(me: usize, throttled: bool) -> Option<usize> {
    unsafe {
        let from = busiest(me, None)?;
        for p in 0..NUM_PRIORITIES {
            // Out of the busiest's queue under its lock; chosen here under
            // this one's.
            let taken = {
                let _queues = held(from);
                if throttled {
                    take_ordinary(from, p, true, me).or_else(|| take_rt(from, p, me))
                } else {
                    take_rt(from, p, me).or_else(|| take_ordinary(from, p, true, me))
                }
            };
            if let Some(t) = taken {
                moved(t, me, p);
                let _queues = held(me);
                chosen_from(me, p, t);
                return Some(t);
            }
        }
        None
    }
}

/// Of the next few ordinary tasks waiting by run time in processor
/// `cpu`'s queue of band `p`, the first weighing less than `under` that may
/// run on processor `for_cpu` — a cold one before a warm one — taken out;
/// the others left as they were.
unsafe fn take_lighter(cpu: usize, p: usize, under: u64, for_cpu: usize) -> Option<usize> {
    unsafe {
        let q = queue(cpu, p);
        let now = crate::clock::now();
        let mut looked = [END; 8];
        let mut set = 0;
        let mut found = None;
        let mut warm = None;
        while q.fair != END && set < looked.len() {
            let t = q.fair as usize;
            q.fair = h_pop(q.fair, fair_first);
            if !ready(t) {
                out(q, t);
                continue;
            }
            let fits = (st(t).weight as u64) < under && super::may_run_on(t, for_cpu);
            if fits && now.saturating_sub(st(t).ran_at) >= WARM_NS {
                out(q, t);
                found = Some(t);
                break;
            }
            if fits && warm.is_none() {
                warm = Some(set);
            }
            looked[set] = t as u16;
            set += 1;
        }
        for (i, &t) in looked[..set].iter().enumerate() {
            if found.is_none() && warm == Some(i) {
                out(q, t as usize);
                found = Some(t as usize);
            } else {
                q.fair = h_push(q.fair, t, fair_first);
            }
        }
        found
    }
}

/// On processor `me`'s tick, every fourth, band by band: if the busiest
/// has more of the band to run than this one, a task of it waiting there
/// light enough that the two are nearer even once it has moved — weighing
/// less than the difference — comes to wait here instead, a cold one first.
/// Moved by weight, never back: a task that would leave this one heavier
/// than the other stays where it is. A band is weighed alone: what a better
/// band runs for a moment is no reason for a task of a worse one to move —
/// weighed together, a server's turn on one processor sent computing
/// programs elsewhere, and two at nought and two at nice 10 stopped being
/// one of each a processor.
///
/// # Safety
/// Interrupts off, the kernel lock held, on processor `me`.
pub(super) unsafe fn balance(me: usize) {
    unsafe {
        for p in 0..NUM_PRIORITIES {
            let Some(from) = busiest(me, Some(p)) else { continue };
            let (theirs, mine) = (load(from, Some(p)), load(me, Some(p)));
            if theirs <= mine {
                continue;
            }
            let taken = {
                let _queues = held(from);
                take_lighter(from, p, theirs - mine, me)
            };
            if let Some(t) = taken {
                moved(t, me, p);
                link(t, me, p, false);
                return;
            }
        }
    }
}

/// `change` made to `tid` while it is out of the queue it is in, if it is
/// in one, and the task put back where it was — the same processor, band
/// and part — in one step under that processor's lock: how far a task has
/// run is what a heap orders it by, and changed in a heap it would be out
/// of its order there.
///
/// # Safety
/// Interrupts off, the kernel lock held.
pub(super) unsafe fn recount(tid: usize, change: impl FnOnce()) {
    unsafe {
        loop {
            if st(tid).queued == NOT_QUEUED {
                change();
                return;
            }
            let cpu = st(tid).queued_on as usize;
            let _queues = held(cpu);
            if st(tid).queued == NOT_QUEUED || st(tid).queued_on as usize != cpu {
                continue;
            }
            let (band, front) = (st(tid).queued as usize, st(tid).heap_in == IN_FRONT);
            unlink_held(tid);
            change();
            link_held(tid, cpu, band, front);
            return;
        }
    }
}

/// `change` made to `tid`'s place while it is out of the queue it is in,
/// if it is in one, and the task put back on the same processor where its
/// place says now, by what it has run — in one step under that processor's
/// lock, as [`recount`]. Whether it was in one.
///
/// # Safety
/// Interrupts off, the kernel lock held.
pub(super) unsafe fn replace(tid: usize, change: impl FnOnce()) -> bool {
    unsafe {
        loop {
            if st(tid).queued == NOT_QUEUED {
                change();
                return false;
            }
            let cpu = st(tid).queued_on as usize;
            let _queues = held(cpu);
            if st(tid).queued == NOT_QUEUED || st(tid).queued_on as usize != cpu {
                continue;
            }
            unlink_held(tid);
            change();
            let band = super::priority_of(tid);
            moved(tid, cpu, band);
            link_held(tid, cpu, band, false);
            return true;
        }
    }
}
