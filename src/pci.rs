//! PCI: every device the machine has, found once, and its configuration,
//! which is the kernel's.
//!
//! A device on the PCI bus is configured through a window every device
//! shares — two ports, 0xCF8 to say which device and where, 0xCFC to read
//! or write what is there; or, where the firmware's MCFG table says so, a
//! page of memory for each — and whoever reaches that window reaches every
//! device: where its registers are, whether it may copy memory, where its
//! interrupts go. So nobody else does. The kernel finds every function on
//! every bus at boot ([`init`]), before there is a task and before the
//! other processors start, sizes its BARs while nothing else can be using
//! them, and keeps what it found. A program reaches a device through a
//! capability for it (`cap::CapType::PciDevice`): it is told what was
//! found (`SYS_PCI_DEVICE`), reads and writes the configuration through
//! the kernel (`SYS_PCI_READ`, `SYS_PCI_WRITE`), and mints what lies in
//! the device's BARs and nothing else ([`memory_covers`], [`ports_cover`]).
//!
//! What a holder may not write ([`kept`]) is what would undo that: a BAR,
//! so that what a capability lets it map is still its device; the MSI
//! capability, so that where a device's message goes is the kernel's to
//! say ([`aim`]); and bus mastering turned on before the device is claimed
//! — every function but a bridge starts with it off, and has it turned off
//! again ([`stop`]) when the program that claimed it goes.

use crate::io;

/// How many functions are kept. A desktop has thirty or forty.
pub const MAX_DEVICES: usize = 128;
/// `PciDevice`'s param0 for every device.
pub const ANY: u64 = 0xFFFF_FFFF;
/// How many words `SYS_PCI_DEVICE` writes.
pub const RECORD: usize = 21;

/// A BAR's flags.
pub const BAR_PORTS: u8 = 1;
pub const BAR_WIDE: u8 = 2;
pub const BAR_PREFETCH: u8 = 4;

const ADDRESS_PORT: u16 = 0xCF8;
const DATA_PORT: u16 = 0xCFC;

const COMMAND: u16 = 0x04;
const COMMAND_PORTS: u16 = 1 << 0;
const COMMAND_MEMORY: u16 = 1 << 1;
const COMMAND_MASTER: u16 = 1 << 2;
const COMMAND_NO_LINE: u16 = 1 << 10;
const STATUS_CAPABILITIES: u16 = 1 << 4;
const CAPABILITY_MSI: u8 = 0x05;
const CAPABILITY_MSIX: u8 = 0x11;
const MSI_ENABLE: u16 = 1;
const MSI_WIDE: u16 = 1 << 7;
const MSI_MASKABLE: u16 = 1 << 8;
const MSIX_ENABLE: u16 = 1 << 15;

#[derive(Clone, Copy)]
pub struct Bar {
    pub base: u64,
    pub size: u64,
    pub flags: u8,
}

const NO_BAR: Bar = Bar { base: 0, size: 0, flags: 0 };

impl Bar {
    /// Where it is, if it is somewhere: a BAR the firmware left at 0 is
    /// not.
    fn placed(&self) -> bool {
        self.size != 0 && self.base != 0
    }

    fn is_ports(&self) -> bool {
        self.flags & BAR_PORTS != 0
    }
}

#[derive(Clone, Copy)]
pub struct Device {
    /// `bus << 8 | device << 3 | function`.
    pub bdf: u16,
    pub vendor: u16,
    pub device: u16,
    pub subsystem_vendor: u16,
    pub subsystem: u16,
    /// Class, subclass and programming interface, a byte each.
    pub class: u32,
    pub revision: u8,
    /// The header's type: 0 a device, 1 a bridge to another bus.
    pub header: u8,
    pub pin: u8,
    pub line: u8,
    /// Where the MSI and MSI-X capabilities are; 0 for none.
    pub msi: u8,
    pub msix: u8,
    /// How long the MSI capability is, which depends on what it can do.
    msi_len: u8,
    pub bars: [Bar; 6],
}

const NO_DEVICE: Device = Device {
    bdf: 0,
    vendor: 0,
    device: 0,
    subsystem_vendor: 0,
    subsystem: 0,
    class: 0,
    revision: 0,
    header: 0,
    pin: 0,
    line: 0,
    msi: 0,
    msix: 0,
    msi_len: 0,
    bars: [NO_BAR; 6],
};

