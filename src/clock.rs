//! What time it is, to the nanosecond, and seeing that what is due at a time
//! is looked at then.
//!
//! The 8254 interrupts a hundred times a second, and for a long time that
//! was the whole of time here: the count of its ticks was the clock, a wait
//! ended on a tick, and a program that asked what time it was got an answer
//! ten milliseconds wide. A toolkit that draws sixty frames a second asks
//! to be woken in sixteen milliseconds and was woken in twenty, or ten.
//!
//! Two things replace that, where the machine has them.
//!
//! **The clock is the processor's time-stamp counter**, which counts at a
//! steady rate the kernel measures once, at boot, against the 8254. Every
//! time the kernel keeps — a deadline, a timer, an alarm — is nanoseconds
//! since boot by that count, and so is every time it tells
//! ([`now`], `SYS_CLOCK`). The counter is used only where it can be trusted
//! to keep counting at one rate whatever the processor is doing: where the
//! processor says so (it calls that *invariant*), or under a hypervisor,
//! whose guest does not put the processor to sleep itself and reads the
//! host's counter with a constant added. And only if every processor's
//! counter reads the same, which is looked at as each one is started
//! (`smp.rs`, [`distrust`]).
//!
//! **What is due is fired by the first processor's own timer**, the one in
//! its local APIC, which the kernel had no use for: that processor is ticked
//! by the 8254. It is set, once, for the earliest thing due, however far
//! off, and the interrupt it raises does what a tick does about time and
//! nothing else ([`expire`]). That is what lets the tick stop: a processor
//! with nothing to run takes none (`scheduler::idle`), the first included,
//! and what is due comes when it is due. It was set only for what came
//! before the next tick, and the tick looked at the rest.
//!
//! Where there is no counter to trust, the clock is the count of ticks, as
//! it was, and everything here answers in multiples of ten milliseconds.
//! Where there is no local APIC, the clock is fine and waking is not: a wait
//! ends on the first tick at or after its time.
//!
//! A *span* handed to the kernel is a number of ticks, as it always was, or
//! with its top bit set a number of nanoseconds ([`span`]): one rule for
//! every call that takes one, and no call's number had to change.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::io::{inb, outb};

/// One tick of the 8254 as the kernel sets it: a hundredth of a second.
pub const TICK_NS: u64 = 10_000_000;

/// The bit that says a span is in nanoseconds and not in ticks. A count of
/// ticks never reaches it: that is three thousand million years of them.
pub const IN_NS: u64 = 1 << 63;

/// The first processor's timer is never set to interrupt sooner than this
/// after it last did. A program may ask for a timer that repeats every
/// nanosecond; what it gets is one that is looked at twenty thousand times
/// a second and counts how many nanoseconds went by, and a machine that
/// goes on doing other things.
const MIN_GAP_NS: u64 = 50_000;

/// Whether the time-stamp counter is the clock.
static FINE: AtomicBool = AtomicBool::new(false);
/// What the counter read when the clock was started.
static BASE: AtomicU64 = AtomicU64::new(0);
/// Nanoseconds for each count, as a fraction of 2^32.
static MUL: AtomicU64 = AtomicU64::new(0);
/// The latest time anybody has been told. Two processors' counters agree to
/// within a few counts, not exactly, and a time is never earlier than one
/// already given out.
static LAST: AtomicU64 = AtomicU64::new(0);

/// Whether the first processor's timer is there to be set.
static TIMER: AtomicBool = AtomicBool::new(false);
/// The time that timer is set for, or `u64::MAX` when it is not set.
static SET_FOR: AtomicU64 = AtomicU64::new(u64::MAX);
/// The soonest deadline said (`due`) since the clock last began to look
/// for what is due (`expire`): one written down after the look passed it
/// is not missed. Only ever lowered, but by `expire`.
static SAID: AtomicU64 = AtomicU64::new(u64::MAX);
/// When it last interrupted.
static LAST_SHOT: AtomicU64 = AtomicU64::new(0);

/// Nanoseconds between 1970 and the moment the clock was started.
static WALL_BASE: AtomicU64 = AtomicU64::new(0);

#[inline(always)]
fn counter() -> u64 {
    unsafe { core::arch::x86_64::_rdtsc() }
}

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

/// Nanoseconds since the clock was started, which is since boot.
pub fn now() -> u64 {
    if !FINE.load(Ordering::Relaxed) {
        return crate::pit::ticks() * TICK_NS;
    }
    let counted = counter().wrapping_sub(BASE.load(Ordering::Relaxed));
    let ns = ((counted as u128 * MUL.load(Ordering::Relaxed) as u128) >> 32) as u64;
    LAST.fetch_max(ns, Ordering::Relaxed).max(ns)
}

