//! The IOMMU: where a device may write.
//!
//! A device that does DMA reaches memory by its physical address, and on a
//! PC without an IOMMU nothing checks the address: a driver — a program in
//! ring 3 here — that tells its card the address of somebody else's page,
//! or of the kernel's, has the card write there. Intel's VT-d puts a table
//! between (`acpi::Drhd`, from the DMAR): each device a driver has claimed
//! (`SYS_DEVICE_CLAIM`) is given its program's memory and nothing else, and
//! a device nobody has claimed is given nothing at all.
//!
//! The memory is what the program asked the kernel for to give a device —
//! `SYS_PHYS_ALLOC`, whose frames have an owner (`pmm::set_owner`) — mapped
//! at its own address, so that a driver tells its device the numbers it
//! always has. It is mapped as it is given (`owned`), and taken away, on
//! every unit and waited for, before the frame can be anybody else's
//! (`disowned`). A program's devices share one table (a domain), which goes
//! when the program does (`program_gone`).
//!
//! What it does not do: interrupts are not remapped, so a device can still
//! send whatever message it is set to send (MSI-X's table is in the
//! device's own registers, and its driver writes it); only memory is
//! guarded. And a machine whose firmware says some memory must stay
//! reachable by some device (an RMRR — a USB controller's keyboard for the
//! firmware's own use, a graphics card's) is left unguarded: honouring that
//! is a mapping no machine here has to test it on.

use crate::acpi;
use core::sync::atomic::{AtomicBool, Ordering};

const MAX_UNITS: usize = acpi::MAX_DRHDS;
const MAX_DOMAINS: usize = 8;
const MAX_CLAIMS: usize = 32;

// Registers, from a unit's base.
const CAP: usize = 0x08;
const ECAP: usize = 0x10;
const GCMD: usize = 0x18;
const GSTS: usize = 0x1C;
const RTADDR: usize = 0x20;
const CCMD: usize = 0x28;
const FSTS: usize = 0x34;
const FECTL: usize = 0x38;
const FEDATA: usize = 0x3C;
const FEADDR: usize = 0x40;
const FEUADDR: usize = 0x44;
const PMEN: usize = 0x64;

/// The fault event control's mask: set, a unit says nothing when it
/// records a fault.
const IM: u32 = 1 << 31;

// The global command and status bits.
const TE: u32 = 1 << 31;
const SRTP: u32 = 1 << 30;
const WBF: u32 = 1 << 27;
/// What of the status register is a setting to keep when a command is
/// written: everything but the one-shot bits, which say a command was done.
const KEEP: u32 = 0x96FF_FFFF;

// Invalidation: of the context cache, and of the IOTLB.
const ICC: u64 = 1 << 63;
const CIRG_GLOBAL: u64 = 1 << 61;
const IVT: u64 = 1 << 63;
const IIRG_GLOBAL: u64 = 1 << 60;
const IIRG_DOMAIN: u64 = 2 << 60;
const DRAIN: u64 = (1 << 49) | (1 << 48);

// A second-level page-table entry: may be read, may be written.
const R: u64 = 1 << 0;
const W: u64 = 1 << 1;
const ADDR: u64 = 0x000F_FFFF_FFFF_F000;
const PAGE: u64 = 4096;

/// What a call about a device answers when the caller may not.
const NOT_ALLOWED: u64 = u64::MAX - 1;

#[derive(Clone, Copy)]
struct Unit {
    regs: usize,
    /// Where its IOTLB registers and its fault records are.
    iotlb: usize,
    faults: usize,
    nfaults: usize,
    /// Its walks of the tables see what the processor wrote without being
    /// told; if not, each change is flushed from the cache.
    coherent: bool,
    root: u64,
    all: bool,
    scopes: [(u8, u8); acpi::MAX_SCOPES],
    nscopes: usize,
}

#[derive(Clone, Copy)]
struct Domain {
    /// The program it is, 0 for none.
    space: u64,
    table: u64,
}