static mut DEVICES: [Device; MAX_DEVICES] = [NO_DEVICE; MAX_DEVICES];
static mut COUNT: usize = 0;
/// Where segment 0's configuration is in memory, and the buses it covers:
/// `(base, first, last)`, if the firmware said and the kernel can reach it.
static mut ECAM: Option<(u64, u8, u8)> = None;

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

/// Every device found, in order of address.
pub fn devices() -> &'static [Device] {
    unsafe {
        let all: &'static [Device; MAX_DEVICES] = &*(&raw const DEVICES);
        &all[..*(&raw const COUNT)]
    }
}

/// The device at `bdf`, if there is one.
pub fn find(bdf: u64) -> Option<&'static Device> {
    devices().iter().find(|d| d.bdf as u64 == bdf)
}

/// The first device at or after `from` that `holds` admits.
pub fn next(from: u64, holds: impl Fn(u16) -> bool) -> Option<&'static Device> {
    devices().iter().find(|d| d.bdf as u64 >= from && holds(d.bdf))
}

/// Where `offset` of `bdf`'s configuration is in memory, if it is.
fn ecam_at(bdf: u16, offset: u16) -> Option<usize> {
    let (base, first, last) = unsafe { *(&raw const ECAM) }?;
    let bus = (bdf >> 8) as u8;
    if bus < first || bus > last || offset >= 4096 {
        return None;
    }
    Some((base + (((bus - first) as u64) << 20 | ((bdf & 0xFF) as u64) << 12 | offset as u64)) as usize)
}

/// Whether `width` bytes at `offset` are a read or write anybody may ask
/// for: 1, 2 or 4, aligned, inside what the device has.
fn sound(offset: u64, width: u64) -> bool {
    matches!(width, 1 | 2 | 4)
        && offset % width == 0
        && offset + width <= if unsafe { (*(&raw const ECAM)).is_some() } { 4096 } else { 256 }
}

/// Read `width` bytes at `offset` of `bdf`'s configuration.
pub fn read(bdf: u16, offset: u16, width: u8) -> Option<u32> {
    if !sound(offset as u64, width as u64) {
        return None;
    }
    if let Some(at) = ecam_at(bdf, offset) {
        return Some(unsafe {
            match width {
                1 => core::ptr::read_volatile(at as *const u8) as u32,
                2 => core::ptr::read_volatile(at as *const u16) as u32,
                _ => core::ptr::read_volatile(at as *const u32),
            }
        });
    }
    if offset >= 256 {
        return None;
    }
    // The address and the data are two steps, and nothing may come
    // between them.
    let flags = irq_save();
    let value = unsafe {
        io::outl(ADDRESS_PORT, 0x8000_0000 | (bdf as u32) << 8 | (offset as u32 & 0xFC));
        match width {
            1 => io::inb(DATA_PORT + (offset & 3)) as u32,
            2 => io::inw(DATA_PORT + (offset & 2)) as u32,
            _ => io::inl(DATA_PORT),
        }
    };
    irq_restore(flags);
    Some(value)
}

/// Write `width` bytes at `offset` of `bdf`'s configuration.
pub fn write(bdf: u16, offset: u16, width: u8, value: u32) -> bool {
    if !sound(offset as u64, width as u64) {
        return false;
    }
    if let Some(at) = ecam_at(bdf, offset) {
        unsafe {
            match width {
                1 => core::ptr::write_volatile(at as *mut u8, value as u8),
                2 => core::ptr::write_volatile(at as *mut u16, value as u16),
                _ => core::ptr::write_volatile(at as *mut u32, value),
            }
        }
        return true;
    }
    if offset >= 256 {
        return false;
    }
    let flags = irq_save();
    unsafe {
        io::outl(ADDRESS_PORT, 0x8000_0000 | (bdf as u32) << 8 | (offset as u32 & 0xFC));
        match width {
            1 => io::outb(DATA_PORT + (offset & 3), value as u8),
            2 => io::outw(DATA_PORT + (offset & 2), value as u16),
            _ => io::outl(DATA_PORT, value),
        }
    }
    irq_restore(flags);
    true
}

fn read8(bdf: u16, offset: u16) -> u8 {
    read(bdf, offset, 1).unwrap_or(0xFF) as u8
}

fn read16(bdf: u16, offset: u16) -> u16 {
    read(bdf, offset, 2).unwrap_or(0xFFFF) as u16
}

fn read32(bdf: u16, offset: u16) -> u32 {
    read(bdf, offset, 4).unwrap_or(0xFFFF_FFFF)
}

