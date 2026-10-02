//! ACPI: what the firmware says the machine is made of.
//!
//! The kernel reads two tables and nothing else. The **MADT** says how many
//! processors there are and where the interrupt controllers are; the
//! **FADT** says how to restart the machine and which ports turn it off.
//! Neither needs the ACPI interpreter: both are plain structures.
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

/// The kernel's identity map ends here, and so does what it can read.
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
}

const NO_CPU: Cpu = Cpu { apic_id: 0 };
const NO_IOAPIC: IoApic = IoApic { id: 0, addr: 0, gsi_base: 0 };
const NO_OVERRIDE: Override = Override { irq: 0, gsi: 0, flags: 0 };

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
            _ => {}
        }
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
    puts(b".\n");
}