#[derive(Clone, Copy)]
struct Claim {
    /// The program whose device it is, 0 for nobody's.
    space: u64,
    bdf: u16,
    /// The unit it is behind, if any, and the domain it is in.
    unit: Option<u8>,
    domain: u8,
    /// How many times it reached for what it may not.
    stopped: u64,
}

const NO_UNIT: Unit = Unit {
    regs: 0,
    iotlb: 0,
    faults: 0,
    nfaults: 0,
    coherent: true,
    root: 0,
    all: false,
    scopes: [(0, 0); acpi::MAX_SCOPES],
    nscopes: 0,
};
const NO_DOMAIN: Domain = Domain { space: 0, table: 0 };
const NO_CLAIM: Claim = Claim { space: 0, bdf: 0, unit: None, domain: 0, stopped: 0 };

static ON: AtomicBool = AtomicBool::new(false);
static mut UNITS: [Unit; MAX_UNITS] = [NO_UNIT; MAX_UNITS];
static mut NUNITS: usize = 0;
/// How many levels the tables have: four (48 bits) or three (39), whichever
/// every unit takes.
static mut LEVELS: u32 = 4;
static mut DOMAINS: [Domain; MAX_DOMAINS] = [NO_DOMAIN; MAX_DOMAINS];
static mut CLAIMS: [Claim; MAX_CLAIMS] = [NO_CLAIM; MAX_CLAIMS];

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

fn read32(at: usize) -> u32 {
    unsafe { core::ptr::read_volatile(at as *const u32) }
}
fn write32(at: usize, v: u32) {
    unsafe { core::ptr::write_volatile(at as *mut u32, v) }
}
fn read64(at: usize) -> u64 {
    unsafe { core::ptr::read_volatile(at as *const u64) }
}
fn write64(at: usize, v: u64) {
    unsafe { core::ptr::write_volatile(at as *mut u64, v) }
}

/// Wait for `done`, a while. A unit that never answers is a machine to say
/// so about, not one to stop on.
fn wait(done: impl Fn() -> bool) -> bool {
    for _ in 0..10_000_000 {
        if done() {
            return true;
        }
        core::hint::spin_loop();
    }
    crate::serial::puts(b"IOMMU: a unit did not answer.\n");
    false
}

/// A command to a unit, keeping what is set; and the wait for it.
fn command(u: &Unit, bit: u32, set: bool) -> bool {
    let now = read32(u.regs + GSTS) & KEEP;
    write32(u.regs + GCMD, if set { now | bit } else { now & !bit });
    wait(|| (read32(u.regs + GSTS) & bit != 0) == set)
}

/// A table entry the unit is to see: written, and flushed from the cache
/// where the unit does not look there.
fn store(u_coherent: bool, at: u64, v: u64) {
    write64(at as usize, v);
    if !u_coherent {
        unsafe { core::arch::asm!("clflush [{}]", in(reg) at as usize, options(nostack)) };
    }
}

fn all_coherent() -> bool {
    unsafe { (&*core::ptr::addr_of!(UNITS))[..NUNITS].iter().all(|u| u.coherent) }
}

/// A frame of nothing, for a table.
fn zeroed() -> Option<u64> {
    let f = crate::pmm::alloc()?.address() as u64;
    unsafe { core::ptr::write_bytes(f as *mut u8, 0, PAGE as usize) };
    if !all_coherent() {
        for line in (0..PAGE).step_by(64) {
            unsafe { core::arch::asm!("clflush [{}]", in(reg) (f + line) as usize, options(nostack)) };
        }
    }
    Some(f)
}

fn flush_context(u: &Unit) {
    unsafe { core::arch::asm!("mfence", options(nostack)) };
    write64(u.regs + CCMD, ICC | CIRG_GLOBAL);
    wait(|| read64(u.regs + CCMD) & ICC == 0);
}

fn flush_iotlb(u: &Unit, domain: Option<u16>) {
    unsafe { core::arch::asm!("mfence", options(nostack)) };
    let at = u.regs + u.iotlb + 8;
    let how = match domain {
        Some(did) => IIRG_DOMAIN | (did as u64) << 32,
        None => IIRG_GLOBAL,
    };
    write64(at, IVT | how | DRAIN);
    wait(|| read64(at) & IVT == 0);
}