/// Whether a program's access of `width` bytes at `port` reaches the
/// configuration window: the address register at 0xCF8, and the data at
/// 0xCFC to 0xCFF. A byte at 0xCF9 is the chipset's reset register, and is
/// not; anything wider there reaches the address.
pub fn config_port(port: u16, width: u16) -> bool {
    let (from, to) = (port as u32, port as u32 + width as u32);
    let touches = |a: u32, b: u32| from < b && a < to;
    touches(0xCF8, 0xCF9) || touches(0xCFC, 0xD00) || (width > 1 && touches(0xCF8, 0xD00))
}

/// The lowest bit set in `mask`: how long a BAR whose address bits read
/// back as `mask` is.
fn lowest(mask: u64) -> u64 {
    mask & mask.wrapping_neg()
}

/// What the kernel found at `bdf`: its ids, its class, its capabilities,
/// and its BARs, sized.
///
/// # Safety
/// At boot, with nothing else touching the device.
unsafe fn probe(bdf: u16) -> Device {
    let mut d = NO_DEVICE;
    let id = read32(bdf, 0);
    d.bdf = bdf;
    d.vendor = id as u16;
    d.device = (id >> 16) as u16;
    let class = read32(bdf, 0x08);
    d.class = class >> 8;
    d.revision = class as u8;
    d.header = read8(bdf, 0x0E) & 0x7F;
    if d.header == 0 {
        let sub = read32(bdf, 0x2C);
        d.subsystem_vendor = sub as u16;
        d.subsystem = (sub >> 16) as u16;
    }
    let line = read32(bdf, 0x3C);
    d.line = line as u8;
    d.pin = (line >> 8) as u8;

    // The capabilities: a list in the device's own configuration, each
    // entry naming its kind and the next. Not trusted to end.
    if read16(bdf, 0x06) & STATUS_CAPABILITIES != 0 {
        let mut at = read8(bdf, 0x34) & 0xFC;
        for _ in 0..48 {
            if at < 0x40 {
                break;
            }
            let entry = read16(bdf, at as u16);
            match entry as u8 {
                CAPABILITY_MSI if d.msi == 0 => {
                    d.msi = at;
                    let control = read16(bdf, at as u16 + 2);
                    d.msi_len = 10
                        + if control & MSI_WIDE != 0 { 4 } else { 0 }
                        + if control & MSI_MASKABLE != 0 { 10 } else { 0 };
                }
                CAPABILITY_MSIX if d.msix == 0 => d.msix = at,
                _ => {}
            }
            at = (entry >> 8) as u8 & 0xFC;
        }
    }

    // The BARs. An IDE controller in compatibility mode answers at the
    // ports an ISA one did, whatever its BARs say, and those are its BARs.
    let count = match d.header {
        0 => 6,
        1 => 2,
        _ => 0,
    };
    let mut legacy = [false; 6];
    if d.class >> 8 == 0x0101 {
        let interface = d.class as u8;
        if interface & 1 == 0 {
            d.bars[0] = Bar { base: 0x1F0, size: 8, flags: BAR_PORTS };
            d.bars[1] = Bar { base: 0x3F6, size: 1, flags: BAR_PORTS };
            legacy[0] = true;
            legacy[1] = true;
        }
        if interface & 4 == 0 {
            d.bars[2] = Bar { base: 0x170, size: 8, flags: BAR_PORTS };
            d.bars[3] = Bar { base: 0x376, size: 1, flags: BAR_PORTS };
            legacy[2] = true;
            legacy[3] = true;
        }
    }
    // Sized with the device answering at none of them: all ones written to
    // a BAR is an address it would otherwise answer at.
    let command = read16(bdf, COMMAND);
    if count > 0 {
        write(bdf, COMMAND, 2, (command & !(COMMAND_PORTS | COMMAND_MEMORY)) as u32);
    }
    let mut i = 0;
    while i < count {
        if legacy[i] {
            i += 1;
            continue;
        }
        let at = 0x10 + 4 * i as u16;
        let was = read32(bdf, at);
        write(bdf, at, 4, 0xFFFF_FFFF);
        let low = read32(bdf, at);
        write(bdf, at, 4, was);
        if was & 1 != 0 {
            let mask = (low & 0xFFFF_FFFC) as u64 | 0xFFFF_FFFF_0000_0000;
            if low & 0xFFFF_FFFC != 0 {
                d.bars[i] = Bar { base: (was & 0xFFFF_FFFC) as u64, size: lowest(mask) & 0xFFFF_FFFF, flags: BAR_PORTS };
            }
        } else {
            let wide = (was >> 1) & 3 == 2 && i + 1 < count;
            let prefetch = was & 8 != 0;
            let mut base = (was & 0xFFFF_FFF0) as u64;
            let mut mask = (low & 0xFFFF_FFF0) as u64;
            if wide {
                let high_was = read32(bdf, at + 4);
                write(bdf, at + 4, 4, 0xFFFF_FFFF);
                let high = read32(bdf, at + 4);
                write(bdf, at + 4, 4, high_was);
                base |= (high_was as u64) << 32;
                mask |= (high as u64) << 32;
            } else if mask != 0 {
                mask |= 0xFFFF_FFFF_0000_0000;
            }
            if mask != 0 {
                let flags = if wide { BAR_WIDE } else { 0 } | if prefetch { BAR_PREFETCH } else { 0 };
                d.bars[i] = Bar { base, size: lowest(mask), flags };
            }
            if wide {
                i += 1;
            }
        }
        i += 1;
    }
    // Answering again as it was; and copying no memory until its driver
    // has claimed it, unless it is a bridge, which copies for the devices
    // behind it.
    let master = if d.class >> 16 == 0x06 { command } else { command & !COMMAND_MASTER };
    if count > 0 || master != command {
        write(bdf, COMMAND, 2, master as u32);
    }
    d
}

