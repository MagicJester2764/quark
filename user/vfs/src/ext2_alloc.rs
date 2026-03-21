/// ext2 block and inode allocation via bitmap operations.

use crate::ext2::{
    flush_bgd, flush_superblock, raw_read_sector, Ext2State,
};
use crate::{DISK_IO_BUF, ERR_IO};

// ---------------------------------------------------------------------------
// Block allocation
// ---------------------------------------------------------------------------

/// Allocate a free block. Returns the block number.
pub fn alloc_block(ext2: &mut Ext2State) -> Result<u32, u64> {
    for group in 0..ext2.num_block_groups {
        let bgd = &ext2.bgd_table[group as usize];
        if bgd.bg_free_blocks_count == 0 {
            continue;
        }

        let bitmap_block = bgd.bg_block_bitmap;

        // Scan bitmap sectors
        for s in 0..ext2.sectors_per_block {
            let abs_lba = ext2.block_to_lba(bitmap_block) + s;
            raw_read_sector(ext2.disk_tid, ext2.buf_phys, abs_lba).map_err(|_| ERR_IO)?;
            let buf = unsafe { core::slice::from_raw_parts_mut(DISK_IO_BUF as *mut u8, 512) };

            for byte_idx in 0..512usize {
                if buf[byte_idx] == 0xFF {
                    continue;
                }
                for bit in 0..8u32 {
                    if buf[byte_idx] & (1 << bit) == 0 {
                        // Found free block
                        let block_in_group =
                            s * 512 * 8 + byte_idx as u32 * 8 + bit;
                        let block_num =
                            group * ext2.blocks_per_group + block_in_group + ext2.first_data_block;

                        if block_num >= ext2.total_blocks {
                            continue;
                        }

                        // Set the bit
                        buf[byte_idx] |= 1 << bit;
                        ext2.write_sector_abs(abs_lba).map_err(|_| ERR_IO)?;

                        // Update counts
                        ext2.bgd_table[group as usize].bg_free_blocks_count -= 1;
                        ext2.free_blocks_count -= 1;

                        flush_bgd(ext2, group)?;
                        flush_superblock(ext2)?;

                        return Ok(block_num);
                    }
                }
            }
        }
    }

    Err(ERR_IO) // no free blocks
}

/// Free a block.
pub fn free_block(ext2: &mut Ext2State, block: u32) -> Result<(), u64> {
    if block < ext2.first_data_block || block >= ext2.total_blocks {
        return Err(ERR_IO);
    }

    let adjusted = block - ext2.first_data_block;
    let group = adjusted / ext2.blocks_per_group;
    let block_in_group = adjusted % ext2.blocks_per_group;
    let byte_idx = (block_in_group / 8) as usize;
    let bit = block_in_group % 8;

    let bitmap_block = ext2.bgd_table[group as usize].bg_block_bitmap;
    let sector_in_bitmap = (byte_idx / 512) as u32;
    let byte_in_sector = byte_idx % 512;

    let abs_lba = ext2.block_to_lba(bitmap_block) + sector_in_bitmap;
    raw_read_sector(ext2.disk_tid, ext2.buf_phys, abs_lba).map_err(|_| ERR_IO)?;
    let buf = unsafe { core::slice::from_raw_parts_mut(DISK_IO_BUF as *mut u8, 512) };

    buf[byte_in_sector] &= !(1 << bit);
    ext2.write_sector_abs(abs_lba).map_err(|_| ERR_IO)?;

    ext2.bgd_table[group as usize].bg_free_blocks_count += 1;
    ext2.free_blocks_count += 1;

    flush_bgd(ext2, group)?;
    flush_superblock(ext2)?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Inode allocation
// ---------------------------------------------------------------------------

/// Allocate a free inode. Returns the inode number (1-based).
pub fn alloc_inode(ext2: &mut Ext2State) -> Result<u32, u64> {
    for group in 0..ext2.num_block_groups {
        let bgd = &ext2.bgd_table[group as usize];
        if bgd.bg_free_inodes_count == 0 {
            continue;
        }

        let bitmap_block = bgd.bg_inode_bitmap;

        for s in 0..ext2.sectors_per_block {
            let abs_lba = ext2.block_to_lba(bitmap_block) + s;
            raw_read_sector(ext2.disk_tid, ext2.buf_phys, abs_lba).map_err(|_| ERR_IO)?;
            let buf = unsafe { core::slice::from_raw_parts_mut(DISK_IO_BUF as *mut u8, 512) };

            for byte_idx in 0..512usize {
                if buf[byte_idx] == 0xFF {
                    continue;
                }
                for bit in 0..8u32 {
                    if buf[byte_idx] & (1 << bit) == 0 {
                        let inode_in_group = s * 512 * 8 + byte_idx as u32 * 8 + bit;
                        let inode_num = group * ext2.inodes_per_group + inode_in_group + 1;

                        if inode_num > ext2.total_inodes {
                            continue;
                        }

                        // Set the bit
                        buf[byte_idx] |= 1 << bit;
                        ext2.write_sector_abs(abs_lba).map_err(|_| ERR_IO)?;

                        // Update counts
                        ext2.bgd_table[group as usize].bg_free_inodes_count -= 1;
                        ext2.free_inodes_count -= 1;

                        flush_bgd(ext2, group)?;
                        flush_superblock(ext2)?;

                        return Ok(inode_num);
                    }
                }
            }
        }
    }

    Err(ERR_IO) // no free inodes
}

/// Free an inode.
pub fn free_inode(ext2: &mut Ext2State, inode_num: u32) -> Result<(), u64> {
    if inode_num == 0 || inode_num > ext2.total_inodes {
        return Err(ERR_IO);
    }

    let adjusted = inode_num - 1;
    let group = adjusted / ext2.inodes_per_group;
    let inode_in_group = adjusted % ext2.inodes_per_group;
    let byte_idx = (inode_in_group / 8) as usize;
    let bit = inode_in_group % 8;

    let bitmap_block = ext2.bgd_table[group as usize].bg_inode_bitmap;
    let sector_in_bitmap = (byte_idx / 512) as u32;
    let byte_in_sector = byte_idx % 512;

    let abs_lba = ext2.block_to_lba(bitmap_block) + sector_in_bitmap;
    raw_read_sector(ext2.disk_tid, ext2.buf_phys, abs_lba).map_err(|_| ERR_IO)?;
    let buf = unsafe { core::slice::from_raw_parts_mut(DISK_IO_BUF as *mut u8, 512) };

    buf[byte_in_sector] &= !(1 << bit);
    ext2.write_sector_abs(abs_lba).map_err(|_| ERR_IO)?;

    ext2.bgd_table[group as usize].bg_free_inodes_count += 1;
    ext2.free_inodes_count += 1;

    flush_bgd(ext2, group)?;
    flush_superblock(ext2)?;

    Ok(())
}
