#!/bin/sh
# The syscall numbers are declared twice — in the kernel and in quark-rt — and
# nothing but this check stops them drifting apart. A mismatch is silent and
# catastrophic: user space calls one number and the kernel runs another.
#
# Phase 1 moves quark-rt out of this repo, at which point the ABI should be a
# shared definition rather than two copies, and this check can go away.
set -e
cd "$(dirname "$0")/.."
extract() {
    grep -E '^pub const SYS_[A-Z_]+: u64 = [0-9]+;' "$1" \
      | sed -E 's/.*(SYS_[A-Z_]+): u64 = ([0-9]+);/\2 \1/' | sort -n
}
if diff -u <(extract src/syscall.rs) <(extract user/quark-rt/src/syscall.rs); then
    echo "abi: kernel and quark-rt agree ($(extract src/syscall.rs | wc -l) syscalls)"
else
    echo "abi: MISMATCH between kernel and quark-rt syscall numbers" >&2
    exit 1
fi
