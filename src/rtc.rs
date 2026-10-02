//! The date, from the PC's battery-backed clock.
//!
//! The kernel's clock counts from boot and knows nothing else (`clock.rs`).
//! The CMOS clock knows the date to the second; it is read once at boot,
//! and the time it gives then plus the time since is the date for as long
//! as the machine runs. Without it every file written here was dated a few
//! seconds after 1970, which fsck reads as something else entirely.
//!
//! And written when somebody who may says what time it is
//! (`SYS_CLOCK_SET`): it is the only clock that goes on while the machine
//! is off, so a date set and not written here is a date the next boot does
//! not have.

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

/// Status B: no updates while this is set, so that what is written is not
/// counted on from half way through.
const SET: u8 = 0x80;

fn read(reg: u8) -> u8 {
    unsafe {
        outb(INDEX, reg);
        inb(DATA)
    }
}

fn put(reg: u8, value: u8) {
    unsafe {
        outb(INDEX, reg);
        outb(DATA, value);
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
    if unix > 0 {
        crate::clock::set_wall(unix as u64 * 1_000_000_000);
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

/// The date `days` days after 1970-01-01: year, month, day. The other way
/// round from [`days_from_civil`].
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + if month <= 2 { 1 } else { 0 };
    (year, month, day)
}

/// Set the battery-backed clock to `unix` seconds after 1970, UTC, in
/// whichever of its forms it keeps time in: binary or decimal digits,
/// twenty-four hours or twelve.
///
/// The century is written only where one was read, which is the only way
/// there is to know the register is the century and not something else's.
pub fn write(unix: u64) {
    let days = (unix / 86_400) as i64;
    let rest = unix % 86_400;
    let (year, month, day) = civil_from_days(days);
    let (hour, min, sec) = (rest / 3_600, rest % 3_600 / 60, rest % 60);

    let flags: u64;
    unsafe { core::arch::asm!("pushfq; pop {}; cli", out(reg) flags, options(nostack)) };
    let status = read(STATUS_B);
    let binary = status & 0x04 != 0;
    let hours_24 = status & 0x02 != 0;
    let form = |v: u64| -> u8 {
        if binary { v as u8 } else { ((v / 10) << 4 | v % 10) as u8 }
    };
    let value = |v: u8| -> u64 {
        if binary { v as u64 } else { ((v >> 4) * 10 + (v & 0x0F)) as u64 }
    };
    let hour = if hours_24 {
        form(hour)
    } else {
        // Twelve is twelve; the afternoon is the top bit.
        let pm = if hour >= 12 { 0x80 } else { 0 };
        form(match hour % 12 { 0 => 12, h => h }) | pm
    };
    let had_century = (19..=21).contains(&value(read(CENTURY)));

    put(STATUS_B, status | SET);
    put(SECONDS, form(sec));
    put(MINUTES, form(min));
    put(HOURS, hour);
    put(DAY, form(day as u64));
    put(MONTH, form(month as u64));
    put(YEAR, form(year as u64 % 100));
    if had_century {
        put(CENTURY, form(year as u64 / 100));
    }
    put(STATUS_B, status);
    unsafe { core::arch::asm!("push {}; popfq", in(reg) flags, options(nostack)) };
}
