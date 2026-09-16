# The Rest of the Font Stack Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. This project runs plans inline (superpowers:executing-plans), on `main`, one commit per task.

**Goal:** A client renders a line of text with cairo and FreeType, in a window, with a font that came off the disk — and the filesystem underneath does what those libraries assume a filesystem does.

**Architecture:** Two halves, filesystem first. *The VFS grows up:* paths travel as lent buffers instead of in six message words, `open` can create, exclude and truncate, files can be removed, renamed and shortened, directories can be read by C programs (`getdents64`), `stat` reports real inode numbers, link counts and times, and a handle names an inode rather than holding a stale copy of one — and dies with the task that opened it. *The ports:* zlib, FreeType (reading fonts, not mapping them), expat, fontconfig (with a writable cache in `/var/cache/fontconfig`) and libxkbcommon, each a build script in `explosion/toolchain`, a test program with host-derived expectations where output is deterministic, and — for fonts and configuration — a root-filesystem overlay ExplOSion stages. cairo is rebuilt with FreeType and fontconfig, and `wlcairo` draws text.

**Tech Stack:** Rust (`no_std` VFS server, quark-rt, dtest), C (musl programs, linux-abi, Quark libc), meson/autotools cross builds with `x86_64-quark-musl-gcc`, QEMU via `explosion/tools/boot-test.sh`, `e2fsck` off the host.

**Spec:** `~/src/osdev/ROADMAP.md`, "Phase 13 — The rest of the font stack". Investigation that shaped it: the spikes recorded under *Findings from the spikes* below.

## Global Constraints

