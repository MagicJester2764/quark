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
const TAG_ICMP_PING: u64 = 5;
const TAG_TCP_CONNECT: u64 = 10;
const TAG_TCP_LISTEN: u64 = 11;
const TAG_TCP_SEND: u64 = 13;
const TAG_TCP_RECV: u64 = 14;
const TAG_TCP_CLOSE: u64 = 15;
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
const IP_PROTO_TCP: u8 = 6;
const IP_PROTO_UDP: u8 = 17;

const ICMP_ECHO_REQUEST: u8 = 8;
const ICMP_ECHO_REPLY: u8 = 0;

const ARP_REQUEST: u16 = 1;
const ARP_REPLY: u16 = 2;
const ARP_HLEN: usize = 28;

const BROADCAST_MAC: [u8; 6] = [0xFF; 6];

// TCP
const TCP_HLEN: usize = 20;
const TCP_MSS: usize = 1460;
const TCP_FIN: u8 = 0x01;
const TCP_SYN: u8 = 0x02;
const TCP_RST: u8 = 0x04;
const TCP_PSH: u8 = 0x08;
const TCP_ACK: u8 = 0x10;
const TCP_RETRANSMIT_TICKS: u64 = 300;
const TCP_TIMEWAIT_TICKS: u64 = 100;
const MAX_TCP_CONNS: usize = 8;
const TCP_BUF_SIZE: usize = 4096;
const TCP_RECV_BUF_BASE: usize = 0x8B_0000_0000;
const TCP_SEND_BUF_BASE: usize = 0x8B_0010_0000;

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

struct PendingIcmp {
    tid: usize,
    id: u16,
    seq: u16,
    send_tick: u64,
}

#[derive(Clone, Copy, PartialEq)]
enum TcpState {
    Closed,
    Listen,
    SynSent,
    SynReceived,
    Established,
    FinWait1,
    FinWait2,
    CloseWait,
    LastAck,
    TimeWait,
}

const TCP_PENDING_NONE: u8 = 0;
const TCP_PENDING_CONNECT: u8 = 1;
const TCP_PENDING_ACCEPT: u8 = 2;
const TCP_PENDING_RECV: u8 = 3;

