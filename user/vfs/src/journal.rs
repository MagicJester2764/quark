//! The ext4 journal (jbd2).
//!
//! A filesystem write is several writes — a bitmap bit, a descriptor count, an
//! inode, a directory entry — and a machine that stops between them leaves a
//! filesystem that describes something that never existed: a block owned by
//! nobody, or an entry naming an inode that was never written. `fsck` can
//! usually guess what was meant. The journal removes the guessing.
//!
//! Every metadata write goes to the journal first, followed by a commit block.
//! Only then is it written where it belongs. A machine that stops before the
//! commit block leaves a transaction that is discarded, and the filesystem is
//! as it was; one that stops after it leaves a transaction that is replayed,
//! and the filesystem becomes what was intended. There is no in-between,
//! because the commit block is a single write.
//!
//! ```text
//!   journal:  [ descriptor | block | block | ... | commit ]
//!                  tags naming        the data      the point
//!                  where each goes    itself        of no return
//! ```
//!
//! jbd2 is big-endian on disk, alone among everything else here.
//!
//! **No revoke records.** They exist so that a stale metadata block in an old
//! transaction is not replayed over a location since reused for file data.
//! This commits one transaction at a time and empties the journal after
//! checkpointing it, so there is never an older transaction to be stale — the
//! cost is that transactions cannot be batched, which is speed rather than
//! correctness. Revoke blocks written by something else are still honoured on
//! recovery.

use crate::ext2::{read_inode, read_sector_bypass, Ext2Inode, Ext2State};
use crate::{DISK_IO_BUF, ERR_IO};
use quark_rt::println;

const JBD2_MAGIC: u32 = 0xC03B_3998;

const DESCRIPTOR_BLOCK: u32 = 1;
const COMMIT_BLOCK: u32 = 2;
const SUPERBLOCK_V1: u32 = 3;
const SUPERBLOCK_V2: u32 = 4;
const REVOKE_BLOCK: u32 = 5;

const INCOMPAT_REVOKE: u32 = 0x0000_0001;
const INCOMPAT_64BIT: u32 = 0x0000_0002;
const INCOMPAT_CSUM_V2: u32 = 0x0000_0008;
const INCOMPAT_CSUM_V3: u32 = 0x0000_0010;

/// What this can make sense of. A journal using anything else is not replayed,
/// because replaying it wrongly is worse than not replaying it: the filesystem
/// is merely stale, rather than half-overwritten with misread blocks.
const INCOMPAT_SUPPORTED: u32 = INCOMPAT_REVOKE | INCOMPAT_64BIT;

const TAG_ESCAPE: u16 = 1;
const TAG_SAME_UUID: u16 = 2;
const TAG_LAST_TAG: u16 = 8;

/// Inode holding the journal, named by the superblock's `s_journal_inum`.
pub const JOURNAL_INO: u32 = 8;

const HEADER_SIZE: usize = 12;
const UUID_SIZE: usize = 16;

/// Where a journal block is staged on its way to or from the disk.
///
/// Two pages: recovery holds a descriptor block in the first while copying
/// each of the blocks it names through the second.
const JBLOCK_BUF: usize = 0x8C_0000_0000;
const JBLOCK_PAGES: usize = 2;
/// The second staging page, used while the first holds a descriptor.
const JDATA_BUF: usize = JBLOCK_BUF + 4096;
/// Blocks a single transaction may hold. A create-and-write touches an inode,
/// its bitmap, the group descriptor, the superblock, a directory block and a
/// data block; sixteen leaves room and bounds what must be mapped.
pub const MAX_TXN_BLOCKS: usize = 16;
/// Where the blocks of the transaction being built are held.
const TXN_BUF: usize = 0x8D_0000_0000;

fn be32(b: &[u8], off: usize) -> u32 {
    u32::from_be_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

fn be16(b: &[u8], off: usize) -> u16 {
    u16::from_be_bytes([b[off], b[off + 1]])
}

fn put_be32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_be_bytes());
}

fn put_be16(b: &mut [u8], off: usize, v: u16) {
    b[off..off + 2].copy_from_slice(&v.to_be_bytes());
}

