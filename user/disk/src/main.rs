#![no_std]
#![no_main]
#![allow(dead_code)]

use libquark::ipc::{Message, TID_ANY};
use libquark::{println, syscall};

const NAMESERVER_TID: usize = 2;

// Nameserver protocol
const TAG_NS_REGISTER: u64 = 1;

// Disk IPC tags
const TAG_READ_SECTOR: u64 = 1;
const TAG_WRITE_SECTOR: u64 = 2;
const TAG_DISK_INFO: u64 = 3;
const TAG_READ_SECTORS: u64 = 4;
const TAG_OK: u64 = 0;
const TAG_ERROR: u64 = u64::MAX;

// ATA PIO ports (primary channel)
const ATA_DATA: u16 = 0x1F0;
const ATA_ERROR: u16 = 0x1F1;
const ATA_SECTOR_COUNT: u16 = 0x1F2;
const ATA_LBA_LO: u16 = 0x1F3;
const ATA_LBA_MID: u16 = 0x1F4;
const ATA_LBA_HI: u16 = 0x1F5;
const ATA_DRIVE_HEAD: u16 = 0x1F6;
const ATA_STATUS: u16 = 0x1F7;
const ATA_COMMAND: u16 = 0x1F7;
const ATA_ALT_STATUS: u16 = 0x3F6;

// ATA status bits
const ATA_SR_BSY: u8 = 0x80;
const ATA_SR_DRDY: u8 = 0x40;
const ATA_SR_DRQ: u8 = 0x08;
const ATA_SR_ERR: u8 = 0x01;

// ATA commands
const ATA_CMD_IDENTIFY: u8 = 0xEC;
const ATA_CMD_READ_PIO: u8 = 0x20;
const ATA_CMD_WRITE_PIO: u8 = 0x30;

// Temp vaddr for mapping client pages
const TEMP_MAP_ADDR: usize = 0x86_0000_0000;

struct DriveInfo {
    present: bool,
    lba28_sectors: u32,
}

static mut DRIVE: DriveInfo = DriveInfo {
    present: false,
    lba28_sectors: 0,
};

fn ata_read_status() -> u8 {
    syscall::sys_ioport_read(ATA_ALT_STATUS) as u8
}

/// Block until IRQ 14 fires (disk operation complete).
fn wait_for_irq() {
    let mut msg = Message::empty();
    let _ = syscall::sys_recv(0, &mut msg);
    syscall::sys_irq_ack(14);
}

fn ata_wait_not_busy() {
    loop {
        let status = ata_read_status();
        if status & ATA_SR_BSY == 0 {
            return;
        }
        wait_for_irq();
    }
}

fn ata_wait_drq() -> bool {
    loop {
        let status = ata_read_status();
        if status & ATA_SR_ERR != 0 {
            return false;
        }
        if status & ATA_SR_BSY == 0 && status & ATA_SR_DRQ != 0 {
            return true;
        }
        wait_for_irq();
    }
}

fn ata_400ns_delay() {
    // Read alt status 4 times (~400ns delay)
    for _ in 0..4 {
        syscall::sys_ioport_read(ATA_ALT_STATUS);
    }
}

