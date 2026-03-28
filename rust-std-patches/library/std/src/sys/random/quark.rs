use crate::io;

pub fn fill_bytes(bytes: &mut [u8]) {
    // Use RDRAND if available, otherwise zero-fill.
    for chunk in bytes.chunks_mut(8) {
        let val: u64;
        let ok: u8;
        unsafe {
            core::arch::asm!(
                "rdrand {val}",
                "setc {ok}",
                val = out(reg) val,
                ok = out(reg_byte) ok,
            );
        }
        let src = if ok != 0 { val.to_le_bytes() } else { [0u8; 8] };
        chunk.copy_from_slice(&src[..chunk.len()]);
    }
}