/// [`now`], without the step that keeps it from going back between two
/// processors' readings: for a span measured where both ends are this
/// processor's, or near enough that a nanosecond's disagreement is a
/// `saturating_sub` — the time a task spends in the kernel, read at every
/// system call.
#[inline]
pub fn now_here() -> u64 {
    if !FINE.load(Ordering::Relaxed) {
        return crate::pit::ticks() * TICK_NS;
    }
    let counted = counter().wrapping_sub(BASE.load(Ordering::Relaxed));
    ((counted as u128 * MUL.load(Ordering::Relaxed) as u128) >> 32) as u64
}

/// Whether the clock is finer than a tick.
pub fn fine() -> bool {
    FINE.load(Ordering::Relaxed)
}

/// A span of time as a call was given it, in nanoseconds: ticks, or with
/// [`IN_NS`] set, nanoseconds already.
pub fn span(arg: u64) -> u64 {
    if arg & IN_NS != 0 {
        arg & !IN_NS
    } else {
        arg.saturating_mul(TICK_NS)
    }
}

/// A span of nanoseconds as a count of ticks, for a call answering in them:
/// rounded up, so that time still to come is never said to be none.
pub fn ticks_of(ns: u64) -> u64 {
    ns.div_ceil(TICK_NS)
}

/// The time a span from now ends at. Never 0, which is how "no deadline" is
/// written everywhere a deadline is kept, and never wrapping: a time too far
/// off to count is as far off as can be.
pub fn after(span_ns: u64) -> u64 {
    now().saturating_add(span_ns).max(1)
}

/// Nanoseconds since 1970, or 0 on a machine that has no clock to say.
pub fn wall() -> u64 {
    match WALL_BASE.load(Ordering::Relaxed) {
        0 => 0,
        base => base.saturating_add(now()),
    }
}

/// Seconds between 1970 and boot: what `SYS_BOOT_TIME` has always answered.
pub fn boot_seconds() -> u64 {
    WALL_BASE.load(Ordering::Relaxed) / 1_000_000_000
}

/// Say what time it is: `ns` nanoseconds since 1970, now.
pub fn set_wall(ns: u64) {
    WALL_BASE.store(ns.saturating_sub(now()).max(1), Ordering::Relaxed);
}

// --- Starting the clock ---------------------------------------------------

const PIT_HZ: u64 = 1_193_182;
const PIT_CH2: u16 = 0x42;
const PIT_CMD: u16 = 0x43;
/// The port the second channel's gate, and the speaker it was wired to, are
/// switched through.
const PIT_GATE: u16 = 0x61;

fn cpuid(leaf: u32) -> core::arch::x86_64::CpuidResult {
    core::arch::x86_64::__cpuid(leaf)
}

/// Whether the counter can be the clock: there is one, and it counts at one
/// rate whatever the processor is doing.
fn trusted() -> bool {
    let features = cpuid(1);
    if features.edx & (1 << 4) == 0 {
        return false;
    }
    // Under a hypervisor: the guest's counter is the host's with a constant
    // added, and goes on counting while the guest is not being run. (A host
    // whose own counter cannot be trusted is not one this can tell.)
    if features.ecx & (1 << 31) != 0 {
        return true;
    }
    cpuid(0x8000_0000).eax >= 0x8000_0007 && cpuid(0x8000_0007).edx & (1 << 8) != 0
}

/// The second channel's count and the counter, read as nearly together as
/// they can be: of eight tries, the one that took least.
///
/// # Safety
/// Interrupts off, and the channel counting.
unsafe fn pair() -> (u64, u16) {
    let mut best = (0u64, 0u16);
    let mut least = u64::MAX;
    for _ in 0..8 {
        let before = counter();
        let count = unsafe {
            outb(PIT_CMD, 0x80); // latch the second channel's count
            let low = inb(PIT_CH2) as u16;
            low | (inb(PIT_CH2) as u16) << 8
        };
        let took = counter().wrapping_sub(before);
        if took < least {
            least = took;
            best = (before.wrapping_add(took / 2), count);
        }
    }
    best
}

/// What one measurement came to.
enum Measured {
    /// The counter's rate.
    Rate(u64),
    /// Nothing: the processor was somewhere else in the middle of it, as a
    /// guest's sometimes is. Worth doing again.
    Spoiled,
    /// Nothing, and there will not be: the channel does not count.
    NoChannel,
}

