use crate::ipc::Message;
use crate::syscall;

const NAMESERVER_TID: usize = 2;
const TAG_NS_LOOKUP: u64 = 2;
const TAG_WRITE: u64 = 1;
const MAX_WRITE_BYTES: usize = 40;

static mut CONSOLE_TID: usize = 0;
/// Whether descriptor 1 is connected to anything: 0 unasked, 1 yes, 2 no.
static mut STDOUT: u8 = 0;

/// Is there a descriptor 1 to write to?
///
/// Asked with a write of nothing, which every kind of descriptor accepts and
/// an empty slot refuses — the kernel's own way of saying "there is nothing
/// here". Asked once: a program's descriptors are wired by whoever started it,
/// before it runs, so the answer does not change under it. A forked child
/// inherits both the descriptor and this answer; an exec starts a new image
/// and asks again.
fn have_stdout() -> bool {
    unsafe {
        if STDOUT == 0 {
            STDOUT = if syscall::sys_fd_write(1, &[]) == u64::MAX { 2 } else { 1 };
        }
        STDOUT == 1
    }
}

fn resolve_console() -> usize {
    unsafe {
        if CONSOLE_TID != 0 {
            return CONSOLE_TID;
        }
    }

    let name = b"console";
    let mut buf = [0u8; 24];
    buf[..name.len()].copy_from_slice(name);
    let w0 = u64::from_le_bytes(buf[0..8].try_into().unwrap());
    let w1 = u64::from_le_bytes(buf[8..16].try_into().unwrap());
    let w2 = u64::from_le_bytes(buf[16..24].try_into().unwrap());

    let msg = Message {
        sender: 0,
        tag: TAG_NS_LOOKUP,
        data: [w0, w1, w2, 0, 0, 0],
    };

    let mut reply = Message::empty();
    if syscall::sys_call(NAMESERVER_TID, &msg, &mut reply).is_ok() && reply.tag != u64::MAX {
        let tid = reply.tag as usize;
        unsafe { CONSOLE_TID = tid };
        tid
    } else {
        0
    }
}

fn bytes_to_data(bytes: &[u8], len: usize) -> [u64; 5] {
    let mut words = [0u64; 5];
    for i in 0..5 {
        let base = i * 8;
        let mut w = [0u8; 8];
        for j in 0..8 {
            if base + j < len {
                w[j] = bytes[base + j];
            }
        }
        words[i] = u64::from_le_bytes(w);
    }
    words
}

/// Write bytes to standard output.
///
/// Descriptor 1 first, and the console service only when there is no
/// descriptor on it. That order is the whole point: a program's output should
/// go wherever its output was pointed — into a pipe, into a terminal's pty,
/// into a file — and the console is where a program with nowhere else to
/// write ends up, not where everything goes regardless.
///
/// The fallback is not a formality. A driver started before the console
/// exists, and `init` before it has wired anybody's descriptors, still have
/// to be able to say something.
pub fn console_write(s: &[u8]) {
    if have_stdout() {
        syscall::sys_fd_write(1, s);
        return;
    }
    let tid = resolve_console();
    if tid == 0 {
        syscall::sys_write(s);
        return;
    }

    let mut offset = 0;
    while offset < s.len() {
        let chunk = (s.len() - offset).min(MAX_WRITE_BYTES);
        let words = bytes_to_data(&s[offset..], chunk);
        let msg = Message {
            sender: 0,
            tag: TAG_WRITE,
            data: [chunk as u64, words[0], words[1], words[2], words[3], words[4]],
        };
        let mut reply = Message::empty();
        if syscall::sys_call(tid, &msg, &mut reply).is_err() {
            // Fallback to kernel console
            syscall::sys_write(&s[offset..offset + chunk]);
        }
        offset += chunk;
    }
}