- **Done when** "a client renders a line of text with cairo and freetype, in a window, with a font that came off the disk."
- Versions: zlib 1.3.2, FreeType 2.14.3, expat 2.6.4 (the host's), fontconfig 2.18.3, libxkbcommon 1.13.2, DejaVu 2.37, gperf 3.3 (host tool). Tarballs in `~/opt/src`, checksums pinned in `toolchain/bootstrap-fonts.sh`.
- Every port installs into musl's prefix (`$HOME/opt/cross/x86_64-quark/musl`), never the shared sysroot.
- Nothing ported is patched beyond build-system portability (`config.sub`, a platform `#if`); a port that needs a behaviour change is a Quark bug.
- The VFS protocol is written down in `quark/docs/vfs.md`; numbers are never reused.
- Filesystem changes are verified with `e2fsck -fn` on the rootfs partition after the guest has written to it, on ext2 **and** ext4.
- Every task ends with a boot test: `make hd WAYLAND_CLIENTS=... TEST_SUITES="..." ROOT_OVERLAYS="..."` in `explosion`, then `timeout N sh tools/boot-test.sh <keys> <ppm>`, with `PATH="$HOME/.local/bin:$HOME/opt/cross/bin:$PATH"`.
- One commit per task on `main` in whichever repos it touches; messages end with the session's `Co-Authored-By`/`Claude-Session` lines; push after each task.

## Findings from the spikes

All five libraries and cairo-with-fonts cross-build today, with three build fixes:

- zlib's `configure` appends `-fPIC` unconditionally; `-fPIC` with Quark's `-mcmodel=large` makes global addresses come out as zero (toolchain README, libwayland). The musl wrapper drops PIC/PIE flags — there are no shared libraries to want them.
- expat's `config.sub` needs the one-line `quark*` hunk, and its unpacked tree is already configured in place for the host, so the cross build uses a private copy.
- fontconfig needs `gperf` on the host, a one-line `#if` in `src/fcstat.c` (`__quark__` beside `__linux__`: musl's `struct statfs` is Linux's), and `DESTDIR` installs (its config goes to `/etc`).

Reading the code found the run-time gaps this plan closes: paths capped at 47 bytes (and `CREATE` silently cut at 40), `O_CREAT` failing on an existing file, `O_TRUNC` accepted and ignored, no unlink/rename/rmdir/truncate, no `getdents64`, a bulk readdir with no continuation and 48-byte names, `stat` reporting the handle as the inode and 0 as every time, 32 handles for the whole system never released when a client dies, handles holding copies of inodes, and an image recipe that makes a fixed list of directories and lowercases every name.

## Design decisions (and why)

**FreeType reads fonts; it does not map them.** Quark has no demand paging, so a file mapping would have to read the whole file when it is made. FreeType's ANSI stream (`-Dmmap=disabled`) reads the tables it uses, which is what `mmap` buys on a system that pages on demand. A real file mapping needs a pager — the gap Phase 11 recorded, and the question Phase 15's `fork` decision reopens. `mmap` of a file stays refused (`ENODEV`); fontconfig falls back to `read` for its caches.

**Paths are lent.** Phase 12 gave calls lent buffers; a path is just one. Requests that name a path lend it (`data[0]` = its length, up to 4095 bytes), and `rename` lends both, end to end. Nothing is cut short: a path too long is refused with its own error.

**One `OPEN`, with flags.** `CREATE`, `EXCLUSIVE`, `TRUNCATE`, `DIRECTORY` — the Linux semantics the C layer needs, decided by the server in one transaction. `CREATE` (tag 7) and the one-entry `READDIR` (tag 4) are retired; `MKDIR` is its own request.

**A handle names an inode.** It holds the inode number and reads the inode when used, so two handles on one file never disagree and nothing writes through a stale block map. A file whose last name goes while a handle has it open stays until that handle closes. The server watches every task it gives a handle to and closes them when it dies — a dead client's handles no longer fill the table or pass to its TID's next owner.

**Times are seconds since boot.** There is no wall clock; `time()` already counts from boot, so files written here are stamped on the same scale. That keeps fontconfig's cache validity and lock staleness honest within the timeline Quark actually has. Directory times change when their entries do.

**Directory records are self-describing.** A bulk read fills the lent page with variable-length records — inode, next position, size, record length, type, name — and says where to continue. `getdents64` is a field-by-field copy of that into Linux's layout.

**The image is built from the stage, not from a list.** Every staged directory is made, overlays (fonts, configuration) are staged like coreutils, and only all-capitals names (Quark's FAT-shaped install) are lowercased. `debugfs` runs once per image, from a command file.

**The font is DejaVu**, the four Sans/Sans Mono faces, with its licence beside them (Bitstream Vera terms: redistribution allowed with the notice).

---

## File map

| File | Change |
|---|---|
| `quark/docs/vfs.md` (new) | The VFS protocol |
| `quark/user/vfs/src/protocol.rs` (new) | Tags, flags, errors, lent paths, stat/dirent/statfs records |
| `quark/user/vfs/src/handles.rs` (new) | Open-file table: inode numbers, owners watched, orphans |
| `quark/user/vfs/src/ext2_ops.rs` (new) | create, unlink, rmdir, rename, truncate, block freeing |
| `quark/user/vfs/src/{main,ext2,ext2_dir,ext4}.rs` | Dispatch, times, entry removal, extent freeing |
| `quark/user/quark-rt/src/vfs.rs` | Client for all of the above |
| `quark/user/libc/{include/quark/vfs.h,src/quark.c,src/io.c}` | C client; Quark libc `open` flags |
| `quark/user/linux-abi/src/{syscall.c,files.c,abi.h}` | open flags, stat, mkdir, unlink, rmdir, rename, link, truncate, getdents64, readlink, uname, getcwd, statfs |
| `quark/user/{ls,init,disktest,fstest,dtest,dchild}` | New readdir; tests |
| `explosion/tools/{populate-ext.sh,stage-overlays.sh,check-rootfs.sh}` (new) | Image from the stage; overlays; fsck |
| `explosion/Makefile` | `ROOT_OVERLAYS`, rootfs recipe, 64 MiB root |
| `explosion/toolchain/{bootstrap-fonts,teach-config-sub,build-zlib,build-freetype,stage-fonts,build-expat,build-fontconfig,build-xkbcommon}.sh` (new) | Ports |
| `explosion/toolchain/{build-musl,build-cairo,build-weston-client}.sh` | Wrapper flags; cairo with fonts; wlcairo links fonts |
| `explosion/toolchain/tests/{filetest,dirtest,ztest,fttest,xmltest,fctest,cairotext,xkbtest}.c` (new) | Tests |
| `explosion/toolchain/tests/{libc,zlib,fonts,xml,fontconfig,cairo,xkb}.tests` | Lists |
| `explosion/toolchain/patches/fontconfig-2.18.3-quark.patch` (new) | The `fcstat.c` hunk |
| `explosion/toolchain/wlcairo.c` | A line of text |
| `quark/CLAUDE.md`, `explosion/toolchain/README.md`, `~/src/osdev/ROADMAP.md` | Written down |

Suite and overlay directories used by the boot tests: `/tmp/claude-1000/suite-c` (build-tests.sh), `suite-pixman`, `suite-zlib`, `suite-fc`, and one overlay, `/tmp/claude-1000/overlay-fonts`.

---
### Task 1: Paths of any length, an `open` that can create, and handles that name inodes

**Files:**
- Create: `quark/docs/vfs.md`, `quark/user/vfs/src/protocol.rs`, `quark/user/vfs/src/handles.rs`, `quark/user/vfs/src/ext2_ops.rs`
- Modify: `quark/user/vfs/src/{main,ext2,ext2_dir}.rs`
- Modify: `quark/user/quark-rt/src/vfs.rs`, `quark/user/libc/include/quark/vfs.h`, `quark/user/libc/src/{quark,io}.c`, `quark/user/linux-abi/src/{syscall.c,files.c,abi.h}`
- Modify: `quark/user/dchild/src/main.rs` (`hold N`), `quark/user/dtest/src/main.rs` (`files` section)
- Test: `explosion/toolchain/tests/filetest.c`, `explosion/toolchain/tests/libc.tests`

**Interfaces:**
- Produces (wire): `OPEN`(1) lends the path, `data[0]` = length, `data[1]` = flags `CREATE 1 | EXCLUSIVE 2 | TRUNCATE 4 | DIRECTORY 8`; reply `[handle, size, is_dir, mode (with type bits), access, inode id]`. `STAT`(5) lends an 88-byte buffer for writing and fills eleven little-endian words `id size mode links uid gid atime mtime ctime blocks blksize`; reply `data[0]` = 88. `MKDIR`(9) lends the path. Errors add `EXISTS 10, NOT_EMPTY 11, NOT_SUPPORTED 12, NAME_TOO_LONG 13`. Tags 4 and 7 stay taken (4 retires in Task 3).
- Produces (Rust): `vfs::MAX_PATH = 4095`; `vfs::OPEN_{CREATE,EXCLUSIVE,TRUNCATE,DIRECTORY}`; `vfs::ERR_{READ_ONLY,EXISTS,NOT_EMPTY,NOT_SUPPORTED,NAME_TOO_LONG}`; `struct Opened { handle: usize, size: u64, is_dir: bool, mode: u32, access: u32, id: u64 }`; `open_with(vfs, path, flags) -> Result<Opened, u64>`; `mkdir(vfs, path) -> Result<(), u64>`; `struct Stat { id, size, mode, links, uid, gid, atime, mtime, ctime, blocks, block_size }`; `stat_full(vfs, handle) -> Result<Stat, u64>`; `open`, `create`, `stat` keep their signatures.
- Produces (C): `quark_vfs_open(path, unsigned long flags, struct quark_vfs_file *)` (gains `id`); `struct quark_vfs_stat` (eleven `unsigned long`s, same order); `quark_vfs_stat(handle, struct quark_vfs_stat *)`; `quark_vfs_mkdir(path)`; `QUARK_VFS_OPEN_*`, `QUARK_VFS_MAX_PATH 4095`, the four new error codes.
- Produces (server): `handles::{OpenFile, FsFileData, alloc, get, close, close_all, inode_is_open}`, `ext2::now() -> u32`, `ext2_ops::{split_path, create}`.

- [x] **Step 1: The failing tests.** `filetest.c` (no `LINK:` line), added to `libc.tests`:

```c
/* The file calls a ported program leans on, answered by Quark's VFS through
   the Linux translation layer. Re-runnable: it tolerates what an earlier run
   left behind, and from Task 2 on it removes it. */
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

static int failed;

static void check(const char *what, int ok) {
    printf("  %s  %s\n", ok ? "ok  " : "FAIL", what);
    if (!ok) {
        failed++;
    }
}

#define DIR    "/tmp/filetest-a-directory-with-a-name-past-the-old-limit"
#define FILE_A DIR "/a-file-whose-whole-path-is-well-over-forty-seven-bytes"
#define FILE_B DIR "/another"

static int put(const char *path, int flags, const char *text) {
    int fd = open(path, flags, 0644);
    if (fd < 0) {
        return -1;
    }
    ssize_t n = write(fd, text, strlen(text));
    close(fd);
    return n == (ssize_t)strlen(text) ? 0 : -1;
}

int main(void) {
    printf("files:\n");
    int r = mkdir(DIR, 0755);
    check("make a directory with a long name", r == 0 || errno == EEXIST);
    check("making it again says it exists", mkdir(DIR, 0755) == -1 && errno == EEXIST);
    check("create a file with a long path", put(FILE_A, O_WRONLY | O_CREAT, "0123456789") == 0);

    int fd = open(FILE_A, O_RDWR | O_CREAT, 0644);
    char buf[16] = {0};
    check("O_CREAT opens a file that exists", fd >= 0);
    check("and keeps what it held", fd >= 0 && read(fd, buf, 10) == 10 && !memcmp(buf, "0123456789", 10));
    close(fd);

    fd = open(FILE_A, O_WRONLY | O_CREAT | O_EXCL, 0644);
    check("O_EXCL refuses one that exists", fd == -1 && errno == EEXIST);
    fd = open(FILE_A, O_RDONLY | O_DIRECTORY);
    check("O_DIRECTORY refuses a file", fd == -1 && errno == ENOTDIR);
    fd = open(DIR, O_WRONLY);
    check("a directory cannot be opened to write", fd == -1 && errno == EISDIR);

    struct stat a1, a2, b, d, f;
    put(FILE_B, O_WRONLY | O_CREAT, "b");
    check("stat a file", stat(FILE_A, &a1) == 0 && S_ISREG(a1.st_mode) && a1.st_size >= 10);
    check("twice, and it is the same inode", stat(FILE_A, &a2) == 0 && a1.st_ino == a2.st_ino);
    check("another file is another inode", stat(FILE_B, &b) == 0 && b.st_ino != a1.st_ino);
    check("a file has a link", a1.st_nlink >= 1);
    check("stat a directory", stat(DIR, &d) == 0 && S_ISDIR(d.st_mode) && d.st_nlink >= 2);
    time_t now = time(NULL);
    check("a file written now is dated now", a1.st_mtime <= now && now - a1.st_mtime < 600);
    fd = open(FILE_A, O_RDONLY);
    check("fstat agrees with stat", fd >= 0 && fstat(fd, &f) == 0 && f.st_ino == a1.st_ino);
    close(fd);

    char longname[sizeof DIR + 300];
    memcpy(longname, DIR "/", sizeof DIR);
    memset(longname + sizeof DIR, 'x', 260);
    longname[sizeof DIR + 260] = 0;
    fd = open(longname, O_WRONLY | O_CREAT, 0644);
    check("a name longer than 255 bytes is refused", fd == -1 && errno == ENAMETOOLONG);

    printf("filetest: %s\n", failed ? "FAILED" : "ok");
    return failed ? 1 : 0;
}
```

dchild gains `hold N` — open `/etc/passwd` N times through `quark_rt::vfs`, exit with how many opened, closing none. dtest gains a `files` section (in the table after `service`):

```rust
fn test_files() {
    println!("files:");
    let Some(vfs_tid) = nameserver::lookup_retry(b"vfs", 20) else {
        check("find the VFS", false);
        return;
    };
    // Paths are lent, so their length is the filesystem's business.
    let dir: &[u8] = b"/tmp/dtest-a-directory-whose-name-alone-is-past-the-old-limit";
    let file: &[u8] = b"/tmp/dtest-a-directory-whose-name-alone-is-past-the-old-limit/and-a-file";
    check(
        "make a directory with a long path",
        matches!(vfs::mkdir(vfs_tid, dir), Ok(()) | Err(vfs::ERR_EXISTS)),
    );
    if let Ok(o) = vfs::open_with(vfs_tid, file, vfs::OPEN_CREATE) {
        let _ = vfs::write(vfs_tid, o.handle, b"long paths", 0);
        let _ = vfs::close(vfs_tid, o.handle);
    }
    let again = vfs::open_with(vfs_tid, file, vfs::OPEN_CREATE);
    check("creating a file again opens it", again.as_ref().is_ok_and(|o| o.size == 10 && !o.is_dir));
    let other = vfs::open(vfs_tid, b"/etc/passwd");
    if let (Ok(a), Ok((b, _, _))) = (&again, &other) {
        let ids = (vfs::stat_full(vfs_tid, a.handle), vfs::stat_full(vfs_tid, *b));
        check(
            "stat names the inode, not the handle",
            matches!(ids, (Ok(x), Ok(y)) if x.id == a.id && x.id != y.id && x.links >= 1),
        );
    }
    for h in [again.map(|o| o.handle), other.map(|o| o.0)].into_iter().flatten() {
        let _ = vfs::close(vfs_tid, h);
    }
    check(
        "creating it exclusively fails",
        vfs::open_with(vfs_tid, file, vfs::OPEN_CREATE | vfs::OPEN_EXCLUSIVE).err() == Some(vfs::ERR_EXISTS),
    );
    check(
        "a file is not a directory",
        vfs::open_with(vfs_tid, file, vfs::OPEN_DIRECTORY).err() == Some(vfs::ERR_NOT_DIR),
    );
    let mut long = [b'y'; 300];
    long[..5].copy_from_slice(b"/tmp/");
    check(
        "a name past 255 bytes is refused",
        vfs::open_with(vfs_tid, &long, vfs::OPEN_CREATE).err() == Some(vfs::ERR_NAME_TOO_LONG),
    );
    // A program that exits holding files gives them back. Two of these hold
    // more handles between them than the table has room for.
    for _ in 0..2 {
        let Some(child) = load_child(&[b"dchild", b"hold", b"100"]) else {
            check("start a program that holds files", false);
            return;
        };
        let _ = child.start();
        check("it opened a hundred files", wait_for(child.tid) == Some(100));
    }
    let mut held = [0usize; 60];
    let mut n = 0;
    for slot in held.iter_mut() {
        if let Ok((h, _, _)) = vfs::open(vfs_tid, b"/etc/passwd") {
            *slot = h;
            n += 1;
        }
    }
    check("and their handles went when they did", n == 60);
    for &h in &held[..n] {
        let _ = vfs::close(vfs_tid, h);
    }
}
```

- [x] **Step 2: Run them against today's tree.** Build (`make` in quark; `sh toolchain/build-tests.sh /tmp/claude-1000/suite-c` in explosion), image, boot, `dtest files` and `runtests /etc/libc.tests`. Expected: they do not build (Rust: no `open_with`), and once stubbed the C checks fail on the long path and `O_CREAT`.

- [x] **Step 3: `quark/docs/vfs.md`.** The contract: transport (a call to the task registered as `vfs`; lent buffers per request), every tag with its words, flags, errors and records, and the retired numbers. Written from the Interfaces block above; extended in Tasks 2 and 3.

- [x] **Step 4: `protocol.rs`.** Tags, flags, error codes (the existing 1–9 move here and `main.rs` re-exports them with `pub use protocol::*` so `crate::ERR_IO` keeps working), `MAX_PATH = 4095`, `MAX_NAME = 255`, `PATH_BUF = 0x88_0000_0000` (two pages, mapped at start beside the other buffers), and:

```rust
/// Copy the `len` bytes of path `sender` lent, from `offset`, to `at` in
/// `PATH_BUF`, and return them. A path is refused rather than shortened.
pub fn lent_path(sender: usize, offset: usize, len: usize, at: usize) -> Result<&'static [u8], u64> {
    if len == 0 {
        return Err(ERR_INVALID_PATH);
    }
    if len > MAX_PATH {
        return Err(ERR_NAME_TOO_LONG);
    }
    let buf = unsafe { core::slice::from_raw_parts_mut((PATH_BUF + at) as *mut u8, len) };
    match syscall::sys_lent_read(sender, offset, buf) {
        Ok(n) if n == len => {}
        _ => return Err(ERR_INVALID_PATH),
    }
    if buf.contains(&0) {
        return Err(ERR_INVALID_PATH);
    }
    Ok(buf)
}

pub const STAT_LEN: usize = 88;

/// What `STAT` fills a lent buffer with.
pub struct StatRecord {
    pub id: u64, pub size: u64, pub mode: u64, pub links: u64, pub uid: u64, pub gid: u64,
    pub atime: u64, pub mtime: u64, pub ctime: u64, pub blocks: u64, pub block_size: u64,
}

impl StatRecord {
    pub fn to_bytes(&self) -> [u8; STAT_LEN] {
        let words = [
            self.id, self.size, self.mode, self.links, self.uid, self.gid,
            self.atime, self.mtime, self.ctime, self.blocks, self.block_size,
        ];
        let mut out = [0u8; STAT_LEN];
        for (i, w) in words.iter().enumerate() {
            out[i * 8..i * 8 + 8].copy_from_slice(&w.to_le_bytes());
        }
        out
    }
}
```

- [x] **Step 5: `handles.rs`.** The table moves out of `main.rs`. `MAX_OPEN_FILES = 128`. `FsFileData::Ext2 { inode_num }` — no inode copy, no parent. `alloc(file)` stores it and calls `syscall::sys_task_watch(file.owner_tid)` (idempotent; a dead owner's error is ignored). `get(handle, tid)`, `close(handle, tid) -> Option<u32>` (the ext2 inode it named, 0 for FAT), `close_all(tid, &mut [u32; MAX_OPEN_FILES]) -> usize`, `inode_is_open(ino) -> bool`. Every ext2 handler reads the inode by number (`ext2::read_inode`) where it used the copy; `handle_write_ext2` writes it back through `write_file_data` as today.

- [x] **Step 6: `ext2::now()` and times.** `pub fn now() -> u32 { (syscall::sys_ticks() / 100) as u32 }` — seconds since boot, the scale `time()` uses. `write_file_data` sets `i_mtime` and `i_ctime` to it before writing the inode.

- [x] **Step 7: `ext2_ops.rs`.**

```rust
/// Split a path into its parent and last name. Trailing slashes are dropped;
/// an empty name, `.`, `..` and a name over 255 bytes are refused.
pub fn split_path(path: &[u8]) -> Result<(&[u8], &[u8]), u64> {
    let mut end = path.len();
    while end > 1 && path[end - 1] == b'/' {
        end -= 1;
    }
    let path = &path[..end];
    let (parent, name) = match path.iter().rposition(|&b| b == b'/') {
        Some(0) => (&path[..1], &path[1..]),
        Some(pos) => (&path[..pos], &path[pos + 1..]),
        None => (&b"/"[..], path),
    };
    if name.is_empty() || name == b"." || name == b".." {
        return Err(ERR_INVALID_PATH);
    }
    if name.len() > MAX_NAME {
        return Err(ERR_NAME_TOO_LONG);
    }
    Ok((parent, name))
}

/// Make `path` a new file or directory owned by `uid`/`gid` and return it.
pub fn create(e2: &mut Ext2State, path: &[u8], uid: u32, gid: u32, is_dir: bool)
    -> Result<(u32, Ext2Inode), u64>
{
    let (parent_path, name) = split_path(path)?;
    let (parent_ino, mut parent, _) = ext2_dir::resolve_path(e2, parent_path, uid, gid)?;
    if !parent.is_dir() {
        return Err(ERR_NOT_DIR);
    }
    if !ext2::check_permission(&parent, uid, gid, 3) {
        return Err(ERR_PERMISSION);
    }
    if ext2_dir::find_entry(e2, &parent, name)?.is_some() {
        return Err(ERR_EXISTS);
    }
    let ino = ext2_alloc::alloc_inode(e2)?;
    ext2::zero_inode(e2, ino)?;
    let t = ext2::now();
    let mut inode = Ext2Inode::empty();
    inode.i_mode = if is_dir { ext2::S_IFDIR | 0o755 } else { ext2::S_IFREG | 0o644 };
    inode.i_uid = uid as u16;
    inode.i_gid = gid as u16;
    inode.i_links_count = if is_dir { 2 } else { 1 };
    inode.i_atime = t;
    inode.i_ctime = t;
    inode.i_mtime = t;
    if e2.is_ext4() {
        ext4::init_extent_root(&mut inode);
    }
    if is_dir {
        let block = ext2_alloc::alloc_block(e2)?;
        if e2.is_ext4() {
            ext4::extent_insert(&mut inode, 0, block)?;
        } else {
            inode.i_block[0] = block;
        }
        inode.i_size = e2.block_size;
        inode.i_blocks = e2.block_size / 512;
        ext2_dir::init_dir_block(e2, block, ino, parent_ino, inode.i_generation)?;
        parent.i_links_count += 1;
        let group = (ino - 1) / e2.inodes_per_group;
        e2.bgd_table[group as usize].bg_used_dirs_count += 1;
        ext2::flush_bgd(e2, group)?;
    }
    ext2::write_inode(e2, ino, &inode)?;
    let kind = if is_dir { ext2::FT_DIR } else { ext2::FT_REG_FILE };
    ext2_dir::create_dir_entry(e2, parent_ino, &mut parent, name, ino, kind)?;
    parent.i_mtime = t;
    parent.i_ctime = t;
    ext2::write_inode(e2, parent_ino, &parent)?;
    Ok((ino, inode))
}
```

`resolve_path` refuses a component over 255 bytes with `ERR_NAME_TOO_LONG`. `create_dir_entry` clears `EXT2_INDEX_FL` (0x1000) on a directory it changes and writes the inode: entries added without updating an htree index corrupt it, and a directory without the flag is read linearly by everything.

- [x] **Step 8: Dispatch in `main.rs`.** `TAG_OPEN` reads flags, lends the path, and runs `transacted` when `CREATE` or `TRUNCATE` is set. ext2 open:

```rust
fn open_ext2(sender: usize, path: &[u8], flags: u64) {
    let (uid, gid) = get_sender_uid_gid(sender);
    let trailing = path.len() > 1 && path[path.len() - 1] == b'/';
    let wants_dir = flags & OPEN_DIRECTORY != 0 || trailing;
    let (ino, inode) = match ext2_dir::resolve_path(ext2_state(), path, uid, gid) {
        Ok((ino, inode, _)) => {
            if flags & OPEN_CREATE != 0 && flags & OPEN_EXCLUSIVE != 0 {
                return error_reply(sender, ERR_EXISTS);
            }
            (ino, inode)
        }
        Err(ERR_NOT_FOUND) if flags & OPEN_CREATE != 0 => {
            if trailing {
                return error_reply(sender, ERR_IS_DIR);
            }
            if ext2_state().read_only {
                return error_reply(sender, ERR_READ_ONLY);
            }
            match ext2_ops::create(ext2_state_mut(), path, uid, gid, false) {
                Ok(made) => made,
                Err(code) => return error_reply(sender, code),
            }
        }
        Err(code) => return error_reply(sender, code),
    };
    if wants_dir && !inode.is_dir() {
        return error_reply(sender, ERR_NOT_DIR);
    }
    if !ext2::check_permission(&inode, uid, gid, 4) {
        return error_reply(sender, ERR_PERMISSION);
    }
    let writable = !ext2_state().read_only && ext2::check_permission(&inode, uid, gid, 2);
    let file = OpenFile {
        in_use: true, owner_tid: sender, file_size: 0, is_dir: inode.is_dir(),
        writable, read_offset: 0, fs: FsFileData::Ext2 { inode_num: ino },
    };
    match handles::alloc(file) {
        Some(handle) => reply(sender, [
            handle as u64, inode.size64(), inode.is_dir() as u64,
            inode.i_mode as u64, access_bits(&inode, uid, gid), ino as u64,
        ]),
        None => error_reply(sender, ERR_TOO_MANY_OPEN),
    }
}
```

FAT32 open takes the same flags: `CREATE` makes an entry with the existing `create_dir_entry` when the name is absent, `EXCLUSIVE` refuses an existing one, `DIRECTORY` refuses a file, and a name that is not 8.3 is `ERR_NAME_TOO_LONG` instead of being squeezed by `to_fat83`. `TAG_MKDIR` lends the path and calls `ext2_ops::create(.., true)` (FAT32: `create_dir_entry` with `is_dir`). `TAG_STAT` builds a `StatRecord` (FAT32: id = first cluster, mode `S_IFDIR|0777` or `S_IFREG|0777`, one link, no owner, no times, blocks from the size, block size = cluster size) and `sys_lent_write`s it. `quark_rt::ipc::TAG_TASK_DIED` closes everything the dead task held, without a reply. `extract_path`, `handle_create*` and `TAG_CREATE` go; `TAG_READDIR` stays until Task 3.

- [x] **Step 9: Clients.** quark-rt as in Interfaces, with one helper:

```rust
fn call_with_path(vfs_tid: usize, tag: u64, path: &[u8], mut data: [u64; 6]) -> Result<Message, u64> {
    if path.is_empty() {
        return Err(ERR_INVALID_PATH);
    }
    if path.len() > MAX_PATH {
        return Err(ERR_NAME_TOO_LONG);
    }
    data[0] = path.len() as u64;
    let msg = Message { sender: 0, tag, data };
    let mut reply = Message::empty();
    if syscall::sys_call_lend(vfs_tid, &msg, &mut reply, path).is_err() {
        return Err(ERR_IO);
    }
    if reply.tag == TAG_ERROR {
        return Err(reply.data[0]);
    }
    Ok(reply)
}
```

`create(.., is_dir: true)` is `mkdir` then `open`; for a file, `open_with(CREATE | EXCLUSIVE)`. `stat` is `stat_full` narrowed. In C, `quark.c` gets the same helper (`vfs_path_call`, lending with `QUARK_LEND_READ`); `quark_vfs_stat` lends a `struct quark_vfs_stat` with `QUARK_LEND_WRITE` and copies it with `copy()` (no `memcpy`: this file is linked under the C library). linux-abi: `LX_O_EXCL 0200`, `LX_O_DIRECTORY 0200000`, `LX_O_ACCMODE 3`; `open` maps flags, and refuses writing a directory (`EISDIR`) or a file the server says this caller may not write (`EACCES`) after opening; `fill_stat` takes a `struct quark_vfs_stat` (mode with its type bits, `st_ino` = id, links, owner, three times, blocks, block size); `fstat` asks the server and refreshes the cached size; `mkdir` (83) and `mkdirat` (258, `AT_FDCWD` only); `vfs_errno` maps `EXISTS→EEXIST(17)`, `NOT_EMPTY→ENOTEMPTY(39)`, `NOT_SUPPORTED→EOPNOTSUPP(95)`, `NAME_TOO_LONG→ENAMETOOLONG(36)`, with the numbers in `abi.h`. Quark's own libc `open` maps `O_CREAT`, `O_EXCL`, `O_DIRECTORY` the same way. The comments that still say a page is mapped are corrected.

- [x] **Step 10: Build and verify.** `make` in quark; suites; image; boot. `dtest files`, `dtest` (all), `runtests /etc/libc.tests` (filetest ok), `runtests /etc/cairo.tests`, `ls /usr/bin`, `cat /etc/passwd`, `wm weston-simple-shm wlcairo` then Esc, `hello`. Then `tools/check-rootfs.sh` — written in this step:

```sh
#!/bin/sh
# Check the root filesystem inside a disk image the way Linux would.
#
#     tools/check-rootfs.sh [hdimage.bin]
#
# The guest writes; e2fsck, off the host, says whether what it wrote is a
# filesystem. The root is the GPT's second partition.
set -e
IMG=${1:-hdimage.bin}
START=$(python3 - "$IMG" <<'PY'
import struct, sys
with open(sys.argv[1], 'rb') as f:
    f.seek(512)
    hdr = f.read(92)
    entries, count, size = struct.unpack_from('<QII', hdr, 72)
    f.seek(entries * 512 + size)          # the second entry
    first, last = struct.unpack_from('<QQ', f.read(size), 32)
    print(first, last - first + 1)
PY
)
set -- $START
PART=$(mktemp)
trap 'rm -f "$PART"' EXIT
dd if="$IMG" of="$PART" bs=512 skip="$1" count="$2" status=none
e2fsck -fn "$PART"
```

Expected: every check passes; `e2fsck` reports no problems.

- [x] **Step 11: Commit.** quark: "Paths of any length, and handles that name inodes"; explosion: "filetest, and a way to fsck the root".

---
### Task 2: Removing, renaming and shortening files

**Files:**
- Modify: `quark/user/vfs/src/{ext2_ops,ext2_dir,ext4,ext2,handles,main,protocol}.rs`, `quark/docs/vfs.md`
- Modify: `quark/user/quark-rt/src/vfs.rs`, `quark/user/libc/{include/quark/vfs.h,src/quark.c}`, `quark/user/linux-abi/src/{syscall.c,files.c,abi.h}`
- Modify: `quark/user/dtest/src/main.rs` (`files`)
- Test: `explosion/toolchain/tests/filetest.c`

**Interfaces:**
- Consumes: Task 1's protocol, handles, `ext2_ops::{split_path, create}`, `ext2::now`.
- Produces (wire): `UNLINK`(10) and `RMDIR`(11) lend a path; `RENAME`(12) lends two paths end to end, `data[0]`/`data[1]` their lengths; `TRUNCATE`(13) `data[0]` handle, `data[1]` new size; `OPEN_TRUNCATE` empties a regular file the caller may write. FAT32 answers all of these `ERR_NOT_SUPPORTED`.
- Produces (Rust): `vfs::{unlink, rmdir}(vfs, path)`, `vfs::rename(vfs, from, to)`, `vfs::truncate(vfs, handle, size: u64)`, each `-> Result<(), u64>`.
- Produces (C): `quark_vfs_unlink(path)`, `quark_vfs_rmdir(path)`, `quark_vfs_rename(from, to)`, `quark_vfs_truncate(handle, size)`; linux-abi answers `unlink`(87), `unlinkat`(263, `AT_REMOVEDIR` → rmdir), `rmdir`(84), `rename`(82), `renameat`(264), `renameat2`(316, flags 0 only), `link`(86)/`linkat`(265) → `EPERM`, `truncate`(76), `ftruncate`(77) on a file, `O_TRUNC`.
- Produces (server): `ext2_ops::{unlink, rmdir, rename, truncate, release}`, `ext2_ops::free_blocks_from`, `ext2_dir::{remove_entry, set_dotdot, is_empty}`, `ext4::{free_tree, truncate_root}`, `handles::{add_orphan, is_orphan, forget_orphan}`.

- [x] **Step 1: The failing tests.** `filetest.c` gains, before its summary line:

```c
    printf("removing, renaming, shortening:\n");
    fd = open(FILE_A, O_WRONLY | O_TRUNC);
    check("O_TRUNC empties a file", fd >= 0 && fstat(fd, &f) == 0 && f.st_size == 0);
    check("which then takes new bytes", fd >= 0 && write(fd, "ab", 2) == 2);
    close(fd);
    check("and holds only those", stat(FILE_A, &f) == 0 && f.st_size == 2);

    check("ftruncate shortens", truncate(FILE_A, 1) == 0 && stat(FILE_A, &f) == 0 && f.st_size == 1);
    check("and lengthens", truncate(FILE_A, 5000) == 0 && stat(FILE_A, &f) == 0 && f.st_size == 5000);
    fd = open(FILE_A, O_RDWR);
    char big[5000];
    memset(big, 1, sizeof big);
    int zeros = fd >= 0 && read(fd, big, sizeof big) == 5000 && big[0] == 'a';
    for (int i = 1; i < 5000 && zeros; i++) {
        zeros = big[i] == 0;
    }
    check("with zeros past the old end", zeros);
    check("and a write into the gap lands", fd >= 0 && lseek(fd, 4096, SEEK_SET) == 4096 && write(fd, "z", 1) == 1);
    close(fd);

    #define GONE DIR "/gone"
    put(GONE, O_WRONLY | O_CREAT, "soon");
    fd = open(GONE, O_RDONLY);
    check("unlink a file", unlink(GONE) == 0);
    check("it is gone", stat(GONE, &f) == -1 && errno == ENOENT);
    memset(buf, 0, sizeof buf);
    check("an open handle still reads it", fd >= 0 && read(fd, buf, 4) == 4 && !memcmp(buf, "soon", 4));
    close(fd);
    check("unlinking it again says so", unlink(GONE) == -1 && errno == ENOENT);
    check("unlink refuses a directory", unlink(DIR) == -1 && errno == EISDIR);
    check("link is not offered", link(FILE_A, DIR "/hard") == -1 && errno == EPERM);

    #define MOVED DIR "/moved"
    unlink(MOVED);
    struct stat before;
    stat(FILE_B, &before);
    check("rename a file", rename(FILE_B, MOVED) == 0);
    check("the old name is gone", stat(FILE_B, &f) == -1 && errno == ENOENT);
    check("the new one is the same file", stat(MOVED, &f) == 0 && f.st_ino == before.st_ino);
    put(FILE_B, O_WRONLY | O_CREAT, "replacement");
    check("rename over a file replaces it", rename(MOVED, FILE_B) == 0 && stat(FILE_B, &f) == 0 && f.st_ino == before.st_ino && f.st_size == 1);

    #define SUB DIR "/sub"
    #define SUB2 DIR "/sub-renamed"
    mkdir(SUB, 0755);
    put(SUB "/inner", O_WRONLY | O_CREAT, "x");
    check("rmdir refuses a directory with something in it", rmdir(SUB) == -1 && errno == ENOTEMPTY);
    check("rename a directory", rename(SUB, SUB2) == 0 && stat(SUB2 "/inner", &f) == 0);
    check("not into itself", rename(SUB2, SUB2 "/itself") == -1 && errno == EINVAL);
    check("empty it", unlink(SUB2 "/inner") == 0);
    check("then rmdir removes it", rmdir(SUB2) == 0 && stat(SUB2, &f) == -1 && errno == ENOENT);

    check("tidy up", unlink(FILE_A) == 0 && unlink(FILE_B) == 0 && rmdir(DIR) == 0);
```

(`filetest` from here on starts from nothing and leaves nothing.) dtest `files` gains the same four operations through quark-rt, ending with the directory removed, and checks `vfs::truncate` on a handle and `vfs::rename` onto an existing name.

- [x] **Step 2: Run them.** Expected: `truncate`, `unlink`, `rename`, `rmdir` all `ENOSYS`; dtest does not build.

- [x] **Step 3: Entries.** In `ext2_dir.rs`:

```rust
/// Remove the entry called `name` from a directory. An entry that follows
/// another is folded into it; the first in a block keeps its length and loses
/// its inode, which is how ext2 marks one unused.
pub fn remove_entry(ext2: &Ext2State, dir_ino: u32, dir_inode: &Ext2Inode, name: &[u8]) -> Result<(), u64> {
    let bs = ext2.block_size;
    let usable = crate::csum::dir_usable_len(ext2, bs);
    let blocks = (dir_inode.i_size + bs - 1) / bs;
    for logical in 0..blocks {
        let phys = block_map(ext2, dir_inode, logical)?;
        if phys == 0 {
            continue;
        }
        read_block_buf_mut(ext2, phys)?;
        let buf = unsafe { &mut DIR_BLOCK_BUF };
        let mut pos = 0u32;
        let mut prev: Option<usize> = None;
        while pos < usable {
            let off = pos as usize;
            let ino = read_u32(buf, off);
            let rec_len = read_u16(buf, off + 4) as u32;
            let len = buf[off + 6] as usize;
            if rec_len == 0 {
                break;
            }
            if ino != 0 && len == name.len() && &buf[off + 8..off + 8 + len] == name {
                match prev {
                    Some(p) => {
                        let prev_len = read_u16(buf, p + 4) as u32;
                        write_u16(buf, p + 4, (prev_len + rec_len) as u16);
                    }
                    None => write_u32(buf, off, 0),
                }
                return write_dir_block(ext2, phys, dir_ino, dir_inode, buf);
            }
            prev = Some(off);
            pos += rec_len;
        }
    }
    Err(ERR_NOT_FOUND)
}
```

`is_empty(ext2, dir) -> Result<bool, u64>`: every in-use entry is `.` or `..`. `set_dotdot(ext2, dir_ino, dir, parent)`: rewrite the `..` entry's inode in the directory's first block and write it back checksummed. `remove_entry` and `create_dir_entry` clear `EXT2_INDEX_FL` as in Task 1.

- [x] **Step 4: Freeing blocks.** In `ext2_ops.rs`:

```rust
/// Free every block of `inode` from logical block `first` on, and whatever
/// indirect or tree blocks then map nothing. `i_blocks` follows.
pub fn free_blocks_from(e2: &mut Ext2State, inode: &mut Ext2Inode, first: u32) -> Result<(), u64> {
    let unit = e2.block_size / 512;
    if ext4::uses_extents(inode) {
        let freed = if first == 0 { ext4::free_tree(e2, inode)? } else { ext4::truncate_root(e2, inode, first)? };
        inode.i_blocks = inode.i_blocks.saturating_sub(freed * unit);
        return Ok(());
    }
    for l in (first as usize).min(12)..12 {
        let b = inode.i_block[l];
        if b != 0 {
            ext2_alloc::free_block(e2, b)?;
            inode.i_block[l] = 0;
            inode.i_blocks = inode.i_blocks.saturating_sub(unit);
        }
    }
    let ppb = e2.ptrs_per_block() as u64;
    let mut base = 12u64;
    let mut covers = ppb; // data blocks the level-1 tree maps
    for level in 1..=3u32 {
        let slot = 11 + level as usize;
        let top = inode.i_block[slot];
        if top != 0 && (first as u64) < base + covers {
            let rel = (first as u64).saturating_sub(base);
            let (freed, empty) = free_indirect(e2, top, level, rel)?;
            inode.i_blocks = inode.i_blocks.saturating_sub(freed * unit);
            if empty {
                ext2_alloc::free_block(e2, top)?;
                inode.i_block[slot] = 0;
                inode.i_blocks = inode.i_blocks.saturating_sub(unit);
            }
        }
        base += covers;
        covers *= ppb;
    }
    Ok(())
}

/// Free what indirect block `block` maps from its `first`-th data block on.
/// Level 1 maps data blocks; each level above maps blocks of the one below.
/// Returns how many blocks went, and whether `block` now maps nothing.
fn free_indirect(e2: &mut Ext2State, block: u32, level: u32, first: u64) -> Result<(u32, bool), u64> {
    let ppb = e2.ptrs_per_block();
    let span = (ppb as u64).pow(level - 1);
    let mut freed = 0;
    let mut empty = true;
    for i in 0..ppb {
        let ptr = ext2::read_block_ptr(e2, block, i)?;
        if ptr == 0 {
            continue;
        }
        let start = i as u64 * span;
        if start + span <= first {
            empty = false;
            continue;
        }
        let gone = if level == 1 {
            ext2_alloc::free_block(e2, ptr)?;
            freed += 1;
            true
        } else {
            let (f, child_empty) = free_indirect(e2, ptr, level - 1, first.saturating_sub(start))?;
            freed += f;
            if child_empty {
                ext2_alloc::free_block(e2, ptr)?;
                freed += 1;
            }
            child_empty
        };
        if gone {
            // A block being freed whole need not be tidied first.
            if first > 0 {
                ext2::write_block_ptr(e2, block, i, 0)?;
            }
        } else {
            empty = false;
        }
    }
    Ok((freed, empty))
}
```

(`read_block_ptr` becomes `pub`.) In `ext4.rs`, `free_tree(ext2, inode) -> Result<u32, u64>` walks every entry of the root recursively — a leaf frees its run (the uninitialised length encoding included), an index frees its child's entries and then the child block, with the existing depth guard — and leaves `init_extent_root`'s empty root. `truncate_root(ext2, inode, first) -> Result<u32, u64>` handles a depth-0 root: extents starting at or past `first` are freed and dropped, one straddling it is cut to `first - ee_block` (keeping the uninitialised flag), the rest are kept and packed; a deeper tree is `ERR_NOT_SUPPORTED`, because shortening one means rewriting leaf blocks nothing here writes. `extent_insert` puts a new extent in logical order instead of at the end, so a block written into a hole keeps the root sorted; `write_file_data` allocates any unmapped block in the range it writes, not just the ones past the old end.

- [x] **Step 5: Operations.** In `ext2_ops.rs`, all taking `(e2, …, uid, gid)` and checking write+search permission on every directory they change:
  - `unlink(path)`: split, resolve the parent, find the entry, refuse a directory (`ERR_IS_DIR`), `remove_entry`, stamp the parent, then `drop_link`.
  - `drop_link(ino, inode, t)`: one link fewer, `i_ctime = t`; with links left, write it; with none and a handle open (`handles::inode_is_open`), `handles::add_orphan` and write it; otherwise `release_inode`.
  - `release_inode(ino, inode, t)`: `free_blocks_from(.., 0)`, size 0, `i_dtime = t`, write, `ext2_alloc::free_inode`. `release(ino)` re-reads it and does this only if it still has no links.
  - `rmdir(path)`: the target must be a directory and `is_empty`; remove its entry; the parent loses the link `..` gave it; `bg_used_dirs_count` goes down; links 0; orphan or release as above.
  - `rename(from, to)`: resolve both parents; the source must exist; a directory may not move under itself (walk `..` from the destination parent up to the root, guard 256 steps, `ERR_INVALID_PATH`); an existing destination that is the same inode is success with nothing done, a directory replacing a directory needs it empty, a file may not replace a directory (`ERR_IS_DIR`) nor a directory a file (`ERR_NOT_DIR`); a replaced name is removed and its inode dropped. Then `create_dir_entry` in the destination (first, so a failure leaves two names rather than none), write that parent, **re-read** the source parent (it may be the same inode, just changed), `remove_entry`, and for a directory changing parents, `set_dotdot` and move one link from the old parent to the new, each parent re-read before it is changed. Both parents and the inode are stamped.
  - `truncate(ino, size)`: regular files only; sizes above `u32::MAX` are `ERR_NOT_SUPPORTED`; shrinking frees from `ceil(size / bs)` and zeroes the kept block's tail (`zero_block_tail(e2, block, from)`, a sector read-modify-write); growing only moves the size; `i_mtime`/`i_ctime` stamped.

- [x] **Step 6: Dispatch, orphans, flags.** `TAG_UNLINK`, `TAG_RMDIR`, `TAG_RENAME` (the second path read at offset `data[0]` into `PATH_BUF + 4096`), `TAG_TRUNCATE` (the handle must be writable) run `transacted`; FAT32 answers `ERR_NOT_SUPPORTED`. `OPEN_TRUNCATE` in `open_ext2` truncates a regular file to 0 after the permission check, and needs `writable`. `handles` keeps `ORPHANS: [u32; MAX_OPEN_FILES]`; after `TAG_CLOSE` and after a death closes a task's handles, every inode they named that is an orphan and no longer open is released in a transaction and forgotten.

- [x] **Step 7: Clients.** quark-rt and C as in Interfaces (the two-path call lends one buffer built on the stack: `from` then `to`, at most `2 * MAX_PATH`). linux-abi: the calls listed; `LX_O_TRUNC` with write access sets `OPEN_TRUNCATE`; `ftruncate` on a descriptor from 32 up is the file's truncate and updates the cached size, below 32 it stays memfd's; `truncate(path)` opens, truncates, closes. New errno numbers in `abi.h`: `EEXIST 17`, `ENOTEMPTY 39`, `ENAMETOOLONG 36`, `EOPNOTSUPP 95`, `EXDEV 18`.

- [x] **Step 8: Verify, ext2 then ext4.** `make hd …` → boot → `runtests /etc/libc.tests`, `dtest files`, `dtest`, then quit and `sh tools/check-rootfs.sh`. The same with `make hd-ext4 …` (the recipe keeps the same image name). Expected: every check passes and `e2fsck -fn` is clean on both. A second boot on each image runs `filetest` again and still passes (it cleans up after itself).

**Found while doing it.** Two things fsck turned up. Freed inodes were dated
seconds after boot, and a deletion time below the inode count is how ext4's
orphan list links inodes — so the kernel now reads the CMOS clock at boot
(`SYS_BOOT_TIME`, ABI 2.1) and everything that tells the time uses the date.
And on ext4 a rename lost the name it had just added: the sector prefetch read
the disk around the open transaction and cached the old block, so the second
change to it in one transaction was made to the old contents.

- [x] **Step 9: Commit.** quark: "Files can be removed, renamed and shortened"; explosion: "filetest removes what it makes".

---
### Task 3: Directories a C program can read

**Files:**
- Modify: `quark/user/vfs/src/{main,protocol,ext2_dir}.rs`, `quark/docs/vfs.md`
- Modify: `quark/user/quark-rt/src/vfs.rs`, `quark/user/{ls,init,disktest}/src/main.rs`
- Modify: `quark/user/libc/{include/quark/vfs.h,src/quark.c}`, `quark/user/linux-abi/src/{syscall.c,files.c,abi.h}`
- Test: `explosion/toolchain/tests/dirtest.c`, `explosion/toolchain/tests/libc.tests`; dtest `files`

**Interfaces:**
- Consumes: Tasks 1–2.
- Produces (wire): `READDIR_BULK`(8) — `data[0]` handle, `data[1]` index of the first entry wanted, `data[2]` length of the buffer lent for writing (at most 4096); fills it with records `u64 id, u64 next, u64 size, u16 reclen, u8 type, u8 namelen, name, NUL`, each padded to 8 bytes (header 28); `type` is `DT_UNKNOWN 0, DT_DIR 4, DT_REG 8, DT_LNK 10`; reply `[bytes, next index, end (1/0)]`. A buffer too small for the first record gets `bytes = 0, end = 0`. `READDIR`(4) is retired. `STATFS`(14) lends 64 bytes for eight words: `magic bsize blocks bfree bavail files ffree namemax` (ext2/ext4 `0xEF53`, FAT32 `0x4d44`).
- Produces (Rust): `struct DirEntry { name: [u8; 256], name_len: usize, size: u64, is_dir: bool, id: u64, kind: u8 }` with `empty()` and `name_bytes()`; `struct Page { count: usize, next: u64, end: bool }`; `readdir_bulk(vfs, handle, start: u64, out: &mut [DirEntry]) -> Result<Page, u64>`; `readdir(vfs, handle, index: u32) -> Result<Option<DirEntry>, u64>` (one entry through the bulk call); `struct FsStat { magic, block_size, blocks, free_blocks, avail_blocks, files, free_files, name_max }` (all `u64`); `statfs(vfs) -> Result<FsStat, u64>`.
- Produces (C): `quark_vfs_readdir(handle, start, buf, len, *used, *next, *end)`, `quark_vfs_statfs(struct quark_vfs_statfs *)`; linux-abi `getdents64`(217), `readlink`(89)/`readlinkat`(267), `uname`(63), `getcwd`(79), `statfs`(137), `fstatfs`(138).

- [x] **Step 1: The failing test.** `dirtest.c` (no `LINK:`), added to `libc.tests`:

```c
/* Reading directories, and the calls realpath and df lean on. */
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <sys/utsname.h>
#include <unistd.h>

static int failed;

static void check(const char *what, int ok) {
    printf("  %s  %s\n", ok ? "ok  " : "FAIL", what);
    if (!ok) {
        failed++;
    }
}

#define TESTDIR "/tmp/dirtest"
#define COUNT 100
/* Long enough that a page of them needs more than one read. */
#define NAME "%s/entry-%03d-with-a-name-long-enough-to-need-more-than-one-page"

static int list(DIR *d, char seen[COUNT], int *dots, int *types_ok) {
    struct dirent *e;
    int n = 0;
    memset(seen, 0, COUNT);
    *dots = 0;
    *types_ok = 1;
    while ((e = readdir(d))) {
        int i;
        if (!strcmp(e->d_name, ".") || !strcmp(e->d_name, "..")) {
            (*dots)++;
            *types_ok &= e->d_type == DT_DIR;
        } else if (sscanf(e->d_name, "entry-%03d-", &i) == 1 && i >= 0 && i < COUNT) {
            seen[i]++;
            *types_ok &= e->d_type == DT_REG;
            n++;
        }
    }
    return n;
}

int main(void) {
    char path[256], seen[COUNT];
    int dots, types_ok;
    printf("directories:\n");
    mkdir(TESTDIR, 0755);
    for (int i = 0; i < COUNT; i++) {
        snprintf(path, sizeof path, NAME, TESTDIR, i);
        int fd = open(path, O_WRONLY | O_CREAT, 0644);
        if (fd >= 0) {
            close(fd);
        }
    }
    DIR *d = opendir(TESTDIR);
    check("open a directory", d != NULL);
    int n = d ? list(d, seen, &dots, &types_ok) : 0;
    int once = 1;
    for (int i = 0; i < COUNT; i++) {
        once &= seen[i] == 1;
    }
    check("every entry, once, across pages", n == COUNT && once);
    check("with . and ..", dots == 2);
    check("and the right types", types_ok);
    if (d) {
        rewinddir(d);
        check("again after rewinding", list(d, seen, &dots, &types_ok) == COUNT);
        closedir(d);
    }
    errno = 0;
    check("a file is not a directory", opendir("/etc/passwd") == NULL && errno == ENOTDIR);

    struct statfs fs;
    check("statfs says ext2", statfs("/", &fs) == 0 && fs.f_type == 0xEF53 && fs.f_blocks > 0 && fs.f_bfree <= fs.f_blocks);
    char link[64];
    check("readlink of a file is EINVAL", readlink("/etc/passwd", link, sizeof link) == -1 && errno == EINVAL);
    check("readlink of nothing is ENOENT", readlink("/no/such", link, sizeof link) == -1 && errno == ENOENT);
    char real[PATH_MAX];
    check("realpath resolves", realpath("/etc/../etc/passwd", real) && !strcmp(real, "/etc/passwd"));
    check("the working directory is the root", getcwd(real, sizeof real) && !strcmp(real, "/"));
    struct utsname u;
    check("uname says Quark", uname(&u) == 0 && !strcmp(u.sysname, "Quark") && !strcmp(u.machine, "x86_64"));

    int gone = 1;
    for (int i = 0; i < COUNT; i++) {
        snprintf(path, sizeof path, NAME, TESTDIR, i);
        gone &= unlink(path) == 0;
    }
    check("tidy up", gone && rmdir(TESTDIR) == 0);
    printf("dirtest: %s\n", failed ? "FAILED" : "ok");
    return failed ? 1 : 0;
}
```

dtest `files` gains: 80 files with 100-byte names in a new directory are all listed by paging `vfs::readdir_bulk` (a 16-entry `out` forces several calls), and `vfs::statfs` reports free blocks that go down by at least 64 KiB worth while a 64 KiB file exists and come back when it is unlinked.

- [x] **Step 2: Run it.** Expected: `opendir` works (open) but `readdir` returns nothing (`getdents64` is `ENOSYS`); `statfs`, `readlink`, `uname` fail. *(Built against the old layer only; its first boot came after Step 5.)*

- [x] **Step 3: The server.** `protocol.rs`:

```rust
pub const DIRENT_HEADER: usize = 28;
pub const DT_UNKNOWN: u8 = 0;
pub const DT_DIR: u8 = 4;
pub const DT_REG: u8 = 8;
pub const DT_LNK: u8 = 10;

/// Write one directory record at `at` in `buf`; its length, or None if it
/// does not fit.
pub fn put_dirent(buf: &mut [u8], at: usize, id: u64, next: u64, size: u64, kind: u8, name: &[u8]) -> Option<usize> {
    let reclen = (DIRENT_HEADER + name.len() + 1 + 7) & !7;
    if at + reclen > buf.len() {
        return None;
    }
    let r = &mut buf[at..at + reclen];
    r.fill(0);
    r[0..8].copy_from_slice(&id.to_le_bytes());
    r[8..16].copy_from_slice(&next.to_le_bytes());
    r[16..24].copy_from_slice(&size.to_le_bytes());
    r[24..26].copy_from_slice(&(reclen as u16).to_le_bytes());
    r[26] = kind;
    r[27] = name.len() as u8;
    r[DIRENT_HEADER..DIRENT_HEADER + name.len()].copy_from_slice(name);
    Some(reclen)
}
```

`ext2_dir::for_each_entry(ext2, dir, mut f: impl FnMut(u32 index, u32 ino, u8 ftype, &[u8] name) -> bool)` walks in-use entries in order and stops when `f` says so. The ext2 bulk handler skips entries before `data[1]`, reads each entry's inode for its size, maps `ftype` (1 regular, 2 directory, 7 symbolic link), and fills `CLIENT_BUF` up to the lent length; FAT32 does the same with `NAME.EXT` names made from the 8.3 fields (trailing spaces dropped, no dot without an extension). `TAG_STATFS` answers from the superblock counts (`Ext2State` keeps the reserved block count it reads at mount for `bavail`) or, on FAT32, the magic, the cluster size and zeros. `handle_readdir*` for tag 4 and the `DirEntryInfo` name copies go.

- [x] **Step 4: Rust clients.** quark-rt as in Interfaces: `readdir_bulk` parses records into `out` and reports the `next` of the last one it kept, so a caller with a small `out` loses nothing. `ls` pages through the directory with a 64-entry buffer and prints every entry; `init` and `disktest` keep calling `readdir(index)`, now with names up to 255 bytes.

- [x] **Step 5: C clients.** `quark_vfs_readdir` lends the caller's buffer; `quark_vfs_statfs` lends 64 bytes. In linux-abi, `struct openfile` gains `unsigned long dir_next` and `int dir_end` (reset by `lseek(fd, 0, SEEK_SET)`, set to the offset by any other seek, which is what `seekdir` passes back):

```c
/* getdents64: Quark's records, copied field by field into Linux's. */
long __quark_getdents(long fd, void *buf, unsigned long count) {
    struct openfile *f = slot(fd);
    if (!f) {
        return -LX_EBADF;
    }
    if (!f->is_dir) {
        return -LX_ENOTDIR;
    }
    if (f->dir_end) {
        return 0;
    }
    unsigned char page[4096];
    unsigned long want = count < sizeof page ? count : sizeof page;
    unsigned long used = 0, next = 0;
    int end = 0;
    int e = quark_vfs_readdir(f->handle, f->dir_next, page, want, &used, &next, &end);
    if (e) {
        return vfs_errno(e);
    }
    unsigned char *out = buf;
    unsigned long in = 0, put = 0;
    while (in + 28 <= used) {
        const unsigned char *r = page + in;
        unsigned long id = rd64(r, 0), after = rd64(r, 8);
        unsigned reclen = rd16(r, 24);
        unsigned namelen = r[27];
        unsigned long lreclen = (19 + namelen + 1 + 7) & ~7UL;
        if (put + lreclen > count) {
            break;
        }
        bytes_zero(out + put, lreclen);
        wr64(out + put, 0, id);
        wr64(out + put, 8, after);
        wr16(out + put, 16, (unsigned)lreclen);
        out[put + 18] = r[26];
        copy_bytes(out + put + 19, r + 28, namelen);
        put += lreclen;
        in += reclen;
        f->dir_next = after;
    }
    if (put == 0) {
        if (used == 0 && end) {
            f->dir_end = 1;
            return 0;
        }
        return -LX_EINVAL; /* the caller's buffer holds no entry */
    }
    if (in >= used && end) {
        f->dir_end = 1;
    }
    return (long)put;
}
```

`rd64`/`rd16`/`wr64`/`wr16`/`copy_bytes` are little-endian byte helpers beside `bytes_zero`. `readlink`: the path exists and is not a link → `EINVAL`, a link → `EOPNOTSUPP` (links cannot be read yet), missing → the open's error. `uname`: `Quark`, `quark`, the kernel's ABI version as `major.minor`, `Quark microkernel`, `x86_64`, domain `(none)`. `getcwd`: `/` — relative paths resolve from the root here, which this now says rather than refusing. `statfs`/`fstatfs` fill Linux's `struct statfs` (`f_type, f_bsize, f_blocks, f_bfree, f_bavail, f_files, f_ffree, f_fsid, f_namelen, f_frsize, f_flags, f_spare`).

- [x] **Step 6: Verify.** ext2 boot: `runtests /etc/libc.tests` (tlstest, ctortest, mmaptest, filetest, dirtest), `dtest files`, `dtest`, `ls /usr/bin` (more than 64 names), `cat /etc/passwd`; `tools/check-rootfs.sh`. Then `make hd-fat32 …` and boot once: `ls /`, `ls /usr/bin`, `cat /etc/passwd`, `dtest spaces` (it loads a program by path) — FAT32 still reads and lists, and says no to what it cannot do.

- [x] **Step 7: Commit.** quark: "C programs can read directories"; explosion: "dirtest".

---
### Task 4: The sources, the tools, and zlib

**Files:**
- Create: `explosion/toolchain/bootstrap-fonts.sh`, `explosion/toolchain/teach-config-sub.sh`, `explosion/toolchain/build-zlib.sh`
- Create: `explosion/toolchain/tests/ztest.c`, `explosion/toolchain/tests/zlib.tests`
- Modify: `explosion/toolchain/build-musl.sh` (the wrapper), `explosion/toolchain/build-libffi.sh` (uses the helper)

**Interfaces:**
- Produces: `libz.a`, `zlib.pc` in the musl prefix; `gperf` in `$HOSTDEPS/bin`; `teach-config-sub.sh <config.sub>`; the suite `/tmp/claude-1000/suite-zlib` (`example`, `minigzip`).

- [x] **Step 1: The failing test.** `ztest.c`:

```c
// LINK: -lz
/* zlib, in memory and through a file. `ztest prepare` and `ztest verify`
   bracket a minigzip round trip in zlib.tests. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <zlib.h>

#define DATA "/tmp/zdata"
#define LEN (64 * 1024)

static int failed;

static void check(const char *what, int ok) {
    printf("  %s  %s\n", ok ? "ok  " : "FAIL", what);
    if (!ok) {
        failed++;
    }
}

/* Compressible and not trivially so. */
static void pattern(unsigned char *p, size_t n) {
    unsigned x = 12345;
    for (size_t i = 0; i < n; i++) {
        x = x * 1103515245u + 12345u;
        p[i] = (unsigned char)("quark zlib "[i % 11] ^ ((x >> 16) & 3));
    }
}

static int prepare(void) {
    unsigned char *p = malloc(LEN);
    pattern(p, LEN);
    FILE *f = fopen(DATA, "wb");
    int ok = f && fwrite(p, 1, LEN, f) == LEN;
    ok &= f && fclose(f) == 0;
    free(p);
    return ok ? 0 : 1;
}

static int verify(void) {
    unsigned char *want = malloc(LEN), *got = malloc(LEN + 1);
    pattern(want, LEN);
    FILE *f = fopen(DATA, "rb");
    size_t n = f ? fread(got, 1, LEN + 1, f) : 0;
    if (f) {
        fclose(f);
    }
    int ok = n == LEN && !memcmp(want, got, LEN);
    printf("ztest: %s after minigzip and back\n", ok ? "identical" : "DIFFERENT");
    free(want);
    free(got);
    return ok && remove(DATA) == 0 ? 0 : 1;
}

int main(int argc, char **argv) {
    if (argc > 1 && !strcmp(argv[1], "prepare")) {
        return prepare();
    }
    if (argc > 1 && !strcmp(argv[1], "verify")) {
        return verify();
    }
    printf("zlib %s:\n", zlibVersion());
    check("crc32 of 123456789", crc32(0, (const Bytef *)"123456789", 9) == 0xCBF43926u);
    check("adler32 of Wikipedia", adler32(1, (const Bytef *)"Wikipedia", 9) == 0x11E60398u);

    unsigned char *src = malloc(LEN), *back = malloc(LEN);
    pattern(src, LEN);
    for (int level = 1; level <= 9; level += 4) {
        uLongf clen = compressBound(LEN);
        unsigned char *c = malloc(clen);
        uLongf blen = LEN;
        int ok = compress2(c, &clen, src, LEN, level) == Z_OK && clen < LEN
              && uncompress(back, &blen, c, clen) == Z_OK && blen == LEN && !memcmp(src, back, LEN);
        char what[64];
        snprintf(what, sizeof what, "level %d round trip (%lu bytes)", level, (unsigned long)clen);
        check(what, ok);
        free(c);
    }

    gzFile gz = gzopen("/tmp/ztest.gz", "wb");
    int wrote = gz && gzwrite(gz, src, LEN) == LEN && gzclose(gz) == Z_OK;
    check("write a gzip file", wrote);
    gz = gzopen("/tmp/ztest.gz", "rb");
    memset(back, 0, LEN);
    int read_back = gz && gzread(gz, back, LEN) == LEN && !memcmp(src, back, LEN);
    check("and read it back", read_back);
    check("to its end", gz && gzread(gz, back, 1) == 0 && gzeof(gz));
    if (gz) {
        gzclose(gz);
    }
    check("and remove it", remove("/tmp/ztest.gz") == 0);
    printf("ztest: %s\n", failed ? "FAILED" : "ok");
    return failed ? 1 : 0;
}
```

`zlib.tests`:

```
# zlib: ours, then zlib's own. Expected: all passed.
ztest
# example leaves the gzip file it names; minigzip overwrites and removes it.
example /tmp/zdata.gz
ztest prepare
minigzip /tmp/zdata
minigzip -d /tmp/zdata.gz
ztest verify
```

- [x] **Step 2: Run it.** `sh toolchain/build-tests.sh /tmp/claude-1000/suite-c` → `ztest skipped, not built yet: z`.

- [x] **Step 3: The wrapper.** `build-musl.sh` writes the wrapper the machine already has (the rotating loop, no `eval`) plus:

```sh
# -fPIC, -fpic, -fPIE, -fpie and -pie are dropped too. Nothing here is a
# shared library or a position-independent executable, and with the large
# code model a program built -fPIC reaches its globals through a GOT whose
# base it never sets up: their addresses come out as zero. libwayland found
# that; zlib's configure adds -fPIC whatever it is told.
	case "$a" in
	-pthread|-fPIC|-fpic|-fPIE|-fpie|-pie) ;;
	*) set -- "$@" "$a" ;;
	esac
```

and the installed `~/opt/cross/bin/x86_64-quark-musl-gcc` is regenerated from it (the heredoc section only). Check: `x86_64-quark-musl-gcc -fPIC -O2 -c` of a file with a global's address, then `objdump -dr` shows no `GOTPCREL`.

- [x] **Step 4: `teach-config-sub.sh`.** The hunk `build-libffi.sh` carries, as a script both use:

```sh
#!/bin/sh
# Teach an autoconf config.sub that quark is an operating system.
#
#     ./teach-config-sub.sh path/to/config.sub
#
# The same one line every autoconf package needs. Idempotent.
set -e
F=${1:?usage: teach-config-sub.sh <config.sub>}
grep -q '| quark\*' "$F" && exit 0
python3 - "$F" <<'PY'
import sys
p = sys.argv[1]
s = open(p).read()
a = "\t     | nsk* | powerunix* | genode* | zvmoe* | qnx* | emx* | zephyr* \\\n"
if s.count(a) != 1:
    sys.exit(p + ": the OS list is not where this expects it")
open(p, "w").write(s.replace(a, a + "\t     | quark* \\\n"))
PY
echo "==> $F knows quark"
```

- [x] **Step 5: `bootstrap-fonts.sh`.** Same shape as `bootstrap-wayland.sh`: fetch into `$QUARK_SRC` (default `~/opt/src`), check each tarball against a pinned SHA-256, unpack if absent; build `gperf` into `$QUARK_HOSTDEPS` if `$QUARK_HOSTDEPS/bin/gperf` is missing. URLs and sums:

```
zlib-1.3.2.tar.xz               https://zlib.net/                                   d7a0654783a4da529d1bb793b7ad9c3318020af77667bcae35f95d0e42a792f3
freetype-2.14.3.tar.xz          https://download.savannah.gnu.org/releases/freetype/ 36bc4f1cc413335368ee656c42afca65c5a3987e8768cc28cf11ba775e785a5f
fontconfig-2.18.3.tar.xz        https://gitlab.freedesktop.org/api/v4/projects/890/packages/generic/fontconfig/2.18.3/ 4f7b554a38cdf78c033f666c8871f3749e14a094f65a07f630c91ed0b43d35e3
libxkbcommon-1.13.2.tar.gz      https://github.com/xkbcommon/libxkbcommon/archive/refs/tags/xkbcommon-1.13.2.tar.gz acc4d5f7c3cbba5f9f8d08d8bdbeede84ecede46792f47929aa9321873385528
dejavu-fonts-ttf-2.37.tar.bz2   https://github.com/dejavu-fonts/dejavu-fonts/releases/download/version_2_37/ fa9ca4d13871dd122f61258a80d01751d603b4d3ee14095d65453b4e846e17d7
gperf-3.3.tar.gz                https://ftp.gnu.org/gnu/gperf/                      fd87e0aba7e43ae054837afd6cd4db03a3f2693deb3619085e6ed9d8d9604ad8
```

(libxkbcommon unpacks as `libxkbcommon-xkbcommon-1.13.2`.) Run it: everything reports "already unpacked", and gperf builds once.

- [x] **Step 6: `build-zlib.sh`.**

```sh
#!/bin/sh
# Build zlib for Quark, and zlib's own two test programs.
#
#     ./build-zlib.sh /path/to/zlib-1.3.2 [outdir]
#
# zlib's configure is its own, not autoconf: CHOST names the cross tools. It
# adds -fPIC whatever it is told; the compiler wrapper drops it.
set -e
SRC=${1:?usage: build-zlib.sh <zlib-src> [outdir]}
OUT=${2:-$PWD/zlib-tests}
PREFIX=${PREFIX:-$HOME/opt/cross/x86_64-quark/musl}
mkdir -p "$OUT"
cd "$SRC"
[ -f Makefile ] && make distclean >/dev/null 2>&1 || true
CHOST=x86_64-quark CC=x86_64-quark-musl-gcc CFLAGS=-O2 ./configure --static --prefix="$PREFIX"
make -j"$(nproc)" libz.a example minigzip
make install
cp example minigzip "$OUT"/
echo "zlib installed into $PREFIX; its tests are in $OUT"
```

- [x] **Step 7: Verify.** `build-zlib.sh ~/opt/src/zlib-1.3.2 /tmp/claude-1000/suite-zlib`, `build-tests.sh /tmp/claude-1000/suite-c`, image with `TEST_SUITES` + `suite-zlib`, boot, `runtests /etc/zlib.tests` (6 passed), `runtests /etc/libc.tests`, `ls /tmp` shows no leftovers; `tools/check-rootfs.sh` clean.

- [x] **Step 8: Commit.** explosion: "zlib, and the tools the font stack needs".

---
### Task 5: FreeType, and fonts on the disk

**Files:**
- Create: `explosion/toolchain/build-freetype.sh`, `explosion/toolchain/stage-fonts.sh`, `explosion/tools/stage-overlays.sh`, `explosion/tools/populate-ext.sh`
- Create: `explosion/toolchain/tests/fttest.c`, `explosion/toolchain/tests/fonts.tests`
- Modify: `explosion/Makefile` (`ROOT_OVERLAYS`, the rootfs rule, `ROOTFS_SIZE_KB := 65536`)

**Interfaces:**
- Consumes: Task 4's zlib; Task 3's directories (nothing here lists them yet, but `populate-ext.sh` makes nested ones).
- Produces: `libfreetype.a`, `freetype2.pc`; host FreeType in `$SRC/build-host/root`; overlay `usr/share/fonts/dejavu/{DejaVuSans,DejaVuSans-Bold,DejaVuSansMono,DejaVuSansMono-Bold}.ttf` + `LICENSE`; `make … ROOT_OVERLAYS="dir …"`.

- [x] **Step 1: The failing test.** `fttest.c`:

```c
// PKG: freetype2
/* FreeType renders glyphs from a font on disk, exactly as it does on the
   host. The checksum covers every bitmap and metric, so a difference in the
   rasteriser, the hinter or the file shows. */
#include <stdio.h>
#include <string.h>
#include <ft2build.h>
#include FT_FREETYPE_H

/* What the same FreeType, built the same way, says on the host. */
#define EXPECTED 0x00000000u
#define GLYPHS 0

static unsigned fnv(unsigned h, const unsigned char *p, size_t n) {
    while (n--) {
        h = (h ^ *p++) * 16777619u;
    }
    return h;
}

static unsigned mix(unsigned h, long v) {
    return fnv(h, (const unsigned char *)&v, sizeof v);
}

int main(int argc, char **argv) {
    const char *font = argc > 1 ? argv[1] : "/usr/share/fonts/dejavu/DejaVuSans.ttf";
    FT_Library lib;
    FT_Face face;
    int failed = 0;
    if (FT_Init_FreeType(&lib) || FT_New_Face(lib, font, 0, &face)) {
        printf("fttest: cannot open %s\n", font);
        return 1;
    }
    printf("fttest: %s %s, %ld glyphs (expected %d)\n", face->family_name,
           face->style_name, face->num_glyphs, GLYPHS);
    failed |= strcmp(face->family_name, "DejaVu Sans") != 0;
    failed |= face->num_glyphs != GLYPHS;
    FT_Set_Pixel_Sizes(face, 0, 32);
    unsigned h = 2166136261u;
    for (const char *s = "Quark 13 renders text"; *s; s++) {
        if (FT_Load_Char(face, (unsigned char)*s, FT_LOAD_RENDER)) {
            failed = 1;
            continue;
        }
        FT_GlyphSlot g = face->glyph;
        h = mix(h, g->bitmap.width);
        h = mix(h, g->bitmap.rows);
        h = mix(h, g->bitmap_left);
        h = mix(h, g->bitmap_top);
        h = mix(h, g->advance.x);
        for (unsigned r = 0; r < g->bitmap.rows; r++) {
            h = fnv(h, g->bitmap.buffer + r * (unsigned)g->bitmap.pitch, g->bitmap.width);
        }
    }
    printf("fttest: checksum %08X, expected %08X\n", h, EXPECTED);
    failed |= h != EXPECTED;
    FT_Done_Face(face);
    FT_Done_FreeType(lib);
    printf("fttest: %s\n", failed ? "FAILED" : "ok");
    return failed;
}
```

`fonts.tests`: `fttest`. `EXPECTED`/`GLYPHS` are placeholders until Step 4 prints the host's values.

- [x] **Step 2: Run it.** `fttest skipped, not built yet: freetype`. *(With `// PKG: freetype2` — FreeType's headers need a `-I`, which `build-tests.sh` now asks pkg-config for — it says `freetype2`.)*

- [x] **Step 3: `build-freetype.sh`.** For Quark: the cross file as `build-cairo.sh` makes it; `OPTIONS="--buildtype=debugoptimized -Ddefault_library=static -Db_staticpic=false --wrap-mode=nofallback -Dmmap=disabled -Dpng=disabled -Dbrotli=disabled -Dbzip2=disabled -Dharfbuzz=disabled -Dtests=disabled"` plus `-Dzlib=system`; setup `build-quark`, build, install. For the host: the same options with `-Dzlib=internal`, prefix `$SRC/build-host/root`, `--libdir=lib`, install; compile `tests/fttest.c` against it with `pkg-config --cflags --libs --static freetype2` and run it on `${DEJAVU:-$HOME/opt/src/dejavu-fonts-ttf-2.37/ttf}/DejaVuSans.ttf`, printing "on the host, which is what tests/fttest.c's EXPECTED and GLYPHS should say". The header comment records the decision (read, don't map) and why.

- [x] **Step 4: Expected values.** Run the script; put the host's checksum and glyph count into `fttest.c`.

- [x] **Step 5: Fonts as an overlay.** `stage-fonts.sh <dejavu-dir> <overlay>` copies the four faces and `LICENSE` into `<overlay>/usr/share/fonts/dejavu/`, with a comment on the licence's one condition (the notice travels with the fonts). `tools/stage-overlays.sh <stage> [overlay…]`:

```sh
#!/bin/sh
# Copy directory trees laid out like the root filesystem into the stage.
#
#     tools/stage-overlays.sh <stage-dir> [overlay...]
#
# Fonts and configuration are built by scripts in toolchain/ that need the
# cross toolchain, the same arrangement as COREUTILS. What was staged is
# recorded, so a later stage without an overlay takes its files back out, and
# the directories that leaves empty go too.
set -e
STAGE=${1:?usage: stage-overlays.sh <stage-dir> [overlay...]}
shift
# Outside usr, etc and var, so the image rules do not install the bookkeeping.
LIST=$STAGE/.overlays
# One path per line, whatever is in it.
set -f
IFS='
'
if [ -f "$LIST" ]; then
    while read -r path; do
        if [ -n "$path" ]; then
            rm -f "$STAGE/$path"
        fi
    done < "$LIST"
    rm -f "$LIST"
    for top in usr etc var; do
        if [ -d "$STAGE/$top" ]; then
            find "$STAGE/$top" -mindepth 1 -depth -type d -empty -delete
        fi
    done
    # Quark's install fills usr and etc; nothing but an overlay makes var.
    rmdir "$STAGE/var" 2>/dev/null || true
fi
for dir in "$@"; do
    if [ ! -d "$dir" ]; then
        echo "overlay: $dir is not a directory" >&2
        exit 1
    fi
    for d in $(cd "$dir" && find . -mindepth 1 -type d | sed 's|^\./||'); do
        mkdir -p "$STAGE/$d"
    done
    n=0
    for f in $(cd "$dir" && find . \( -type f -o -type l \) | sed 's|^\./||'); do
        cp -L "$dir/$f" "$STAGE/$f"
        echo "$f" >> "$LIST"
        n=$((n + 1))
    done
    echo "overlay: staged $n files from $dir"
done
```

- [x] **Step 6: The image from the stage.** `tools/populate-ext.sh <image> <stage>` replaces the per-file `debugfs` loop in `ROOTFS_RULE`:

```sh
#!/bin/sh
# Fill an ext2 or ext4 image from the staged root, with one debugfs run.
#
#     tools/populate-ext.sh <image> <stage-dir>
#
# Every staged directory under usr, etc and var is made, and every file in
# them written. Quark's own install names its files the way FAT wants them,
# HELLO.ELF and PASSWD, in usr/bin and etc; there the image gets the names the
# shell and init look for, hello and passwd. Everywhere else, and for any name
# with a lowercase letter in it, the staged name was chosen on purpose and is
# kept: DejaVuSans.ttf, and the LICENSE beside it.
#
# debugfs exits 0 whatever happened, so its complaints are read instead, and
# every file is looked for afterwards. Neither is enough alone: a write that
# runs out of room leaves the file's name and size behind with no blocks, so
# it is found, reads as zeros, and e2fsck calls the image clean.
set -e
if [ $# -ne 2 ]; then
    echo "usage: populate-ext.sh <image> <stage-dir>" >&2
    exit 2
fi
IMG=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
cd "$2"

target() {
    dir=$(dirname "$1")
    base=$(basename "$1")
    case "$dir:$base" in
    usr/bin:*[a-z]* | etc:*[a-z]*) ;;
    usr/bin:* | etc:*)
        base=$(printf '%s' "$base" | tr '[:upper:]' '[:lower:]' | sed 's/\.elf$//') ;;
    esac
    printf '%s/%s\n' "$dir" "$base"
}

CMDS=$(mktemp)
OUT=$(mktemp)
ERR=$(mktemp)
trap 'rm -f "$CMDS" "$OUT" "$ERR"' EXIT
{
    printf 'mkdir home\nmkdir home/root\nmkdir tmp\n'
    find usr etc var -type d 2>/dev/null | sort | sed 's/^/mkdir /'
    find usr etc var -type f 2>/dev/null | sort | while read -r f; do
        printf 'write %s %s\n' "$f" "$(target "$f")"
    done
} > "$CMDS"
debugfs -w -f "$CMDS" "$IMG" >/dev/null 2>"$ERR"
# After its banner, a run where everything worked says nothing on stderr.
if grep -v '^debugfs [0-9]' "$ERR" >&2; then
    echo "$IMG: debugfs could not write everything" >&2
    exit 1
fi

find usr etc var -type f 2>/dev/null | sort | while read -r f; do
    printf 'stat %s\n' "$(target "$f")"
done > "$CMDS"
debugfs -f "$CMDS" "$IMG" > "$OUT" 2>&1 || true
# The error names the path. The command echo that precedes it on stdout is
# buffered, so the two do not reliably arrive in order.
if grep -q 'File not found' "$OUT"; then
    grep 'File not found' "$OUT" | sed 's/: File not found.*//; s/^/  MISSING from the image: /' >&2
    echo "$IMG is incomplete" >&2
    exit 1
fi
```

`ROOTFS_RULE` becomes `dd`, `mkfs`, `./tools/populate-ext.sh $(1) $(STAGE)`. The `stage` target runs `./tools/stage-overlays.sh $(STAGE) $(ROOT_OVERLAYS)` after the test suites. `ROOTFS_SIZE_KB := 65536` (fonts, the font tools and the new tests would leave the old 33 MiB root nearly full).

- [x] **Step 7: Verify.** `build-freetype.sh ~/opt/src/freetype-2.14.3`; `stage-fonts.sh ~/opt/src/dejavu-fonts-ttf-2.37/ttf /tmp/claude-1000/overlay-fonts`; `build-tests.sh`; `make hd … ROOT_OVERLAYS=/tmp/claude-1000/overlay-fonts`; `debugfs -R 'ls -l /usr/share/fonts/dejavu' rootfs-ext2.img` shows the four mixed-case names; boot; `runtests /etc/fonts.tests` passes with the host's numbers; `ls /usr/share/fonts/dejavu`; `runtests /etc/libc.tests`; `dtest`; `wm weston-simple-shm wlcairo`; `check-rootfs.sh` clean. `make hd` without `ROOT_OVERLAYS` afterwards leaves no fonts in the stage.

- [x] **Step 8: Commit.** explosion: "FreeType, and fonts on the disk".

---
### Task 6: expat

**Files:**
- Create: `explosion/toolchain/build-expat.sh`, `explosion/toolchain/tests/xmltest.c`, `explosion/toolchain/tests/xml.tests`

**Interfaces:**
- Consumes: `teach-config-sub.sh`.
- Produces: `libexpat.a`, `expat.pc` in the musl prefix.

- [x] **Step 1: The failing test.** `xmltest.c`:

```c
// LINK: -lexpat
/* expat parses what fontconfig will hand it, and says where a bad document
   goes wrong. */
#include <stdio.h>
#include <string.h>
#include <expat.h>

static int failed;

static void check(const char *what, int ok) {
    printf("  %s  %s\n", ok ? "ok  " : "FAIL", what);
    if (!ok) {
        failed++;
    }
}

struct seen {
    int elements, dirs, prefixed;
    char text[256];
    size_t len;
    int in_dir;
};

static void start(void *u, const XML_Char *name, const XML_Char **attr) {
    struct seen *s = u;
    s->elements++;
    if (!strcmp(name, "dir")) {
        s->dirs++;
        s->in_dir = 1;
        for (int i = 0; attr[i]; i += 2) {
            if (!strcmp(attr[i], "prefix") && !strcmp(attr[i + 1], "xdg")) {
                s->prefixed++;
            }
        }
    }
}

static void end(void *u, const XML_Char *name) {
    struct seen *s = u;
    if (!strcmp(name, "dir")) {
        s->in_dir = 0;
        if (s->len < sizeof s->text - 1) {
            s->text[s->len++] = '|';
        }
    }
}

static void chars(void *u, const XML_Char *p, int n) {
    struct seen *s = u;
    if (s->in_dir) {
        for (int i = 0; i < n && s->len < sizeof s->text - 1; i++) {
            s->text[s->len++] = p[i];
        }
    }
}

static const char DOC[] =
    "<?xml version=\"1.0\"?>\n"
    "<!DOCTYPE fontconfig SYSTEM \"urn:fontconfig:fonts.dtd\">\n"
    "<fontconfig>\n"
    "  <dir>/usr/share/fonts</dir>\n"
    "  <dir prefix=\"xdg\">fonts</dir>\n"
    "  <match target=\"pattern\"><test name=\"family\"><string>mono &amp; co</string></test></match>\n"
    "</fontconfig>\n";

int main(void) {
    printf("expat %s:\n", XML_ExpatVersion());
    struct seen s = {0};
    XML_Parser p = XML_ParserCreate(NULL);
    XML_SetUserData(p, &s);
    XML_SetElementHandler(p, start, end);
    XML_SetCharacterDataHandler(p, chars);
    /* Fed in small pieces, so the parser's buffering is what is tested. */
    int ok = 1;
    for (size_t i = 0; i < sizeof DOC - 1; i += 7) {
        size_t n = sizeof DOC - 1 - i < 7 ? sizeof DOC - 1 - i : 7;
        ok &= XML_Parse(p, DOC + i, (int)n, 0) == XML_STATUS_OK;
    }
    ok &= XML_Parse(p, "", 0, 1) == XML_STATUS_OK;
    s.text[s.len] = 0;
    check("a fontconfig document parses in pieces", ok);
    check("with all six elements", s.elements == 6);
    check("and both directories", s.dirs == 2 && !strcmp(s.text, "/usr/share/fonts|fonts|"));
    check("and their attributes", s.prefixed == 1);
    XML_ParserFree(p);

    p = XML_ParserCreate(NULL);
    int bad = XML_Parse(p, "<a>\n<b></a>", 11, 1) == XML_STATUS_ERROR;
    check("a mismatched tag is an error", bad && XML_GetErrorCode(p) == XML_ERROR_TAG_MISMATCH);
    check("on line 2", XML_GetCurrentLineNumber(p) == 2);
    XML_ParserFree(p);
    printf("xmltest: %s\n", failed ? "FAILED" : "ok");
    return failed ? 1 : 0;
}
```

`xml.tests`: `xmltest`.

- [x] **Step 2: Run it.** `xmltest skipped, not built yet: expat`. *(Run on the host against bootstrap-wayland's expat first, which caught the element count: the document has six, not seven.)*

- [x] **Step 3: `build-expat.sh <expat tarball>`.** Unpacks a private tree (`$QUARK_SRC/expat-2.6.4-quark`, removed first) because the host's copy is configured in place and autoconf refuses a second configure against it; `teach-config-sub.sh conftools/config.sub`; `./configure --host=x86_64-quark CC=x86_64-quark-musl-gcc CFLAGS=-O2 --prefix="$PREFIX" --disable-shared --enable-static --without-docbook --without-tests --without-examples --without-xmlwf`; `make`; `make install`.

- [x] **Step 4: Verify.** Build, `build-tests.sh`, image, boot: `runtests /etc/xml.tests` passes; the earlier suites still pass.

- [x] **Step 5: Commit.** explosion: "expat, cross-built".

---

### Task 7: fontconfig, with a cache it writes

**Files:**
- Create: `explosion/toolchain/build-fontconfig.sh`, `explosion/toolchain/patches/fontconfig-2.18.3-quark.patch`
- Create: `explosion/toolchain/tests/fctest.c`, `explosion/toolchain/tests/fontconfig.tests`

**Interfaces:**
- Consumes: FreeType, expat, zlib; `gperf`; Tasks 1–3 (everything the cache writer does); the fonts overlay.
- Produces: `libfontconfig.a`, `fontconfig.pc`; the suite `/tmp/claude-1000/suite-fc` (`fc-cache fc-cat fc-conflist fc-list fc-match fc-pattern fc-query fc-scan fc-validate`); in the overlay, `etc/fonts/fonts.conf`, `etc/fonts/conf.d/*.conf` (real files), and an empty `var/cache/fontconfig`.

- [x] **Step 1: The failing test.** `fctest.c`:

```c
// LINK: -lfontconfig -lfreetype -lexpat -lz -lm
/* fontconfig finds the fonts on disk, answers the generic families with
   them, and uses the cache fc-cache wrote. */
#include <stdio.h>
#include <string.h>
#include <fontconfig/fontconfig.h>

static int failed;

static void check(const char *what, int ok) {
    printf("  %s  %s\n", ok ? "ok  " : "FAIL", what);
    if (!ok) {
        failed++;
    }
}

static int matches(const char *pattern, const char *want) {
    FcPattern *p = FcNameParse((const FcChar8 *)pattern);
    FcConfigSubstitute(NULL, p, FcMatchPattern);
    FcDefaultSubstitute(p);
    FcResult r;
    FcPattern *m = FcFontMatch(NULL, p, &r);
    FcChar8 *file = NULL;
    int ok = m && FcPatternGetString(m, FC_FILE, 0, &file) == FcResultMatch
             && !strcmp((const char *)file, want);
    if (!ok) {
        printf("        %s gave %s\n", pattern, file ? (const char *)file : "nothing");
    }
    if (m) {
        FcPatternDestroy(m);
    }
    FcPatternDestroy(p);
    return ok;
}

int main(void) {
    printf("fontconfig %d:\n", FcGetVersion());
    check("initialise", FcInit());
    FcPattern *all = FcPatternCreate();
    FcObjectSet *os = FcObjectSetBuild(FC_FAMILY, FC_FILE, (char *)0);
    FcFontSet *fs = FcFontList(NULL, all, os);
    int sans = 0, mono = 0;
    for (int i = 0; fs && i < fs->nfont; i++) {
        FcChar8 *family;
        if (FcPatternGetString(fs->fonts[i], FC_FAMILY, 0, &family) == FcResultMatch) {
            sans += !strcmp((const char *)family, "DejaVu Sans");
            mono += !strcmp((const char *)family, "DejaVu Sans Mono");
        }
    }
    check("the fonts on disk are listed", fs && fs->nfont == 4);
    check("DejaVu Sans among them", sans == 2);
    check("and DejaVu Sans Mono", mono == 2);
    check("sans-serif is DejaVu Sans", matches("sans-serif", "/usr/share/fonts/dejavu/DejaVuSans.ttf"));
    check("bold is its bold face", matches("sans-serif:bold", "/usr/share/fonts/dejavu/DejaVuSans-Bold.ttf"));
    check("monospace is DejaVu Sans Mono", matches("monospace", "/usr/share/fonts/dejavu/DejaVuSansMono.ttf"));

    FcChar8 *cache_file = NULL;
    FcCache *cache = FcDirCacheLoad((const FcChar8 *)"/usr/share/fonts/dejavu", NULL, &cache_file);
    check("the directory's cache loads", cache != NULL);
    check("from /var/cache/fontconfig",
          cache_file && !strncmp((const char *)cache_file, "/var/cache/fontconfig/", 22));
    if (cache) {
        FcDirCacheUnload(cache);
    }
    FcFontSetDestroy(fs);
    FcObjectSetDestroy(os);
    FcPatternDestroy(all);
    FcFini();
    printf("fctest: %s\n", failed ? "FAILED" : "ok");
    return failed ? 1 : 0;
}
```

`fontconfig.tests`:

```
# fontconfig: the cache is written, then used. Expected: all passed.
fc-cache -v
fctest
fc-list
fc-match monospace
```

- [x] **Step 2: Run it.** `fctest skipped, not built yet: fontconfig`.

- [x] **Step 3: The patch.** `patches/fontconfig-2.18.3-quark.patch` is the one hunk in `src/fcstat.c` (`defined(__quark__)` beside `defined(__linux__)` where `f_type` is read), with a header line saying why: Quark's C library is musl, whose `struct statfs` is Linux's.

- [x] **Step 4: `build-fontconfig.sh <src> <suite-out> <overlay-out>`.** Puts `$QUARK_HOSTDEPS/bin` on `PATH` and fails early without `gperf`; applies the patch unless `src/fcstat.c` already mentions `__quark__`; `meson setup build-quark --cross-file … --prefix="$PREFIX" --sysconfdir=/etc --localstatedir=/var --buildtype=debugoptimized -Ddefault_library=static -Db_staticpic=false --wrap-mode=nofallback -Dxml-backend=expat -Ddoc=disabled -Dnls=disabled -Dtests=disabled -Dcache-build=disabled -Dtools=enabled -Diconv=disabled -Dfontations=disabled`; build; `DESTDIR=$tmp meson install --no-rebuild`; copy `$tmp$PREFIX/lib` and `include` into `$PREFIX`; the `fc-*` programs and `fontconfig.tests` into the suite; `fonts.conf`, every `conf.d/*.conf` dereferenced (they are links into a `conf.avail` the image does not carry), and an empty `var/cache/fontconfig` into the overlay. The header says why it installs through `DESTDIR` (its configuration belongs in the target's `/etc`, not the host's).

- [x] **Step 5: Verify.** Build it; `build-tests.sh`; image with `suite-fc` and the overlay; boot: `runtests /etc/fontconfig.tests` (4 passed — `fc-cache -v` wrote the caches, `fctest` loaded one), `ls /var/cache/fontconfig` (cache files, `CACHEDIR.TAG`, no `.LCK` or `.TMP-` leftovers), `fc-match sans-serif:bold`, run the suite a second time (the cache is reused: `fc-cache -v` says "skipping"), then `check-rootfs.sh` — clean after fontconfig's lock, rename and unlink dance. The same on `make hd-ext4`.

- [x] **Step 6: Commit.** explosion: "fontconfig, with a cache it writes".

**What Task 7 found** (recorded after the fact):

- fontconfig's configure looks for `mkostemp` without `_GNU_SOURCE`, which musl (like glibc) needs, so it makes its lock file with `mkstemp` and `fcntl(F_DUPFD_CLOEXEC)` — the ordinary Linux path. The Linux layer refused to duplicate a VFS file descriptor, so every cache write failed and left a `.TMP-` file behind. Fixed in the layer: descriptors name a shared open file, reference counted, and `dup`, `dup2` and `dup3` are answered (`filetest` "second descriptors"). quark: "A file can have more than one descriptor".
- With the caches failing, fontconfig scanned every font twice per run, and about one run in two hung for good. A task dump from the idle loop showed the VFS runnable and never run: a call's hand-over reopened interrupts between marking the callee runnable (unqueued) and switching to it. Fixed in the kernel, with `dtest calls` as the test (three million timed calls in three seconds; it hung on its first run before). quark: "A call's hand-over no longer strands the task called".
- fontconfig 2.18 writes the caches while it loads its configuration, before `fc-cache` looks, so even the first `fc-cache -v` on a fresh image says "skipping, existing cache is valid". The image's `/var/cache/fontconfig` is empty before boot and holds the two caches and `CACHEDIR.TAG` after.
- It builds ten tools (`fc-genconf` is new), and `-Dadditional-fonts-dirs=no` keeps the build machine's X11 font directories out of `fonts.conf`.
- `tools/drive-qemu.py` gained `hmp <command>`: `hmp info registers` said the CPU was halted in the kernel, which is what made this a deadlock rather than slow work.

---
### Task 8: cairo draws text

**Files:**
- Modify: `explosion/toolchain/build-cairo.sh`, `explosion/toolchain/build-weston-client.sh`, `explosion/toolchain/wlcairo.c`
- Create: `explosion/toolchain/tests/cairotext.c`; Modify: `explosion/toolchain/tests/{cairotest.c,cairo.tests}`

**Interfaces:**
- Consumes: FreeType (Quark and host), fontconfig, the fonts overlay.
- Produces: `libcairo.a` with `CAIRO_HAS_FT_FONT` and `CAIRO_HAS_FC_FONT`; `wlcairo` showing text.

- [ ] **Step 1: The failing test.** `cairotext.c`:

```c
// LINK: -lcairo -lpixman-1 -lfontconfig -lfreetype -lexpat -lz -lm
/* Text drawn by cairo with glyphs FreeType rasterised from a font on disk —
   first from a face opened directly, which must match the host bit for bit,
   then through fontconfig, which must find DejaVu Sans. */
#include <stdio.h>
#include <string.h>
#include <cairo/cairo.h>
#include <cairo/cairo-ft.h>
#include <ft2build.h>
#include FT_FREETYPE_H

#define EXPECTED 0x00000000u

static int failed;

static void check(const char *what, int ok) {
    printf("  %s  %s\n", ok ? "ok  " : "FAIL", what);
    if (!ok) {
        failed++;
    }
}

static unsigned fnv(const unsigned char *p, size_t n) {
    unsigned h = 2166136261u;
    while (n--) {
        h = (h ^ *p++) * 16777619u;
    }
    return h;
}

static cairo_t *canvas(cairo_surface_t **s) {
    *s = cairo_image_surface_create(CAIRO_FORMAT_ARGB32, 320, 48);
    cairo_t *cr = cairo_create(*s);
    cairo_set_source_rgb(cr, 1, 1, 1);
    cairo_paint(cr);
    cairo_set_source_rgb(cr, 0, 0, 0);
    cairo_font_options_t *o = cairo_font_options_create();
    cairo_font_options_set_antialias(o, CAIRO_ANTIALIAS_GRAY);
    cairo_font_options_set_hint_style(o, CAIRO_HINT_STYLE_NONE);
    cairo_font_options_set_hint_metrics(o, CAIRO_HINT_METRICS_OFF);
    cairo_set_font_options(cr, o);
    cairo_font_options_destroy(o);
    cairo_set_font_size(cr, 24);
    cairo_move_to(cr, 8, 32);
    return cr;
}

static int inked(cairo_surface_t *s) {
    unsigned char *px = cairo_image_surface_get_data(s);
    int n = 0;
    for (int i = 0; i < 320 * 48; i++) {
        n += px[i * 4] < 128;
    }
    return n;
}

int main(int argc, char **argv) {
    const char *font = argc > 1 ? argv[1] : "/usr/share/fonts/dejavu/DejaVuSans.ttf";
    printf("cairo %s, text:\n", cairo_version_string());
    FT_Library lib;
    FT_Face face;
    if (FT_Init_FreeType(&lib) || FT_New_Face(lib, font, 0, &face)) {
        printf("cairotext: cannot open %s\n", font);
        return 1;
    }
    cairo_surface_t *s;
    cairo_t *cr = canvas(&s);
    cairo_font_face_t *ff = cairo_ft_font_face_create_for_ft_face(face, 0);
    cairo_set_font_face(cr, ff);
    cairo_show_text(cr, "Quark draws text.");
    cairo_surface_flush(s);
    unsigned sum = fnv(cairo_image_surface_get_data(s),
                       (size_t)cairo_image_surface_get_stride(s) * 48);
    printf("cairotext: checksum %08X, expected %08X\n", sum, EXPECTED);
    check("text from a face on disk matches the host", sum == EXPECTED);
    check("and put ink on the page", inked(s) > 200);
    cairo_destroy(cr);
    cairo_surface_destroy(s);
    cairo_font_face_destroy(ff);

    cr = canvas(&s);
    cairo_select_font_face(cr, "sans-serif", CAIRO_FONT_SLANT_NORMAL, CAIRO_FONT_WEIGHT_NORMAL);
    cairo_scaled_font_t *sf = cairo_get_scaled_font(cr);
    FT_Face chosen = cairo_ft_scaled_font_lock_face(sf);
    printf("cairotext: sans-serif is %s\n", chosen ? chosen->family_name : "not a FreeType font");
#ifdef __quark__
    check("sans-serif, through fontconfig, is DejaVu Sans",
          chosen && !strcmp(chosen->family_name, "DejaVu Sans"));
#endif
    if (chosen) {
        cairo_ft_scaled_font_unlock_face(sf);
    }
    cairo_show_text(cr, "Quark draws text.");
    cairo_surface_flush(s);
    check("and draws", inked(s) > 200);
    cairo_destroy(cr);
    cairo_surface_destroy(s);
    FT_Done_Face(face);
    FT_Done_FreeType(lib);
    printf("cairotext: %s\n", failed ? "FAILED" : "ok");
    return failed ? 1 : 0;
}
```

`cairo.tests` gains `cairotext`; `cairotest.c`'s `LINK:` line gains `-lfontconfig -lfreetype -lexpat -lz` (cairo's objects now call them).

- [ ] **Step 2: Run it.** Against today's cairo it does not link (`cairo_ft_font_face_create_for_ft_face`).

- [ ] **Step 3: `build-cairo.sh <cairo-src> [pixman-src] [freetype-src]`.** Quark: `-Dfreetype=enabled -Dfontconfig=enabled`. Host: `-Dfreetype=enabled -Dfontconfig=disabled`, with the host FreeType's `lib/pkgconfig` on `PKG_CONFIG_PATH`; the host run prints both `cairotest`'s and `cairotext`'s checksums (the latter on the tarball's `DejaVuSans.ttf`). The header's "freetype and fontconfig arrive with the rest of the font stack" becomes what happened.

- [ ] **Step 4: Expected value.** Put the host's `cairotext` checksum in. `cairotest`'s must not have moved.

- [ ] **Step 5: `wlcairo` writes.** Below its drawing, two lines through the toy API — `sans-serif` 18 px: "Quark renders this with cairo, FreeType and fontconfig", `monospace` 14 px: "DejaVu Sans Mono, from /usr/share/fonts" — antialiased gray, repainted with the frame. `build-weston-client.sh` links `wlcairo` with `-lcairo -lpixman-1 -lfontconfig -lfreetype -lexpat -lz -lm`.

- [ ] **Step 6: Verify.** Rebuild cairo (both), tests, clients (`WESTON_SRC=… build-weston-client.sh …`); image with every suite and the overlay; boot: `runtests /etc/cairo.tests` (2 passed), `runtests /etc/fontconfig.tests`, `runtests /etc/pixman.tests`, then `wm wlcairo` and a screenshot: **the Done-when** — both lines legible in the window. `wm weston-simple-shm wlcairo`, Esc, console back.

- [ ] **Step 7: Commit.** explosion: "cairo draws text from a font on disk".

---

### Task 9: libxkbcommon

**Files:**
- Create: `explosion/toolchain/build-xkbcommon.sh`, `explosion/toolchain/tests/xkbtest.c`, `explosion/toolchain/tests/xkb.tests`

**Interfaces:**
- Produces: `libxkbcommon.a`, `xkbcommon.pc`; overlay `usr/share/xkb/us.xkb` (a copy of `quark/user/wm/src/us.xkb`, the keymap the compositor sends).

- [ ] **Step 1: The failing test.** `xkbtest.c`:

```c
// LINK: -lxkbcommon
/* The keymap the compositor sends compiles, and turns keys into what a
   terminal needs: symbols, text, and modifiers. No XKB data files. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <xkbcommon/xkbcommon.h>

#define KEY(evdev) ((evdev) + 8)

static int failed;

static void check(const char *what, int ok) {
    printf("  %s  %s\n", ok ? "ok  " : "FAIL", what);
    if (!ok) {
        failed++;
    }
}

static int text_is(struct xkb_state *st, int evdev, const char *want) {
    char buf[8];
    xkb_state_key_get_utf8(st, KEY(evdev), buf, sizeof buf);
    return !strcmp(buf, want);
}

int main(int argc, char **argv) {
    const char *path = argc > 1 ? argv[1] : "/usr/share/xkb/us.xkb";
    FILE *f = fopen(path, "rb");
    if (!f) {
        printf("xkbtest: cannot open %s\n", path);
        return 1;
    }
    static char text[1 << 20];
    size_t n = fread(text, 1, sizeof text - 1, f);
    fclose(f);
    text[n] = 0;

    printf("xkbcommon:\n");
    struct xkb_context *ctx =
        xkb_context_new(XKB_CONTEXT_NO_DEFAULT_INCLUDES | XKB_CONTEXT_NO_ENVIRONMENT_NAMES);
    check("a context with no data files", ctx != NULL);
    struct xkb_keymap *km =
        xkb_keymap_new_from_string(ctx, text, XKB_KEYMAP_FORMAT_TEXT_V1, XKB_KEYMAP_COMPILE_NO_FLAGS);
    check("the compositor's keymap compiles", km != NULL);
    if (!km) {
        return 1;
    }
    check("with one layout", xkb_keymap_num_layouts(km) == 1);
    struct xkb_state *st = xkb_state_new(km);
    check("a is a", xkb_state_key_get_one_sym(st, KEY(30)) == XKB_KEY_a && text_is(st, 30, "a"));
    xkb_state_update_key(st, KEY(42), XKB_KEY_DOWN);
    check("shift is on", xkb_state_mod_name_is_active(st, XKB_MOD_NAME_SHIFT, XKB_STATE_MODS_EFFECTIVE) == 1);
    check("shift a is A", xkb_state_key_get_one_sym(st, KEY(30)) == XKB_KEY_A);
    check("shift 2 is @", xkb_state_key_get_one_sym(st, KEY(3)) == XKB_KEY_at && text_is(st, 3, "@"));
    xkb_state_update_key(st, KEY(42), XKB_KEY_UP);
    check("return is Return", xkb_state_key_get_one_sym(st, KEY(28)) == XKB_KEY_Return && text_is(st, 28, "\r"));
    xkb_state_update_key(st, KEY(58), XKB_KEY_DOWN);
    xkb_state_update_key(st, KEY(58), XKB_KEY_UP);
    check("caps lock locks", xkb_state_key_get_one_sym(st, KEY(30)) == XKB_KEY_A);
    xkb_state_update_key(st, KEY(58), XKB_KEY_DOWN);
    xkb_state_update_key(st, KEY(58), XKB_KEY_UP);
    xkb_state_update_key(st, KEY(29), XKB_KEY_DOWN);
    check("ctrl c is an interrupt", text_is(st, 46, "\x03"));
    xkb_state_update_key(st, KEY(29), XKB_KEY_UP);
    char *again = xkb_keymap_get_as_string(km, XKB_KEYMAP_FORMAT_TEXT_V1);
    struct xkb_keymap *km2 = again ? xkb_keymap_new_from_string(ctx, again, XKB_KEYMAP_FORMAT_TEXT_V1, 0) : NULL;
    check("and what it says it is compiles again", km2 != NULL);
    free(again);
    xkb_keymap_unref(km2);
    xkb_state_unref(st);
    xkb_keymap_unref(km);
    xkb_context_unref(ctx);
    printf("xkbtest: %s\n", failed ? "FAILED" : "ok");
    return failed ? 1 : 0;
}
```

`xkb.tests`: `xkbtest /usr/share/xkb/us.xkb`.

- [ ] **Step 2: Run it.** `xkbtest skipped, not built yet: xkbcommon`.

- [ ] **Step 3: `build-xkbcommon.sh <src> <overlay-out>`.** `meson setup build-quark --cross-file … --prefix="$PREFIX" --buildtype=debugoptimized -Ddefault_library=static -Db_staticpic=false --wrap-mode=nofallback -Denable-tools=false -Denable-x11=false -Denable-wayland=false -Denable-xkbregistry=false -Denable-docs=false -Denable-bash-completion=false -Dxkb-config-root=/usr/share/X11/xkb -Dx-locale-root=/usr/share/X11/locale`; build the library target only (its own tests need data files the image does not carry); install; copy `../../quark/user/wm/src/us.xkb` into `<overlay>/usr/share/xkb/`. The header: no `xkeyboard-config` — a client is sent a whole keymap and needs none.

- [ ] **Step 4: Verify.** Build, tests, image with the overlay, boot: `runtests /etc/xkb.tests` passes; the other suites still pass.

- [ ] **Step 5: Commit.** explosion: "libxkbcommon, with the compositor's keymap".

---

### Task 10: Write it down

- [ ] `quark/CLAUDE.md`: invariants — a handle names an inode, never a copy; a task's handles close when it dies; paths are lent, never cut; directory times change with their entries. Known gaps — times are seconds since boot (no clock); no hard or symbolic links; FAT32 roots cannot remove, rename or truncate; shortening a file whose extent tree has grown past the inode is refused; file `mmap` is refused (no pager).
- [ ] `explosion/toolchain/README.md`: a section per port (what it needed, the three build fixes, the fonts and their licence, `bootstrap-fonts.sh`), and the stale sentences about a mapped transfer page and `getdents64` corrected.
- [ ] `~/src/osdev/ROADMAP.md`: Phase 13 done — what the ports needed of the system, what the filesystem gained, the mmap decision and why, what was deferred; the running order updated.
- [ ] Tick this plan; commit it; push quark and explosion.

---

## Acceptance

A window shows a line of text rendered by cairo and FreeType from DejaVu Sans read off `/usr/share/fonts`, chosen by fontconfig from a cache it wrote to `/var/cache/fontconfig`. `fttest` and `cairotext` match the host bit for bit. `filetest` and `dirtest` pass on ext2 and ext4, and `e2fsck` finds nothing to fix after them or after fontconfig. zlib's own tests, `xmltest`, `fctest` and `xkbtest` pass, and every earlier suite, `dtest`, the compositor and the hosted `hello` still do.