struct TcpConn {
    state: TcpState,
    local_port: u16,
    remote_ip: [u8; 4],
    remote_port: u16,
    snd_una: u32,
    snd_nxt: u32,
    rcv_nxt: u32,
    snd_wnd: u16,
    recv_len: usize,
    send_len: usize,
    retransmit_tick: u64,
    timewait_tick: u64,
    pending_tid: usize,
    pending_op: u8,
    pending_phys: usize,
    pending_max: usize,
    in_use: bool,
    fin_received: bool,
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
    pending_icmp: Option<PendingIcmp>,
    ip_id: u16,
    tcp_conns: [TcpConn; MAX_TCP_CONNS],
    next_ephemeral_port: u16,
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
    pending_icmp: None,
    ip_id: 0,
    tcp_conns: {
        const EMPTY: TcpConn = TcpConn {
            state: TcpState::Closed, local_port: 0, remote_ip: [0; 4], remote_port: 0,
            snd_una: 0, snd_nxt: 0, rcv_nxt: 0, snd_wnd: 0,
            recv_len: 0, send_len: 0, retransmit_tick: 0, timewait_tick: 0,
            pending_tid: 0, pending_op: TCP_PENDING_NONE,
            pending_phys: 0, pending_max: 0, in_use: false, fin_received: false,
        };
        [EMPTY; MAX_TCP_CONNS]
    },
    next_ephemeral_port: 49152,
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

fn handle_icmp(data: &[u8], src_ip: &[u8; 4], ttl: u8) {
    if data.len() < 8 { return; }
    if data[0] == ICMP_ECHO_REQUEST && data[1] == 0 {
        send_icmp_echo_reply(src_ip, data);
    } else if data[0] == ICMP_ECHO_REPLY && data[1] == 0 {
        let id = u16::from_be_bytes([data[4], data[5]]);
        let seq = u16::from_be_bytes([data[6], data[7]]);
        unsafe {
            if let Some(ref pending) = NET.pending_icmp {
                if pending.id == id && pending.seq == seq {
                    let rtt = syscall::sys_ticks() - pending.send_tick;
                    let reply = Message {
                        sender: 0,
                        tag: TAG_OK,
                        data: [rtt, ttl as u64, data.len() as u64, 0, 0, 0],
                    };
                    let _ = syscall::sys_reply(pending.tid, &reply);
                    NET.pending_icmp = None;
                }
            }
        }
    }
}

fn send_icmp_echo_request(dst_ip: &[u8; 4], id: u16, seq: u16) -> bool {
    let mac = unsafe { NET.mac };
    let ip = unsafe { NET.ip };

    let dst_mac = match resolve_mac(dst_ip) {
        Some(m) => m,
        None => return false,
    };

    // ICMP echo request: type(1) + code(1) + checksum(2) + id(2) + seq(2) + 32 bytes payload
    const ICMP_LEN: usize = 40;
    let total = ETH_HLEN + IP_HLEN + ICMP_LEN;

    let mut pkt = [0u8; MAX_PKT];

    // Ethernet
    pkt[0..6].copy_from_slice(&dst_mac);
    pkt[6..12].copy_from_slice(&mac);
    pkt[12..14].copy_from_slice(&ETHERTYPE_IP.to_be_bytes());

    // IP
    build_ip_header(&mut pkt[ETH_HLEN..], &ip, dst_ip, IP_PROTO_ICMP, ICMP_LEN as u16);

    // ICMP
    let off = ETH_HLEN + IP_HLEN;
    pkt[off] = ICMP_ECHO_REQUEST;
    pkt[off + 1] = 0; // code
    pkt[off + 2] = 0; // checksum placeholder
    pkt[off + 3] = 0;
    pkt[off + 4..off + 6].copy_from_slice(&id.to_be_bytes());
    pkt[off + 6..off + 8].copy_from_slice(&seq.to_be_bytes());
    // Fill payload with sequence pattern
    for i in 0..32 {
        pkt[off + 8 + i] = i as u8;
    }
    let cksum = checksum(&pkt[off..off + ICMP_LEN]);
    pkt[off + 2..off + 4].copy_from_slice(&cksum.to_be_bytes());

    send_raw(&pkt[..total]);
    true
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

    let ttl = data[8];
    let proto = data[9];
    let src_ip: [u8; 4] = data[12..16].try_into().unwrap();
    let dst_ip: [u8; 4] = data[16..20].try_into().unwrap();

    if dst_ip != unsafe { NET.ip } && dst_ip != [255, 255, 255, 255] {
        return;
    }

    let payload = &data[ihl..total_len];
    match proto {
        IP_PROTO_ICMP => handle_icmp(payload, &src_ip, ttl),
        IP_PROTO_TCP => handle_tcp(payload, &src_ip),
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
// TCP
// ---------------------------------------------------------------------------

fn seq_lt(a: u32, b: u32) -> bool { (a.wrapping_sub(b) as i32) < 0 }
fn seq_lte(a: u32, b: u32) -> bool { a == b || seq_lt(a, b) }

fn tcp_recv_buf_vaddr(idx: usize) -> usize { TCP_RECV_BUF_BASE + idx * 0x1000 }
fn tcp_send_buf_vaddr(idx: usize) -> usize { TCP_SEND_BUF_BASE + idx * 0x1000 }

fn tcp_checksum(src_ip: &[u8; 4], dst_ip: &[u8; 4], tcp_data: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    sum += u16::from_be_bytes([src_ip[0], src_ip[1]]) as u32;
    sum += u16::from_be_bytes([src_ip[2], src_ip[3]]) as u32;
    sum += u16::from_be_bytes([dst_ip[0], dst_ip[1]]) as u32;
    sum += u16::from_be_bytes([dst_ip[2], dst_ip[3]]) as u32;
    sum += IP_PROTO_TCP as u32;
    sum += tcp_data.len() as u32;
    let mut i = 0;
    while i + 1 < tcp_data.len() {
        sum += u16::from_be_bytes([tcp_data[i], tcp_data[i + 1]]) as u32;
        i += 2;
    }
    if i < tcp_data.len() {
        sum += (tcp_data[i] as u32) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

fn alloc_tcp_conn() -> Option<usize> {
    unsafe {
        for i in 0..MAX_TCP_CONNS {
            if !NET.tcp_conns[i].in_use {
                return Some(i);
            }
        }
    }
    None
}

fn init_tcp_buffers(idx: usize) -> bool {
    let rv = tcp_recv_buf_vaddr(idx);
    let sv = tcp_send_buf_vaddr(idx);
    // Pages may already be mapped from a previous connection — mmap is idempotent failure
    let _ = syscall::sys_mmap(rv, 1);
    let _ = syscall::sys_mmap(sv, 1);
    unsafe {
        core::ptr::write_bytes(rv as *mut u8, 0, 4096);
        core::ptr::write_bytes(sv as *mut u8, 0, 4096);
    }
    true
}

fn free_tcp_conn(idx: usize) {
    unsafe {
        NET.tcp_conns[idx].state = TcpState::Closed;
        NET.tcp_conns[idx].in_use = false;
        NET.tcp_conns[idx].recv_len = 0;
        NET.tcp_conns[idx].send_len = 0;
        NET.tcp_conns[idx].pending_op = TCP_PENDING_NONE;
        NET.tcp_conns[idx].fin_received = false;
    }
    let _ = syscall::sys_munmap(tcp_recv_buf_vaddr(idx), 1);
    let _ = syscall::sys_munmap(tcp_send_buf_vaddr(idx), 1);
}

fn alloc_ephemeral_port() -> u16 {
    unsafe {
        let port = NET.next_ephemeral_port;
        NET.next_ephemeral_port = if port >= 65534 { 49152 } else { port + 1 };
        port
    }
}

fn find_tcp_conn(remote_ip: &[u8; 4], remote_port: u16, local_port: u16) -> Option<usize> {
    unsafe {
        for i in 0..MAX_TCP_CONNS {
            let c = &NET.tcp_conns[i];
            if c.in_use && c.state != TcpState::Listen
                && c.local_port == local_port
                && c.remote_port == remote_port
                && c.remote_ip == *remote_ip
            {
                return Some(i);
            }
        }
    }
    None
}

fn find_tcp_listener(local_port: u16) -> Option<usize> {
    unsafe {
        for i in 0..MAX_TCP_CONNS {
            let c = &NET.tcp_conns[i];
            if c.in_use && c.state == TcpState::Listen && c.local_port == local_port {
                return Some(i);
            }
        }
    }
    None
}

fn send_tcp_segment(
    remote_ip: &[u8; 4],
    local_port: u16,
    remote_port: u16,
    seq: u32,
    ack: u32,
    flags: u8,
    window: u16,
    payload: &[u8],
) -> bool {
    let mac = unsafe { NET.mac };
    let ip = unsafe { NET.ip };

    let dst_mac = match resolve_mac(remote_ip) {
        Some(m) => m,
        None => return false,
    };

    let tcp_len = TCP_HLEN + payload.len();
    let total = ETH_HLEN + IP_HLEN + tcp_len;
    if total > MAX_PKT { return false; }

    let mut pkt = [0u8; MAX_PKT];

    pkt[0..6].copy_from_slice(&dst_mac);
    pkt[6..12].copy_from_slice(&mac);
    pkt[12..14].copy_from_slice(&ETHERTYPE_IP.to_be_bytes());

    build_ip_header(&mut pkt[ETH_HLEN..], &ip, remote_ip, IP_PROTO_TCP, tcp_len as u16);

    let off = ETH_HLEN + IP_HLEN;
    pkt[off..off + 2].copy_from_slice(&local_port.to_be_bytes());
    pkt[off + 2..off + 4].copy_from_slice(&remote_port.to_be_bytes());
    pkt[off + 4..off + 8].copy_from_slice(&seq.to_be_bytes());
    pkt[off + 8..off + 12].copy_from_slice(&ack.to_be_bytes());
    pkt[off + 12] = (TCP_HLEN as u8 / 4) << 4; // data offset = 5 words
    pkt[off + 13] = flags;
    pkt[off + 14..off + 16].copy_from_slice(&window.to_be_bytes());
    // checksum and urgent pointer at [16..20] are zero

    if !payload.is_empty() {
        pkt[off + TCP_HLEN..off + TCP_HLEN + payload.len()].copy_from_slice(payload);
    }

    let cksum = tcp_checksum(&ip, remote_ip, &pkt[off..off + tcp_len]);
    pkt[off + 16..off + 18].copy_from_slice(&cksum.to_be_bytes());

    send_raw(&pkt[..total]);
    true
}

fn send_tcp_rst(remote_ip: &[u8; 4], local_port: u16, remote_port: u16, seq: u32, ack: u32) {
    send_tcp_segment(remote_ip, local_port, remote_port, seq, ack, TCP_RST | TCP_ACK, 0, &[]);
}

fn tcp_conn_send_segment(idx: usize, flags: u8, payload: &[u8]) -> bool {
    let c = unsafe { &NET.tcp_conns[idx] };
    let ok = send_tcp_segment(
        &c.remote_ip, c.local_port, c.remote_port,
        c.snd_nxt, c.rcv_nxt, flags,
        (TCP_BUF_SIZE - c.recv_len) as u16, payload,
    );
    if ok && !payload.is_empty() {
        unsafe {
            NET.tcp_conns[idx].retransmit_tick = syscall::sys_ticks();
        }
    }
    ok
}

fn tcp_deliver_recv(idx: usize) {
    unsafe {
        let c = &NET.tcp_conns[idx];
        if c.pending_op != TCP_PENDING_RECV { return; }
        if c.recv_len == 0 && !c.fin_received { return; }

        let n = c.recv_len.min(c.pending_max);
        if n > 0 && syscall::sys_map_phys(c.pending_phys, CLIENT_BUF, 1).is_ok() {
            let rv = tcp_recv_buf_vaddr(idx);
            core::ptr::copy_nonoverlapping(rv as *const u8, CLIENT_BUF as *mut u8, n);
            // Compact
            if n < c.recv_len {
                core::ptr::copy((rv + n) as *const u8, rv as *mut u8, c.recv_len - n);
            }
        }

        let reply = Message {
            sender: 0,
            tag: TAG_OK,
            data: [n as u64, 0, 0, 0, 0, 0],
        };
        let _ = syscall::sys_reply(c.pending_tid, &reply);
        NET.tcp_conns[idx].recv_len -= n;
        NET.tcp_conns[idx].pending_op = TCP_PENDING_NONE;
    }
}

fn tcp_notify_error(idx: usize, code: u64) {
    unsafe {
        let c = &NET.tcp_conns[idx];
        if c.pending_op != TCP_PENDING_NONE {
            let reply = Message {
                sender: 0,
                tag: TAG_ERROR,
                data: [code, 0, 0, 0, 0, 0],
            };
            let _ = syscall::sys_reply(c.pending_tid, &reply);
            NET.tcp_conns[idx].pending_op = TCP_PENDING_NONE;
        }
    }
}

fn handle_tcp(data: &[u8], src_ip: &[u8; 4]) {
    if data.len() < TCP_HLEN { return; }

    let src_port = u16::from_be_bytes([data[0], data[1]]);
    let dst_port = u16::from_be_bytes([data[2], data[3]]);
    let seq = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
    let ack = u32::from_be_bytes([data[8], data[9], data[10], data[11]]);
    let data_offset = ((data[12] >> 4) as usize) * 4;
    let flags = data[13];
    let window = u16::from_be_bytes([data[14], data[15]]);

    if data_offset > data.len() { return; }
    let payload = if data_offset < data.len() { &data[data_offset..] } else { &[] };

    // Find matching connection
    if let Some(idx) = find_tcp_conn(src_ip, src_port, dst_port) {
        process_tcp_segment(idx, seq, ack, flags, window, payload);
    } else if flags & TCP_SYN != 0 && flags & TCP_ACK == 0 {
        // Incoming SYN — check for listener
        if let Some(idx) = find_tcp_listener(dst_port) {
            accept_tcp_syn(idx, src_ip, src_port, seq, window);
        } else {
            send_tcp_rst(src_ip, dst_port, src_port, 0, seq.wrapping_add(1));
        }
    } else if flags & TCP_RST == 0 {
        // No connection, not a SYN, not a RST — send RST
        if flags & TCP_ACK != 0 {
            send_tcp_rst(src_ip, dst_port, src_port, ack, 0);
        } else {
            let ack_val = seq.wrapping_add(payload.len() as u32);
            send_tcp_rst(src_ip, dst_port, src_port, 0, ack_val);
        }
    }
}

fn accept_tcp_syn(listener_idx: usize, remote_ip: &[u8; 4], remote_port: u16, seq: u32, window: u16) {
    // Create a new connection for this SYN (listener stays in LISTEN)
    let idx = match alloc_tcp_conn() {
        Some(i) => i,
        None => return, // No free connections
    };

    if !init_tcp_buffers(idx) {
        return;
    }

    let iss = (syscall::sys_ticks() as u32).wrapping_mul(64000);
    let listener = unsafe { &NET.tcp_conns[listener_idx] };
    let local_port = listener.local_port;
    let pending_tid = listener.pending_tid;
    let pending_op = listener.pending_op;

    unsafe {
        NET.tcp_conns[idx] = TcpConn {
            state: TcpState::SynReceived,
            local_port,
            remote_ip: *remote_ip,
            remote_port,
            snd_una: iss,
            snd_nxt: iss.wrapping_add(1),
            rcv_nxt: seq.wrapping_add(1),
            snd_wnd: window,
            recv_len: 0,
            send_len: 0,
            retransmit_tick: syscall::sys_ticks(),
            timewait_tick: 0,
            pending_tid,
            pending_op,
            pending_phys: 0,
            pending_max: 0,
            in_use: true,
            fin_received: false,
        };
        // Clear the listener's pending (it's been moved to the new conn)
        NET.tcp_conns[listener_idx].pending_op = TCP_PENDING_NONE;
        NET.tcp_conns[listener_idx].pending_tid = 0;
    }

    // Send SYN-ACK
    send_tcp_segment(
        remote_ip, local_port, remote_port,
        iss, seq.wrapping_add(1),
        TCP_SYN | TCP_ACK, TCP_BUF_SIZE as u16, &[],
    );
}

fn process_tcp_segment(idx: usize, seq: u32, ack: u32, flags: u8, window: u16, payload: &[u8]) {
    let state = unsafe { NET.tcp_conns[idx].state };

    // RST handling — reset the connection
    if flags & TCP_RST != 0 {
        tcp_notify_error(idx, 2); // connection reset
        free_tcp_conn(idx);
        return;
    }

    match state {
        TcpState::SynSent => {
            // Expecting SYN-ACK
            if flags & TCP_SYN != 0 && flags & TCP_ACK != 0 {
                let c = unsafe { &mut NET.tcp_conns[idx] };
                if ack == c.snd_nxt {
                    c.snd_una = ack;
                    c.rcv_nxt = seq.wrapping_add(1);
                    c.snd_wnd = window;
                    c.state = TcpState::Established;
                    c.retransmit_tick = 0;

                    // Send ACK
                    tcp_conn_send_segment(idx, TCP_ACK, &[]);

                    // Notify client
                    if c.pending_op == TCP_PENDING_CONNECT {
                        let reply = Message {
                            sender: 0,
                            tag: TAG_OK,
                            data: [idx as u64, 0, 0, 0, 0, 0],
                        };
                        let _ = syscall::sys_reply(c.pending_tid, &reply);
                        c.pending_op = TCP_PENDING_NONE;
                    }
                }
            }
        }
        TcpState::SynReceived => {
            // Expecting ACK (completing 3-way handshake)
            if flags & TCP_ACK != 0 {
                let c = unsafe { &mut NET.tcp_conns[idx] };
                if ack == c.snd_nxt {
                    c.snd_una = ack;
                    c.snd_wnd = window;
                    c.state = TcpState::Established;
                    c.retransmit_tick = 0;

                    // Notify client (accept)
                    if c.pending_op == TCP_PENDING_ACCEPT {
                        let ip_packed = u32::from_be_bytes(c.remote_ip) as u64;
                        let reply = Message {
                            sender: 0,
                            tag: TAG_OK,
                            data: [idx as u64, ip_packed, c.remote_port as u64, 0, 0, 0],
                        };
                        let _ = syscall::sys_reply(c.pending_tid, &reply);
                        c.pending_op = TCP_PENDING_NONE;
                    }

                    // Process any piggybacked data
                    if !payload.is_empty() {
                        process_tcp_data(idx, seq, payload);
                    }
                }
            }
        }
        TcpState::Established => {
            process_tcp_established(idx, seq, ack, flags, window, payload);
        }
        TcpState::FinWait1 => {
            let c = unsafe { &mut NET.tcp_conns[idx] };
            // Process ACK of our FIN
            if flags & TCP_ACK != 0 && ack == c.snd_nxt {
                c.snd_una = ack;
                c.snd_wnd = window;
                if flags & TCP_FIN != 0 {
                    // Simultaneous close: FIN+ACK
                    c.rcv_nxt = seq.wrapping_add(1);
                    c.state = TcpState::TimeWait;
                    c.timewait_tick = syscall::sys_ticks();
                    tcp_conn_send_segment(idx, TCP_ACK, &[]);
                } else {
                    c.state = TcpState::FinWait2;
                }
            } else if flags & TCP_FIN != 0 {
                // FIN without ACK of ours
                c.rcv_nxt = seq.wrapping_add(1);
                c.state = TcpState::TimeWait; // simplified: skip CLOSING
                c.timewait_tick = syscall::sys_ticks();
                tcp_conn_send_segment(idx, TCP_ACK, &[]);
            }
        }
        TcpState::FinWait2 => {
            if flags & TCP_FIN != 0 {
                // Deliver any final data
                if !payload.is_empty() {
                    process_tcp_data(idx, seq, payload);
                }
                let c = unsafe { &mut NET.tcp_conns[idx] };
                c.rcv_nxt = seq.wrapping_add(payload.len() as u32).wrapping_add(1);
                c.state = TcpState::TimeWait;
                c.timewait_tick = syscall::sys_ticks();
                tcp_conn_send_segment(idx, TCP_ACK, &[]);
                // Signal EOF to pending recv
                c.fin_received = true;
                tcp_deliver_recv(idx);
            } else if !payload.is_empty() {
                // Data in FIN_WAIT_2
                process_tcp_data(idx, seq, payload);
            }
        }
        TcpState::LastAck => {
            if flags & TCP_ACK != 0 {
                let c = unsafe { &NET.tcp_conns[idx] };
                if ack == c.snd_nxt {
                    free_tcp_conn(idx);
                }
            }
        }
        TcpState::CloseWait => {
            // Just process ACKs for any remaining data
            if flags & TCP_ACK != 0 {
                process_tcp_ack(idx, ack, window);
            }
        }
        TcpState::TimeWait => {
            // Retransmit ACK if we get a FIN
            if flags & TCP_FIN != 0 {
                tcp_conn_send_segment(idx, TCP_ACK, &[]);
            }
        }
        _ => {}
    }
}

fn process_tcp_established(idx: usize, seq: u32, ack: u32, flags: u8, window: u16, payload: &[u8]) {
    // Process ACK
    if flags & TCP_ACK != 0 {
        process_tcp_ack(idx, ack, window);
    }

    // Process incoming data
    if !payload.is_empty() {
        process_tcp_data(idx, seq, payload);
    }

    // Process FIN
    if flags & TCP_FIN != 0 {
        let c = unsafe { &mut NET.tcp_conns[idx] };
        c.rcv_nxt = seq.wrapping_add(payload.len() as u32).wrapping_add(1);
        c.state = TcpState::CloseWait;
        c.fin_received = true;
        tcp_conn_send_segment(idx, TCP_ACK, &[]);
        // Deliver EOF to pending recv
        tcp_deliver_recv(idx);
    }
}

fn process_tcp_ack(idx: usize, ack: u32, window: u16) {
    let c = unsafe { &mut NET.tcp_conns[idx] };
    if seq_lt(c.snd_una, ack) && seq_lte(ack, c.snd_nxt) {
        let acked = ack.wrapping_sub(c.snd_una) as usize;
        c.snd_una = ack;
        c.snd_wnd = window;

        // Remove acked data from send buffer
        if acked > 0 && acked <= c.send_len {
            let sv = tcp_send_buf_vaddr(idx);
            let remaining = c.send_len - acked;
            if remaining > 0 {
                unsafe {
                    core::ptr::copy((sv + acked) as *const u8, sv as *mut u8, remaining);
                }
            }
            c.send_len -= acked;
        }

        // All data acked — cancel retransmit timer
        if c.snd_una == c.snd_nxt {
            c.retransmit_tick = 0;
        } else {
            c.retransmit_tick = syscall::sys_ticks();
        }
    }
}

fn process_tcp_data(idx: usize, seq: u32, payload: &[u8]) {
    let c = unsafe { &mut NET.tcp_conns[idx] };

    // Only accept in-order data
    if seq != c.rcv_nxt { return; }

    let free = TCP_BUF_SIZE - c.recv_len;
    let n = payload.len().min(free);
    if n == 0 { return; }

    let rv = tcp_recv_buf_vaddr(idx);
    unsafe {
        core::ptr::copy_nonoverlapping(payload.as_ptr(), (rv + c.recv_len) as *mut u8, n);
    }
    c.recv_len += n;
    c.rcv_nxt = c.rcv_nxt.wrapping_add(n as u32);

    // Send ACK
    tcp_conn_send_segment(idx, TCP_ACK, &[]);

    // Deliver to pending recv if any
    tcp_deliver_recv(idx);
}

fn tcp_retransmit(idx: usize) {
    let c = unsafe { &NET.tcp_conns[idx] };
    if c.send_len == 0 { return; }

    let sv = tcp_send_buf_vaddr(idx);
    let len = c.send_len.min(TCP_MSS);
    let payload = unsafe { core::slice::from_raw_parts(sv as *const u8, len) };

    // Retransmit from snd_una
    send_tcp_segment(
        &c.remote_ip, c.local_port, c.remote_port,
        c.snd_una, c.rcv_nxt,
        TCP_ACK | TCP_PSH, (TCP_BUF_SIZE - c.recv_len) as u16,
        payload,
    );

    unsafe {
        NET.tcp_conns[idx].retransmit_tick = syscall::sys_ticks();
    }
}

fn tcp_check_timers() {
    let now = syscall::sys_ticks();
    for i in 0..MAX_TCP_CONNS {
        let c = unsafe { &NET.tcp_conns[i] };
        if !c.in_use { continue; }

        match c.state {
            TcpState::TimeWait => {
                if now - c.timewait_tick > TCP_TIMEWAIT_TICKS {
                    free_tcp_conn(i);
                }
            }
            TcpState::SynSent | TcpState::SynReceived => {
                if c.retransmit_tick != 0 && now - c.retransmit_tick > TCP_RETRANSMIT_TICKS {
                    // Connection timeout
                    tcp_notify_error(i, 3); // timeout
                    free_tcp_conn(i);
                }
            }
            TcpState::Established | TcpState::CloseWait => {
                if c.retransmit_tick != 0 && now - c.retransmit_tick > TCP_RETRANSMIT_TICKS {
                    tcp_retransmit(i);
                }
            }
            TcpState::FinWait1 | TcpState::LastAck => {
                if c.retransmit_tick != 0 && now - c.retransmit_tick > TCP_RETRANSMIT_TICKS {
                    // Retransmit FIN
                    let c = unsafe { &NET.tcp_conns[i] };
                    send_tcp_segment(
                        &c.remote_ip, c.local_port, c.remote_port,
                        c.snd_nxt.wrapping_sub(1), c.rcv_nxt,
                        TCP_FIN | TCP_ACK, 0, &[],
                    );
                    unsafe { NET.tcp_conns[i].retransmit_tick = syscall::sys_ticks(); }
                }
            }
            _ => {}
        }
    }
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
            // Check ICMP ping timeout (300 ticks = 3 seconds at 100 Hz)
            unsafe {
                if let Some(ref pending) = NET.pending_icmp {
                    if syscall::sys_ticks() - pending.send_tick > 300 {
                        let reply = Message {
                            sender: 0,
                            tag: TAG_ERROR,
                            data: [1, 0, 0, 0, 0, 0],
                        };
                        let _ = syscall::sys_reply(pending.tid, &reply);
                        NET.pending_icmp = None;
                    }
                }
            }
            // Check TCP timers
            tcp_check_timers();
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
            TAG_ICMP_PING => {
                let dst_ip = (msg.data[0] as u32).to_be_bytes();
                let id = msg.data[1] as u16;
                let seq = msg.data[2] as u16;

                // If ARP isn't cached, resolve_mac sends a request and returns
                // false.  Poll the NIC directly so the ARP reply can arrive
                // before we give up.
                let mut sent = send_icmp_echo_request(&dst_ip, id, seq);
                if !sent {
                    let io_base = unsafe { NET.io_base };
                    let next_hop = if is_same_subnet(&dst_ip) {
                        dst_ip
                    } else {
                        unsafe { NET.gateway }
                    };
                    for _ in 0..200 {
                        syscall::sys_yield();
                        let isr = inw(io_base + REG_ISR);
                        if isr != 0 {
                            outw(io_base + REG_ISR, isr);
                            if isr & ISR_ROK != 0 {
                                process_rx();
                            }
                        }
                        if arp_lookup(&next_hop).is_some() {
                            sent = send_icmp_echo_request(&dst_ip, id, seq);
                            break;
                        }
                    }
                }

                if sent {
                    unsafe {
                        NET.pending_icmp = Some(PendingIcmp {
                            tid: msg.sender,
                            id,
                            seq,
                            send_tick: syscall::sys_ticks(),
                        });
                    }
                    // Deferred reply — do not reply now
                } else {
                    let reply = Message {
                        sender: 0,
                        tag: TAG_ERROR,
                        data: [2, 0, 0, 0, 0, 0],
                    };
                    let _ = syscall::sys_reply(msg.sender, &reply);
                }
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
            TAG_TCP_CONNECT => {
                let dst_ip = (msg.data[0] as u32).to_be_bytes();
                let ports = msg.data[1];
                let dst_port = (ports >> 16) as u16;
                let mut src_port = (ports & 0xFFFF) as u16;
                if src_port == 0 { src_port = alloc_ephemeral_port(); }

                let idx = match alloc_tcp_conn() {
                    Some(i) => i,
                    None => {
                        let reply = Message { sender: 0, tag: TAG_ERROR, data: [1, 0, 0, 0, 0, 0] };
                        let _ = syscall::sys_reply(msg.sender, &reply);
                        continue;
                    }
                };

                if !init_tcp_buffers(idx) {
                    let reply = Message { sender: 0, tag: TAG_ERROR, data: [2, 0, 0, 0, 0, 0] };
                    let _ = syscall::sys_reply(msg.sender, &reply);
                    continue;
                }

                let iss = (syscall::sys_ticks() as u32).wrapping_mul(64000);
                unsafe {
                    NET.tcp_conns[idx] = TcpConn {
                        state: TcpState::SynSent,
                        local_port: src_port,
                        remote_ip: dst_ip,
                        remote_port: dst_port,
                        snd_una: iss,
                        snd_nxt: iss.wrapping_add(1),
                        rcv_nxt: 0,
                        snd_wnd: 0,
                        recv_len: 0,
                        send_len: 0,
                        retransmit_tick: syscall::sys_ticks(),
                        timewait_tick: 0,
                        pending_tid: msg.sender,
                        pending_op: TCP_PENDING_CONNECT,
                        pending_phys: 0,
                        pending_max: 0,
                        in_use: true,
                        fin_received: false,
                    };
                }

                // Send SYN
                send_tcp_segment(
                    &dst_ip, src_port, dst_port,
                    iss, 0, TCP_SYN, TCP_BUF_SIZE as u16, &[],
                );
                // Deferred reply — will reply when ESTABLISHED or timeout
            }
            TAG_TCP_LISTEN => {
                let port = msg.data[0] as u16;

                let idx = match alloc_tcp_conn() {
                    Some(i) => i,
                    None => {
                        let reply = Message { sender: 0, tag: TAG_ERROR, data: [1, 0, 0, 0, 0, 0] };
                        let _ = syscall::sys_reply(msg.sender, &reply);
                        continue;
                    }
                };

                // Listener doesn't need buffers — they're allocated when SYN arrives
                unsafe {
                    NET.tcp_conns[idx] = TcpConn {
                        state: TcpState::Listen,
                        local_port: port,
                        remote_ip: [0; 4],
                        remote_port: 0,
                        snd_una: 0, snd_nxt: 0, rcv_nxt: 0, snd_wnd: 0,
                        recv_len: 0, send_len: 0,
                        retransmit_tick: 0, timewait_tick: 0,
                        pending_tid: msg.sender,
                        pending_op: TCP_PENDING_ACCEPT,
                        pending_phys: 0, pending_max: 0,
                        in_use: true, fin_received: false,
                    };
                }
                // Deferred reply — will reply when connection established
            }
            TAG_TCP_SEND => {
                let handle = msg.data[0] as usize;
                let phys_addr = msg.data[1] as usize;
                let len = msg.data[2] as usize;

                if handle >= MAX_TCP_CONNS {
                    let reply = Message { sender: 0, tag: TAG_ERROR, data: [1, 0, 0, 0, 0, 0] };
                    let _ = syscall::sys_reply(msg.sender, &reply);
                    continue;
                }

                let state = unsafe { NET.tcp_conns[handle].state };
                let in_use = unsafe { NET.tcp_conns[handle].in_use };
                if !in_use || (state != TcpState::Established && state != TcpState::CloseWait) {
                    let reply = Message { sender: 0, tag: TAG_ERROR, data: [2, 0, 0, 0, 0, 0] };
                    let _ = syscall::sys_reply(msg.sender, &reply);
                    continue;
                }

                let mut total_sent = 0usize;
                if len > 0 && syscall::sys_map_phys(phys_addr, CLIENT_BUF, 1).is_ok() {
                    let send_len = len.min(4096);
                    let sv = tcp_send_buf_vaddr(handle);
                    let free = TCP_BUF_SIZE - unsafe { NET.tcp_conns[handle].send_len };
                    let to_queue = send_len.min(free);

                    if to_queue > 0 {
                        let cur_len = unsafe { NET.tcp_conns[handle].send_len };
                        unsafe {
                            core::ptr::copy_nonoverlapping(
                                CLIENT_BUF as *const u8,
                                (sv + cur_len) as *mut u8,
                                to_queue,
                            );
                            NET.tcp_conns[handle].send_len += to_queue;
                        }

                        // Send segments
                        let mut offset = 0;
                        while offset < to_queue {
                            let chunk = (to_queue - offset).min(TCP_MSS);
                            let payload = unsafe {
                                core::slice::from_raw_parts(
                                    (sv + cur_len + offset) as *const u8,
                                    chunk,
                                )
                            };
                            let snd_nxt = unsafe { NET.tcp_conns[handle].snd_nxt };
                            let c = unsafe { &NET.tcp_conns[handle] };
                            send_tcp_segment(
                                &c.remote_ip, c.local_port, c.remote_port,
                                snd_nxt, c.rcv_nxt,
                                TCP_ACK | TCP_PSH,
                                (TCP_BUF_SIZE - c.recv_len) as u16,
                                payload,
                            );
                            unsafe {
                                NET.tcp_conns[handle].snd_nxt =
                                    snd_nxt.wrapping_add(chunk as u32);
                                NET.tcp_conns[handle].retransmit_tick = syscall::sys_ticks();
                            }
                            offset += chunk;
                        }
                        total_sent = to_queue;
                    }
                }

                let reply = Message {
                    sender: 0,
                    tag: TAG_OK,
                    data: [total_sent as u64, 0, 0, 0, 0, 0],
                };
                let _ = syscall::sys_reply(msg.sender, &reply);
            }
            TAG_TCP_RECV => {
                let handle = msg.data[0] as usize;
                let phys_addr = msg.data[1] as usize;
                let max_len = msg.data[2] as usize;

                if handle >= MAX_TCP_CONNS {
                    let reply = Message { sender: 0, tag: TAG_ERROR, data: [1, 0, 0, 0, 0, 0] };
                    let _ = syscall::sys_reply(msg.sender, &reply);
                    continue;
                }

                let c = unsafe { &NET.tcp_conns[handle] };
                if !c.in_use {
                    let reply = Message { sender: 0, tag: TAG_ERROR, data: [2, 0, 0, 0, 0, 0] };
                    let _ = syscall::sys_reply(msg.sender, &reply);
                    continue;
                }

                // If data available or FIN received, reply immediately
                if c.recv_len > 0 || c.fin_received {
                    let n = c.recv_len.min(max_len);
                    if n > 0 && syscall::sys_map_phys(phys_addr, CLIENT_BUF, 1).is_ok() {
                        let rv = tcp_recv_buf_vaddr(handle);
                        unsafe {
                            core::ptr::copy_nonoverlapping(
                                rv as *const u8,
                                CLIENT_BUF as *mut u8,
                                n,
                            );
                            if n < c.recv_len {
                                core::ptr::copy(
                                    (rv + n) as *const u8,
                                    rv as *mut u8,
                                    c.recv_len - n,
                                );
                            }
                            NET.tcp_conns[handle].recv_len -= n;
                        }
                    }
                    let reply = Message {
                        sender: 0,
                        tag: TAG_OK,
                        data: [n as u64, 0, 0, 0, 0, 0],
                    };
                    let _ = syscall::sys_reply(msg.sender, &reply);
                } else {
                    // Defer reply
                    unsafe {
                        NET.tcp_conns[handle].pending_tid = msg.sender;
                        NET.tcp_conns[handle].pending_op = TCP_PENDING_RECV;
                        NET.tcp_conns[handle].pending_phys = phys_addr;
                        NET.tcp_conns[handle].pending_max = max_len;
                    }
                }
            }
            TAG_TCP_CLOSE => {
                let handle = msg.data[0] as usize;

                if handle >= MAX_TCP_CONNS || !unsafe { NET.tcp_conns[handle].in_use } {
                    let reply = Message { sender: 0, tag: TAG_OK, data: [0; 6] };
                    let _ = syscall::sys_reply(msg.sender, &reply);
                    continue;
                }

                let state = unsafe { NET.tcp_conns[handle].state };
                match state {
                    TcpState::Established | TcpState::SynReceived => {
                        // Send FIN
                        tcp_conn_send_segment(handle, TCP_FIN | TCP_ACK, &[]);
                        unsafe {
                            NET.tcp_conns[handle].snd_nxt =
                                NET.tcp_conns[handle].snd_nxt.wrapping_add(1);
                            NET.tcp_conns[handle].state = TcpState::FinWait1;
                            NET.tcp_conns[handle].retransmit_tick = syscall::sys_ticks();
                        }
                    }
                    TcpState::CloseWait => {
                        // Send FIN
                        tcp_conn_send_segment(handle, TCP_FIN | TCP_ACK, &[]);
                        unsafe {
                            NET.tcp_conns[handle].snd_nxt =
                                NET.tcp_conns[handle].snd_nxt.wrapping_add(1);
                            NET.tcp_conns[handle].state = TcpState::LastAck;
                            NET.tcp_conns[handle].retransmit_tick = syscall::sys_ticks();
                        }
                    }
                    TcpState::Listen | TcpState::SynSent => {
                        free_tcp_conn(handle);
                    }
                    _ => {} // Already closing
                }

                let reply = Message { sender: 0, tag: TAG_OK, data: [0; 6] };
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