/// The domain id a domain's index is known by on a unit. Nought is not one.
fn did(domain: usize) -> u16 {
    domain as u16 + 1
}

/// Turn the machine's IOMMUs on, with nothing reachable by any device.
///
/// # Safety
/// Once, on the first processor, after `acpi::init` and before the first
/// task: a driver that ran before would have its device stopped under it.
pub unsafe fn init() {
    let info = acpi::info();
    if info.ndrhds == 0 {
        return;
    }
    if info.rmrrs != 0 {
        crate::serial::puts(b"IOMMU: the firmware reserves memory for a device; left as it was.\n");
        return;
    }
    let units = unsafe { &mut *core::ptr::addr_of_mut!(UNITS) };
    let mut levels = 4;
    let mut n = 0;
    for d in &info.drhds[..info.ndrhds] {
        let regs = d.base as usize;
        let cap = read64(regs + CAP);
        let ecap = read64(regs + ECAP);
        let sagaw = (cap >> 8) & 0x1F;
        if sagaw & 0b100 == 0 {
            levels = 3;
        }
        if sagaw & 0b110 == 0 {
            crate::serial::puts(b"IOMMU: a unit takes neither three levels of table nor four; left as it was.\n");
            return;
        }
        units[n] = Unit {
            regs,
            iotlb: (((ecap >> 8) & 0x3FF) * 16) as usize,
            faults: (((cap >> 24) & 0x3FF) * 16) as usize,
            nfaults: ((cap >> 40) & 0xFF) as usize + 1,
            coherent: ecap & 1 != 0,
            root: 0,
            all: d.all,
            scopes: d.scopes,
            nscopes: d.nscopes,
        };
        n += 1;
    }
    unsafe {
        LEVELS = levels;
        NUNITS = n;
    }
    for u in units[..n].iter_mut() {
        let Some(root) = zeroed() else {
            crate::serial::puts(b"IOMMU: no memory for a root table; left as it was.\n");
            return;
        };
        u.root = root;
        // Off first, if the firmware left it on; and the regions a firmware
        // can protect from devices before a system is ready, which the
        // tables now do instead.
        if read32(u.regs + GSTS) & TE != 0 {
            command(u, TE, false);
        }
        if read32(u.regs + PMEN) & (1 << 31) != 0 {
            write32(u.regs + PMEN, 0);
            wait(|| read32(u.regs + PMEN) & 1 == 0);
        }
        write64(u.regs + RTADDR, root);
        command(u, SRTP, true);
        // A unit that buffers its writes to memory is told to finish them:
        // the command bit is set, and the status bit says it is still
        // flushing.
        if read64(u.regs + CAP) & (1 << 4) != 0 {
            write32(u.regs + GCMD, (read32(u.regs + GSTS) & KEEP) | WBF);
            wait(|| read32(u.regs + GSTS) & WBF == 0);
        }
        flush_context(u);
        flush_iotlb(u, None);
        if !command(u, TE, true) {
            return;
        }
        // What it stops is said as it stops it: a message to this
        // processor, the first (`idt::VEC_IOMMU`). It was looked for on
        // every tick, and a processor with nothing to run takes none.
        write32(u.regs + FEDATA, crate::idt::VEC_IOMMU as u32);
        write32(u.regs + FEADDR, 0xFEE0_0000 | (crate::lapic::id() & 0xFF) << 12);
        write32(u.regs + FEUADDR, 0);
        write32(u.regs + FECTL, read32(u.regs + FECTL) & !IM);
    }
    ON.store(true, Ordering::SeqCst);
    crate::serial::puts(b"IOMMU: on; a device reaches only what its driver was given.\n");
}

/// Whether a device's memory is guarded here.
pub fn on() -> bool {
    ON.load(Ordering::Relaxed)
}