/// How fast the counter counts, measured once against the 8254's second
/// channel — the one nothing uses, which needs no interrupt to be read.
///
/// # Safety
/// Interrupts off.
unsafe fn measure_once() -> Measured {
    /// Twenty milliseconds of the channel's counting, of the fifty-five it
    /// has before it has counted all the way down.
    const OVER: u16 = (PIT_HZ / 50) as u16;
    unsafe {
        // The gate on, the speaker off; the channel to count down once from
        // as far as it can.
        let gate = inb(PIT_GATE);
        outb(PIT_GATE, (gate & !0x02) | 0x01);
        outb(PIT_CMD, 0xB0);
        outb(PIT_CH2, 0xFF);
        outb(PIT_CH2, 0xFF);
        // The count is taken in on the channel's next pulse, not on the
        // write: one reading thrown away is longer than that.
        let _ = pair();

        let (from, start) = pair();
        let mut result = Measured::NoChannel;
        let mut last = start;
        // Bounded: a channel that is not there reads the same for ever.
        for _ in 0..400_000u32 {
            outb(PIT_CMD, 0x80);
            let low = inb(PIT_CH2) as u16;
            let count = low | (inb(PIT_CH2) as u16) << 8;
            // It counts down. One that has gone up has been all the way
            // round, and nothing can be said about how far it went.
            if count > last {
                result = Measured::Spoiled;
                break;
            }
            last = count;
            if start - count >= OVER {
                let (to, end) = pair();
                result = if end <= count {
                    let counted = to.wrapping_sub(from);
                    Measured::Rate((counted as u128 * PIT_HZ as u128 / (start - end) as u128) as u64)
                } else {
                    Measured::Spoiled
                };
                break;
            }
        }
        outb(PIT_GATE, gate);
        result
    }
}

/// The middle of three measurements: one that was a little wrong — the
/// processor taken away for a moment at one end of it — is not the one
/// kept.
///
/// # Safety
/// Interrupts off.
unsafe fn measure() -> Option<u64> {
    let mut hz = [0u64; 3];
    let mut have = 0;
    for _ in 0..12 {
        match unsafe { measure_once() } {
            Measured::Rate(rate) => {
                hz[have] = rate;
                have += 1;
                if have == hz.len() {
                    hz.sort_unstable();
                    return Some(hz[1]);
                }
            }
            Measured::Spoiled => {}
            Measured::NoChannel => return None,
        }
    }
    None
}

/// What the processor says its counter's rate is, where it says: the rate
/// of its crystal and the ratio of the counter to that.
fn stated() -> Option<u64> {
    if cpuid(0).eax < 0x15 {
        return None;
    }
    let ratio = cpuid(0x15);
    if ratio.eax == 0 || ratio.ebx == 0 || ratio.ecx == 0 {
        return None;
    }
    Some(ratio.ecx as u64 * ratio.ebx as u64 / ratio.eax as u64)
}

/// Start the clock: from here, [`now`] counts up from nothing.
///
/// # Safety
/// Once, on the first processor, before interrupts are first turned on —
/// so that the count of ticks starts from nothing at the same moment — and
/// after the local APIC has been looked for.
pub unsafe fn init() {
    use crate::serial::{put_usize, puts};
    if !trusted() {
        puts(b"Clock: no counter to keep time by; the clock is the tick, ten milliseconds.\n");
        return;
    }
    let Some(hz) = stated().or_else(|| unsafe { measure() }).filter(|&hz| hz >= 1_000_000) else {
        puts(b"Clock: the counter could not be measured; the clock is the tick, ten milliseconds.\n");
        return;
    };
    MUL.store(((1_000_000_000u128 << 32) / hz as u128) as u64, Ordering::Relaxed);
    BASE.store(counter(), Ordering::Relaxed);
    FINE.store(true, Ordering::Relaxed);
    puts(b"Clock: the time-stamp counter, at ");
    put_usize((hz / 1_000_000) as usize);
    puts(b" MHz");
    // And something to wake by it with.
    if unsafe { crate::lapic::start_one_shot() } {
        TIMER.store(true, Ordering::Relaxed);
        puts(b"; what is due is fired when it is due.\n");
    } else {
        puts(b"; what is due is fired on the next tick.\n");
    }
}

/// Another processor's counter does not read what the first's does: the
/// counter is not a clock for this machine. Before there is a task, so
/// nothing is waiting on a time yet.
pub fn distrust() {
    if FINE.swap(false, Ordering::Relaxed) {
        TIMER.store(false, Ordering::Relaxed);
        crate::lapic::cancel_one_shot();
        crate::serial::puts(
            b"Clock: the processors' counters do not agree; the clock is the tick, ten milliseconds.\n",
        );
    }
}

/// The counter, for a processor being started to be compared by.
pub fn raw() -> u64 {
    counter()
}

