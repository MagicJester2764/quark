//! The date, from the PC's battery-backed clock.
//!
//! The PIT counts ticks from boot and knows nothing else. The CMOS clock knows
//! the date to the second; it is read once at boot, and the boot time it gives
//! plus the ticks since is the time for as long as the machine runs. Without
//! it every file written here was dated a few seconds after 1970, which fsck
//! reads as something else entirely.

use core::sync::atomic::{AtomicU64, Ordering};

use crate::io::{inb, outb};

const INDEX: u16 = 0x70;
const DATA: u16 = 0x71;

const SECONDS: u8 = 0x00;
const MINUTES: u8 = 0x02;
const HOURS: u8 = 0x04;
const DAY: u8 = 0x07;
const MONTH: u8 = 0x08;
const YEAR: u8 = 0x09;
const STATUS_A: u8 = 0x0A;
const STATUS_B: u8 = 0x0B;
/// Where the century usually is. ACPI's FADT says so; nothing else does.
const CENTURY: u8 = 0x32;

/// Seconds since 1970 when tick 0 was counted; 0 if the clock was unreadable.
static BOOT_TIME: AtomicU64 = AtomicU64::new(0);

fn read(reg: u8) -> u8 {
    unsafe {
        outb(INDEX, reg);
        inb(DATA)
    }
}

#[derive(Clone, Copy, PartialEq)]
struct Reading {
    sec: u8,
    min: u8,
    hour: u8,
    day: u8,
    month: u8,
    year: u8,
    century: u8,
}

/// One reading, taken outside an update. An update takes under two
/// milliseconds; the bound only stops a clock that never finishes one from
/// hanging the boot.
fn sample() -> Reading {
    let mut spins = 0u32;
    while read(STATUS_A) & 0x80 != 0 && spins < 1_000_000 {
        spins += 1;
        core::hint::spin_loop();
    }
    Reading {
        sec: read(SECONDS),
        min: read(MINUTES),
        hour: read(HOURS),
        day: read(DAY),
        month: read(MONTH),
        year: read(YEAR),
        century: read(CENTURY),
    }
}

/// Days from 1970-01-01 to a date in the proleptic Gregorian calendar.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Read the clock and remember when the machine booted.
pub fn init() {
    // Two identical readings in a row, so that none straddles an update.
    let mut a = sample();
    for _ in 0..8 {
        let b = sample();
        if a == b {
            break;
        }
        a = b;
    }
    let status = read(STATUS_B);
    let binary = status & 0x04 != 0;
    let hours_24 = status & 0x02 != 0;
    let value = |v: u8| -> i64 {
        if binary { v as i64 } else { ((v >> 4) * 10 + (v & 0x0F)) as i64 }
    };

    let sec = value(a.sec);
    let min = value(a.min);
    let mut hour = value(a.hour & 0x7F);
    if !hours_24 {
        let pm = a.hour & 0x80 != 0;
        hour = match (pm, hour) {
            (false, 12) => 0,
            (true, 12) => 12,
            (true, h) => h + 12,
            (false, h) => h,
        };
    }
    let day = value(a.day);
    let month = value(a.month);
    let century = value(a.century);
    let year = if (19..=21).contains(&century) {
        century * 100 + value(a.year)
    } else {
        // No century register; the machine is from this one.
        2000 + value(a.year)
    };
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || min > 59 || sec > 60 {
        crate::serial::puts(b"[rtc] the clock reads nonsense; no date\n");
        return;
    }

    let unix = days_from_civil(year, month, day) * 86_400 + hour * 3_600 + min * 60 + sec;
    let since_boot = (crate::pit::ticks() / 100) as i64;
    if unix > since_boot {
        BOOT_TIME.store((unix - since_boot) as u64, Ordering::Relaxed);
    }
    crate::serial::puts(b"[rtc] ");
    crate::serial::put_usize(year as usize);
    crate::serial::puts(b"-");
    two_digits(month);
    crate::serial::puts(b"-");
    two_digits(day);
    crate::serial::puts(b" ");
    two_digits(hour);
    crate::serial::puts(b":");
    two_digits(min);
    crate::serial::puts(b":");
    two_digits(sec);
    crate::serial::puts(b" UTC\n");
}

fn two_digits(v: i64) {
    if v < 10 {
        crate::serial::puts(b"0");
    }
    crate::serial::put_usize(v as usize);
}

/// Seconds since 1970 when tick 0 was counted, or 0 if there is no clock.
pub fn boot_time() -> u64 {
    BOOT_TIME.load(Ordering::Relaxed)
}