/// Find every device.
///
/// # Safety
/// Once, at boot, after `acpi::init`, before the other processors start
/// and before there is a task.
pub unsafe fn init() {
    let info = crate::acpi::info();
    if info.ecam_base != 0 {
        let end = info.ecam_base + ((info.ecam_last as u64 - info.ecam_first as u64 + 1) << 20);
        if end <= crate::paging::identity_end() as u64 {
            unsafe { *(&raw mut ECAM) = Some((info.ecam_base, info.ecam_first, info.ecam_last)) };
        }
    }
    let (first, last) = match unsafe { *(&raw const ECAM) } {
        Some((_, first, last)) => (first, last),
        None => (0, 255),
    };
    let devices = unsafe { &mut *(&raw mut DEVICES) };
    let mut count = 0;
    'buses: for bus in first..=last {
        for slot in 0..32u8 {
            let zero = (bus as u16) << 8 | (slot as u16) << 3;
            if read16(zero, 0) == 0xFFFF {
                continue;
            }
            let functions = if read8(zero, 0x0E) & 0x80 != 0 { 8 } else { 1 };
            for function in 0..functions {
                let bdf = zero | function;
                if read16(bdf, 0) == 0xFFFF {
                    continue;
                }
                if count == MAX_DEVICES {
                    crate::serial::puts(b"PCI: more devices than are kept; the rest are left out.\n");
                    break 'buses;
                }
                devices[count] = unsafe { probe(bdf) };
                count += 1;
            }
        }
    }
    unsafe { *(&raw mut COUNT) = count };
    report();
}

fn report() {
    use crate::serial::{put_hex8 as hex2, put_hex_usize, put_usize, puts};
    puts(b"PCI: ");
    put_usize(devices().len());
    puts(b" devices, configured ");
    match unsafe { *(&raw const ECAM) } {
        Some((base, _, _)) => {
            puts(b"in memory at 0x");
            put_hex_usize(base as usize);
        }
        None => puts(b"by ports"),
    }
    puts(b".\n");
    for d in devices() {
        puts(b"  ");
        hex2((d.bdf >> 8) as u8);
        puts(b":");
        hex2((d.bdf >> 3) as u8 & 0x1F);
        puts(b".");
        put_usize((d.bdf & 7) as usize);
        puts(b" ");
        hex2((d.vendor >> 8) as u8);
        hex2(d.vendor as u8);
        puts(b":");
        hex2((d.device >> 8) as u8);
        hex2(d.device as u8);
        puts(b" class ");
        hex2((d.class >> 16) as u8);
        hex2((d.class >> 8) as u8);
        for bar in d.bars.iter().filter(|b| b.size != 0) {
            puts(if bar.is_ports() { b" ports 0x" } else { b" memory 0x" });
            put_hex_usize(bar.base as usize);
            puts(b"+0x");
            put_hex_usize(bar.size as usize);
        }
        puts(b"\n");
    }
}

/// What `SYS_PCI_DEVICE` writes about `d`.
pub fn record(d: &Device) -> [u64; RECORD] {
    let mut r = [0u64; RECORD];
    r[0] = d.bdf as u64
        | (d.header as u64) << 16
        | (d.pin as u64) << 24
        | (d.line as u64) << 32
        | (d.msi as u64) << 40
        | (d.msix as u64) << 48;
    r[1] = d.vendor as u64 | (d.device as u64) << 16 | (d.subsystem_vendor as u64) << 32 | (d.subsystem as u64) << 48;
    r[2] = d.class as u64 | (d.revision as u64) << 24;
    for (n, bar) in d.bars.iter().enumerate() {
        r[3 + 3 * n] = bar.base;
        r[4 + 3 * n] = bar.size;
        r[5 + 3 * n] = bar.flags as u64;
    }
    r
}

