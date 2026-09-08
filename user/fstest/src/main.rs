#![no_std]
#![no_main]

//! Exercise the filesystem through the VFS: create, write, read back, list.
//!
//! The shell has pipelines but no output redirection, so before this there was
//! no way to make the system write a file from the console — the write path
//! was only ever exercised by programs that happened to use it. That made it
//! the least-tested half of the filesystem, which is the wrong half to leave
//! untested when adding a second on-disk format.

use quark_rt::manifest::CapReq;
use quark_rt::{nameserver, println, syscall, vfs};

// A page to hand the VFS for file data, and nothing else: the server owns the
// disk and the buffers behind it.
quark_rt::manifest!([CapReq::phys_alloc(4)]);

const BUF: usize = 0x88_0000_0000;
const CONTENT: &[u8] = b"ext4 write path: extents, allocation and directory entries.\n";

static mut PASSED: u32 = 0;
static mut FAILED: u32 = 0;

fn check(what: &str, ok: bool) {
    unsafe {
        if ok {
            PASSED += 1;
            println!("  ok    {}", what);
        } else {
            FAILED += 1;
            println!("  FAIL  {}", what);
        }
    }
}

#[unsafe(no_mangle)]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    let vfs_tid = match nameserver::lookup_retry(b"vfs", 50) {
        Some(t) => t,
        None => {
            println!("fstest: no vfs service");
            syscall::sys_exit_code(1);
        }
    };

    // A page the VFS reads from and writes into.
    let phys = match syscall::sys_phys_alloc(1) {
        Ok(p) => p,
        Err(()) => {
            println!("fstest: no memory");
            syscall::sys_exit_code(1);
        }
    };
    if syscall::sys_map_phys(phys, BUF, 1).is_err() {
        println!("fstest: cannot map buffer");
        syscall::sys_exit_code(1);
    }

    // `fstest loop` writes files until it is killed, which is how the journal
    // gets tested: stop the machine at an arbitrary moment and see whether
    // what comes back up is consistent.
    if quark_rt::args::argv(1) == Some(b"loop") {
        let mut n = 0u32;
        loop {
            let mut path = *b"/home/root/loop00.txt";
            path[15] = b'0' + ((n / 10) % 10) as u8;
            path[16] = b'0' + (n % 10) as u8;
            match vfs::create(vfs_tid, &path, false) {
                Ok((h, _, _)) => {
                    unsafe {
                        core::ptr::copy_nonoverlapping(
                            CONTENT.as_ptr(),
                            BUF as *mut u8,
                            CONTENT.len(),
                        );
                    }
                    let _ = vfs::write(vfs_tid, h, phys, 0, CONTENT.len() as u32);
                    let _ = vfs::close(vfs_tid, h);
                    println!("wrote {}", Str(&path));
                }
                Err(e) => {
                    println!("create {} failed: {}", Str(&path), e);
                    syscall::sys_exit_code(1);
                }
            }
            n = (n + 1) % 100;
        }
    }

    let path = b"/home/root/fstest.txt";
    println!("fstest: {}", Str(path));

    // Create. An existing file from a previous run is not a failure of the
    // thing being tested, so fall back to opening it.
    let handle = match vfs::create(vfs_tid, path, false) {
        Ok((h, _, _)) => {
            check("create", true);
            h
        }
        Err(e) => {
            match vfs::open(vfs_tid, path) {
                Ok((h, _, _)) => {
                    println!("  note  already existed; reusing it");
                    h
                }
                Err(_) => {
                    check("create", false);
                    println!("        error {}", e);
                    report();
                }
            }
        }
    };

    // Write.
    unsafe {
        core::ptr::copy_nonoverlapping(CONTENT.as_ptr(), BUF as *mut u8, CONTENT.len());
    }
    let written = vfs::write(vfs_tid, handle, phys, 0, CONTENT.len() as u32);
    check("write", written == Ok(CONTENT.len() as u32));
    if let Err(e) = written {
        println!("        error {}", e);
    }
    let _ = vfs::close(vfs_tid, handle);

    // Read it back through a fresh open, so the bytes come off the disk rather
    // than out of whatever the write left in memory.
    unsafe { core::ptr::write_bytes(BUF as *mut u8, 0, 4096) };
    match vfs::open(vfs_tid, path) {
        Ok((h, size, _)) => {
            check("reopen", true);
            check("size", size == CONTENT.len() as u32);
            if size != CONTENT.len() as u32 {
                println!("        got {} want {}", size, CONTENT.len());
            }
            match vfs::read(vfs_tid, h, phys, 0, CONTENT.len() as u32) {
                Ok(n) => {
                    let got = unsafe {
                        core::slice::from_raw_parts(BUF as *const u8, n as usize)
                    };
                    check("read length", n == CONTENT.len() as u32);
                    check("read content", got == CONTENT);
                    if got != CONTENT {
                        println!("        got: {}", Str(got));
                    }
                }
                Err(e) => {
                    check("read", false);
                    println!("        error {}", e);
                }
            }
            let _ = vfs::close(vfs_tid, h);
        }
        Err(e) => {
            check("reopen", false);
            println!("        error {}", e);
        }
    }

    // And that the directory entry is really there.
    match vfs::open(vfs_tid, b"/home/root") {
        Ok((h, _, _)) => {
            let mut found = false;
            for i in 0..64 {
                match vfs::readdir(vfs_tid, h, i) {
                    Ok(Some(e)) => {
                        if e.name_bytes() == b"fstest.txt" {
                            found = true;
                        }
                    }
                    Ok(None) => break,
                    Err(_) => break,
                }
            }
            check("directory entry", found);
            let _ = vfs::close(vfs_tid, h);
        }
        Err(_) => check("open directory", false),
    }

    report();
}

fn report() -> ! {
    let (p, f) = unsafe { (PASSED, FAILED) };
    println!("fstest: {} passed, {} failed", p, f);
    syscall::sys_exit_code(if f == 0 { 0 } else { 1 });
}

/// Print a byte slice as text without allocating.
struct Str<'a>(&'a [u8]);

impl core::fmt::Display for Str<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for &c in self.0 {
            write!(f, "{}", if c == b'\n' { ' ' } else { c as char })?;
        }
        Ok(())
    }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("fstest: PANIC: {}", info);
    quark_rt::syscall::sys_exit_code(255);
}
