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

Permission is checked against the caller's user and group, which the server
asks the kernel for (`SYS_GET_TUID`). User 0 is not checked. FAT32 has no
owners or modes and checks nothing.

## Paths

A request that names a path lends it for reading, with its length in
`data[0]`. It is not NUL-terminated and may be up to 4095 bytes; a longer one
is refused with `NAME_TOO_LONG` rather than shortened. Paths are absolute;
there is no working directory. A trailing `/` means the path must name a
directory.

## Requests

| Tag | Name | Words | Lent | Reply |
|---|---|---|---|---|
| 1 | `OPEN` | `[len, flags]` | path | `[handle, size, is_dir, mode, access, id]` |
| 2 | `READ` | `[handle, -, offset, len]` | `len` bytes to fill (at most 4096) | `[bytes read]` |
| 3 | `CLOSE` | `[handle]` | — | — |
| 4 | `READDIR` | `[handle, index]` | — | one entry: name (32 bytes) in `data[0..4]`, `len \| attr << 8`, size |
| 5 | `STAT` | `[handle]` | 88 bytes to fill | `[88]` |
| 6 | `WRITE` | `[handle, -, offset, len]` | `len` bytes to copy (at most 4096) | `[bytes written]` |
| 8 | `READDIR_BULK` | `[handle]` | 4096 bytes to fill | `[count]` |
| 9 | `MKDIR` | `[len]` | path | — |

Numbers are never reused. 7 was `CREATE`, which carried its path in the
message and cut it to 40 bytes.

### OPEN

`flags` is a set of:

| Bit | Name | Effect |
|---|---|---|
| 1 | `CREATE` | Make the file if the name is free |
| 2 | `EXCLUSIVE` | With `CREATE`: fail with `EXISTS` if it is not |
| 4 | `TRUNCATE` | Empty a regular file the caller may write |
| 8 | `DIRECTORY` | Fail with `NOT_DIR` unless it is a directory |

A file made by `CREATE` is a regular file, mode 0644, owned by the caller.
The reply's `mode` includes the file-type bits (`0o170000`), `access` is what
this caller may do (4 read, 2 write, 1 execute), and `id` is the inode number
(FAT32: the first cluster), stable for as long as the file exists.

A handle belongs to the task that opened it. The server watches every task it
gives a handle to and closes them all when the task dies.

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

Times are seconds since this machine booted: there is no clock, and the C
library's `time()` counts on the same scale. A file written on another machine
keeps whatever time that machine gave it.

### MKDIR

Makes a directory, mode 0755, owned by the caller. `EXISTS` if the name is
taken.

### READDIR_BULK

Fills the lent page with up to 64 fixed entries of 64 bytes — 48 bytes of
name, its length, an attribute byte (`0x10` for a directory), two spare, the
size and the inode or cluster as 32-bit words, four spare — starting from the
directory's first entry. A name longer than 48 bytes is cut.
