#![no_std]
#![no_main]
#![allow(dead_code)]

use libquark::ipc::{Message, TID_ANY};
use libquark::{println, syscall};

// ---------------------------------------------------------------------------
// IPC protocol
// ---------------------------------------------------------------------------

const NAMESERVER_TID: usize = 2;
const TAG_NS_REGISTER: u64 = 1;

const TAG_UDP_SEND: u64 = 1;
const TAG_UDP_RECV: u64 = 2;
const TAG_NET_CONFIG: u64 = 3;
const TAG_NET_INFO: u64 = 4;
const TAG_OK: u64 = 0;
const TAG_ERROR: u64 = u64::MAX;

// ---------------------------------------------------------------------------
// PCI config space
// ---------------------------------------------------------------------------

const PCI_CONFIG_ADDR: u16 = 0xCF8;
const PCI_CONFIG_DATA: u16 = 0xCFC;
const RTL8139_VENDOR: u16 = 0x10EC;
const RTL8139_DEVICE: u16 = 0x8139;

fn pci_read32(bus: u8, device: u8, func: u8, offset: u8) -> u32 {
    let addr = 0x8000_0000u32
        | ((bus as u32) << 16)
        | ((device as u32) << 11)
        | ((func as u32) << 8)
        | ((offset as u32) & 0xFC);
    syscall::sys_ioport_write32(PCI_CONFIG_ADDR, addr);
    syscall::sys_ioport_read32(PCI_CONFIG_DATA)
}

fn pci_write32(bus: u8, device: u8, func: u8, offset: u8, value: u32) {
    let addr = 0x8000_0000u32
        | ((bus as u32) << 16)
        | ((device as u32) << 11)
        | ((func as u32) << 8)
        | ((offset as u32) & 0xFC);
    syscall::sys_ioport_write32(PCI_CONFIG_ADDR, addr);
    syscall::sys_ioport_write32(PCI_CONFIG_DATA, value);
}

