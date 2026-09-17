//! Random bytes, from the kernel's generator.

use crate::syscall;

/// Fill all of `buf` with random bytes.
///
/// One call gives at most a mebibyte, so a longer buffer takes several. Fails
/// only if the kernel refuses the buffer, which a slice it can write never is.
pub fn fill(buf: &mut [u8]) -> Result<(), ()> {
    let mut done = 0;
    while done < buf.len() {
        match syscall::sys_getrandom(&mut buf[done..])? {
            0 => return Err(()),
            n => done += n,
        }
    }
    Ok(())
}
