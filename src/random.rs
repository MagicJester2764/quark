//! Random numbers: ChaCha20 (RFC 8439) with fast key erasure.
//!
//! Each request draws keystream from the current key. The first 32 bytes of
//! the first block become the next key and are never handed out, so a key
//! read out of memory later cannot recompute anything given out before it.
//!
//! The first key comes from RDSEED, else RDRAND, mixed with the TSC, the timer
//! and the clock. Every timer tick folds the TSC into a pool, and every request
//! takes the pool into its nonce, which the next key then carries forward. A
//! CPU with neither instruction leaves only the timing, which is guessable,
//! and the kernel says so at boot.

use core::sync::atomic::{AtomicU64, Ordering};

/// Timing folded in since the last request.
static POOL: AtomicU64 = AtomicU64::new(0);

struct State {
    key: [u8; 32],
    /// Requests so far; the rest of the nonce, so that no two share one.
    requests: u32,
}

static mut STATE: State = State { key: [0; 32], requests: 0 };

#[inline(always)]
fn irq_save() -> u64 {
    let flags: u64;
    unsafe {
        core::arch::asm!("pushfq; pop {}; cli", out(reg) flags, options(nostack));
    }
    flags
}

#[inline(always)]
fn irq_restore(flags: u64) {
    unsafe {
        core::arch::asm!("push {}; popfq", in(reg) flags, options(nostack));
    }
}

/// One ChaCha20 block (RFC 8439 §2.3): `out` is the keystream for `key`,
/// `counter` and `nonce`.
fn block(key: &[u8; 32], counter: u32, nonce: &[u8; 12], out: &mut [u8; 64]) {
    const C: [u32; 4] = [0x6170_7865, 0x3320_646e, 0x7962_2d32, 0x6b20_6574];
    fn word(b: &[u8]) -> u32 {
        u32::from_le_bytes([b[0], b[1], b[2], b[3]])
    }
    fn qr(w: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
        w[a] = w[a].wrapping_add(w[b]);
        w[d] = (w[d] ^ w[a]).rotate_left(16);
        w[c] = w[c].wrapping_add(w[d]);
        w[b] = (w[b] ^ w[c]).rotate_left(12);
        w[a] = w[a].wrapping_add(w[b]);
        w[d] = (w[d] ^ w[a]).rotate_left(8);
        w[c] = w[c].wrapping_add(w[d]);
        w[b] = (w[b] ^ w[c]).rotate_left(7);
    }
    let mut s = [0u32; 16];
    s[..4].copy_from_slice(&C);
    for i in 0..8 {
        s[4 + i] = word(&key[i * 4..]);
    }
    s[12] = counter;
    for i in 0..3 {
        s[13 + i] = word(&nonce[i * 4..]);
    }
    let mut w = s;
    for _ in 0..10 {
        qr(&mut w, 0, 4, 8, 12);
        qr(&mut w, 1, 5, 9, 13);
        qr(&mut w, 2, 6, 10, 14);
        qr(&mut w, 3, 7, 11, 15);
        qr(&mut w, 0, 5, 10, 15);
        qr(&mut w, 1, 6, 11, 12);
        qr(&mut w, 2, 7, 8, 13);
        qr(&mut w, 3, 4, 9, 14);
    }
    for i in 0..16 {
        out[i * 4..i * 4 + 4].copy_from_slice(&w[i].wrapping_add(s[i]).to_le_bytes());
    }
    w.fill(0);
    s.fill(0);
}

/// Fill `out` with random bytes. Any length; the caller chunks what it copies.
pub fn fill(out: &mut [u8]) {
    let mut ks = [0u8; 64];
    let mut nonce = [0u8; 12];
    let flags = irq_save();
    // SAFETY: interrupts are off and there is one CPU, so nothing else is in
    // here — the timer only touches POOL.
    let state = unsafe { &mut *core::ptr::addr_of_mut!(STATE) };
    nonce[..8].copy_from_slice(&POOL.swap(0, Ordering::Relaxed).to_le_bytes());
    nonce[8..].copy_from_slice(&state.requests.to_le_bytes());
    state.requests = state.requests.wrapping_add(1);

    // Every block of this request comes from the key as it was; the start of
    // the first replaces it before anything is handed out.
    let mut key = state.key;
    block(&key, 0, &nonce, &mut ks);
    state.key.copy_from_slice(&ks[..32]);
    let mut done = (ks.len() - 32).min(out.len());
    out[..done].copy_from_slice(&ks[32..32 + done]);
    let mut counter = 1u32;
    while done < out.len() {
        block(&key, counter, &nonce, &mut ks);
        let n = ks.len().min(out.len() - done);
        out[done..done + n].copy_from_slice(&ks[..n]);
        done += n;
        counter = counter.wrapping_add(1);
    }
    irq_restore(flags);
    key.fill(0);
    ks.fill(0);
    nonce.fill(0);
}

