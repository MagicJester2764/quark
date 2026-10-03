//! Device memory: the addresses the machine has that are not memory.
//!
//! A device's registers are at addresses the firmware chose for it, and it
//! chose them from what the machine's memory map leaves out: below four
//! gigabytes, an address no entry of the map covers is not RAM, not the
//! firmware's own, not an ACPI table — it is where devices are. A driver
//! has to map its device's registers, and the authority to map anything
//! comes from what the first task was started with, so those ranges are
//! worked out here, once, and the right to map within them is handed to
//! `init` (`userspace::spawn_init`).
//!
//! Three things are left out of them:
//!
//! - **The first megabyte.** The map says little about it and what is there
//!   is the firmware's and the display's.
//! - **The interrupt controllers' own registers** — the local APIC's page
//!   and each I/O APIC's. They are in the same stretch of addresses as any
//!   device, and a program that could write to them could stop the clock.
//! - **Everything, if the map was not kept whole.** A hole in what the
//!   kernel remembers of the map is not a hole in the map.
//!
//! It is one authority for all devices, not one for each: which device is
//! at which address is in the devices' own configuration, which the kernel
//! does not read. So what is handed on is not the ranges but the right to
//! a range within them (`cap::CapType::DeviceMemory`): its holder mints a
//! `PhysRange` for the registers of the device it drives, having read
//! where they are. A driver that holds it may map any device's, as one
//! that holds the I/O ports may write to any device's.

use crate::multiboot2::MemoryRegion;

/// How many separate ranges are kept: the gaps of a PC's map are three or
/// four.
pub const MAX_RANGES: usize = 8;

/// Above the first megabyte, below four gigabytes.
const FLOOR: u64 = 0x10_0000;
const CEILING: u64 = 1 << 32;
/// A gap smaller than this is the map's rounding, not somewhere a device is.
const SMALLEST: u64 = 0x1_0000;
const PAGE: u64 = 4096;

static mut RANGES: [(usize, usize); MAX_RANGES] = [(0, 0); MAX_RANGES];
static mut COUNT: usize = 0;

/// Whether `[start, end)` lies in one of the ranges, whole: whether all of
/// it is device memory.
pub fn covers(start: u64, end: u64) -> bool {
    start < end && ranges().iter().any(|&(from, to)| start >= from as u64 && end <= to as u64)
}

/// The ranges, as `(start, end)`: page-aligned, in order of address.
pub fn ranges() -> &'static [(usize, usize)] {
    unsafe {
        let all: &'static [(usize, usize); MAX_RANGES] = &*(&raw const RANGES);
        &all[..*(&raw const COUNT)]
    }
}

/// Work the ranges out from the memory map.
///
/// # Safety
/// Once, on the first processor, after `acpi::init`, before `init` is made.
pub unsafe fn init(regions: &[MemoryRegion]) {
    if !crate::multiboot2::map_is_whole() {
        crate::serial::puts(b"Device memory: the memory map was not kept whole; none is handed out.\n");
        return;
    }
    // What is spoken for, below four gigabytes: every entry of the map,
    // whatever it says the memory is for, and the interrupt controllers.
    const SPANS: usize =
        crate::multiboot2::MAX_MEMORY_REGIONS + 1 + crate::acpi::MAX_IOAPICS + crate::acpi::MAX_DRHDS;
    let mut spans = [(0u64, 0u64); SPANS];
    let mut n = 0;
    let mut take = |base: u64, end: u64| {
        if base < CEILING && end > base && n < SPANS {
            spans[n] = (base, end.min(CEILING));
            n += 1;
        }
    };
    for r in regions {
        take(r.base, r.base.saturating_add(r.length));
    }
    let info = crate::acpi::info();
    take(info.lapic_addr & !(PAGE - 1), (info.lapic_addr & !(PAGE - 1)) + PAGE);
    for io in &info.ioapics[..info.nioapics] {
        let at = io.addr as u64 & !(PAGE - 1);
        take(at, at + PAGE);
    }
    // And the IOMMUs': a driver that could write there could let its
    // device reach anything.
    crate::iommu::register_pages(&mut take);
    // In order of address. There are a few dozen at most.
    for i in 1..n {
        let mut j = i;
        while j > 0 && spans[j - 1].0 > spans[j].0 {
            spans.swap(j - 1, j);
            j -= 1;
        }
    }
    let out = unsafe { &mut *(&raw mut RANGES) };
    let mut count = 0;
    let mut gap = |from: u64, to: u64| {
        let from = (from + PAGE - 1) & !(PAGE - 1);
        let to = to & !(PAGE - 1);
        if to > from && to - from >= SMALLEST && count < MAX_RANGES {
            out[count] = (from as usize, to as usize);
            count += 1;
        }
    };
    let mut cursor = FLOOR;
    for &(base, end) in &spans[..n] {
        if base > cursor {
            gap(cursor, base);
        }
        cursor = cursor.max(end);
    }
    if cursor < CEILING {
        gap(cursor, CEILING);
    }
    unsafe { *(&raw mut COUNT) = count };

    crate::serial::puts(b"Device memory:");
    for &(start, end) in ranges() {
        crate::serial::puts(b" ");
        crate::serial::put_hex_usize(start);
        crate::serial::puts(b"-");
        crate::serial::put_hex_usize(end);
    }
    crate::serial::puts(b"\n");
}
