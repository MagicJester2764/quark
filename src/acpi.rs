//! ACPI: what the firmware says the machine is made of.
//!
//! The kernel reads three tables, and one object out of a fourth. The
//! **MADT** says how many processors there are and where the interrupt
//! controllers are; the **FADT** says how to restart the machine and which
//! ports turn it off; the **DMAR**, where there is one, where the machine's
//! IOMMUs are and which devices are behind each (`iommu.rs`); and the
//! **MCFG**, where there is one, where PCI devices' configuration is in
//! memory (`pci.rs`). None needs the ACPI interpreter: all are plain
//! structures.
//! What to write to those ports to turn it off is in the **DSDT**, which is
//! not a structure but a program; the one object wanted from it is found by
//! what it looks like ([`s5`]).
//!
//! Where the tables are is the bootloader's to say (`multiboot2::rsdp`),
//! because on a machine started by UEFI nothing else can; failing that the
//! places a BIOS leaves the root pointer are searched. A machine with no
//! tables, or with tables that do not add up, is one processor with an 8259,
//! which is what every machine was before this was written.
//!
//! Everything a table says is checked before it is followed: a length that
//! runs past four gigabytes, where the kernel's map ends, a checksum that
//! does not come to nothing, an entry shorter than its kind. The tables are
//! the firmware's and usually right; "usually" is a fault in ring 0.

/// How many processors, I/O APICs and overrides are kept. More than that is
/// a bigger machine than this kernel runs on, and the rest are left out.
pub const MAX_CPUS: usize = 16;
pub const MAX_IOAPICS: usize = 4;
pub const MAX_OVERRIDES: usize = 16;
pub const MAX_DRHDS: usize = 4;
/// Devices named one by one under an IOMMU that does not take every device
/// there is.
pub const MAX_SCOPES: usize = 8;

/// The kernel's map of memory as the machine starts ends here, and so does
/// what it can read: the tables are read before the map is made longer, and
/// a firmware keeps them below this in any case.
const MAP_LIMIT: u64 = 1 << 32;
/// The header every table but the root pointer begins with.
const SDT_HEADER: usize = 36;
/// No table is this long. One that says it is, is not a table.
const SDT_MAX: usize = 1 << 20;

#[derive(Clone, Copy)]
pub struct Cpu {
    /// The id its local APIC answers to, which is what an interrupt is
    /// addressed to.
    pub apic_id: u32,
}

#[derive(Clone, Copy)]
#[allow(dead_code)] // what the tables say is kept whole; not all of it is acted on yet
pub struct IoApic {
    pub id: u8,
    pub addr: u32,
    /// The first of the system's interrupts this one takes.
    pub gsi_base: u32,
}

/// An ISA interrupt that arrives somewhere other than its own number, or
/// with other than the usual polarity and trigger.
#[derive(Clone, Copy)]
#[allow(dead_code)]
pub struct Override {
    pub irq: u8,
    pub gsi: u32,
    pub flags: u16,
}

/// An IOMMU (a DMA remapping unit): where its registers are, how many pages
/// of them, and which devices it takes — every device on its segment that
/// no other unit names, or the ones named, each a bus and a device and a
/// function.
#[derive(Clone, Copy)]
pub struct Drhd {
    pub base: u64,
    pub pages: u32,
    pub all: bool,
    pub scopes: [(u8, u8); MAX_SCOPES],
    pub nscopes: usize,
}

/// A register named the way ACPI names one: which space, and where.
#[derive(Clone, Copy)]
pub struct Register {
    /// 0 for memory, 1 for an I/O port.
    pub space: u8,
    pub addr: u64,
}

