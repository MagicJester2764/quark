# Floating-point state

Every task has its own x87 and SSE registers. The kernel saves them from the
task that is leaving the CPU and loads them for the one that is arriving, on
every switch, and that is the whole of it: `src/fpu.rs` is eighty lines.

This file used to be a note that the work was needed. It is what was done and
what would have to change.

## Why it is the kernel's problem

The kernel is built soft-float and never touches these registers. Every user
program may — the hosted Rust target and every C program are compiled with
SSE2, which is part of the x86-64 ABI — and there is one set of them per CPU.
Without a save area per task, a task preempted in the middle of a
floating-point computation resumes holding whatever the last task to run left
behind, and can read it: one task observing another's data.

It went unnoticed for a long time because nothing did much floating-point
work. pixman did. Its own test suite failed on Quark with blend results equal
to the destination pixel.

## What happens

- **At boot**, `boot.s` clears `CR0.EM`, sets `CR0.MP`, and sets `CR4.OSFXSR`
  and `CR4.OSXMMEXCPT`. Then `fpu::init` resets the unit, loads the default
  `MXCSR` (every exception masked, round to nearest) and captures the result
  with `FXSAVE`: that is the state every task starts in. It is taken from the
  processor rather than written out by hand, so its reserved fields are
  whatever this processor considers valid.
- **Every task** carries a 512-byte `FpuState`, aligned to 64 bytes, inline in
  its `Task`.
- **On every switch** the scheduler does `FXSAVE` into the outgoing task's
  area and `FXRSTOR` from the incoming one's, before it switches stacks. Since
  the kernel never uses the registers, loading early is safe, and a task that
  has never run starts from its own clean state without its entry path knowing
  anything about this.
- **`fork`** saves the caller's state from the registers into the child — it
  is the caller's system call, so that is where the state is — and **`exec`**
  gives the task the clean state again.

## Eager, not lazy

Lazy switching sets `CR0.TS` and waits for the `#NM` a task takes when it first
touches the registers, saving the cost for tasks that never do. It is also how
the LazyFP vulnerability happened. One `FXSAVE` and one `FXRSTOR` per switch is
cheap enough that Linux gave up on lazy switching in 2016, and it is what
happens here.

## FXSAVE is enough only while AVX is off

`FXSAVE` covers x87, MMX and SSE, and nothing wider. `CR4.OSXSAVE` is clear, so
an AVX instruction faults in user space and there is no wider state to lose.

**Setting `CR4.OSXSAVE` — to let programs use AVX — without moving this to
`XSAVE` and its full component mask would hand one task the upper halves of
another's YMM registers.** That is the one change here that must not be made
alone. The save area is already aligned to the 64 bytes `XSAVE` wants so that
moving to it does not also have to move the structure; its size would have to
come from CPUID rather than be 512.

## References

- Intel SDM Vol. 1, Chapter 10 (Programming with SSE) and Chapter 13 (XSAVE)
- Linux commit `58122bf1d856`, which made eager switching the only kind