/// The pages a memory BAR is in.
fn pages(bar: &Bar) -> (u64, u64) {
    (bar.base & !0xFFF, (bar.base.saturating_add(bar.size) + 0xFFF) & !0xFFF)
}

/// Whether `[start, end)` lies in the pages of a memory BAR of a device
/// `holds` admits, where no other device's BAR is in those pages.
pub fn memory_covers(start: u64, end: u64, holds: impl Fn(u16) -> bool) -> bool {
    start < end
        && devices().iter().filter(|d| holds(d.bdf)).any(|d| {
            d.bars.iter().filter(|b| b.placed() && !b.is_ports()).any(|bar| {
                let (from, to) = pages(bar);
                start >= from
                    && end <= to
                    && !devices().iter().filter(|other| other.bdf != d.bdf).any(|other| {
                        other.bars.iter().any(|b| b.placed() && !b.is_ports() && b.base < to && b.base + b.size > from)
                    })
            })
        })
}

/// Whether ports `first` to `last` lie in an I/O BAR of a device `holds`
/// admits.
pub fn ports_cover(first: u64, last: u64, holds: impl Fn(u16) -> bool) -> bool {
    first <= last
        && devices().iter().filter(|d| holds(d.bdf)).any(|d| {
            d.bars.iter().any(|b| b.placed() && b.is_ports() && first >= b.base && last < b.base + b.size)
        })
}

/// Whether a write of `width` bytes at `offset` of `d`'s configuration
/// touches what the kernel keeps: a BAR, the ROM's base, a bridge's buses
/// and windows, or the MSI capability.
pub fn kept(d: &Device, offset: u16, width: u8) -> bool {
    let (from, to) = (offset, offset + width as u16);
    let touches = |a: u16, b: u16| from < b && a < to;
    let placed = match d.header {
        0 => touches(0x10, 0x28) || touches(0x30, 0x34),
        1 => touches(0x10, 0x3C),
        _ => touches(0x10, 0x40),
    };
    placed || (d.msi != 0 && touches(d.msi as u16, d.msi as u16 + d.msi_len as u16))
}

/// Whether a write of `value`, `width` bytes at `offset` of `bdf`'s
/// configuration, turns its bus mastering on.
pub fn turns_master_on(bdf: u16, offset: u16, value: u32) -> bool {
    offset == COMMAND && value as u16 & COMMAND_MASTER != 0 && read16(bdf, COMMAND) & COMMAND_MASTER == 0
}

/// Have `d` send its interrupt as `data` written to `address`: one message,
/// enabled, and its line off. False if it has no MSI capability, which a
/// device with MSI-X alone has not: its messages are in a table in one of
/// its BARs, and its driver writes them.
pub fn aim(d: &Device, address: u32, data: u16) -> bool {
    if d.msi == 0 {
        return false;
    }
    let at = d.msi as u16;
    let control = read16(d.bdf, at + 2);
    write(d.bdf, at + 4, 4, address);
    if control & MSI_WIDE != 0 {
        write(d.bdf, at + 8, 4, 0);
        write(d.bdf, at + 12, 2, data as u32);
    } else {
        write(d.bdf, at + 8, 2, data as u32);
    }
    write(d.bdf, at + 2, 2, ((control & !(0x7 << 4)) | MSI_ENABLE) as u32);
    write(d.bdf, COMMAND, 2, (read16(d.bdf, COMMAND) | COMMAND_NO_LINE) as u32);
    true
}

/// The program that drove device `bdf` has gone: it copies no more memory
/// and sends no more messages.
pub fn stop(bdf: u16) {
    let Some(d) = find(bdf as u64) else { return };
    write(bdf, COMMAND, 2, (read16(bdf, COMMAND) & !COMMAND_MASTER) as u32);
    if d.msi != 0 {
        let at = d.msi as u16 + 2;
        write(bdf, at, 2, (read16(bdf, at) & !MSI_ENABLE) as u32);
    }
    if d.msix != 0 {
        let at = d.msix as u16 + 2;
        write(bdf, at, 2, (read16(bdf, at) & !MSIX_ENABLE) as u32);
    }
}