pub struct Info {
    /// Whether there were tables at all.
    pub found: bool,
    /// Where every processor's local APIC is.
    pub lapic_addr: u64,
    /// Whether the machine also has a pair of 8259s, to be masked.
    pub has_pic: bool,
    pub cpus: [Cpu; MAX_CPUS],
    pub ncpus: usize,
    pub ioapics: [IoApic; MAX_IOAPICS],
    pub nioapics: usize,
    pub overrides: [Override; MAX_OVERRIDES],
    pub noverrides: usize,
    /// How to restart the machine: write `reset_value` to this.
    pub reset: Option<Register>,
    pub reset_value: u8,
    /// The two control ports that put the machine to sleep, the second 0
    /// where there is only one.
    pub pm1a_cnt: u32,
    pub pm1b_cnt: u32,
    /// Where the table of the machine's own methods is: the value that
    /// means "off" is in there.
    pub dsdt: u64,
    /// The port and the value that ask the firmware to hand power
    /// management over, on a machine where it has not: 0 where there is
    /// nothing to ask.
    pub smi_cmd: u32,
    pub acpi_enable: u8,
    /// What "off" is, for each of the two control ports: the kind of sleep
    /// the machine's own table calls `\_S5`. `None` if it has none that
    /// could be read.
    pub s5: Option<(u8, u8)>,
    /// The machine's IOMMUs, and how many regions of memory its firmware
    /// says a device must go on reaching (RMRRs).
    pub drhds: [Drhd; MAX_DRHDS],
    pub ndrhds: usize,
    pub rmrrs: usize,
    /// Where the first PCI segment's configuration is in memory, and the
    /// buses it covers: 0 where the firmware does not say.
    pub ecam_base: u64,
    pub ecam_first: u8,
    pub ecam_last: u8,
}

const NO_CPU: Cpu = Cpu { apic_id: 0 };
const NO_IOAPIC: IoApic = IoApic { id: 0, addr: 0, gsi_base: 0 };
const NO_OVERRIDE: Override = Override { irq: 0, gsi: 0, flags: 0 };
const NO_DRHD: Drhd = Drhd { base: 0, pages: 0, all: false, scopes: [(0, 0); MAX_SCOPES], nscopes: 0 };

static mut INFO: Info = Info {
    found: false,
    lapic_addr: 0xFEE0_0000,
    has_pic: true,
    cpus: [NO_CPU; MAX_CPUS],
    ncpus: 0,
    ioapics: [NO_IOAPIC; MAX_IOAPICS],
    nioapics: 0,
    overrides: [NO_OVERRIDE; MAX_OVERRIDES],
    noverrides: 0,
    reset: None,
    reset_value: 0,
    pm1a_cnt: 0,
    pm1b_cnt: 0,
    dsdt: 0,
    smi_cmd: 0,
    acpi_enable: 0,
    s5: None,
    drhds: [NO_DRHD; MAX_DRHDS],
    ndrhds: 0,
    rmrrs: 0,
    ecam_base: 0,
    ecam_first: 0,
    ecam_last: 0,
};

/// What the tables said. Unchanged after [`init`].
pub fn info() -> &'static Info {
    unsafe { &*core::ptr::addr_of!(INFO) }
}

fn sums_to_zero(bytes: &[u8]) -> bool {
    bytes.iter().fold(0u8, |sum, &b| sum.wrapping_add(b)) == 0
}

fn le16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn le64(b: &[u8], at: usize) -> u64 {
    le32(b, at) as u64 | (le32(b, at + 4) as u64) << 32
}