fn pci_find_rtl8139() -> Option<(u16, u8)> {
    for bus in 0..8u8 {
        for device in 0..32u8 {
            let id = pci_read32(bus, device, 0, 0);
            let vendor = (id & 0xFFFF) as u16;
            let dev_id = ((id >> 16) & 0xFFFF) as u16;
            if vendor == RTL8139_VENDOR && dev_id == RTL8139_DEVICE {
                let bar0 = pci_read32(bus, device, 0, 0x10);
                let irq_reg = pci_read32(bus, device, 0, 0x3C);
                let irq_line = (irq_reg & 0xFF) as u8;
                let io_base = (bar0 & 0xFFFC) as u16;

                // Enable bus mastering (PCI command register bit 2)
                let cmd = pci_read32(bus, device, 0, 0x04);
                pci_write32(bus, device, 0, 0x04, cmd | 0x0005); // I/O + bus master

                return Some((io_base, irq_line));
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Port I/O helpers
// ---------------------------------------------------------------------------

fn inb(port: u16) -> u8 { syscall::sys_ioport_read(port) as u8 }
fn inw(port: u16) -> u16 { syscall::sys_ioport_read16(port) }
fn inl(port: u16) -> u32 { syscall::sys_ioport_read32(port) }
fn outb(port: u16, val: u8) { syscall::sys_ioport_write(port, val) }
fn outw(port: u16, val: u16) { syscall::sys_ioport_write16(port, val) }
fn outl(port: u16, val: u32) { syscall::sys_ioport_write32(port, val) }

// ---------------------------------------------------------------------------
// RTL8139 registers and constants
// ---------------------------------------------------------------------------

const REG_IDR: u16 = 0x00;     // MAC address (6 bytes)
const REG_TSD0: u16 = 0x10;    // TX status descriptor 0
const REG_TSAD0: u16 = 0x20;   // TX start address descriptor 0
const REG_RBSTART: u16 = 0x30; // RX buffer start (physical)
const REG_CR: u16 = 0x37;      // Command register
const REG_CAPR: u16 = 0x38;    // Current address of packet read
const REG_IMR: u16 = 0x3C;     // Interrupt mask
const REG_ISR: u16 = 0x3E;     // Interrupt status
const REG_TCR: u16 = 0x40;     // TX config
const REG_RCR: u16 = 0x44;     // RX config
const REG_CONFIG1: u16 = 0x52; // Config register 1

const CR_RST: u8 = 0x10;
const CR_RE: u8 = 0x08;
const CR_TE: u8 = 0x04;
const CR_BUFE: u8 = 0x01;

const ISR_ROK: u16 = 0x0001;
const ISR_TOK: u16 = 0x0004;

// RCR: accept physical match + multicast + broadcast, wrap, 8K buf, max DMA
const RCR_VALUE: u32 = 0x0000_E78E;

// TCR: standard IFG, max DMA burst
const TCR_VALUE: u32 = 0x0300_0700;

// ---------------------------------------------------------------------------
// Buffer layout
// ---------------------------------------------------------------------------

const RX_BUF_PAGES: usize = 3;     // 12K > 8K + 16 + 1500 wrap pad
const NUM_TX_DESC: usize = 4;
const MAX_PKT: usize = 1536;

const RX_BUF_VADDR: usize = 0x89_0000_0000;
const TX_BUF_VADDR: usize = 0x89_0010_0000;
const CLIENT_BUF: usize = 0x8A_0000_0000;

// ---------------------------------------------------------------------------
// Protocol constants
// ---------------------------------------------------------------------------

const ETH_HLEN: usize = 14;
const IP_HLEN: usize = 20;
const UDP_HLEN: usize = 8;

const ETHERTYPE_IP: u16 = 0x0800;
const ETHERTYPE_ARP: u16 = 0x0806;

const IP_PROTO_ICMP: u8 = 1;
const IP_PROTO_UDP: u8 = 17;

const ICMP_ECHO_REQUEST: u8 = 8;
const ICMP_ECHO_REPLY: u8 = 0;

const ARP_REQUEST: u16 = 1;
const ARP_REPLY: u16 = 2;
const ARP_HLEN: usize = 28;

const BROADCAST_MAC: [u8; 6] = [0xFF; 6];

// Default config for QEMU user-mode networking
const DEFAULT_IP: [u8; 4] = [10, 0, 2, 15];
const DEFAULT_GATEWAY: [u8; 4] = [10, 0, 2, 2];
const DEFAULT_NETMASK: [u8; 4] = [255, 255, 255, 0];

// ---------------------------------------------------------------------------
// Driver state
// ---------------------------------------------------------------------------

struct ArpEntry {
    ip: [u8; 4],
    mac: [u8; 6],
    valid: bool,
}

struct UdpReader {
    tid: usize,
    phys_addr: usize,
    max_len: usize,
    port: u16,
}

struct NetState {
    io_base: u16,
    irq: u8,
    mac: [u8; 6],
    ip: [u8; 4],
    netmask: [u8; 4],
    gateway: [u8; 4],
    rx_offset: usize,
    tx_cur: usize,
    rx_phys: usize,
    tx_phys: [usize; NUM_TX_DESC],
    arp_cache: [ArpEntry; 8],
    pending_udp: Option<UdpReader>,
    ip_id: u16,
}

static mut NET: NetState = NetState {
    io_base: 0,
    irq: 0,
    mac: [0; 6],
    ip: DEFAULT_IP,
    netmask: DEFAULT_NETMASK,
    gateway: DEFAULT_GATEWAY,
    rx_offset: 0,
    tx_cur: 0,
    rx_phys: 0,
    tx_phys: [0; NUM_TX_DESC],
    arp_cache: {
        const EMPTY: ArpEntry = ArpEntry { ip: [0; 4], mac: [0; 6], valid: false };
        [EMPTY; 8]
    },
    pending_udp: None,
    ip_id: 0,
};

// ---------------------------------------------------------------------------
// DMA buffer allocation
// ---------------------------------------------------------------------------

fn alloc_dma_pages(count: usize, vaddr: usize) -> usize {
    let first = match syscall::sys_phys_alloc(1) {
        Ok(f) => f,
        Err(()) => return 0,
    };
    if syscall::sys_map_phys(first, vaddr, 1).is_err() { return 0; }
    unsafe { core::ptr::write_bytes(vaddr as *mut u8, 0, 4096); }

    for i in 1..count {
        let frame = match syscall::sys_phys_alloc(1) {
            Ok(f) => f,
            Err(()) => return 0,
        };
        if frame != first + i * 4096 {
            println!("[net] WARNING: DMA pages not contiguous");
        }
        if syscall::sys_map_phys(frame, vaddr + i * 4096, 1).is_err() { return 0; }
        unsafe { core::ptr::write_bytes((vaddr + i * 4096) as *mut u8, 0, 4096); }
    }

    first
}

// ---------------------------------------------------------------------------
// RTL8139 initialization
// ---------------------------------------------------------------------------

fn rtl8139_init() -> bool {
    let (io_base, irq) = match pci_find_rtl8139() {
        Some(x) => x,
        None => {
            println!("[net] RTL8139 not found on PCI bus.");
            return false;
        }
    };

    println!("[net] RTL8139 at I/O {:#x}, IRQ {}", io_base, irq);

    unsafe {
        NET.io_base = io_base;
        NET.irq = irq;
    }

    // Power on
    outb(io_base + REG_CONFIG1, 0x00);

    // Software reset
    outb(io_base + REG_CR, CR_RST);
    for _ in 0..10000 {
        if inb(io_base + REG_CR) & CR_RST == 0 { break; }
        syscall::sys_yield();
    }

    // Read MAC address
    unsafe {
        for i in 0..6 {
            NET.mac[i] = inb(io_base + REG_IDR + i as u16);
        }
        println!("[net] MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            NET.mac[0], NET.mac[1], NET.mac[2], NET.mac[3], NET.mac[4], NET.mac[5]);
    }

    // Allocate RX ring buffer
    let rx_phys = alloc_dma_pages(RX_BUF_PAGES, RX_BUF_VADDR);
    if rx_phys == 0 {
        println!("[net] Failed to allocate RX buffer.");
        return false;
    }
    unsafe { NET.rx_phys = rx_phys; }

    // Allocate TX buffers (one page per descriptor)
    for i in 0..NUM_TX_DESC {
        let phys = alloc_dma_pages(1, TX_BUF_VADDR + i * 4096);
        if phys == 0 {
            println!("[net] Failed to allocate TX buffer {}.", i);
            return false;
        }
        unsafe { NET.tx_phys[i] = phys; }
    }

    // Configure NIC
    outl(io_base + REG_RBSTART, rx_phys as u32);
    outw(io_base + REG_IMR, ISR_ROK | ISR_TOK);
    outl(io_base + REG_RCR, RCR_VALUE);
    outl(io_base + REG_TCR, TCR_VALUE);
    outb(io_base + REG_CR, CR_RE | CR_TE);
    outw(io_base + REG_CAPR, 0xFFF0);

    println!("[net] RTL8139 initialized.");
    true
}

// ---------------------------------------------------------------------------
// Raw packet TX/RX
// ---------------------------------------------------------------------------

fn send_raw(data: &[u8]) {
    if data.len() > MAX_PKT { return; }

    let desc = unsafe { NET.tx_cur };
    let io_base = unsafe { NET.io_base };
    let tx_vaddr = TX_BUF_VADDR + desc * 4096;
    let tx_phys = unsafe { NET.tx_phys[desc] };

    unsafe {
        core::ptr::copy_nonoverlapping(data.as_ptr(), tx_vaddr as *mut u8, data.len());
    }

    outl(io_base + REG_TSAD0 + (desc as u16 * 4), tx_phys as u32);
    outl(io_base + REG_TSD0 + (desc as u16 * 4), data.len() as u32);

    unsafe { NET.tx_cur = (desc + 1) % NUM_TX_DESC; }
}

fn process_rx() {
    let io_base = unsafe { NET.io_base };

    loop {
        let cmd = inb(io_base + REG_CR);
        if cmd & CR_BUFE != 0 { break; }

        let offset = unsafe { NET.rx_offset };
        let header = unsafe { *((RX_BUF_VADDR + offset) as *const u32) };
        let status = (header & 0xFFFF) as u16;
        let length = ((header >> 16) & 0xFFFF) as usize;

        if status & 0x0001 == 0 || length == 0 || length > MAX_PKT + 4 {
            // Error or invalid — skip
            break;
        }

        let pkt_len = length - 4; // subtract CRC
        let pkt_start = RX_BUF_VADDR + offset + 4;
        let pkt = unsafe { core::slice::from_raw_parts(pkt_start as *const u8, pkt_len) };

        handle_packet(pkt);

        // Advance (4-byte aligned): header(4) + length bytes
        let advance = (4 + length + 3) & !3;
        let new_offset = (offset + advance) % 8192;
        unsafe { NET.rx_offset = new_offset; }
        outw(io_base + REG_CAPR, new_offset.wrapping_sub(16) as u16);
    }
}

// ---------------------------------------------------------------------------
// Internet checksum
// ---------------------------------------------------------------------------

fn checksum(data: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    let mut i = 0;
    while i + 1 < data.len() {
        sum += u16::from_be_bytes([data[i], data[i + 1]]) as u32;
        i += 2;
    }
    if i < data.len() {
        sum += (data[i] as u32) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

// ---------------------------------------------------------------------------
// IP header construction
// ---------------------------------------------------------------------------

fn build_ip_header(buf: &mut [u8], src: &[u8; 4], dst: &[u8; 4], proto: u8, payload_len: u16) {
    let total_len = IP_HLEN as u16 + payload_len;
    buf[0] = 0x45; // version=4, IHL=5
    buf[1] = 0;
    buf[2..4].copy_from_slice(&total_len.to_be_bytes());
    let id = unsafe { NET.ip_id };
    buf[4..6].copy_from_slice(&id.to_be_bytes());
    unsafe { NET.ip_id = id.wrapping_add(1); }
    buf[6] = 0x40; // DF
    buf[7] = 0;
    buf[8] = 64; // TTL
    buf[9] = proto;
    buf[10] = 0; // checksum placeholder
    buf[11] = 0;
    buf[12..16].copy_from_slice(src);
    buf[16..20].copy_from_slice(dst);

    let cksum = checksum(&buf[..IP_HLEN]);
    buf[10..12].copy_from_slice(&cksum.to_be_bytes());
}

// ---------------------------------------------------------------------------
// ARP
// ---------------------------------------------------------------------------

fn arp_cache_update(ip: &[u8; 4], mac: &[u8; 6]) {
    unsafe {
        for e in NET.arp_cache.iter_mut() {
            if e.valid && e.ip == *ip {
                e.mac = *mac;
                return;
            }
        }
        for e in NET.arp_cache.iter_mut() {
            if !e.valid {
                e.ip = *ip;
                e.mac = *mac;
                e.valid = true;
                return;
            }
        }
        // Full — overwrite first
        NET.arp_cache[0] = ArpEntry { ip: *ip, mac: *mac, valid: true };
    }
}

fn arp_lookup(ip: &[u8; 4]) -> Option<[u8; 6]> {
    unsafe {
        for e in NET.arp_cache.iter() {
            if e.valid && e.ip == *ip {
                return Some(e.mac);
            }
        }
    }
    None
}

fn send_arp(op: u16, target_ip: &[u8; 4], target_mac: &[u8; 6]) {
    let mac = unsafe { NET.mac };
    let ip = unsafe { NET.ip };
    let dst_eth = if op == ARP_REQUEST { BROADCAST_MAC } else { *target_mac };

    let mut pkt = [0u8; ETH_HLEN + ARP_HLEN];

    // Ethernet header
    pkt[0..6].copy_from_slice(&dst_eth);
    pkt[6..12].copy_from_slice(&mac);
    pkt[12..14].copy_from_slice(&ETHERTYPE_ARP.to_be_bytes());

    // ARP body
    let a = &mut pkt[ETH_HLEN..];
    a[0..2].copy_from_slice(&1u16.to_be_bytes()); // hardware = Ethernet
    a[2..4].copy_from_slice(&0x0800u16.to_be_bytes()); // protocol = IPv4
    a[4] = 6; // hw addr len
    a[5] = 4; // proto addr len
    a[6..8].copy_from_slice(&op.to_be_bytes());
    a[8..14].copy_from_slice(&mac);
    a[14..18].copy_from_slice(&ip);
    a[18..24].copy_from_slice(target_mac);
    a[24..28].copy_from_slice(target_ip);

    send_raw(&pkt);
}

fn handle_arp(data: &[u8]) {
    if data.len() < ARP_HLEN { return; }

    let op = u16::from_be_bytes([data[6], data[7]]);
    let sender_mac: [u8; 6] = data[8..14].try_into().unwrap();
    let sender_ip: [u8; 4] = data[14..18].try_into().unwrap();
    let target_ip: [u8; 4] = data[24..28].try_into().unwrap();

    arp_cache_update(&sender_ip, &sender_mac);

    if op == ARP_REQUEST && target_ip == unsafe { NET.ip } {
        send_arp(ARP_REPLY, &sender_ip, &sender_mac);
    }
}

/// Resolve destination MAC, routing through gateway if needed.
fn resolve_mac(dst_ip: &[u8; 4]) -> Option<[u8; 6]> {
    let next_hop = if is_same_subnet(dst_ip) {
        *dst_ip
    } else {
        unsafe { NET.gateway }
    };
    match arp_lookup(&next_hop) {
        Some(m) => Some(m),
        None => {
            send_arp(ARP_REQUEST, &next_hop, &[0; 6]);
            None
        }
    }
}

fn is_same_subnet(ip: &[u8; 4]) -> bool {
    let mask = unsafe { NET.netmask };
    let my_ip = unsafe { NET.ip };
    (0..4).all(|i| (ip[i] & mask[i]) == (my_ip[i] & mask[i]))
}

// ---------------------------------------------------------------------------
// ICMP
// ---------------------------------------------------------------------------

fn handle_icmp(data: &[u8], src_ip: &[u8; 4]) {
    if data.len() < 8 { return; }
    if data[0] == ICMP_ECHO_REQUEST && data[1] == 0 {
        send_icmp_echo_reply(src_ip, data);
    }
}

fn send_icmp_echo_reply(dst_ip: &[u8; 4], request: &[u8]) {
    let mac = unsafe { NET.mac };
    let ip = unsafe { NET.ip };

    let dst_mac = match resolve_mac(dst_ip) {
        Some(m) => m,
        None => return, // ARP pending, drop this reply
    };

    let total = ETH_HLEN + IP_HLEN + request.len();
    if total > MAX_PKT { return; }

    let mut pkt = [0u8; MAX_PKT];

    // Ethernet
    pkt[0..6].copy_from_slice(&dst_mac);
    pkt[6..12].copy_from_slice(&mac);
    pkt[12..14].copy_from_slice(&ETHERTYPE_IP.to_be_bytes());

    // IP
    build_ip_header(&mut pkt[ETH_HLEN..], &ip, dst_ip, IP_PROTO_ICMP, request.len() as u16);

    // ICMP: copy request, change type to reply, fix checksum
    let off = ETH_HLEN + IP_HLEN;
    pkt[off..off + request.len()].copy_from_slice(request);
    pkt[off] = ICMP_ECHO_REPLY;
    pkt[off + 2] = 0; // clear checksum
    pkt[off + 3] = 0;
    let cksum = checksum(&pkt[off..off + request.len()]);
    pkt[off + 2..off + 4].copy_from_slice(&cksum.to_be_bytes());

    send_raw(&pkt[..total]);
}

// ---------------------------------------------------------------------------
// IP
// ---------------------------------------------------------------------------

fn handle_ip(data: &[u8]) {
    if data.len() < IP_HLEN { return; }
    if data[0] >> 4 != 4 { return; }

    let ihl = ((data[0] & 0x0F) as usize) * 4;
    let total_len = u16::from_be_bytes([data[2], data[3]]) as usize;
    if data.len() < total_len || total_len < ihl { return; }

    let proto = data[9];
    let src_ip: [u8; 4] = data[12..16].try_into().unwrap();
    let dst_ip: [u8; 4] = data[16..20].try_into().unwrap();

    if dst_ip != unsafe { NET.ip } && dst_ip != [255, 255, 255, 255] {
        return;
    }

    let payload = &data[ihl..total_len];
    match proto {
        IP_PROTO_ICMP => handle_icmp(payload, &src_ip),
        IP_PROTO_UDP => handle_udp(payload, &src_ip),
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// UDP
// ---------------------------------------------------------------------------

fn handle_udp(data: &[u8], src_ip: &[u8; 4]) {
    if data.len() < UDP_HLEN { return; }

    let src_port = u16::from_be_bytes([data[0], data[1]]);
    let dst_port = u16::from_be_bytes([data[2], data[3]]);
    let udp_len = u16::from_be_bytes([data[4], data[5]]) as usize;
    if udp_len < UDP_HLEN || data.len() < udp_len { return; }

    let payload = &data[UDP_HLEN..udp_len];

    // Deliver to pending reader if port matches
    unsafe {
        if let Some(ref reader) = NET.pending_udp {
            if reader.port == dst_port || reader.port == 0 {
                let copy_len = payload.len().min(reader.max_len);

                if syscall::sys_map_phys(reader.phys_addr, CLIENT_BUF, 1).is_ok() {
                    core::ptr::copy_nonoverlapping(
                        payload.as_ptr(),
                        CLIENT_BUF as *mut u8,
                        copy_len,
                    );

                    let ip_packed = u32::from_be_bytes(*src_ip) as u64;
                    let reply = Message {
                        sender: 0,
                        tag: TAG_OK,
                        data: [
                            copy_len as u64,
                            ip_packed,
                            ((src_port as u64) << 16) | (dst_port as u64),
                            0, 0, 0,
                        ],
                    };
                    let _ = syscall::sys_reply(reader.tid, &reply);
                    NET.pending_udp = None;
                }
            }
        }
    }
}

fn send_udp_packet(dst_ip: &[u8; 4], src_port: u16, dst_port: u16, payload: &[u8]) -> bool {
    let mac = unsafe { NET.mac };
    let ip = unsafe { NET.ip };

    let dst_mac = match resolve_mac(dst_ip) {
        Some(m) => m,
        None => return false,
    };

    let udp_len = UDP_HLEN + payload.len();
    let total = ETH_HLEN + IP_HLEN + udp_len;
    if total > MAX_PKT { return false; }

    let mut pkt = [0u8; MAX_PKT];

    // Ethernet
    pkt[0..6].copy_from_slice(&dst_mac);
    pkt[6..12].copy_from_slice(&mac);
    pkt[12..14].copy_from_slice(&ETHERTYPE_IP.to_be_bytes());

    // IP
    build_ip_header(&mut pkt[ETH_HLEN..], &ip, dst_ip, IP_PROTO_UDP, udp_len as u16);

    // UDP
    let off = ETH_HLEN + IP_HLEN;
    pkt[off..off + 2].copy_from_slice(&src_port.to_be_bytes());
    pkt[off + 2..off + 4].copy_from_slice(&dst_port.to_be_bytes());
    pkt[off + 4..off + 6].copy_from_slice(&(udp_len as u16).to_be_bytes());
    pkt[off + 6..off + 8].copy_from_slice(&[0, 0]); // checksum optional for IPv4 UDP
    pkt[off + 8..off + 8 + payload.len()].copy_from_slice(payload);

    send_raw(&pkt[..total]);
    true
}

// ---------------------------------------------------------------------------
// Packet dispatch
// ---------------------------------------------------------------------------

fn handle_packet(pkt: &[u8]) {
    if pkt.len() < ETH_HLEN { return; }

    let ethertype = u16::from_be_bytes([pkt[12], pkt[13]]);
    let payload = &pkt[ETH_HLEN..];

    match ethertype {
        ETHERTYPE_ARP => handle_arp(payload),
        ETHERTYPE_IP => handle_ip(payload),
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Nameserver registration
// ---------------------------------------------------------------------------

fn register_with_nameserver() {
    let name = b"net";
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
        println!("[net] Registered with nameserver.");
    } else {
        println!("[net] Failed to register with nameserver.");
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    println!("[net] Started.");

    if !rtl8139_init() {
        println!("[net] No NIC. Exiting.");
        syscall::sys_exit();
    }

    let irq = unsafe { NET.irq };
    if syscall::sys_irq_register(irq).is_err() {
        println!("[net] Failed to register IRQ {}!", irq);
        syscall::sys_exit();
    }

    register_with_nameserver();

    let ip = unsafe { NET.ip };
    println!("[net] IP {}.{}.{}.{} — ready.", ip[0], ip[1], ip[2], ip[3]);

    // Announce ourselves via gratuitous ARP
    send_arp(ARP_REQUEST, &ip, &[0; 6]);

    // Service loop
    loop {
        let mut msg = Message::empty();
        if syscall::sys_recv(TID_ANY, &mut msg).is_err() {
            continue;
        }

        if msg.sender == 0 {
            // IRQ or kernel notification
            let io_base = unsafe { NET.io_base };
            let isr = inw(io_base + REG_ISR);
            if isr != 0 {
                outw(io_base + REG_ISR, isr); // acknowledge
                if isr & ISR_ROK != 0 {
                    process_rx();
                }
            }
            syscall::sys_irq_ack(irq);
            continue;
        }

        // IPC request from client
        match msg.tag {
            TAG_UDP_SEND => {
                let phys_addr = msg.data[0] as usize;
                let len = msg.data[1] as usize;
                let dst_ip = (msg.data[2] as u32).to_be_bytes();
                let ports = msg.data[3];
                let dst_port = (ports >> 16) as u16;
                let src_port = (ports & 0xFFFF) as u16;

                let ok = if len > 0 && syscall::sys_map_phys(phys_addr, CLIENT_BUF, 1).is_ok() {
                    let payload = unsafe {
                        core::slice::from_raw_parts(CLIENT_BUF as *const u8, len.min(1472))
                    };
                    send_udp_packet(&dst_ip, src_port, dst_port, payload)
                } else {
                    false
                };

                let reply = if ok {
                    Message { sender: 0, tag: TAG_OK, data: [0; 6] }
                } else {
                    Message { sender: 0, tag: TAG_ERROR, data: [1, 0, 0, 0, 0, 0] }
                };
                let _ = syscall::sys_reply(msg.sender, &reply);
            }
            TAG_UDP_RECV => {
                // Store pending reader — reply deferred until UDP data arrives
                let phys_addr = msg.data[0] as usize;
                let max_len = msg.data[1] as usize;
                let port = msg.data[2] as u16;
                unsafe {
                    NET.pending_udp = Some(UdpReader { tid: msg.sender, phys_addr, max_len, port });
                }
            }
            TAG_NET_CONFIG => {
                let new_ip = (msg.data[0] as u32).to_be_bytes();
                let new_mask = (msg.data[1] as u32).to_be_bytes();
                let new_gw = (msg.data[2] as u32).to_be_bytes();
                unsafe {
                    NET.ip = new_ip;
                    NET.netmask = new_mask;
                    NET.gateway = new_gw;
                }
                println!("[net] Config: {}.{}.{}.{}", new_ip[0], new_ip[1], new_ip[2], new_ip[3]);
                let reply = Message { sender: 0, tag: TAG_OK, data: [0; 6] };
                let _ = syscall::sys_reply(msg.sender, &reply);
            }
            TAG_NET_INFO => {
                let mac = unsafe { NET.mac };
                let ip = unsafe { NET.ip };
                let mac_packed = (mac[0] as u64)
                    | ((mac[1] as u64) << 8)
                    | ((mac[2] as u64) << 16)
                    | ((mac[3] as u64) << 24)
                    | ((mac[4] as u64) << 32)
                    | ((mac[5] as u64) << 40);
                let ip_packed = u32::from_be_bytes(ip) as u64;
                let reply = Message {
                    sender: 0,
                    tag: TAG_OK,
                    data: [mac_packed, ip_packed, 0, 0, 0, 0],
                };
                let _ = syscall::sys_reply(msg.sender, &reply);
            }
            _ => {
                let reply = Message { sender: 0, tag: TAG_ERROR, data: [0xFF, 0, 0, 0, 0, 0] };
                let _ = syscall::sys_reply(msg.sender, &reply);
            }
        }
    }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("[net] PANIC: {}", info);
    loop { core::hint::spin_loop(); }
}