/// Fold timing into the pool. Called on every timer tick.
pub fn stir() {
    let t = rdtsc();
    let p = POOL.load(Ordering::Relaxed);
    POOL.store((p.rotate_left(13) ^ t).wrapping_mul(0x9E37_79B9_7F4A_7C15), Ordering::Relaxed);
}

fn rdtsc() -> u64 {
    let (hi, lo): (u32, u32);
    unsafe {
        core::arch::asm!("rdtsc", out("eax") lo, out("edx") hi, options(nomem, nostack));
    }
    (u64::from(hi) << 32) | u64::from(lo)
}

/// RDSEED, then RDRAND, each retried a little as Intel recommends.
fn hardware_word(seed: bool) -> Option<u64> {
    for _ in 0..10 {
        let value: u64;
        let ok: u8;
        unsafe {
            if seed {
                core::arch::asm!("rdseed {v}", "setc {ok}", v = out(reg) value,
                    ok = out(reg_byte) ok, options(nomem, nostack));
            } else {
                core::arch::asm!("rdrand {v}", "setc {ok}", v = out(reg) value,
                    ok = out(reg_byte) ok, options(nomem, nostack));
            }
        }
        if ok != 0 {
            return Some(value);
        }
    }
    None
}

/// The latched count of the timer's channel 0: where in the current tick we
/// are, which is timing a remote observer cannot see.
fn pit_count() -> u64 {
    unsafe {
        crate::io::outb(0x43, 0x00);
        let lo = crate::io::inb(0x40);
        let hi = crate::io::inb(0x40);
        u64::from(lo) | (u64::from(hi) << 8)
    }
}

/// Check the block function against RFC 8439 §2.3.2. A wrong generator hands
/// out something other than what it claims, and must not boot.
fn self_test() -> bool {
    const EXPECTED: [u8; 64] = [
        0x10, 0xf1, 0xe7, 0xe4, 0xd1, 0x3b, 0x59, 0x15, 0x50, 0x0f, 0xdd, 0x1f, 0xa3, 0x20, 0x71, 0xc4,
        0xc7, 0xd1, 0xf4, 0xc7, 0x33, 0xc0, 0x68, 0x03, 0x04, 0x22, 0xaa, 0x9a, 0xc3, 0xd4, 0x6c, 0x4e,
        0xd2, 0x82, 0x64, 0x46, 0x07, 0x9f, 0xaa, 0x09, 0x14, 0xc2, 0xd7, 0x05, 0xd9, 0x8b, 0x02, 0xa2,
        0xb5, 0x12, 0x9c, 0xd1, 0xde, 0x16, 0x4e, 0xb9, 0xcb, 0xd0, 0x83, 0xe8, 0xa2, 0x50, 0x3c, 0x4e,
    ];
    let mut key = [0u8; 32];
    for (i, b) in key.iter_mut().enumerate() {
        *b = i as u8;
    }
    let nonce = [0, 0, 0, 0x09, 0, 0, 0, 0x4a, 0, 0, 0, 0];
    let mut out = [0u8; 64];
    block(&key, 1, &nonce, &mut out);
    out == EXPECTED
}

/// Seed the generator. Runs once at boot, after the clock and the timer.
pub fn init() {
    if !self_test() {
        crate::serial::puts(b"[random] ChaCha20 self-test failed\n");
        panic!("ChaCha20 self-test failed");
    }
    let (rdseed, rdrand) = crate::cpu::random_instructions();
    let mut words = [0u64; 8];
    let mut from_hardware = false;
    for w in words.iter_mut().take(4) {
        let hw = if rdseed { hardware_word(true) } else { None }
            .or_else(|| if rdrand { hardware_word(false) } else { None });
        if let Some(v) = hw {
            *w = v;
            from_hardware = true;
        }
    }
    words[4] = rdtsc();
    words[5] = pit_count() ^ (crate::pit::ticks() << 16);
    words[6] = crate::rtc::boot_time();
    words[7] = rdtsc();

    let mut seed = [0u8; 32];
    for (i, w) in words.iter().enumerate() {
        // The hardware words cover the key end to end; the timing words land
        // across their seams. The first request's block mixes all of it.
        let at = if i < 4 { i * 8 } else { (i - 4) * 8 + 4 };
        let bytes = w.to_le_bytes();
        for (j, b) in bytes.iter().enumerate() {
            seed[(at + j) % 32] ^= *b;
        }
    }
    let flags = irq_save();
    // SAFETY: interrupts are off, and nothing has asked for a number yet.
    unsafe {
        (*core::ptr::addr_of_mut!(STATE)).key = seed;
    }
    irq_restore(flags);
    seed.fill(0);
    words.fill(0);
    // Stir once so the first key handed to a caller is already a derived one.
    let mut discard = [0u8; 32];
    fill(&mut discard);

    if !from_hardware {
        for out in [crate::console::puts as fn(&[u8]), crate::serial::puts as fn(&[u8])] {
            out(b"[random] no RDRAND or RDSEED; seeded from timing\n");
        }
    }
}