/// The journal, as the filesystem describes it.
pub struct Journal {
    /// The journal file's inode, used to turn a journal block number into a
    /// filesystem block.
    inode: Ext2Inode,
    /// `s_maxlen`: how many blocks the journal has.
    maxlen: u32,
    /// `s_first`: the first block that may hold log records; block 0 is the
    /// journal's own superblock.
    first: u32,
    /// `s_sequence`: the transaction ID the log starts at.
    sequence: u32,
    /// `s_start`: where the log starts, or 0 when the journal is empty.
    start: u32,
    incompat: u32,
    /// Next transaction ID to write.
    next_sequence: u32,
    /// Whether a transaction is open, and what it holds.
    txn: Txn,
    pub loaded: bool,
}

/// A transaction being built: filesystem blocks and their new contents.
struct Txn {
    open: bool,
    blocks: [u32; MAX_TXN_BLOCKS],
    count: usize,
    /// Set when a write did not fit. The transaction is then abandoned rather
    /// than committed short, since a half-transaction describes a filesystem
    /// that never existed.
    overflowed: bool,
}

impl Journal {
    pub const fn empty() -> Self {
        Journal {
            inode: Ext2Inode::empty(),
            maxlen: 0,
            first: 1,
            sequence: 1,
            start: 0,
            incompat: 0,
            next_sequence: 1,
            txn: Txn { open: false, blocks: [0; MAX_TXN_BLOCKS], count: 0, overflowed: false },
            loaded: false,
        }
    }

    /// Filesystem block holding journal block `n`.
    fn map(&self, ext2: &Ext2State, n: u32) -> Result<u32, u64> {
        let b = crate::ext2::block_map(ext2, &self.inode, n)?;
        if b == 0 { Err(ERR_IO) } else { Ok(b) }
    }

    /// Advance a journal block number, wrapping back to `first` at the end.
    fn wrap(&self, n: u32) -> u32 {
        if n >= self.maxlen { self.first } else { n }
    }
}

// ---------------------------------------------------------------------------
// Block I/O
// ---------------------------------------------------------------------------

/// Read filesystem block `fs_block` into the buffer at `vaddr`.
fn read_fs_block(ext2: &Ext2State, fs_block: u32, vaddr: usize) -> Result<(), u64> {
    let base = ext2.block_to_lba(fs_block);
    for s in 0..ext2.sectors_per_block {
        read_sector_bypass(ext2.disk_tid, ext2.buf_phys, base + s).map_err(|_| ERR_IO)?;
        let disk = unsafe { core::slice::from_raw_parts(DISK_IO_BUF as *const u8, 512) };
        let dst = unsafe {
            core::slice::from_raw_parts_mut((vaddr + (s * 512) as usize) as *mut u8, 512)
        };
        dst.copy_from_slice(disk);
    }
    Ok(())
}

/// Write the buffer at `vaddr` to filesystem block `fs_block`.
fn write_fs_block(ext2: &Ext2State, fs_block: u32, vaddr: usize) -> Result<(), u64> {
    let base = ext2.block_to_lba(fs_block);
    for s in 0..ext2.sectors_per_block {
        let src = unsafe {
            core::slice::from_raw_parts((vaddr + (s * 512) as usize) as *const u8, 512)
        };
        let disk = unsafe { core::slice::from_raw_parts_mut(DISK_IO_BUF as *mut u8, 512) };
        disk.copy_from_slice(src);
        ext2.write_sector_raw(base + s).map_err(|_| ERR_IO)?;
    }
    Ok(())
}

fn jbuf() -> &'static mut [u8] {
    unsafe { core::slice::from_raw_parts_mut(JBLOCK_BUF as *mut u8, 4096) }
}

