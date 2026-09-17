# The VFS protocol

The contract between the file server and its clients. `user/vfs` is the
server; `quark-rt`'s `vfs` module, Quark's C library (`include/quark/vfs.h`)
and the Linux translation layer are clients. Each keeps its own copy of the
numbers below; this document is the one they are checked against.

A client finds the server by looking up `vfs` with the nameserver, which also
gives it the right to call it. Every request is a synchronous call. Where a
request carries more than six words — a path, a record, file data — the client
lends a buffer with the call (`SYS_CALL_LEND`, see `docs/abi.md`) and the
server copies into or out of it before replying. The server never maps a
client's memory and cannot reach a lent buffer once it has answered.

## Replies

A reply's tag is `0` (OK) or `u64::MAX` (error). An error reply carries its
code in `data[0]`:

| Code | Name | Meaning |
|---|---|---|
| 1 | `NOT_FOUND` | No such file or directory |
| 2 | `INVALID_HANDLE` | Not a handle this client holds |
| 3 | `IO` | The disk failed, or the filesystem is damaged |
| 4 | `TOO_MANY_OPEN` | The server's handle table is full |
| 5 | `INVALID_PATH` | Empty, not NUL-free, or names `.` or `..` where a new name is needed |
| 6 | `NOT_DIR` | A directory was required |
| 7 | `IS_DIR` | A directory was not allowed |
| 8 | `PERMISSION` | The caller may not do this |
| 9 | `READ_ONLY` | The filesystem is mounted read-only |
| 10 | `EXISTS` | The name is taken |
| 11 | `NOT_EMPTY` | The directory still has entries |
| 12 | `NOT_SUPPORTED` | This filesystem cannot do that |
| 13 | `NAME_TOO_LONG` | A path over 4095 bytes, or a name over 255 |
| 14 | `NO_SPACE` | Nowhere to put what was written |
| 15 | `LOOP` | A lookup followed more than 40 symbolic links |
| 16 | `WOULD_BLOCK` | A lock is held that keeps this one out |
| 17 | `DEADLOCK` | Waiting for this lock would wait for ever |
| 18 | `TOO_MANY_LINKS` | The file has as many names as it can |

Permission is checked against the caller's user and group, which the server
asks the kernel for (`SYS_GET_TUID`). User 0 is not checked. FAT32 has no
owners or modes and checks nothing.

## Paths

A request that names a path lends it for reading, with its length in
`data[0]`. It is not NUL-terminated and may be up to 4095 bytes; a longer one
is refused with `NAME_TOO_LONG` rather than shortened. A trailing `/` means
the path must name a directory.

A path that does not start with `/` starts from a *base*, which every request
naming a path carries in `data[5]`: 0 is the calling program's working
directory, and `h + 1` is the directory open as handle `h`. `RENAME` and
`LINK` carry their second path's base in `data[4]`. A base that is not an
open directory of the caller's program is `INVALID_HANDLE` or `NOT_DIR`; an
absolute path ignores it.

A symbolic link met on the way is followed: its target takes the place of the
part of the path that named it, from the root if the target starts with `/`
and from the directory holding the link if not. The last component is
followed too, unless the request says otherwise (`OPEN_NOFOLLOW`, and every
request that changes a name: `UNLINK`, `RENAME` and `LINK` act on a link
itself). A path that follows more than 40 links is `LOOP`. `..` is the
directory's own `..` entry, so it goes up from where a link led, not from
where the link was.

## Requests

| Tag | Name | Words | Lent | Reply |
|---|---|---|---|---|
| 1 | `OPEN` | `[len, flags]` | path | `[handle, size, is_dir, mode, access, id]` |
| 2 | `READ` | `[handle, -, offset, len]` | `len` bytes to fill (at most 4096) | `[bytes read]` |
| 3 | `CLOSE` | `[handle]` | — | — |
| 5 | `STAT` | `[handle]` | 88 bytes to fill | `[88]` |
| 6 | `WRITE` | `[handle, -, offset, len]` | `len` bytes to copy (at most 4096) | `[bytes written]` |
| 8 | `READDIR_BULK` | `[handle, start, len]` | `len` bytes to fill (at most 4096) | `[bytes, next, end]` |
| 9 | `MKDIR` | `[len]` | path | — |
| 10 | `UNLINK` | `[len]` | path | — |
| 11 | `RMDIR` | `[len]` | path | — |
| 12 | `RENAME` | `[from_len, to_len]` | both paths, end to end | — |
| 13 | `TRUNCATE` | `[handle, size]` | — | — |
| 14 | `STATFS` | — | 64 bytes to fill | `[64]` |
| 15 | `LINK` | `[from_len, to_len, follow]` | both paths, end to end | — |
| 16 | `SYMLINK` | `[target_len, path_len]` | the target, then the path | — |
| 17 | `READLINK` | `[path_len, room]` | the path, then `room` bytes to fill | `[target_len]` |
| 18 | `CHDIR` | `[len]` | path | — |
| 19 | `FCHDIR` | `[handle]` | — | — |
| 20 | `GETCWD` | — | 4096 bytes to fill | `[len]` |
| 21 | `GIVE_CWD` | `[child_tid]` | — | — |
| 22 | `LOCK` | `[handle, kind, start, len, flags]` | — | `[kind, start, len, holder]` for a query |

