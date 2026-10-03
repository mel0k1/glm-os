//! v2.4: the wall clock — CMOS RTC (MC146818-compatible) on ports 0x70/0x71.
//!
//! The kernel until now only knew UPTIME (PIT @ 100 Hz): the taskbar clock
//! counted hours since boot and the OS could not tell what day it was.
//! This module reads the RTC once at boot, converts it to a Unix epoch and
//! keeps that as a base; the current wall time is then
//!
//!     now = base_epoch + uptime + adjustment
//!
//! Why not read the RTC on every query? The CMOS is slow (polled ports,
//! update-in-progress windows) and awkward to read from arbitrary contexts;
//! uptime is already maintained by the PIT everywhere. The base+delta model
//! gives microsecond-cheap wall time with one hardware read per boot.
//!
//! `clock_set` (NTP path): a ring-3 SNTP client may adjust the clock. The
//! adjustment is a delta against base+uptime, NOT a CMOS write — the RTC
//! chips is left untouched, so after a reboot the hardware time (what QEMU
//! keeps in sync with the host) shows through again. That is honest
//! behaviour for a single-user hobby OS: a lying hardware clock is worse
//! than a session-local correction.
//!
//! All values are UTC. QEMU defaults `-rtc base=utc`, so the epoch we
//! compute matches the host clock; `date` verifies it, NTP.ELF refines it.

use crate::cpu::pit;
use crate::io::ports::{inb, outb};
use core::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering::Relaxed};

const CMOS_ADDR: u16 = 0x70;
const CMOS_DATA: u16 = 0x71;

const REG_SEC: u8 = 0x00;
const REG_MIN: u8 = 0x02;
const REG_HOUR: u8 = 0x04;
const REG_DAY: u8 = 0x07;
const REG_MON: u8 = 0x08;
const REG_YEAR: u8 = 0x09;
const REG_CENTURY: u8 = 0x32; // ACPI century register (q35 keeps it)
const REG_STATUS_A: u8 = 0x0A;
const REG_STATUS_B: u8 = 0x0B;

static BASE_EPOCH: AtomicU64 = AtomicU64::new(0);
static ADJUST: AtomicI64 = AtomicI64::new(0); // clock_set delta (seconds)
static HAVE_RTC: AtomicBool = AtomicBool::new(false);

#[inline]
fn cmos_read(reg: u8) -> u8 {
    unsafe {
        outb(CMOS_ADDR, reg & 0x7F); // bit7 = NMI disable while addressing
        inb(CMOS_DATA)
    }
}

fn rtc_updating() -> bool {
    cmos_read(REG_STATUS_A) & 0x80 != 0
}

/// One consistent snapshot of the date/time registers. Returns None while
/// the chip is mid-update or the two halves disagree (retry then).
fn read_snapshot() -> Option<(u8, u8, u8, u8, u8, u8, u8, bool, bool)> {
    // (sec, min, hour, day, mon, year, century, is_bcd, is_12h)
    if rtc_updating() {
        return None;
    }
    let status_b = cmos_read(REG_STATUS_B);
    let is_bcd = status_b & 0x04 == 0;
    let is_12h = status_b & 0x02 == 0;
    let sec = cmos_read(REG_SEC);
    let min = cmos_read(REG_MIN);
    let hour = cmos_read(REG_HOUR);
    let day = cmos_read(REG_DAY);
    let mon = cmos_read(REG_MON);
    let year = cmos_read(REG_YEAR);
    let century = cmos_read(REG_CENTURY);
    // the update flag can rise mid-read: re-check and reject torn reads
    if rtc_updating() {
        return None;
    }
    let again = (cmos_read(REG_SEC), cmos_read(REG_MIN), cmos_read(REG_HOUR));
    if again.0 != sec || again.1 != min || again.2 != hour {
        return None;
    }
    Some((sec, min, hour, day, mon, year, century, is_bcd, is_12h))
}

fn bcd_or_bin(v: u8, is_bcd: bool) -> u8 {
    if is_bcd {
        (v & 0x0F) + (v >> 4) * 10
    } else {
        v
    }
}

/// Days since 1970-01-01 for a proleptic-Gregorian civil date
/// (Howard Hinnant's days_from_civil, public domain shape).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64; // [0, 399]
    let mp = if m > 2 { m - 3 } else { m + 9 } as u64; // [0, 11]
    let doy = (153 * mp + 2) / 5 + d as u64 - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe as i64 - 719_468
}

/// Inverse of days_from_civil: (year, month, day) for days since epoch.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (y + if m <= 2 { 1 } else { 0 }, m, d)
}