// --- Seeing that what is due is looked at ----------------------------------

/// Something is due at `at`: if that is before what the first processor's
/// timer is already set for, set it.
///
/// Called by whatever writes a deadline down, after it has. The timer is
/// the first processor's, so anywhere else this asks that processor to look
/// (`idt::VEC_CLOCK`): it will find the deadline, and set its timer itself.
/// Whatever is not said here is not seen to while the first processor
/// sleeps: it takes no tick then to find it by.
///
/// Said whatever the timer is set for (`SAID`), and then the timer asked
/// after: a deadline is said by a call made without the one lock while the
/// clock looks, and the look may have passed it. The clock sets its timer
/// and then asks what was said (`expire`), so between the two, one of them
/// sees the other.
pub fn due(at: u64) {
    if !TIMER.load(Ordering::Relaxed) {
        return;
    }
    let flags = irq_save();
    // Written only where it lowers what was said: a call with a deadline
    // a second off, made again and again on every processor, would
    // otherwise write one word they all share on every call. One said
    // already that is no later is a look at least as soon, and one after
    // this deadline was written.
    if at < SAID.load(Ordering::Relaxed) {
        SAID.fetch_min(at, Ordering::SeqCst);
    }
    let now = now();
    if at < SET_FOR.load(Ordering::SeqCst) {
        if crate::percpu::index() == 0 {
            set(at, now);
        } else {
            // Said to be set, so that the next thing due after this does not
            // ask again before the first processor has looked.
            SET_FOR.store(at, Ordering::Relaxed);
            crate::lapic::send(crate::percpu::apic_id(0), crate::idt::VEC_CLOCK);
        }
    }
    irq_restore(flags);
}

/// Set the first processor's timer for `at`, or as soon after it as the
/// timer may interrupt again. On the first processor, interrupts off.
fn set(at: u64, now: u64) {
    let at = at.max(LAST_SHOT.load(Ordering::Relaxed).saturating_add(MIN_GAP_NS));
    SET_FOR.store(at, Ordering::SeqCst);
    crate::lapic::one_shot(at.saturating_sub(now));
}

/// Wake what is due, and see that what is due next is looked at when it is.
///
/// The whole of what the kernel does about time passing: from the tick, and
/// from the first processor's timer (`shot`). On the first processor,
/// interrupts off, the kernel lock held.
///
/// It may not return. First it ends the programs whose signal deadline has
/// passed, raises the alarms that are due and the signals of programs'
/// timers, and hangs up on the groups a death left stopped — and for a
/// program that has said nothing about the signal, one is the end: if that
/// is the program this interrupted, there is nothing to come back to. So
/// while those are seen to the timer is set for a tick from now, which is
/// when the rest is looked at if this does not come back; and once they
/// have been, for what is due next. Left to the next tick, as it was, the
/// rest waited for a tick that a processor with nothing to run no longer
/// takes.
pub fn expire(shot: bool) {
    let now = now();
    if shot {
        LAST_SHOT.store(now, Ordering::Relaxed);
    }
    let timer = TIMER.load(Ordering::Relaxed);
    if timer {
        set(now.saturating_add(TICK_NS), now);
    }
    // What is said from here on is looked at below, if the look misses it.
    SAID.store(u64::MAX, Ordering::SeqCst);
    crate::ipc::check_signal_deadlines(now);
    crate::signal::alarms(now);
    crate::signal::timers(now);
    crate::job::hang_up();
    let next = crate::timerfd::expire(now)
        .min(crate::ipc::check_timeouts(now))
        .min(crate::futex::check_timeouts(now))
        .min(crate::ipc::signal_deadline_after(now))
        .min(crate::fdtable::alarm_after(now))
        .min(crate::ptimer::after(now));
    if timer {
        if next == u64::MAX {
            SET_FOR.store(u64::MAX, Ordering::SeqCst);
            crate::lapic::cancel_one_shot();
        } else {
            set(next, now);
        }
        // A deadline said while this looked — after the look passed whoever
        // said it — and later than the timer was then set for, asked
        // nothing of this processor: it is set for it now. Asked after the
        // timer is set, as `due` asks the timer after it says, so that one
        // of the two sees the other.
        let said = SAID.load(Ordering::SeqCst);
        if said < next {
            set(said, now);
        }
    }
}

/// Whether the first processor may stop its tick while it has nothing to
/// run: where the clock is the counter, and its timer is there to fire
/// what is due. Elsewhere the tick is the clock, or the only thing that
/// sees to what is due.
pub fn tick_may_stop() -> bool {
    FINE.load(Ordering::Relaxed) && TIMER.load(Ordering::Relaxed)
}
