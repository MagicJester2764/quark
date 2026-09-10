//! What a client has asked for, and what each of those things is.
//!
//! One table per client, which is what makes "a client cannot name another
//! client's objects" true by construction rather than by a check: the id it
//! sends is only ever looked up in its own.
//!
//! Ids below `CLIENT_ID_MAX` are the client's to allocate and this only
//! validates them. Ids at or above are the compositor's; nothing here allocates
//! one yet, because the only interface that needs the compositor to name an
//! object is the clipboard.

pub const MAX_OBJECTS: usize = 64;
pub const CLIENT_ID_MAX: u32 = 0xFF00_0000;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    None,
    Display,
    Registry,
    Compositor,
    Shm,
    ShmPool { pool: usize },
    Buffer { buffer: usize },
    Surface { surface: usize },
    /// An opaque or input region. This compositor composites and routes the
    /// same either way, so the object exists so that clients may name it and
    /// its requests do nothing.
    Region,
    Output,
    Callback,
    XdgWmBase,
    XdgSurface { surface: usize },
    XdgToplevel { surface: usize },
}

#[derive(Clone, Copy)]
pub struct Table {
    ids: [u32; MAX_OBJECTS],
    kinds: [Kind; MAX_OBJECTS],
}

pub const EMPTY: Table = Table { ids: [0; MAX_OBJECTS], kinds: [Kind::None; MAX_OBJECTS] };

impl Table {
    pub fn clear(&mut self) {
        self.ids = [0; MAX_OBJECTS];
        self.kinds = [Kind::None; MAX_OBJECTS];
    }

    /// Record a new object. False if the id is the compositor's to give, is
    /// already in use, or there is no room.
    pub fn insert(&mut self, id: u32, kind: Kind) -> bool {
        if id == 0 || id >= CLIENT_ID_MAX || self.get(id).is_some() {
            return false;
        }
        for i in 0..MAX_OBJECTS {
            if self.ids[i] == 0 {
                self.ids[i] = id;
                self.kinds[i] = kind;
                return true;
            }
        }
        false
    }

    pub fn get(&self, id: u32) -> Option<Kind> {
        for i in 0..MAX_OBJECTS {
            if self.ids[i] == id && self.ids[i] != 0 {
                return Some(self.kinds[i]);
            }
        }
        None
    }

    /// The id of the first object matching a predicate.
    ///
    /// Events go to objects, and the compositor's own tables are keyed by its
    /// own indices — so telling a client that *its* buffer is free means
    /// finding the name it knows that buffer by.
    pub fn find(&self, pred: impl Fn(&Kind) -> bool) -> Option<u32> {
        (0..MAX_OBJECTS)
            .find(|&i| self.ids[i] != 0 && pred(&self.kinds[i]))
            .map(|i| self.ids[i])
    }

    pub fn remove(&mut self, id: u32) {
        for i in 0..MAX_OBJECTS {
            if self.ids[i] == id {
                self.ids[i] = 0;
                self.kinds[i] = Kind::None;
            }
        }
    }
}