fn ata_identify() -> bool {
    // Select drive 0 (master)
    syscall::sys_ioport_write(ATA_DRIVE_HEAD, 0xA0);
    ata_400ns_delay();

    // Zero out sector count and LBA registers
    syscall::sys_ioport_write(ATA_SECTOR_COUNT, 0);
    syscall::sys_ioport_write(ATA_LBA_LO, 0);
    syscall::sys_ioport_write(ATA_LBA_MID, 0);
    syscall::sys_ioport_write(ATA_LBA_HI, 0);

    // Send IDENTIFY command
    syscall::sys_ioport_write(ATA_COMMAND, ATA_CMD_IDENTIFY);
    ata_400ns_delay();

    // Check if drive exists
    let status = ata_read_status();
    if status == 0 {
        println!("[disk] No drive detected on primary master.");
        return false;
    }

    // Wait for BSY to clear
    ata_wait_not_busy();

    // Check for non-ATA devices (ATAPI, SATA, etc.)
    let lba_mid = syscall::sys_ioport_read(ATA_LBA_MID) as u8;
    let lba_hi = syscall::sys_ioport_read(ATA_LBA_HI) as u8;
    if lba_mid != 0 || lba_hi != 0 {
        println!("[disk] Non-ATA device detected (mid={:#x}, hi={:#x}).", lba_mid, lba_hi);
        return false;
    }

    // Wait for DRQ
    if !ata_wait_drq() {
        println!("[disk] IDENTIFY command failed (error).");
        return false;
    }

    // Read 256 words of identify data
    let mut identify = [0u16; 256];
    let _ = syscall::sys_ioport_rep_insw(ATA_DATA, &mut identify);

    // Extract model string (words 27-46, swapped byte pairs)
    let mut model = [0u8; 40];
    for i in 0..20 {
        let word = identify[27 + i];
        model[i * 2] = (word >> 8) as u8;
        model[i * 2 + 1] = word as u8;
    }
    // Trim trailing spaces
    let model_len = model.iter().rposition(|&b| b != b' ' && b != 0).map_or(0, |p| p + 1);

    // LBA28 sector count (words 60-61)
    let lba28_sectors = (identify[61] as u32) << 16 | (identify[60] as u32);

    unsafe {
        DRIVE.present = true;
        DRIVE.lba28_sectors = lba28_sectors;
    }

    // Print drive info
    if let Ok(model_str) = core::str::from_utf8(&model[..model_len]) {
        println!("[disk] ATA drive: {}", model_str);
    }
    println!("[disk] {} sectors ({} MiB)", lba28_sectors, lba28_sectors / 2048);

    true
}

fn ata_read_sector(lba: u32, buf: *mut u8) -> bool {
    let max_sectors = unsafe { DRIVE.lba28_sectors };
    if lba >= max_sectors {
        return false;
    }

    ata_wait_not_busy();

    // Select drive 0, LBA mode, top 4 bits of LBA
    syscall::sys_ioport_write(ATA_DRIVE_HEAD, 0xE0 | ((lba >> 24) & 0x0F) as u8);
    ata_400ns_delay();

    // Set sector count = 1
    syscall::sys_ioport_write(ATA_SECTOR_COUNT, 1);

    // Set LBA
    syscall::sys_ioport_write(ATA_LBA_LO, lba as u8);
    syscall::sys_ioport_write(ATA_LBA_MID, (lba >> 8) as u8);
    syscall::sys_ioport_write(ATA_LBA_HI, (lba >> 16) as u8);

    // Send READ SECTORS command
    syscall::sys_ioport_write(ATA_COMMAND, ATA_CMD_READ_PIO);
    ata_400ns_delay();

    // Wait for DRQ
    if !ata_wait_drq() {
        return false;
    }

    // Read 256 words (512 bytes)
    let words = unsafe { core::slice::from_raw_parts_mut(buf as *mut u16, 256) };
    let _ = syscall::sys_ioport_rep_insw(ATA_DATA, words);

    true
}

fn ata_read_sectors(lba: u32, count: u32, buf: *mut u8) -> bool {
    let max_sectors = unsafe { DRIVE.lba28_sectors };
    if lba + count > max_sectors || count == 0 || count > 8 {
        return false;
    }

    ata_wait_not_busy();

    syscall::sys_ioport_write(ATA_DRIVE_HEAD, 0xE0 | ((lba >> 24) & 0x0F) as u8);
    ata_400ns_delay();

    syscall::sys_ioport_write(ATA_SECTOR_COUNT, count as u8);
    syscall::sys_ioport_write(ATA_LBA_LO, lba as u8);
    syscall::sys_ioport_write(ATA_LBA_MID, (lba >> 8) as u8);
    syscall::sys_ioport_write(ATA_LBA_HI, (lba >> 16) as u8);

    syscall::sys_ioport_write(ATA_COMMAND, ATA_CMD_READ_PIO);
    ata_400ns_delay();

    for i in 0..count {
        if !ata_wait_drq() {
            return false;
        }
        let offset = (i as usize) * 512;
        let words = unsafe { core::slice::from_raw_parts_mut(buf.add(offset) as *mut u16, 256) };
        let _ = syscall::sys_ioport_rep_insw(ATA_DATA, words);
    }

    true
}