/// The pages of every unit's registers, which no driver may map.
pub fn register_pages(mut each: impl FnMut(u64, u64)) {
    let info = acpi::info();
    for d in &info.drhds[..info.ndrhds] {
        each(d.base & !(PAGE - 1), (d.base & !(PAGE - 1)) + d.pages as u64 * PAGE);
    }
}

/// The unit that takes device `bdf`: the one that names it, or the one that
/// takes everything.
fn unit_for(bdf: u16) -> Option<usize> {
    let units = unsafe { &(&*core::ptr::addr_of!(UNITS))[..NUNITS] };
    let (bus, devfn) = ((bdf >> 8) as u8, bdf as u8);
    units
        .iter()
        .position(|u| u.scopes[..u.nscopes].contains(&(bus, devfn)))
        .or_else(|| units.iter().position(|u| u.all))
}

/// Map `pa` at its own address in the table under `table`.
fn map(table: u64, pa: u64) -> bool {
    let coherent = all_coherent();
    let mut at = table;
    for level in (1..unsafe { LEVELS }).rev() {
        let entry = at + ((pa >> (12 + 9 * level)) & 511) * 8;
        let mut e = read64(entry as usize);
        if e & (R | W) == 0 {
            let Some(next) = zeroed() else { return false };
            e = next | R | W;
            store(coherent, entry, e);
        }
        at = e & ADDR;
    }
    store(coherent, at + ((pa >> 12) & 511) * 8, pa | R | W);
    true
}

/// Take `pa` out of the table under `table`. Whether it was there.
fn unmap(table: u64, pa: u64) -> bool {
    let coherent = all_coherent();
    let mut at = table;
    for level in (1..unsafe { LEVELS }).rev() {
        let e = read64((at + ((pa >> (12 + 9 * level)) & 511) * 8) as usize);
        if e & (R | W) == 0 {
            return false;
        }
        at = e & ADDR;
    }
    let leaf = at + ((pa >> 12) & 511) * 8;
    if read64(leaf as usize) == 0 {
        return false;
    }
    store(coherent, leaf, 0);
    true
}

/// Give back the frames of a table, and of every table under it.
fn free_table(table: u64, level: u32) {
    if level > 1 {
        for i in 0..512 {
            let e = read64((table + i * 8) as usize);
            if e & (R | W) != 0 {
                free_table(e & ADDR, level - 1);
            }
        }
    }
    crate::pmm::free(crate::pmm::PhysFrame::from_address(table as usize));
}

/// Every unit that has a device of domain `d` behind it is told the domain's
/// table changed.
fn flush_domain(d: usize) {
    let units = unsafe { &(&*core::ptr::addr_of!(UNITS))[..NUNITS] };
    let claims = unsafe { &*core::ptr::addr_of!(CLAIMS) };
    for (i, u) in units.iter().enumerate() {
        if claims.iter().any(|c| c.space != 0 && c.domain as usize == d && c.unit == Some(i as u8)) {
            flush_iotlb(u, Some(did(d)));
        }
    }
}

/// The domain of program `space`, made if it has none: a table with every
/// frame the program's tasks own in it.
fn domain_for(space: u64) -> Option<usize> {
    let domains = unsafe { &mut *core::ptr::addr_of_mut!(DOMAINS) };
    if let Some(d) = domains.iter().position(|d| d.space == space) {
        return Some(d);
    }
    let d = domains.iter().position(|d| d.space == 0)?;
    let table = zeroed()?;
    domains[d] = Domain { space, table };
    crate::pmm::each_owned(|owner| crate::scheduler::task_in_space(owner, space), |frame| {
        map(table, frame as u64);
    });
    Some(d)
}

/// The context entry for `bdf` on unit `u`: its address, the context table
/// made if there is none.
fn context_entry(u: &Unit, bdf: u16) -> Option<u64> {
    let root_entry = u.root + (bdf >> 8) as u64 * 16;
    let mut e = read64(root_entry as usize);
    if e & 1 == 0 {
        let table = zeroed()?;
        e = table | 1;
        store(u.coherent, root_entry, e);
    }
    Some((e & ADDR) + (bdf & 0xFF) as u64 * 16)
}

