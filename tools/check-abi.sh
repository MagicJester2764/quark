#!/bin/sh
# The kernel's half of the ABI check.
#
#     ./tools/check-abi.sh
#
# The system call numbers are written in one place here — `src/syscall.rs`,
# which is what the dispatch is compiled from — so there is nothing in this
# tree for them to drift from. What can still be wrong is the two things only
# a reader would notice.
#
# **Two calls on one number.** `match` takes the first arm that matches, so the
# second call never runs and every use of it does something else:
# `SYS_ADDRSPACE_DESTROY` and `SYS_MAP_PHYS` were both 38 for a phase, which
# meant every address space a failed spawn left behind stayed behind. rustc
# says "unreachable pattern" about it, in a build that prints other warnings;
# this says it in a way that fails.
#
# **A document that does not describe the kernel.** docs/abi.md is the
# contract, and since the userland moved to a repository of its own it is all
# the userland has: a call the kernel answers and the document has no row for
# is a call nobody outside this tree can use correctly. Three were found that
# way the first time this ran — named in the version history and nowhere else.
#
# The other half lives with the userland (quarkutils' tools/check-abi.sh): its
# own copies of the numbers, compared with the header `make install` writes
# from these.
set -e
cd "$(dirname "$0")/.."

S=src/syscall.rs
D=docs/abi.md
T=$(mktemp -d)
trap 'rm -rf "$T"' EXIT

# "number name", sorted by number then name.
grep -E '^pub const SYS_[A-Z_0-9]+: u64 = [0-9]+;' "$S" \
  | sed -E 's/.*(SYS_[A-Z_0-9]+): u64 = ([0-9]+);/\2 \1/' \
  | sort -k1,1n -k2,2 > "$T/kernel"

fail=0

# Two names on one number.
dup=$(awk '{print $1}' "$T/kernel" | uniq -d)
if [ -n "$dup" ]; then
    echo "abi: two syscalls share a number — the second is unreachable:" >&2
    for n in $dup; do
        echo "  $n: $(awk -v n="$n" '$1 == n {printf "%s ", $2}' "$T/kernel")" >&2
    done
    fail=1
else
    echo "abi: every number is used once ($(wc -l < "$T/kernel") calls)"
fi

# A reference row is `| number | \`SYS_NAME\` | ...`, or the other way round in
# the table of deprecated calls.
{
    grep -E '^\| [0-9]+ \| `SYS_[A-Z_0-9]+`' "$D" \
      | sed -E 's/^\| ([0-9]+) \| `(SYS_[A-Z_0-9]+)`.*/\1 \2/'
    grep -E '^\| `SYS_[A-Z_0-9]+` \| [0-9]+ \|' "$D" \
      | sed -E 's/^\| `(SYS_[A-Z_0-9]+)` \| ([0-9]+) \|.*/\2 \1/'
} | sort -k1,1n -k2,2 > "$T/doc"

if d=$(diff "$T/kernel" "$T/doc"); then
    echo "abi: $D has a row for every call"
else
    echo "abi: $D and the kernel disagree about which calls exist:" >&2
    echo "$d" | sed -n 's/^< \(.*\)/  the kernel has, the document has no row for: \1/p; s/^> \(.*\)/  the document has a row for, the kernel has not: \1/p' >&2
    fail=1
fi

# And the version it says it describes is the one the kernel reports.
src_ver=$(sed -nE 's/^pub const ABI_VERSION_(MAJOR|MINOR): u64 = ([0-9]+);/\2/p' "$S" | paste -sd.)
doc_ver=$(sed -nE 's/^\*\*Version ([0-9]+\.[0-9]+)\.\*\*.*/\1/p' "$D" | head -1)
if [ "$src_ver" = "$doc_ver" ]; then
    echo "abi: the document and the kernel both say $src_ver"
else
    echo "abi: the kernel reports $src_ver and $D says ${doc_ver:-nothing}" >&2
    fail=1
fi

exit $fail