fn ata_write_sector(lba: u32, buf: *const u8) -> bool {
    let max_sectors = unsafe { DRIVE.lba28_sectors };
    if lba >= max_sectors {
        return false;
    }

    ata_wait_not_busy();

    // Select drive 0, LBA mode, top 4 bits of LBA
    syscall::sys_ioport_write(ATA_DRIVE_HEAD, 0xE0 | ((lba >> 24) & 0x0F) as u8);
    ata_400ns_delay();

    // Set sector count = 1
    syscall::sys_ioport_write(ATA_SECTOR_COUNT, 1);

    // Set LBA
    syscall::sys_ioport_write(ATA_LBA_LO, lba as u8);
    syscall::sys_ioport_write(ATA_LBA_MID, (lba >> 8) as u8);
    syscall::sys_ioport_write(ATA_LBA_HI, (lba >> 16) as u8);

    // Send WRITE SECTORS command
    syscall::sys_ioport_write(ATA_COMMAND, ATA_CMD_WRITE_PIO);
    ata_400ns_delay();

    // Wait for DRQ (device ready to accept data)
    if !ata_wait_drq() {
        return false;
    }

    // Write 256 words (512 bytes)
    let words = unsafe { core::slice::from_raw_parts(buf as *const u16, 256) };
    let _ = syscall::sys_ioport_rep_outsw(ATA_DATA, words);

    // Flush cache — wait for BSY to clear after write
    ata_wait_not_busy();

    // Check for errors
    let status = ata_read_status();
    if status & ATA_SR_ERR != 0 {
        return false;
    }

    true
}

fn register_with_nameserver() {
    let name = b"disk";
    let mut buf = [0u8; 24];
    buf[..name.len()].copy_from_slice(name);
    let w0 = u64::from_le_bytes([buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7]]);
    let w1 = u64::from_le_bytes([buf[8], buf[9], buf[10], buf[11], buf[12], buf[13], buf[14], buf[15]]);
    let w2 = u64::from_le_bytes([buf[16], buf[17], buf[18], buf[19], buf[20], buf[21], buf[22], buf[23]]);

    let msg = Message {
        sender: 0,
        tag: TAG_NS_REGISTER,
        data: [w0, w1, w2, 0, 0, 0],
    };

    let mut reply = Message::empty();
    if syscall::sys_call(NAMESERVER_TID, &msg, &mut reply).is_ok() {
        println!("[disk] Registered with nameserver.");
    } else {
        println!("[disk] Failed to register with nameserver.");
    }
}

