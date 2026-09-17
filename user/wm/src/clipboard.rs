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
//!
//! There are two of them, and they work the same way. The clipboard is what a
//! client copied on purpose; the *primary* selection is what was last
//! highlighted, which the middle button pastes. X11 has had both for thirty
//! years and Wayland keeps them apart for the same reason: a program that puts
//! every selection on the clipboard destroys what somebody copied to paste
//! next. Everything below is indexed by which of the two it is.

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

/// Which of the two. Used as an index, so the pair is one array and every
/// function below takes it rather than existing twice.
pub const CLIPBOARD: usize = 0;
pub const PRIMARY: usize = 1;
pub const KINDS: usize = 2;

const NO_SELECTION: Selection =
    Selection { held: false, owner: 0, source: 0, mimes: NO_MIMES };

static mut SELECTIONS: [Selection; KINDS] = [NO_SELECTION; KINDS];

pub fn is_held(which: usize) -> bool {
    unsafe { SELECTIONS[which].held }
}

pub fn mimes(which: usize) -> Mimes {
    unsafe { SELECTIONS[which].mimes }
}

/// Who to ask for the bytes.
pub fn owner(which: usize) -> Option<(usize, u32)> {
    unsafe {
        let s = &SELECTIONS[which];
        if s.held { Some((s.owner, s.source)) } else { None }
    }
}

/// Take the selection. Returns the previous owner, who must be told it has been
/// cancelled — a source that is still offering something nobody can reach is a
/// program waiting for a request that will never come.
pub fn take(which: usize, owner: usize, source: u32, mimes: Mimes) -> Option<(usize, u32)> {
    let previous = self::owner(which);
    unsafe {
        SELECTIONS[which] = Selection { held: true, owner, source, mimes };
    }
    previous
}

/// Drop the selection if `owner`/`source` is the one holding it.
///
/// Scoped rather than unconditional: a client destroying an old source it has
/// already replaced must not clear somebody else's clipboard.
pub fn release(which: usize, owner: usize, source: u32) -> bool {
    unsafe {
        let s = &mut SELECTIONS[which];
        if s.held && s.owner == owner && s.source == source {
            *s = NO_SELECTION;
            return true;
        }
    }
    false
}

/// Everything a client had, of either kind. A client may disconnect holding a
/// selection, and what it was offering goes with it.
pub fn forget_client(owner_slot: usize) {
    unsafe {
        for s in SELECTIONS.iter_mut() {
            if s.held && s.owner == owner_slot {
                *s = NO_SELECTION;
            }
        }
    }
}