/// `SYS_DEVICE_CLAIM`: device `bdf` (bus << 8 | device << 3 | function, on
/// the first PCI segment) is the program of task `tid`'s, to be driven by
/// it alone. Asked by a program that holds the device (`PciDevice`), as its
/// driver does. Answers 1 if the device can reach only the program's memory
/// from now on, 0 if nothing on this machine can make it, and
/// `NOT_ALLOWED` if it is another program's or the caller may not. With
/// `ask`, how many times the device reached for something it may not,
/// instead.
pub fn claim(tid: usize, bdf: u64, ask: bool) -> u64 {
    if bdf > 0xFFFF {
        return u64::MAX;
    }
    if !crate::cap::task_has_pci_device(tid, bdf) {
        return NOT_ALLOWED;
    }
    let bdf = bdf as u16;
    let space = crate::scheduler::space_of_task(tid);
    if space == 0 {
        return u64::MAX;
    }
    let flags = irq_save();
    let answer = unsafe { claim_locked(space, bdf, ask) };
    irq_restore(flags);
    answer
}

unsafe fn claim_locked(space: u64, bdf: u16, ask: bool) -> u64 {
    let claims = unsafe { &mut *core::ptr::addr_of_mut!(CLAIMS) };
    if let Some(c) = claims.iter().find(|c| c.space != 0 && c.bdf == bdf) {
        return if c.space != space {
            NOT_ALLOWED
        } else if ask {
            c.stopped
        } else {
            c.unit.is_some() as u64
        };
    }
    if ask {
        return u64::MAX;
    }
    let Some(slot) = claims.iter().position(|c| c.space == 0) else {
        return u64::MAX;
    };
    claims[slot] = Claim { space, bdf, unit: None, domain: 0, stopped: 0 };
    if !on() {
        return 0;
    }
    let Some(u) = unit_for(bdf) else { return 0 };
    let Some(d) = domain_for(space) else {
        claims[slot].space = 0;
        return u64::MAX;
    };
    let unit = unsafe { &(&*core::ptr::addr_of!(UNITS))[u] };
    let Some(entry) = context_entry(unit, bdf) else {
        claims[slot].space = 0;
        return u64::MAX;
    };
    let table = unsafe { (*core::ptr::addr_of!(DOMAINS))[d].table };
    // The width the tables are for (1: 39 bits, three levels; 2: 48 bits,
    // four), and the domain; then the table, and present, last.
    let width = if unsafe { LEVELS } == 4 { 2 } else { 1 };
    store(unit.coherent, entry + 8, width | (did(d) as u64) << 8);
    store(unit.coherent, entry, table | 1);
    claims[slot].unit = Some(u as u8);
    claims[slot].domain = d as u8;
    flush_context(unit);
    flush_iotlb(unit, Some(did(d)));
    1
}

/// Whether program `space` has claimed device `bdf`.
pub fn claimed_by(space: u64, bdf: u16) -> bool {
    let flags = irq_save();
    let claimed = unsafe { (*core::ptr::addr_of!(CLAIMS)).iter().any(|c| c.space != 0 && c.space == space && c.bdf == bdf) };
    irq_restore(flags);
    claimed
}

/// Program `space` has gone: its devices master the bus no more and reach
/// nothing again, and its table is given back.
pub fn program_gone(space: u64) {
    if space == 0 {
        return;
    }
    let flags = irq_save();
    unsafe {
        let claims = &mut *core::ptr::addr_of_mut!(CLAIMS);
        let units = &(&*core::ptr::addr_of!(UNITS))[..NUNITS];
        for c in claims.iter_mut().filter(|c| c.space == space) {
            // Before its frames can be anybody's: on a machine with no
            // IOMMU, this is all that stops it writing to them.
            crate::pci::stop(c.bdf);
            if let Some(u) = c.unit {
                let unit = &units[u as usize];
                if let Some(entry) = context_entry(unit, c.bdf) {
                    store(unit.coherent, entry, 0);
                    store(unit.coherent, entry + 8, 0);
                }
                flush_context(unit);
                flush_iotlb(unit, Some(did(c.domain as usize)));
            }
            *c = NO_CLAIM;
        }
        let domains = &mut *core::ptr::addr_of_mut!(DOMAINS);
        if let Some(d) = domains.iter_mut().find(|d| d.space == space) {
            free_table(d.table, LEVELS);
            *d = NO_DOMAIN;
        }
    }
    irq_restore(flags);
}