Numbers are never reused. 4 was `READDIR`, which returned one entry per call
and cut its name to 32 bytes. 7 was `CREATE`, which carried its path in the
message and cut it to 40 bytes.

### OPEN

`flags` is a set of:

| Bit | Name | Effect |
|---|---|---|
| 1 | `CREATE` | Make the file if the name is free |
| 2 | `EXCLUSIVE` | With `CREATE`: fail with `EXISTS` if it is not |
| 4 | `TRUNCATE` | Empty a regular file the caller may write |
| 8 | `DIRECTORY` | Fail with `NOT_DIR` unless it is a directory |
| 16 | `NOFOLLOW` | A symbolic link at the end is opened itself |

A file made by `CREATE` is a regular file, mode 0644, owned by the caller.
The reply's `mode` includes the file-type bits (`0o170000`), `access` is what
this caller may do (4 read, 2 write, 1 execute), and `id` is the inode number
(FAT32: the first cluster), stable for as long as the file exists.

A symbolic link opened with `NOFOLLOW` answers `STAT` (mode `0120777`, its
size the target's length) and `CLOSE`, and `NOT_SUPPORTED` to everything else.
`CREATE` through a link whose target does not exist says `EXISTS`; Linux would
make the target.

A handle belongs to the program that opened it — every thread of it may use
it — and the server closes a program's handles when its last task dies. It
knows a program by its address space (`SYS_TASK_SPACE`) and watches each one
it gives a handle to (`SYS_SPACE_WATCH`).

### STAT

The lent buffer is filled with eleven little-endian 64-bit words:

| Word | Field |
|---|---|
| 0 | `id` — as `OPEN` reports it |
| 1 | `size` in bytes |
| 2 | `mode`, with the file-type bits |
| 3 | `links` |
| 4 | `uid` |
| 5 | `gid` |
| 6 | `atime` |
| 7 | `mtime` |
| 8 | `ctime` |
| 9 | `blocks`, in 512-byte units |
| 10 | `block_size` |

Times are seconds since 1970, from the clock the kernel reads at boot
(`SYS_BOOT_TIME`). A machine whose clock cannot be read counts from boot
instead, and so does everything else on it.

### MKDIR

Makes a directory, mode 0755, owned by the caller. `EXISTS` if the name is
taken.

### UNLINK, RMDIR, RENAME and LINK

`UNLINK` removes a name that is not a directory's; the file goes with its last
name, or, if a handle still names it, when that handle closes. `RMDIR` removes
an empty directory (`NOT_EMPTY` otherwise). Both need write permission on the
parent.

`RENAME` lends the source path followed directly by the destination, with the
two lengths in `data[0]` and `data[1]`. It replaces a destination of the same
kind — a file for a file, an empty directory for a directory — and refuses to
move a directory inside itself (`INVALID_PATH`).

`LINK` lends its two paths the same way and gives the file at the first a
second name at the second. A link at the first path gets the name itself,
unless `data[2]` has bit 0 set, which follows it. A directory is refused (`IS_DIR`), and so is a name
that is taken (`EXISTS`) and a file with as many names as the filesystem
allows (`TOO_MANY_LINKS`: 32000 on ext2, 65000 on ext4). The new name's
directory needs write permission, as for `UNLINK`. FAT32 answers all five
name-changing requests with `NOT_SUPPORTED`.

### SYMLINK and READLINK

`SYMLINK` lends the target followed by the new path, and makes the path a
symbolic link, mode `0777`, owned by the caller. The target is kept as given:
it is not resolved and need not exist, but it must not be empty (`NOT_FOUND`)
and must fit a block with room for a NUL (`NAME_TOO_LONG` past 1023 bytes on a
1 KiB-block filesystem). One shorter than 60 bytes is kept in the inode; a
longer one takes a block.

`READLINK` lends one buffer for reading and writing: the path, then `room`
bytes. The link's target is written into those bytes, as much as fits, and the
reply is its whole length. A path that is not a link is `INVALID_PATH`.

### TRUNCATE

Sets a writable handle's regular file to `size` bytes, which must fit in 32
bits. Growing it adds a hole, which reads as zeroes and takes no blocks until
it is written. ext4 shortens only files whose extent tree fits in the inode;
that, and FAT32 at all, is `NOT_SUPPORTED`. `OPEN_TRUNCATE` is the same
operation to size 0.

### READDIR_BULK

Fills up to `len` bytes of the lent buffer with directory records, starting
with entry `start` (the first is 0). Each record is:

| Offset | Size | Field |
|---|---|---|
| 0 | 8 | `id` — the entry's inode number, or FAT32 first cluster |
| 8 | 8 | `next` — the `start` that continues after this entry |
| 16 | 8 | `size` in bytes |
| 24 | 2 | `reclen` — this record's length, a multiple of 8 |
| 26 | 1 | `type` — `DT_DIR` 4, `DT_REG` 8, `DT_LNK` 10, `DT_CHR` 2, or 0 |
| 27 | 1 | `namelen` |
| 28 | | the name, a NUL, and padding to `reclen` |

The reply says how many bytes were written, where to continue, and whether
that is the end of the directory. A buffer too small for the next record
yields zero bytes and `end` clear. `.` and `..` are listed where the
filesystem stores them. A position is only meaningful for the directory it
came from, and entries made or removed between two calls may be missed or
seen twice, as with any `readdir`.

### Working directories

Each program has one, kept by the server and named by the program's address
space like its handles. A program nobody gave a directory is at `/`.
`CHDIR` moves the caller's program to a directory it may search, following
links; `FCHDIR` to an open directory handle's. `GETCWD` fills the lent buffer
with the directory's path and replies with its length; it is `NOT_FOUND` once
the directory has been removed.

The server holds the directory by inode, as Linux does, so a rename above it
changes what `GETCWD` says and nothing else, and a directory removed while a
program is in it lasts until the program leaves or goes. FAT32, which renames
nothing, keeps the path, and has no directory handles to start from
(`NOT_SUPPORTED`).

`GIVE_CWD` puts a program being made in the caller's directory. The child
must be a task the caller's program made (`SYS_TASK_CREATE_IN` lets that be
before it runs) for another program; anything else is `PERMISSION`. A
spawner calls it before starting the child, so the child's first relative
path already starts in the right place.