/// The DMAR: each remapping unit (DRHD), with the devices it names where it
/// does not take them all; and how many reserved regions (RMRR) there are.
/// A unit on another PCI segment is left out, as everything here is on the
/// first; and of a device named below a bridge, only the device on the
/// first bus is kept, as only those are claimed.
fn dmar(t: &[u8], info: &mut Info) {
    let mut at = 48;
    while at + 4 <= t.len() {
        let (kind, len) = (le16(t, at), le16(t, at + 2) as usize);
        if len < 4 || at + len > t.len() {
            break;
        }
        let e = &t[at..at + len];
        match kind {
            0 if len >= 16 && le16(e, 6) == 0 && info.ndrhds < MAX_DRHDS => {
                let mut unit = NO_DRHD;
                unit.all = e[4] & 1 != 0;
                // The size, in a newer table: two to its power in pages.
                unit.pages = 1 << (e[5] & 0x0F).min(4);
                unit.base = le64(e, 8);
                // The device scopes: a kind, a length, two reserved bytes,
                // an enumeration id, the bus, and then (device, function)
                // pairs down through bridges.
                let mut s = 16;
                while s + 6 <= len {
                    let (skind, slen) = (e[s], e[s + 1] as usize);
                    if slen < 6 || s + slen > len {
                        break;
                    }
                    if skind == 1 && slen == 8 && unit.nscopes < MAX_SCOPES {
                        let (bus, dev, func) = (e[s + 5], e[s + 6], e[s + 7]);
                        unit.scopes[unit.nscopes] = (bus, (dev & 0x1F) << 3 | (func & 7));
                        unit.nscopes += 1;
                    }
                    s += slen;
                }
                if unit.base != 0 && unit.base < MAP_LIMIT {
                    info.drhds[info.ndrhds] = unit;
                    info.ndrhds += 1;
                }
            }
            1 => info.rmrrs += 1,
            _ => {}
        }
        at += len;
    }
}

/// The MCFG: where each PCI segment's configuration is in memory, a page
/// for each function of each device of each bus from the first to the last.
/// Only the first segment's is kept, as everything here is on the first.
fn mcfg(t: &[u8], info: &mut Info) {
    let mut at = SDT_HEADER + 8;
    while at + 16 <= t.len() {
        let (base, segment, first, last) = (le64(t, at), le16(t, at + 8), t[at + 10], t[at + 11]);
        if segment == 0 && base != 0 && last >= first && info.ecam_base == 0 {
            info.ecam_base = base;
            info.ecam_first = first;
            info.ecam_last = last;
        }
        at += 16;
    }
}

/// The table at `addr`, if there is a whole, sound one there that the
/// kernel can reach.
///
/// # Safety
/// `addr` must be an address the firmware gave for a table. What is there
/// is read only as far as the identity map goes.
unsafe fn table(addr: u64) -> Option<&'static [u8]> { unsafe {
    if addr == 0 || addr.checked_add(SDT_HEADER as u64)? > MAP_LIMIT {
        return None;
    }
    let head = core::slice::from_raw_parts(addr as *const u8, SDT_HEADER);
    let len = le32(head, 4) as usize;
    if len < SDT_HEADER || len > SDT_MAX || addr + len as u64 > MAP_LIMIT {
        return None;
    }
    let whole = core::slice::from_raw_parts(addr as *const u8, len);
    sums_to_zero(whole).then_some(whole)
}}

/// The root pointer in `bytes`, checked: where the table of tables is, and
/// whether its entries are sixty-four bits wide.
fn root(bytes: &[u8]) -> Option<(u64, bool)> {
    if bytes.len() < 20 || &bytes[..8] != b"RSD PTR " || !sums_to_zero(&bytes[..20]) {
        return None;
    }
    let revision = bytes[15];
    if revision >= 2 && bytes.len() >= 36 {
        let len = le32(bytes, 20) as usize;
        let xsdt = le64(bytes, 24);
        if len >= 36 && sums_to_zero(&bytes[..36]) && xsdt != 0 && xsdt < MAP_LIMIT {
            return Some((xsdt, true));
        }
    }
    match le32(bytes, 16) {
        0 => None,
        rsdt => Some((rsdt as u64, false)),
    }
}

/// Look for a root pointer where a BIOS leaves one: the first kilobyte of
/// the extended BIOS data area, and the ROM between 0xE0000 and 0xFFFFF, on
/// sixteen-byte boundaries.
///
/// # Safety
/// The first megabyte must be mapped, as it is.
unsafe fn search() -> Option<&'static [u8]> { unsafe {
    let ebda = (core::ptr::read_unaligned(0x40E as *const u16) as usize) << 4;
    let ranges = [(ebda, ebda + 1024), (0xE_0000, 0x10_0000)];
    for (from, to) in ranges {
        if from < 0x400 || to > 0x10_0000 {
            continue;
        }
        let mut at = from & !15;
        while at + 36 <= to {
            let here = core::slice::from_raw_parts(at as *const u8, 36);
            if &here[..8] == b"RSD PTR " && sums_to_zero(&here[..20]) {
                return Some(here);
            }
            at += 16;
        }
    }
    None
}}

