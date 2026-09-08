//! Talking to a display server.
//!
//! The client half of the window protocol, so that the tags are written down
//! once rather than once per program. A client asks for a window, gets shared
//! memory back, writes pixels into it, and says when it has finished; keys
//! arrive by asking, because a display server cannot send to a program it
//! spawned — originating IPC needs an `Endpoint` capability naming the
//! destination, and a task ID that did not exist at spawn time is not one it
//! can mint.

use crate::ipc::Message;
use crate::{nameserver, syscall};

/// Ask for a window. `data[0] = (width << 32) | height`, `data[1..]` the title.
pub const TAG_CREATE: u64 = 1;
/// This window's contents have changed: `data[0] = id`.
pub const TAG_COMMIT: u64 = 2;
/// Put a window somewhere: `data[0] = id`, `data[1] = (x << 32) | y`.
pub const TAG_MOVE: u64 = 3;
/// Raise a window and give it focus: `data[0] = id`.
pub const TAG_FOCUS: u64 = 4;
/// How big is the screen, and how are its pixels laid out?
pub const TAG_SCREEN: u64 = 5;
/// Give a window back: `data[0] = id`.
pub const TAG_DESTROY: u64 = 6;
/// Anything happened to this window? `data[0] = id`. Answers now, either way.
pub const TAG_POLL_EVENT: u64 = 7;

pub const TAG_OK: u64 = 0;
pub const TAG_ERROR: u64 = u64::MAX;

/// One key, as the display server saw it.
#[derive(Clone, Copy)]
pub struct Event {
    pub press: bool,
    pub ascii: u8,
    pub scancode: u8,
    pub modifiers: u8,
}

/// A window, and the memory behind it.
pub struct Window {
    pub server: usize,
    pub id: usize,
    /// Where the pixels are mapped in this address space.
    pub buf: usize,
    pub w: usize,
    pub h: usize,
    /// Bytes per row, which is not always `w * 4`.
    pub stride: usize,
    pub r_pos: u8,
    pub g_pos: u8,
    pub b_pos: u8,
    /// Whether this window would receive the next key, as of the last poll.
    pub focused: bool,
}

/// Find the display server. `None` if there is not one.
pub fn connect() -> Option<usize> {
    nameserver::lookup_retry(b"wm", 20)
}

impl Window {
    /// Ask for a window and map it at `vaddr`.
    ///
    /// The address is the caller's to choose because only the caller knows
    /// what else its address space holds.
    pub fn create(server: usize, w: usize, h: usize, title: &[u8], vaddr: usize) -> Option<Window> {
        let mut data = [0u64; 6];
        data[0] = ((w as u64) << 32) | (h as u64);
        for (i, chunk) in title.chunks(8).take(5).enumerate() {
            let mut word = [0u8; 8];
            word[..chunk.len()].copy_from_slice(chunk);
            data[1 + i] = u64::from_le_bytes(word);
        }

        let mut reply = Message::empty();
        let create = Message { sender: 0, tag: TAG_CREATE, data };
        if syscall::sys_call(server, &create, &mut reply).is_err() || reply.tag == TAG_ERROR {
            return None;
        }
        let id = reply.data[0] as usize;
        let shmem = reply.data[1] as usize;
        let stride = (reply.data[2] >> 32) as usize;
        if syscall::sys_shmem_map(shmem, vaddr).is_err() {
            return None;
        }

        // The pixel format, which a client needs as much as the size: a window
        // buffer is copied to the screen verbatim, so it has to be in the
        // screen's format.
        let mut fmt = Message::empty();
        let ask = Message { sender: 0, tag: TAG_SCREEN, data: [0; 6] };
        let (mut r_pos, mut g_pos, mut b_pos) = (16u8, 8u8, 0u8);
        if syscall::sys_call(server, &ask, &mut fmt).is_ok() && fmt.tag == TAG_OK {
            r_pos = ((fmt.data[1] >> 16) & 0xFF) as u8;
            g_pos = ((fmt.data[1] >> 8) & 0xFF) as u8;
            b_pos = (fmt.data[1] & 0xFF) as u8;
        }

        Some(Window {
            server,
            id,
            buf: vaddr,
            w,
            h,
            stride,
            r_pos,
            g_pos,
            b_pos,
            focused: false,
        })
    }

    pub fn colour(&self, r: u8, g: u8, b: u8) -> u32 {
        ((r as u32) << self.r_pos) | ((g as u32) << self.g_pos) | ((b as u32) << self.b_pos)
    }

    /// One row of the window, to write into directly.
    ///
    /// A slice rather than a pointer per pixel: the compositor reads this
    /// buffer only after the round trip that [`commit`](Self::commit) makes,
    /// so there is nothing for a volatile write to order against, and it stops
    /// the compiler from doing a row in one go.
    pub fn row(&self, y: usize) -> &mut [u32] {
        let at = self.buf + y * self.stride;
        unsafe { core::slice::from_raw_parts_mut(at as *mut u32, self.w) }
    }

    /// Say that the contents have changed.
    pub fn commit(&self) {
        let msg = Message { sender: 0, tag: TAG_COMMIT, data: [self.id as u64, 0, 0, 0, 0, 0] };
        let mut reply = Message::empty();
        let _ = syscall::sys_call(self.server, &msg, &mut reply);
    }

    /// Take the next event, if there is one, and refresh [`focused`](Self::focused).
    ///
    /// `Err(())` means the display server has gone, and so has the window.
    pub fn poll(&mut self) -> Result<Option<Event>, ()> {
        let msg = Message { sender: 0, tag: TAG_POLL_EVENT, data: [self.id as u64, 0, 0, 0, 0, 0] };
        let mut reply = Message::empty();
        if syscall::sys_call(self.server, &msg, &mut reply).is_err() || reply.tag == TAG_ERROR {
            return Err(());
        }
        self.focused = reply.data[5] != 0;
        if reply.data[0] == 0 {
            return Ok(None);
        }
        Ok(Some(Event {
            press: reply.data[4] != 0,
            ascii: reply.data[1] as u8,
            scancode: reply.data[2] as u8,
            modifiers: reply.data[3] as u8,
        }))
    }

    /// Give the window back.
    pub fn destroy(&self) {
        let msg = Message { sender: 0, tag: TAG_DESTROY, data: [self.id as u64, 0, 0, 0, 0, 0] };
        let mut reply = Message::empty();
        let _ = syscall::sys_call(self.server, &msg, &mut reply);
    }
}
