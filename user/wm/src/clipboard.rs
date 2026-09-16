//! The selection: what was last copied, and who has it.
//!
//! The compositor never sees the data. A client offering a selection hands over
//! a `wl_data_source` and a list of MIME types it can produce; a client taking
//! it hands back a pipe, and the compositor passes that pipe to the source. The
//! bytes go from one client to the other directly, and copying a gigabyte
//! between two programs costs this compositor one descriptor.
//!
//! That is not a clever optimisation — it is the only arrangement that works
//! without an allocator, which this compositor does not have. There is nowhere
//! here to put a gigabyte, or a kilobyte.
//!
//! The selection follows keyboard focus. `wl_data_device.selection` goes to
//! whichever client has it, so a client that never has focus can never read the
//! clipboard, which is the rule that makes having one safe.

pub const MAX_MIMES: usize = 4;
pub const MIME_LEN: usize = 64;

/// What a client said it could produce.
#[derive(Clone, Copy)]
pub struct Mimes {
    names: [[u8; MIME_LEN]; MAX_MIMES],
    lens: [usize; MAX_MIMES],
    count: usize,
}

pub const NO_MIMES: Mimes =
    Mimes { names: [[0; MIME_LEN]; MAX_MIMES], lens: [0; MAX_MIMES], count: 0 };

impl Mimes {
    pub fn push(&mut self, name: &[u8]) -> bool {
        if self.count >= MAX_MIMES || name.len() > MIME_LEN {
            return false;
        }
        self.names[self.count][..name.len()].copy_from_slice(name);
        self.lens[self.count] = name.len();
        self.count += 1;
        true
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn get(&self, i: usize) -> Option<&[u8]> {
        if i >= self.count {
            return None;
        }
        Some(&self.names[i][..self.lens[i]])
    }

    /// Whether a name was offered. A receiver naming a type the source never
    /// claimed is asking for something nobody promised.
    pub fn has(&self, name: &[u8]) -> bool {
        (0..self.count).any(|i| self.get(i) == Some(name))
    }
}

/// The current selection.
#[derive(Clone, Copy)]
struct Selection {
    /// Whether there is one at all.
    held: bool,
    /// The client slot that owns the source, and the source's object id in
    /// *that* client's table — ids are per client, so both halves are needed.
    owner: usize,
    source: u32,
    mimes: Mimes,
}

static mut SELECTION: Selection =
    Selection { held: false, owner: 0, source: 0, mimes: NO_MIMES };

pub fn is_held() -> bool {
    unsafe { SELECTION.held }
}

pub fn mimes() -> Mimes {
    unsafe { SELECTION.mimes }
}

/// Who to ask for the bytes.
pub fn owner() -> Option<(usize, u32)> {
    unsafe {
        if SELECTION.held {
            Some((SELECTION.owner, SELECTION.source))
        } else {
            None
        }
    }
}

/// Take the selection. Returns the previous owner, who must be told it has been
/// cancelled — a source that is still offering something nobody can reach is a
/// program waiting for a request that will never come.
pub fn take(owner: usize, source: u32, mimes: Mimes) -> Option<(usize, u32)> {
    let previous = self_owner();
    unsafe {
        SELECTION = Selection { held: true, owner, source, mimes };
    }
    previous
}

/// Drop the selection if `owner`/`source` is the one holding it.
///
/// Scoped rather than unconditional: a client destroying an old source it has
/// already replaced must not clear somebody else's clipboard.
pub fn release(owner: usize, source: u32) -> bool {
    unsafe {
        if SELECTION.held && SELECTION.owner == owner && SELECTION.source == source {
            SELECTION = Selection { held: false, owner: 0, source: 0, mimes: NO_MIMES };
            return true;
        }
    }
    false
}

/// Everything a client had. A client may disconnect holding the selection, and
/// what it was offering goes with it.
pub fn forget_client(owner_slot: usize) {
    unsafe {
        if SELECTION.held && SELECTION.owner == owner_slot {
            SELECTION = Selection { held: false, owner: 0, source: 0, mimes: NO_MIMES };
        }
    }
}

fn self_owner() -> Option<(usize, u32)> {
    unsafe {
        if SELECTION.held {
            Some((SELECTION.owner, SELECTION.source))
        } else {
            None
        }
    }
}