/// Frames `[base, base + count pages)` have been given to task `owner`: if
/// its program has a domain, its devices may reach them now.
pub fn owned(base: usize, count: usize, owner: usize) {
    reach(crate::scheduler::space_of_task(owner), base, count);
}

/// Program `space`'s devices may reach `[base, base + count pages)` from now
/// on, if it has claimed any: its own frames as they come, and its display
/// device's screen, which is nobody's (`display.rs`). Until the program goes,
/// and its domain with it.
pub fn reach(space: u64, base: usize, count: usize) {
    if !on() || space == 0 {
        return;
    }
    let flags = irq_save();
    unsafe {
        let domains = &*core::ptr::addr_of!(DOMAINS);
        if let Some(d) = domains.iter().position(|d| d.space != 0 && d.space == space) {
            for i in 0..count {
                map(domains[d].table, (base + i * PAGE as usize) as u64);
            }
            flush_domain(d);
        }
    }
    irq_restore(flags);
}

/// Frames `[base, base + count pages)` are leaving whoever owned them: no
/// device reaches them by the time this returns.
pub fn disowned(base: usize, count: usize) {
    if !on() {
        return;
    }
    let flags = irq_save();
    unsafe {
        let domains = &*core::ptr::addr_of!(DOMAINS);
        for (d, domain) in domains.iter().enumerate() {
            if domain.space == 0 {
                continue;
            }
            let mut any = false;
            for i in 0..count {
                any |= unmap(domain.table, (base + i * PAGE as usize) as u64);
            }
            if any {
                flush_domain(d);
            }
        }
    }
    irq_restore(flags);
}

/// What the units have stopped since they were last asked, counted against
/// the device that tried, and said on the serial line: the first few of
/// each device. When a unit says it has stopped something
/// (`idt::VEC_IOMMU`), which it does again only once what it recorded has
/// been taken.
pub fn poll() {
    if !on() {
        return;
    }
    let flags = irq_save();
    unsafe {
        let units = &(&*core::ptr::addr_of!(UNITS))[..NUNITS];
        let claims = &mut *core::ptr::addr_of_mut!(CLAIMS);
        for u in units {
            let status = read32(u.regs + FSTS);
            if status & 0b11 == 0 {
                continue;
            }
            for i in 0..u.nfaults {
                let record = u.regs + u.faults + i * 16;
                let high = read64(record + 8);
                if high & (1 << 63) == 0 {
                    continue;
                }
                let at = read64(record) & !0xFFF;
                let sid = (high & 0xFFFF) as u16;
                let write = high & (1 << 62) == 0;
                let reason = (high >> 32) & 0xFF;
                write64(record + 8, 1 << 63);
                let count = match claims.iter_mut().find(|c| c.space != 0 && c.bdf == sid) {
                    Some(c) => {
                        c.stopped += 1;
                        c.stopped
                    }
                    None => 1,
                };
                if count <= 3 {
                    use crate::serial::{put_hex_usize, puts};
                    puts(b"[IOMMU] stopped a ");
                    puts(if write { b"write" } else { b"read" });
                    puts(b" by device ");
                    put_hex_usize(sid as usize);
                    puts(b" at ");
                    put_hex_usize(at as usize);
                    puts(b" (reason ");
                    put_hex_usize(reason as usize);
                    puts(b")\n");
                }
            }
            // An overflow, and the pending fault, are said to be seen.
            write32(u.regs + FSTS, status & 0b11);
        }
    }
    irq_restore(flags);
}