#[unsafe(no_mangle)]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    println!("[disk] Started.");

    // Register for IRQ 14 (primary ATA)
    if syscall::sys_irq_register(14).is_err() {
        println!("[disk] Failed to register IRQ 14!");
        syscall::sys_exit();
    }

    // Identify drive
    if !ata_identify() {
        println!("[disk] No usable drive found. Exiting.");
        syscall::sys_exit();
    }

    // Register with nameserver
    register_with_nameserver();

    // Service loop
    loop {
        let mut msg = Message::empty();
        if syscall::sys_recv(TID_ANY, &mut msg).is_err() {
            continue;
        }

        if msg.sender == 0 {
            // IRQ 14 notification — just ACK it
            syscall::sys_irq_ack(14);
            continue;
        }

        match msg.tag {
            TAG_READ_SECTOR => {
                let lba = msg.data[0] as u32;
                let phys_addr = msg.data[1] as usize;

                // Map the client's physical page at our temp address
                if syscall::sys_map_phys(phys_addr, TEMP_MAP_ADDR, 1).is_err() {
                    let reply = Message {
                        sender: 0,
                        tag: TAG_ERROR,
                        data: [1, 0, 0, 0, 0, 0], // map error
                    };
                    let _ = syscall::sys_reply(msg.sender, &reply);
                    continue;
                }

                let success = ata_read_sector(lba, TEMP_MAP_ADDR as *mut u8);

                let reply = if success {
                    Message {
                        sender: 0,
                        tag: TAG_OK,
                        data: [512, 0, 0, 0, 0, 0],
                    }
                } else {
                    Message {
                        sender: 0,
                        tag: TAG_ERROR,
                        data: [2, 0, 0, 0, 0, 0], // read error
                    }
                };
                let _ = syscall::sys_reply(msg.sender, &reply);
            }
            TAG_WRITE_SECTOR => {
                let lba = msg.data[0] as u32;
                let phys_addr = msg.data[1] as usize;

                // Map the client's physical page at our temp address
                if syscall::sys_map_phys(phys_addr, TEMP_MAP_ADDR, 1).is_err() {
                    let reply = Message {
                        sender: 0,
                        tag: TAG_ERROR,
                        data: [1, 0, 0, 0, 0, 0],
                    };
                    let _ = syscall::sys_reply(msg.sender, &reply);
                    continue;
                }

                let success = ata_write_sector(lba, TEMP_MAP_ADDR as *const u8);

                let reply = if success {
                    Message {
                        sender: 0,
                        tag: TAG_OK,
                        data: [512, 0, 0, 0, 0, 0],
                    }
                } else {
                    Message {
                        sender: 0,
                        tag: TAG_ERROR,
                        data: [3, 0, 0, 0, 0, 0], // write error
                    }
                };
                let _ = syscall::sys_reply(msg.sender, &reply);
            }
            TAG_READ_SECTORS => {
                let lba = msg.data[0] as u32;
                let phys_addr = msg.data[1] as usize;
                let count = (msg.data[2] as u32).min(8).max(1);

                if syscall::sys_map_phys(phys_addr, TEMP_MAP_ADDR, 1).is_err() {
                    let reply = Message {
                        sender: 0,
                        tag: TAG_ERROR,
                        data: [1, 0, 0, 0, 0, 0],
                    };
                    let _ = syscall::sys_reply(msg.sender, &reply);
                    continue;
                }

                let success = ata_read_sectors(lba, count, TEMP_MAP_ADDR as *mut u8);

                let reply = if success {
                    Message {
                        sender: 0,
                        tag: TAG_OK,
                        data: [(count * 512) as u64, 0, 0, 0, 0, 0],
                    }
                } else {
                    Message {
                        sender: 0,
                        tag: TAG_ERROR,
                        data: [2, 0, 0, 0, 0, 0],
                    }
                };
                let _ = syscall::sys_reply(msg.sender, &reply);
            }
            TAG_DISK_INFO => {
                let sectors = unsafe { DRIVE.lba28_sectors };
                let reply = Message {
                    sender: 0,
                    tag: TAG_OK,
                    data: [sectors as u64, 512, 0, 0, 0, 0],
                };
                let _ = syscall::sys_reply(msg.sender, &reply);
            }
            _ => {
                let reply = Message {
                    sender: 0,
                    tag: TAG_ERROR,
                    data: [0xFF, 0, 0, 0, 0, 0], // unknown tag
                };
                let _ = syscall::sys_reply(msg.sender, &reply);
            }
        }
    }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("[disk] PANIC: {}", info);
    loop {
        core::hint::spin_loop();
    }
}