fn madt(t: &[u8], info: &mut Info) {
    if t.len() < 44 {
        return;
    }
    info.lapic_addr = le32(t, 36) as u64;
    info.has_pic = le32(t, 40) & 1 != 0;
    let mut at = 44;
    while at + 2 <= t.len() {
        let (kind, len) = (t[at], t[at + 1] as usize);
        if len < 2 || at + len > t.len() {
            break;
        }
        let e = &t[at..at + len];
        match kind {
            // A processor's local APIC. Bit 0 of the flags: it can be used.
            0 if len >= 8 => {
                if le32(e, 4) & 1 != 0 && info.ncpus < MAX_CPUS {
                    info.cpus[info.ncpus] = Cpu { apic_id: e[3] as u32 };
                    info.ncpus += 1;
                }
            }
            1 if len >= 12 => {
                if info.nioapics < MAX_IOAPICS {
                    info.ioapics[info.nioapics] = IoApic { id: e[2], addr: le32(e, 4), gsi_base: le32(e, 8) };
                    info.nioapics += 1;
                }
            }
            2 if len >= 10 => {
                if info.noverrides < MAX_OVERRIDES {
                    info.overrides[info.noverrides] = Override { irq: e[3], gsi: le32(e, 4), flags: le16(e, 8) };
                    info.noverrides += 1;
                }
            }
            // The local APICs are somewhere a 32-bit field cannot say.
            5 if len >= 12 => info.lapic_addr = le64(e, 4),
            // A processor whose id needs more than eight bits.
            9 if len >= 16 => {
                if le32(e, 8) & 1 != 0 && info.ncpus < MAX_CPUS {
                    info.cpus[info.ncpus] = Cpu { apic_id: le32(e, 4) };
                    info.ncpus += 1;
                }
            }
            _ => {}
        }
        at += len;
    }
}

fn fadt(t: &[u8], info: &mut Info) {
    if t.len() < 76 {
        return;
    }
    info.dsdt = le32(t, 40) as u64;
    info.smi_cmd = le32(t, 48);
    info.acpi_enable = t[52];
    info.pm1a_cnt = le32(t, 64);
    info.pm1b_cnt = le32(t, 68);
    // The reset register is there from the table's second revision, and
    // counts only if the flags say it is supported.
    if t.len() >= 129 && le32(t, 112) & (1 << 10) != 0 {
        let (space, addr) = (t[116], le64(t, 120));
        if space <= 1 && addr != 0 {
            info.reset = Some(Register { space, addr });
            info.reset_value = t[128];
        }
    }
    if t.len() >= 148 {
        match le64(t, 140) {
            0 => {}
            x if x < MAP_LIMIT => info.dsdt = x,
            _ => {}
        }
    }
}