fn txn_slot(i: usize) -> usize {
    TXN_BUF + i * 4096
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

/// Map the buffers the journal needs. Called once, before [`load`].
pub fn map_buffers() -> Result<(), ()> {
    for i in 0..JBLOCK_PAGES + MAX_TXN_BLOCKS {
        let phys = quark_rt::syscall::sys_phys_alloc(1)?;
        let vaddr = if i < JBLOCK_PAGES {
            JBLOCK_BUF + i * 4096
        } else {
            txn_slot(i - JBLOCK_PAGES)
        };
        quark_rt::syscall::sys_map_phys(phys, vaddr, 1)?;
    }
    Ok(())
}

/// Read the journal's superblock and make it usable.
///
/// Returns `Ok(false)` when the filesystem has no journal, which is not an
/// error: an ext2 volume and an ext4 one made without `has_journal` both work
/// without one, they simply have nothing to recover.
pub fn load(j: &mut Journal, ext2: &Ext2State) -> Result<bool, u64> {
    if ext2.feature_compat & crate::ext4::COMPAT_HAS_JOURNAL == 0 {
        return Ok(false);
    }

    j.inode = read_inode(ext2, JOURNAL_INO)?;

    // Block 0 of the journal file is the journal's own superblock.
    let sb_block = crate::ext2::block_map(ext2, &j.inode, 0)?;
    if sb_block == 0 {
        println!("[vfs] journal inode has no blocks");
        return Err(ERR_IO);
    }
    read_fs_block(ext2, sb_block, JBLOCK_BUF)?;
    let sb = jbuf();

    if be32(sb, 0) != JBD2_MAGIC {
        println!("[vfs] journal superblock magic is wrong");
        return Err(ERR_IO);
    }
    let blocktype = be32(sb, 4);
    if blocktype != SUPERBLOCK_V1 && blocktype != SUPERBLOCK_V2 {
        println!("[vfs] journal superblock has type {}", blocktype);
        return Err(ERR_IO);
    }

    let blocksize = be32(sb, 12);
    if blocksize != ext2.block_size {
        println!(
            "[vfs] journal block size {} does not match the filesystem's {}",
            blocksize, ext2.block_size
        );
        return Err(ERR_IO);
    }

    j.maxlen = be32(sb, 16);
    j.first = be32(sb, 20).max(1);
    j.sequence = be32(sb, 24);
    j.start = be32(sb, 28);
    j.incompat = if blocktype == SUPERBLOCK_V2 { be32(sb, 40) } else { 0 };

    let unknown = j.incompat & !INCOMPAT_SUPPORTED;
    if unknown != 0 {
        println!("[vfs] journal uses unsupported features 0x{:x}", unknown);
        return Err(ERR_IO);
    }
    if j.maxlen <= j.first {
        println!("[vfs] journal is too small ({} blocks)", j.maxlen);
        return Err(ERR_IO);
    }

    j.next_sequence = j.sequence;
    j.loaded = true;
    Ok(true)
}

/// Write the journal superblock back, which is what makes a transaction
/// findable — or, once it is checkpointed, makes it not.
fn write_super(j: &Journal, ext2: &Ext2State, start: u32, sequence: u32) -> Result<(), u64> {
    let sb_block = crate::ext2::block_map(ext2, &j.inode, 0)?;
    read_fs_block(ext2, sb_block, JBLOCK_BUF)?;
    let sb = jbuf();
    put_be32(sb, 24, sequence);
    put_be32(sb, 28, start);
    write_fs_block(ext2, sb_block, JBLOCK_BUF)
}

// ---------------------------------------------------------------------------
// Recovery
// ---------------------------------------------------------------------------

/// Blocks a recovery pass will believe were revoked. A transaction this
/// implementation wrote has none; the limit only bounds what somebody else's
/// journal can ask of us.
const MAX_REVOKES: usize = 512;

struct Revokes {
    block: [u32; MAX_REVOKES],
    seq: [u32; MAX_REVOKES],
    count: usize,
    overflowed: bool,
}

impl Revokes {
    fn record(&mut self, block: u32, seq: u32) {
        for i in 0..self.count {
            if self.block[i] == block {
                if seq > self.seq[i] {
                    self.seq[i] = seq;
                }
                return;
            }
        }
        if self.count == MAX_REVOKES {
            self.overflowed = true;
            return;
        }
        self.block[self.count] = block;
        self.seq[self.count] = seq;
        self.count += 1;
    }

    /// Was this block revoked at or after `seq`? If so, replaying it would
    /// write metadata over something that has since become other data.
    fn revoked(&self, block: u32, seq: u32) -> bool {
        for i in 0..self.count {
            if self.block[i] == block {
                return self.seq[i] >= seq;
            }
        }
        false
    }
}

static mut REVOKES: Revokes =
    Revokes { block: [0; MAX_REVOKES], seq: [0; MAX_REVOKES], count: 0, overflowed: false };

/// Bytes each block tag occupies in a descriptor block.
fn tag_size(j: &Journal) -> usize {
    if j.incompat & INCOMPAT_CSUM_V3 != 0 {
        16
    } else if j.incompat & INCOMPAT_64BIT != 0 {
        12
    } else {
        8
    }
}

/// A descriptor tag: which filesystem block the next journal block belongs to.
struct Tag {
    block: u32,
    flags: u16,
    /// How far past the tag the next one starts, UUID included.
    stride: usize,
}

fn parse_tag(j: &Journal, buf: &[u8], off: usize) -> Option<Tag> {
    let size = tag_size(j);
    if off + size > buf.len() {
        return None;
    }
    let (flags, hi) = if j.incompat & INCOMPAT_CSUM_V3 != 0 {
        (be32(buf, off + 4) as u16, be32(buf, off + 8))
    } else {
        (
            be16(buf, off + 6),
            if j.incompat & INCOMPAT_64BIT != 0 { be32(buf, off + 8) } else { 0 },
        )
    };
    if hi != 0 {
        return None; // past what this can address; see Ext2State::block32
    }
    let mut stride = size;
    if flags & TAG_SAME_UUID == 0 {
        stride += UUID_SIZE;
    }
    Some(Tag { block: be32(buf, off), flags, stride })
}

/// What a walk of the log is for.
#[derive(Clone, Copy, PartialEq)]
enum Pass {
    /// Find how far the log's committed transactions reach.
    Scan,
    /// Collect revoke records, which must all be known before replaying any.
    Revoke,
    /// Write the journalled blocks where they belong.
    Replay,
}

/// Walk the log once. Returns the transaction ID one past the last committed.
fn walk(j: &Journal, ext2: &Ext2State, pass: Pass, end: u32) -> Result<u32, u64> {
    let mut next_block = j.start;
    let mut seq = j.sequence;
    if next_block == 0 {
        return Ok(seq); // an empty journal
    }

    loop {
        if pass != Pass::Scan && seq == end {
            break;
        }

        let fs_block = j.map(ext2, next_block)?;
        read_fs_block(ext2, fs_block, JBLOCK_BUF)?;
        let buf = jbuf();

        // The log ends where a block stops looking like one. There is no
        // end marker: the journal is a ring of whatever was there before.
        if be32(buf, 0) != JBD2_MAGIC || be32(buf, 8) != seq {
            break;
        }

        match be32(buf, 4) {
            DESCRIPTOR_BLOCK => {
                let bs = ext2.block_size as usize;
                let mut off = HEADER_SIZE;
                let mut data_block = j.wrap(next_block + 1);
                loop {
                    let Some(tag) = parse_tag(j, &buf[..bs], off) else { break };
                    let revoked = unsafe { (*core::ptr::addr_of!(REVOKES)).revoked(tag.block, seq) };
                    if pass == Pass::Replay && !revoked {
                        let src = j.map(ext2, data_block)?;
                        read_fs_block(ext2, src, JDATA_BUF)?;

                        // A data block that happened to begin with the jbd2
                        // magic had its first word zeroed on the way in, so
                        // that it could not be mistaken for a log record.
                        if tag.flags & TAG_ESCAPE != 0 {
                            let d = unsafe {
                                core::slice::from_raw_parts_mut(JDATA_BUF as *mut u8, 4)
                            };
                            d.copy_from_slice(&JBD2_MAGIC.to_be_bytes());
                        }
                        write_fs_block(ext2, tag.block, JDATA_BUF)?;

                        // The descriptor was read into the same buffer the
                        // replay used, so read it back before the next tag.
                        read_fs_block(ext2, fs_block, JBLOCK_BUF)?;
                    }
                    data_block = j.wrap(data_block + 1);
                    off += tag.stride;
                    if tag.flags & TAG_LAST_TAG != 0 || off + tag_size(j) > bs {
                        break;
                    }
                }
                next_block = data_block;
            }
            COMMIT_BLOCK => {
                seq = seq.wrapping_add(1);
                next_block = j.wrap(next_block + 1);
            }
            REVOKE_BLOCK => {
                if pass == Pass::Revoke {
                    let bytes = be32(buf, HEADER_SIZE) as usize;
                    let bs = ext2.block_size as usize;
                    let mut off = HEADER_SIZE + 4;
                    let step = if j.incompat & INCOMPAT_64BIT != 0 { 8 } else { 4 };
                    while off + step <= bytes.min(bs) {
                        let block = be32(buf, off + step - 4);
                        unsafe { (*core::ptr::addr_of_mut!(REVOKES)).record(block, seq) };
                        off += step;
                    }
                }
                next_block = j.wrap(next_block + 1);
            }
            _ => break,
        }
    }

    Ok(seq)
}

/// Replay the journal, if it holds anything.
///
/// Three passes, and the order matters: everything revoked must be known
/// before anything is replayed, or a block would be written and then found to
/// have been freed.
pub fn recover(j: &mut Journal, ext2: &Ext2State) -> Result<(), u64> {
    if !j.loaded || j.start == 0 {
        return Ok(());
    }

    unsafe {
        let r = &mut *core::ptr::addr_of_mut!(REVOKES);
        r.count = 0;
        r.overflowed = false;
    }

    let end = walk(j, ext2, Pass::Scan, 0)?;
    if end == j.sequence {
        // Nothing committed: a transaction that was interrupted before its
        // commit block is not a transaction, and is discarded by ignoring it.
        println!("[vfs] journal: an uncommitted transaction was discarded");
        j.next_sequence = j.sequence;
        return Ok(());
    }

    walk(j, ext2, Pass::Revoke, end)?;
    if unsafe { (*core::ptr::addr_of!(REVOKES)).overflowed } {
        println!("[vfs] journal has more revoked blocks than can be tracked; not replaying");
        return Err(ERR_IO);
    }

    println!(
        "[vfs] journal: replaying transactions {}..{}",
        j.sequence,
        end - 1
    );
    walk(j, ext2, Pass::Replay, end)?;

    j.next_sequence = end;
    Ok(())
}

/// Mark the journal empty, once everything in it is where it belongs.
pub fn checkpoint_done(j: &mut Journal, ext2: &Ext2State) -> Result<(), u64> {
    if !j.loaded {
        return Ok(());
    }
    j.start = 0;
    j.sequence = j.next_sequence;
    write_super(j, ext2, 0, j.next_sequence)?;
    // Nothing outstanding, so nothing to recover. Cleared after the journal
    // superblock, never before: the window where both say work is pending is
    // harmless, and the one where neither does would lose it.
    crate::ext2::set_needs_recovery(ext2, false)
}

// ---------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------

/// Start collecting metadata writes into one transaction.
pub fn begin(j: &mut Journal) {
    if !j.loaded || j.txn.open {
        return;
    }
    j.txn.open = true;
    j.txn.count = 0;
    j.txn.overflowed = false;
}

/// Is a transaction collecting writes right now?
pub fn in_transaction(j: &Journal) -> bool {
    j.loaded && j.txn.open
}

/// Which slot holds `fs_block`, if the open transaction has it.
fn slot_of(j: &Journal, fs_block: u32) -> Option<usize> {
    (0..j.txn.count).find(|&i| j.txn.blocks[i] == fs_block)
}

/// Split an absolute sector into the filesystem block holding it and the
/// offset within that block.
fn locate(ext2: &Ext2State, abs_lba: u32) -> Option<(u32, usize)> {
    let rel = abs_lba.checked_sub(ext2.part_lba)?;
    Some((
        rel / ext2.sectors_per_block,
        (rel % ext2.sectors_per_block) as usize * 512,
    ))
}

/// Divert a sector write into the open transaction.
///
/// The new contents are in `DISK_IO_BUF`, where every write path leaves them.
/// Returns false if there is no transaction, or no room in it, in which case
/// the caller must write to the disk itself — the write still has to land,
/// even when it cannot be made atomic.
pub fn capture_write(j: &mut Journal, ext2: &Ext2State, abs_lba: u32) -> bool {
    if !j.txn.open {
        return false;
    }
    let Some((fs_block, off)) = locate(ext2, abs_lba) else { return false };

    // Copy the sector out first: staging a block not yet in the transaction
    // reads it off the disk, and that reads through the same buffer.
    let mut sector = [0u8; 512];
    sector.copy_from_slice(unsafe {
        core::slice::from_raw_parts(DISK_IO_BUF as *const u8, 512)
    });

    let slot = match slot_of(j, fs_block) {
        Some(i) => i,
        None => {
            if j.txn.count == MAX_TXN_BLOCKS {
                // Out of room. Say so once, and let the write through
                // unjournalled: losing it would be worse than not being able
                // to undo it.
                if !j.txn.overflowed {
                    j.txn.overflowed = true;
                    println!("[vfs] journal: transaction is full; the rest is not atomic");
                }
                return false;
            }
            let i = j.txn.count;
            // The journal records whole blocks, so the parts of this one that
            // are not being written have to come from the disk.
            if read_fs_block(ext2, fs_block, txn_slot(i)).is_err() {
                return false;
            }
            j.txn.blocks[i] = fs_block;
            j.txn.count += 1;
            i
        }
    };

    let dst = unsafe {
        core::slice::from_raw_parts_mut((txn_slot(slot) + off) as *mut u8, 512)
    };
    dst.copy_from_slice(&sector);
    true
}

/// Serve a sector read from the open transaction, if it holds one.
///
/// Without this a read-modify-write inside a transaction would read what is on
/// the disk — which is what the transaction is deliberately not writing yet —
/// and put back a block missing every change made so far.
pub fn peek_read(j: &Journal, ext2: &Ext2State, abs_lba: u32, out: &mut [u8]) -> bool {
    if !j.txn.open {
        return false;
    }
    let Some((fs_block, off)) = locate(ext2, abs_lba) else { return false };
    let Some(slot) = slot_of(j, fs_block) else { return false };
    let src = unsafe {
        core::slice::from_raw_parts((txn_slot(slot) + off) as *const u8, 512)
    };
    out.copy_from_slice(src);
    true
}

/// Write the transaction to the journal and commit it.
///
/// After this returns the change is durable: a machine that stops now will
/// replay it on the way back up. The blocks still have to be written where
/// they belong, which is [`checkpoint`].
pub fn commit(j: &mut Journal, ext2: &Ext2State) -> Result<bool, u64> {
    if !j.txn.open {
        return Ok(false);
    }
    j.txn.open = false;

    if j.txn.count == 0 {
        return Ok(false);
    }

    let bs = ext2.block_size as usize;
    let seq = j.next_sequence;
    let needed = 1 + j.txn.count + 1; // descriptor, data, commit
    if needed as u32 > j.maxlen - j.first {
        println!("[vfs] journal: transaction does not fit in the log");
        return Ok(false);
    }

    // Descriptor: which filesystem block each of the blocks that follow is.
    let mut block = j.first;
    {
        let buf = jbuf();
        buf[..bs].fill(0);
        put_be32(buf, 0, JBD2_MAGIC);
        put_be32(buf, 4, DESCRIPTOR_BLOCK);
        put_be32(buf, 8, seq);

        let size = tag_size(j);
        let mut off = HEADER_SIZE;
        for i in 0..j.txn.count {
            put_be32(buf, off, j.txn.blocks[i]);
            // Every tag says SAME_UUID, so none carries one: they all belong
            // to the one filesystem this journal serves.
            let mut flags = TAG_SAME_UUID;
            if i + 1 == j.txn.count {
                flags |= TAG_LAST_TAG;
            }
            // A staged block that begins with the magic would be read back as
            // a log record, so its first word is zeroed and restored on replay.
            let staged = unsafe { core::slice::from_raw_parts(txn_slot(i) as *const u8, 4) };
            if u32::from_be_bytes([staged[0], staged[1], staged[2], staged[3]]) == JBD2_MAGIC {
                flags |= TAG_ESCAPE;
            }
            if j.incompat & INCOMPAT_CSUM_V3 != 0 {
                put_be32(buf, off + 4, flags as u32);
            } else {
                put_be16(buf, off + 6, flags);
            }
            off += size;
        }
        let fs = j.map(ext2, block)?;
        write_fs_block(ext2, fs, JBLOCK_BUF)?;
    }
    block = j.wrap(block + 1);

    // The blocks themselves.
    for i in 0..j.txn.count {
        let staged = unsafe { core::slice::from_raw_parts_mut(txn_slot(i) as *mut u8, bs) };
        let escaped = u32::from_be_bytes([staged[0], staged[1], staged[2], staged[3]]) == JBD2_MAGIC;
        if escaped {
            staged[..4].fill(0);
        }
        let fs = j.map(ext2, block)?;
        write_fs_block(ext2, fs, txn_slot(i))?;
        if escaped {
            staged[..4].copy_from_slice(&JBD2_MAGIC.to_be_bytes());
        }
        block = j.wrap(block + 1);
    }

    // Say the filesystem needs recovering *before* the journal says it has
    // anything to recover. Between the two writes one of them is wrong, and
    // this is the harmless order: a filesystem that claims to need recovery
    // and has nothing in its journal recovers nothing, where the reverse is a
    // journal holding a transaction that nothing will ever be told to replay
    // — which is what `e2fsck` reports as "needs_recovery is clear, but
    // journal has data".
    crate::ext2::set_needs_recovery(ext2, true)?;
    mark_staged_super(j, ext2);

    // Now the journal can point at the transaction. The commit block is what
    // makes it real, and a recoverer that cannot find where to start looking
    // would not see it at all.
    write_super(j, ext2, j.first, seq)?;
    j.start = j.first;
    j.sequence = seq;

    // The commit block. One write, and the transaction either happened or did
    // not.
    {
        let buf = jbuf();
        buf[..bs].fill(0);
        put_be32(buf, 0, JBD2_MAGIC);
        put_be32(buf, 4, COMMIT_BLOCK);
        put_be32(buf, 8, seq);
        let fs = j.map(ext2, block)?;
        write_fs_block(ext2, fs, JBLOCK_BUF)?;
    }

    j.next_sequence = seq.wrapping_add(1);
    Ok(true)
}

/// Set `needs_recovery` in the transaction's own copy of the superblock.
///
/// The superblock is usually one of the blocks a transaction changes — it
/// carries the free counts — and its copy was taken before the flag was set.
/// Checkpointing it would therefore clear the flag while the journal still
/// held the transaction, which is the one combination that loses work:
/// `e2fsck` reports it as "needs_recovery is clear, but journal has data" and
/// declines to replay.
fn mark_staged_super(j: &mut Journal, ext2: &Ext2State) {
    // The superblock is 1024 bytes at byte offset 1024, so which block holds
    // it depends on the block size: its own with 1K blocks, the first with 4K.
    let bs = ext2.block_size as usize;
    let sb_block = 1024 / ext2.block_size;
    let off = 1024 % bs;
    let Some(slot) = slot_of(j, sb_block) else { return };
    if off + 1024 > bs {
        return; // a 512-byte block size, which nothing here makes
    }

    let sb = unsafe {
        core::slice::from_raw_parts_mut((txn_slot(slot) + off) as *mut u8, 1024)
    };
    let incompat = u32::from_le_bytes([sb[96], sb[97], sb[98], sb[99]]);
    sb[96..100].copy_from_slice(&(incompat | crate::ext4::INCOMPAT_RECOVER).to_le_bytes());
    crate::csum::set_superblock(sb);
}

/// Write a committed transaction's blocks where they belong, then empty the
/// journal.
///
/// Stopping part way through is safe: the transaction is still in the journal
/// and is replayed, writing the same blocks again.
pub fn checkpoint(j: &mut Journal, ext2: &Ext2State) -> Result<(), u64> {
    for i in 0..j.txn.count {
        write_fs_block(ext2, j.txn.blocks[i], txn_slot(i))?;
    }
    j.txn.count = 0;
    checkpoint_done(j, ext2)
}

/// Abandon the open transaction without committing it.
///
/// Nothing it holds reached the disk, so there is nothing to undo — but the
/// sector cache was kept agreeing with the transaction, and now holds writes
/// that will never happen.
pub fn abort(j: &mut Journal) {
    j.txn.open = false;
    j.txn.count = 0;
    j.txn.overflowed = false;
    unsafe { crate::SECTOR_CACHE.flush_all() };
}
