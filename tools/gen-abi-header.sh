#!/bin/sh
# Write the kernel's system call numbers as a C header, on stdout.
#
#     ./tools/gen-abi-header.sh > quark/abi.h
#
# This is what the kernel *installs*: `make install` puts it at
# usr/include/quark/abi.h beside docs/abi.md, and those two files are the whole
# of what a userland in another repository may know about this one. The numbers
# are written down in exactly one place here — `src/syscall.rs`, which is also
# what the dispatch is compiled from — and this is derived from it, so there is
# nothing to keep in step by hand and nothing here to edit.
#
# The numbers and the version, and no more. What each call takes and returns
# is docs/abi.md's to say; a header that tried to would be a second description
# with nothing checking it against the first.
set -e
cd "$(dirname "$0")/.."
S=src/syscall.rs

major=$(sed -nE 's/^pub const ABI_VERSION_MAJOR: u64 = ([0-9]+);/\1/p' "$S")
minor=$(sed -nE 's/^pub const ABI_VERSION_MINOR: u64 = ([0-9]+);/\1/p' "$S")
[ -n "$major" ] && [ -n "$minor" ] || { echo "no ABI version in $S" >&2; exit 1; }

cat <<HEAD
/* The Quark system call ABI, version $major.$minor.
 *
 * Generated from the kernel's src/syscall.rs by tools/gen-abi-header.sh and
 * installed by \`make install\`. Not to be edited: the kernel's dispatch is
 * compiled from the same constants, so this cannot disagree with it.
 *
 * Numbers are assigned in blocks of sixteen, one subsystem to a block. What
 * each call takes and returns is in abi.md, installed beside this.
 */
#ifndef _QUARK_ABI_H
#define _QUARK_ABI_H

#define QUARK_ABI_VERSION_MAJOR $major
#define QUARK_ABI_VERSION_MINOR $minor
HEAD

grep -E '^pub const SYS_[A-Z_0-9]+: u64 = [0-9]+;' "$S" \
  | sed -E 's/.*(SYS_[A-Z_0-9]+): u64 = ([0-9]+);/\2 \1/' \
  | sort -n \
  | awk '{
        block = int($1 / 16)
        if (block != last) { printf "\n/* 0x%02X */\n", block * 16; last = block }
        printf "#define %-26s %s\n", $2, $1
    } BEGIN { last = -1 }'

printf '\n#endif\n'