/// Boot probe: read the RTC (retrying over update windows), validate and
/// publish base_epoch. Never panics: a dead or garbage RTC just leaves
/// have() == false and the OS keeps living on uptime alone.
pub fn init() {
    let mut snap: Option<(u8, u8, u8, u8, u8, u8, u8, bool, bool)> = None;
    for _ in 0..1_000_000 {
        // a few hundreds of microseconds at most in practice
        if let Some(s) = read_snapshot() {
            snap = Some(s);
            break;
        }
    }
    let Some((sec, min, hour, day, mon, year, century, is_bcd, is_12h)) = snap else {
        return;
    };

    let mut sec = bcd_or_bin(sec, is_bcd) as i64;
    let mut min = bcd_or_bin(min, is_bcd) as i64;
    let mut hour = bcd_or_bin(hour, is_bcd) as i64;
    let day = bcd_or_bin(day, is_bcd) as i64;
    let mon = bcd_or_bin(mon, is_bcd) as i64;
    let mut year = bcd_or_bin(year, is_bcd) as i64;
    let century = bcd_or_bin(century, is_bcd) as i64;

    // 12-hour mode: 0x80 PM flag (the register keeps the BCD/bin value)
    if is_12h {
        let pm = hour & 0x80 != 0;
        hour &= 0x7F;
        if pm && hour < 12 {
            hour += 12;
        } else if !pm && hour == 12 {
            hour = 0;
        }
    }
    if century >= 19 && century <= 22 {
        year += century * 100;
    } else {
        // no usable century register: the RTC epoch is 1900-based for the
        // year byte, so assume the current century
        year += 2000;
    }

    // sanity gate: a garbage read must not become a "valid" 1970 date
    if !(1..=12).contains(&mon)
        || !(1..=31).contains(&day)
        || !(0..=23).contains(&hour)
        || !(0..=59).contains(&min)
        || !(0..=60).contains(&sec)
        || !(2019..=2099).contains(&year)
    {
        return;
    }
    if sec == 60 {
        sec = 59; // leap-second smear: clamp instead of failing the whole read
    }

    let epoch = days_from_civil(year, mon as u32, day as u32) * 86_400
        + hour * 3_600
        + min * 60
        + sec;
    BASE_EPOCH.store(epoch as u64, Relaxed);
    ADJUST.store(0, Relaxed);
    HAVE_RTC.store(true, Relaxed);
}

/// Is a wall clock available at all?
pub fn have() -> bool {
    HAVE_RTC.load(Relaxed)
}

/// Current wall time as a Unix epoch (seconds), or -1 without an RTC.
/// Cost: one add — safe to call from any task, every frame if needed.
pub fn now_epoch() -> i64 {
    if !HAVE_RTC.load(Relaxed) {
        return -1;
    }
    BASE_EPOCH.load(Relaxed) as i64 + (pit::uptime_ms() / 1000) as i64 + ADJUST.load(Relaxed)
}

/// v2.4: ring-3 clock adjustment (SNTP). Returns the new epoch or -1
/// without an RTC. Session-local by design (see module docs).
pub fn set_epoch(epoch: i64) -> i64 {
    if !HAVE_RTC.load(Relaxed) || epoch < 0 {
        return -1;
    }
    let delta = epoch - now_epoch();
    ADJUST.store(delta, Relaxed);
    epoch
}

/// "HH:MM:SS" (9 bytes) for the taskbar tray / `date`. Alloc-free.
pub fn fmt_hms(epoch: i64, buf: &mut [u8; 9]) -> &str {
    let s = epoch.rem_euclid(86_400);
    let (h, m, sec) = (s / 3_600, (s / 60) % 60, s % 60);
    buf[0] = b'0' + (h / 10) as u8;
    buf[1] = b'0' + (h % 10) as u8;
    buf[2] = b':';
    buf[3] = b'0' + (m / 10) as u8;
    buf[4] = b'0' + (m % 10) as u8;
    buf[5] = b':';
    buf[6] = b'0' + (sec / 10) as u8;
    buf[7] = b'0' + (sec % 10) as u8;
    buf[8] = 0;
    core::str::from_utf8(&buf[..8]).unwrap_or("??:?:?")
}

/// "YYYY-MM-DD HH:MM:SS" (20 bytes with NUL). Alloc-free.
pub fn fmt_datetime(epoch: i64, buf: &mut [u8; 20]) -> &str {
    let days = epoch.div_euclid(86_400);
    let secs = epoch.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let (h, mi, s) = (secs / 3_600, (secs / 60) % 60, secs % 60);
    let mut i = 0;
    let mut put = |buf: &mut [u8; 20], i: &mut usize, v: i64, pad: usize| {
        let mut digits = [0u8; 4];
        let mut n = 0;
        let mut v = v;
        if v == 0 {
            digits[0] = b'0';
            n = 1;
        }
        while v > 0 && n < 4 {
            digits[n] = b'0' + (v % 10) as u8;
            v /= 10;
            n += 1;
        }
        for k in n..pad {
            buf[*i] = b'0';
            *i += 1;
        }
        for k in (0..n).rev() {
            buf[*i] = digits[k];
            *i += 1;
        }
    };
    put(buf, &mut i, y, 4);
    buf[i] = b'-';
    i += 1;
    put(buf, &mut i, m as i64, 2);
    buf[i] = b'-';
    i += 1;
    put(buf, &mut i, d as i64, 2);
    buf[i] = b' ';
    i += 1;
    put(buf, &mut i, h, 2);
    buf[i] = b':';
    i += 1;
    put(buf, &mut i, mi, 2);
    buf[i] = b':';
    i += 1;
    put(buf, &mut i, s, 2);
    buf[i] = 0;
    core::str::from_utf8(&buf[..i]).unwrap_or("????-??-?? ??:??:??")
}
