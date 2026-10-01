#!/bin/sh
# The syscall numbers are declared three times — in the kernel, in quark-rt and
# in the C library's header — and nothing but this check stops them drifting
# apart. A mismatch is silent and catastrophic: user space calls one number and
# the kernel runs another.
#
# Two numbers that are *equal* are as bad and quieter still. `match` takes the
# first arm that matches, so the second call never runs and every use of it
# does something else: `SYS_ADDRSPACE_DESTROY` and `SYS_MAP_PHYS` were both 38
# for a phase, which meant every address space a failed spawn left behind
# stayed behind. rustc says "unreachable pattern" about it, in a build that
# prints other warnings; this says it in a way that fails.
#
# Phase 1 moves quark-rt out of this repo, at which point the ABI should be a
# shared definition rather than three copies, and this check can go away.
set -e
cd "$(dirname "$0")/.."

extract() {
    grep -E '^pub const SYS_[A-Z_0-9]+: u64 = [0-9]+;' "$1" \
      | sed -E 's/.*(SYS_[A-Z_0-9]+): u64 = ([0-9]+);/\2 \1/' | sort -n
}
extract_h() {
    grep -E '^#define SYS_[A-Z_]+[[:space:]]+[0-9]+' "$1" \
      | sed -E 's/#define[[:space:]]+(SYS_[A-Z_]+)[[:space:]]+([0-9]+).*/\2 \1/' | sort -n
}

fail=0

if diff -u <(extract src/syscall.rs) <(extract user/quark-rt/src/syscall.rs); then
    echo "abi: kernel and quark-rt agree ($(extract src/syscall.rs | wc -l) syscalls)"
else
    echo "abi: MISMATCH between kernel and quark-rt syscall numbers" >&2
    fail=1
fi

# The C header declares a subset — it carries what C programs and the Linux
# layer call — so it is checked for disagreement rather than for completeness.
H=user/libc/include/quark/syscall.h
if wrong=$(join -j 2 -o 0,1.1,2.1 \
        <(extract src/syscall.rs | awk '{print $1" "$2}' | sort -k2,2) \
        <(extract_h "$H" | awk '{print $1" "$2}' | sort -k2,2) \
      | awk '$2 != $3'); [ -n "$wrong" ]; then
    echo "abi: the C header disagrees with the kernel:" >&2
    echo "$wrong" | while read -r name k h; do
        echo "  $name: kernel $k, $H $h" >&2
    done
    fail=1
else
    echo "abi: the C header agrees ($(extract_h "$H" | wc -l) numbers)"
fi

# Two names on one number.
if dup=$(extract src/syscall.rs | awk '{print $1}' | uniq -d); [ -n "$dup" ]; then
    echo "abi: two syscalls share a number — the second is unreachable:" >&2
    for n in $dup; do
        echo "  $n: $(extract src/syscall.rs | awk -v n="$n" '$1 == n {printf "%s ", $2}')" >&2
    done
    fail=1
else
    echo "abi: every number is used once"
fi

# The document is the contract. With the runtime in this tree the source was
# what it was checked against and docs/abi.md described it; with the userland
# in a repository of its own the document is all it has, so a call the kernel
# answers and the document does not describe is a call nobody outside this
# tree can use correctly. Three were found that way the first time this ran:
# named in the version history, and nowhere else.
#
# A reference row is `| number | \`SYS_NAME\` | ...`, or the other way round in
# the table of deprecated calls.
D=docs/abi.md
doc_rows() {
    {
        grep -E '^\| [0-9]+ \| `SYS_[A-Z_0-9]+`' "$D" \
          | sed -E 's/^\| ([0-9]+) \| `(SYS_[A-Z_0-9]+)`.*/\1 \2/'
        grep -E '^\| `SYS_[A-Z_0-9]+` \| [0-9]+ \|' "$D" \
          | sed -E 's/^\| `(SYS_[A-Z_0-9]+)` \| ([0-9]+) \|.*/\2 \1/'
    } | sort -k1,1n -k2,2
}
if d=$(diff <(extract src/syscall.rs | sort -k1,1n -k2,2) <(doc_rows)); then
    echo "abi: $D has a row for every call ($(doc_rows | wc -l))"
else
    echo "abi: $D and the kernel disagree about which calls exist:" >&2
    echo "$d" | sed -n 's/^< \(.*\)/  the kernel has, the document has no row for: \1/p; s/^> \(.*\)/  the document has a row for, the kernel has not: \1/p' >&2
    fail=1
fi

# And the version it says it describes is the one the kernel reports.
src_ver=$(sed -nE 's/^pub const ABI_VERSION_(MAJOR|MINOR): u64 = ([0-9]+);/\2/p' src/syscall.rs | paste -sd.)
doc_ver=$(sed -nE 's/^\*\*Version ([0-9]+\.[0-9]+)\.\*\*.*/\1/p' "$D" | head -1)
if [ "$src_ver" = "$doc_ver" ]; then
    echo "abi: the document and the kernel both say $src_ver"
else
    echo "abi: the kernel reports $src_ver and $D says ${doc_ver:-nothing}" >&2
    fail=1
fi

exit $fail