### LOCK

A record lock on the file open as `handle`: `kind` 0 unlocks, 1 is shared, 2
exclusive, over `len` bytes from `start` (`len` 0 runs to the end of the file
and past it). `flags` is a set of:

| Bit | Name | Effect |
|---|---|---|
| 1 | `WAIT` | Answer when the lock is granted, rather than `WOULD_BLOCK` now |
| 2 | `OFD` | The lock is the handle's, not the program's |
| 4 | `QUERY` | Grant nothing: reply with the first lock in the way |

A program's locks are Linux's POSIX locks: its own never conflict, a new one
replaces what the program held over its range and joins neighbours of the same
kind, and closing any of the program's handles on the file drops all of them.
A handle's locks (`OFD`, which is also what `flock` is) conflict with every
other owner, another handle of the same program included, and go when the
handle closes. A program's death drops everything it held or was waiting for.

A query's reply names the lock in the way — `kind` (0 if none), `start`, `len`
(0 for "to the end") and the program holding it, all ones for a handle's
lock. A waiting request is answered when a release lets it in; closing the
handle it was made through answers it `INVALID_HANDLE`. Before a program
waits, the server follows the programs holding what it wants, and the
requests they are waiting on in turn; if that leads back to the program
asking, the answer is `DEADLOCK`. Locks live in the server's memory, 256 at
once (`NO_SPACE` beyond), and are keyed by inode — on FAT32 by first cluster,
or for an empty file by its directory and name.

### Devices

`/dev` is the server's own, whatever the root filesystem holds there: the
lookup answers for its names itself, so a path reaches the devices however it
is spelled and whatever links it passes through. (A root with no `/dev`
directory, and FAT32, match the path as written instead.) It holds five character devices, mode `0666`,
with ids from `0xFFFF_FF00` in this order:

| Name | Read | Write |
|---|---|---|
| `null` | nothing: 0 bytes | accepted and dropped |
| `zero` | zeroes | accepted and dropped |
| `full` | zeroes | `NO_SPACE` |
| `random` | random bytes (`SYS_GETRANDOM`) | accepted and dropped |
| `urandom` | the same | accepted and dropped |

`/dev` itself (id `0xFFFF_FF05`, mode `0755`) lists `.`, `..` and the five,
typed `DT_CHR`. Nothing can be made, removed or renamed under it
(`PERMISSION`); `OPEN_CREATE` on a device opens it, and with `OPEN_EXCLUSIVE`
says `EXISTS`. `STAT` gives a device size 0 and the current time. The Linux
layer reports Linux's device numbers for them (1:3, 1:5, 1:7, 1:8, 1:9). The
images carry an empty `/dev` directory so that listing `/` shows it.

### STATFS

The lent buffer is filled with eight little-endian 64-bit words: the
filesystem's magic (`0xEF53` for ext2 and ext4, `0x4d44` for FAT32), block
size, block count, free blocks, blocks free to anyone (less those reserved
for user 0), inodes, free inodes, and the longest name. FAT32 reports its
cluster size and zeroes for the counts.