/// What the machine's table of methods says "off" is: the first two values
/// of the object named `\_S5`, one for each control port.
///
/// The table is a program in a language of its own, and this does not run
/// it. It looks for the one thing wanted by its shape: a name, `_S5_`,
/// being given — the byte before it says so, with or without the mark that
/// the name is at the root — to a package, whose first two members are
/// small numbers. A number in that language is itself when it is 0 or 1 and
/// has a byte before it saying "a byte follows" otherwise.
fn s5(dsdt: &[u8]) -> Option<(u8, u8)> {
    const NAME: u8 = 0x08;
    const PACKAGE: u8 = 0x12;
    const BYTE_FOLLOWS: u8 = 0x0A;
    let body = dsdt.get(SDT_HEADER..)?;
    let number = |at: &mut usize| -> Option<u8> {
        let value = match *body.get(*at)? {
            BYTE_FOLLOWS => {
                *at += 1;
                *body.get(*at)?
            }
            small @ (0 | 1) => small,
            _ => return None,
        };
        *at += 1;
        (value <= 7).then_some(value)
    };
    (0..body.len().saturating_sub(4)).find_map(|name| {
        if &body[name..name + 4] != b"_S5_" {
            return None;
        }
        let given = (name >= 1 && body[name - 1] == NAME)
            || (name >= 2 && body[name - 1] == b'\\' && body[name - 2] == NAME);
        if !given || *body.get(name + 4)? != PACKAGE {
            return None;
        }
        // The package's length, which says in its top two bits how many
        // more bytes it is written in; then how many members it has.
        let mut at = name + 5;
        at += 1 + (*body.get(at)? >> 6) as usize;
        if *body.get(at)? < 2 {
            return None;
        }
        at += 1;
        let a = number(&mut at)?;
        let b = number(&mut at)?;
        Some((a, b))
    })
}

/// Read the tables, starting from the root pointer the bootloader passed
/// (`rsdp`), or one found by looking.
///
/// # Safety
/// Once, at boot, before anything reads [`info`].
pub unsafe fn init(rsdp: Option<&[u8]>) { unsafe {
    let info = &mut *core::ptr::addr_of_mut!(INFO);
    let Some((root_at, wide)) = rsdp.and_then(root).or_else(|| search().and_then(root)) else {
        report(info);
        return;
    };
    let Some(root_table) = table(root_at) else {
        report(info);
        return;
    };
    let width = if wide { 8 } else { 4 };
    for entry in root_table[SDT_HEADER..].chunks_exact(width) {
        let addr = if wide { le64(entry, 0) } else { le32(entry, 0) as u64 };
        let Some(t) = table(addr) else { continue };
        match &t[..4] {
            b"APIC" => {
                info.found = true;
                madt(t, info);
            }
            b"FACP" => fadt(t, info),
            b"DMAR" => dmar(t, info),
            b"MCFG" => mcfg(t, info),
            _ => {}
        }
    }
    if let Some(dsdt) = table(info.dsdt) {
        info.s5 = s5(dsdt);
    }
    // A table of processors with none in it that can be used says nothing.
    if info.ncpus == 0 {
        info.found = false;
    }
    report(info);
}}

fn report(info: &Info) {
    use crate::serial::{put_hex_usize, put_usize, puts};
    let put_hex = |n: u64| {
        puts(b"0x");
        put_hex_usize(n as usize);
    };
    let put_dec = |n: u64| put_usize(n as usize);
    if !info.found {
        puts(b"ACPI: no tables; one processor, and the 8259.\n");
        return;
    }
    puts(b"ACPI: ");
    put_dec(info.ncpus as u64);
    puts(if info.ncpus == 1 { b" processor" } else { b" processors" });
    puts(b", local APIC at ");
    put_hex(info.lapic_addr);
    for io in &info.ioapics[..info.nioapics] {
        puts(b", I/O APIC at ");
        put_hex(io.addr as u64);
    }
    puts(b", ");
    put_dec(info.noverrides as u64);
    puts(b" overrides");
    if let Some(reset) = info.reset {
        puts(if reset.space == 1 { b", reset by port " } else { b", reset by memory " });
        put_hex(reset.addr);
    }
    if let Some((a, b)) = info.s5 {
        puts(b", off by port ");
        put_hex(info.pm1a_cnt as u64);
        puts(b" (");
        put_dec(a as u64);
        puts(b", ");
        put_dec(b as u64);
        puts(b")");
    }
    for unit in &info.drhds[..info.ndrhds] {
        puts(b", IOMMU at ");
        put_hex(unit.base);
    }
    if info.ecam_base != 0 {
        puts(b", PCI configuration at ");
        put_hex(info.ecam_base);
    }
    puts(b".\n");
}
