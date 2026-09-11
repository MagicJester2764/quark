#![no_std]
#![no_main]

//! The other half of `dtest`'s cross-process check.
//!
//! Started by `dtest` with one end of a socketpair already at descriptor 3.
//! Allocates memory, writes a witness into it, and sends the descriptor back —
//! which is what a Wayland client does with `wl_shm`, minus the drawing.

use quark_rt::manifest::CapReq;
use quark_rt::{println, sync, syscall};

quark_rt::manifest!([CapReq::phys_alloc(16)]);

const CONN: usize = 3;
const MINE: usize = 0x97_0000_0000;
const WITNESS: u64 = 0x0D15_EA5E_D15C_0DE5;
/// Where the verdict on the capability test goes, in the memory both halves
/// share. Reported this way rather than over the stream because the stream's
/// message boundaries are what the other half is asserting about.
const VERDICT: usize = MINE + 128;
/// A CSpace slot of this child's own, well clear of anything the manifest
/// filled, to mint into.
const SCRATCH: usize = 8;
/// A slot in somebody else's CSpace to try to fill.
const VICTIM_SLOT: usize = 14;

#[unsafe(no_mangle)]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    // Wait for the parent's byte before answering, so this proves the stream
    // carries data in both directions between address spaces.
    let mut buf = [0u8; 8];
    let n = syscall::sys_fd_read(CONN, &mut buf);
    if n != 4 || &buf[..4] != b"go!\n" {
        println!("[dchild] bad greeting: {} bytes", n);
        syscall::sys_exit_code(2);
    }

    let Ok(mem) = syscall::sys_memfd_create(2) else {
        println!("[dchild] no memory");
        syscall::sys_exit_code(3);
    };
    if syscall::sys_mmap_fd(mem, MINE).is_err() {
        println!("[dchild] cannot map my own memory");
        syscall::sys_exit_code(4);
    }
    unsafe { core::ptr::write_volatile(MINE as *mut u64, WITNESS) };

    // Can this child push a capability into a task it has no authority over?
    //
    // It holds no TaskMgmt at all — its manifest asks for phys_alloc and
    // nothing else — and its parent is not calling it, so the answer must be
    // no. A grant can never *raise* anyone's authority, since it only ever
    // adds; what it can do is fill sixteen slots, and a service that can no
    // longer be handed a capability can no longer be handed the display.
    //
    // An Endpoint naming only the caller is the one capability anybody may
    // always mint, which is what makes this test about the grant rather than
    // about the mint.
    let me = syscall::sys_getpid() as usize;
    let parent = syscall::sys_task_info(me).map(|(_, p, _)| p).unwrap_or(0);
    let minted = syscall::sys_cap_mint(SCRATCH, syscall::CAP_TYPE_ENDPOINT, 1u64 << me, 0).is_ok();
    let refused = syscall::sys_cap_grant(parent, SCRATCH, VICTIM_SLOT).is_err();
    unsafe {
        core::ptr::write_volatile(VERDICT as *mut u64, (minted && refused) as u64);
    }

    if syscall::sys_fd_send(CONN, b"here", Some(mem)) != Ok(4) {
        println!("[dchild] send failed");
        syscall::sys_exit_code(5);
    }

    // A lock in memory the two of us share. The parent holds it when this
    // arrives, so acquiring it means blocking in one address space and being
    // woken from another — which works because the kernel keys its wait queue
    // on the physical address of the word, not the virtual one.
    let shared = unsafe { &*((MINE + 64) as *const sync::Mutex<u64>) };
    {
        let mut held = shared.lock();
        *held += 1;
    }
    println!("[dchild] sent, and took the shared lock");
    syscall::sys_exit_code(0);
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("[dchild] PANIC: {}", info);
    syscall::sys_exit_code(255);
}
