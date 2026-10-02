# Floating-point and vector state

Every task has its own x87, SSE, AVX and AVX-512 registers — as many of
those as the processor has. The kernel saves them from the task that is
leaving a processor and loads them for the one that is arriving, on every
switch, and that is the whole of it: `src/fpu.rs`.

## Why it is the kernel's problem

The kernel is built soft-float and never touches these registers. Every user
program may — the hosted Rust target and every C program are compiled with
SSE2, which is part of the x86-64 ABI — and there is one set of them per
processor. Without a save area per task, a task preempted in the middle of a
floating-point computation resumes holding whatever the last task to run left
behind, and can read it: one task observing another's data.

It went unnoticed for a long time because nothing did much floating-point
work. pixman did. Its own test suite failed on Quark with blend results equal
to the destination pixel.

## What happens

- **At boot**, `boot.s` clears `CR0.EM`, sets `CR0.MP`, and sets `CR4.OSFXSR`
  and `CR4.OSXMMEXCPT`. Then `fpu::init` resets the unit, loads the default
  `MXCSR` (every exception masked, round to nearest), and — where the
  processor has `XSAVE` — sets `CR4.OSXSAVE` and writes XCR0: x87 and SSE,
  AVX if the processor has it, and AVX-512's three components if it has all
  three. That is what programs may use from then on, and exactly what is
  saved.
- **The clean state** every task starts in is made there too: what `FXSAVE`
  says of a unit just reset, so that its reserved fields are whatever this
  processor considers valid, with the vector registers nought and — for
  `XSAVE` — a header saying no component is other than as the processor
  first has it.
- **Every task** carries an `FpuState` of 2688 bytes, aligned to 64, inline
  in its `Task`: room for everything that can be turned on.
- **On every switch** the scheduler saves into the outgoing task's area and
  restores from the incoming one's, before it switches stacks: `XSAVE` and
  `XRSTOR` with every component that is on, or `FXSAVE` and `FXRSTOR` on a
  processor with no `XSAVE`. Since the kernel never uses the registers,
  loading early is safe, and a task that has never run starts from its own
  clean state without its entry path knowing anything about this.
- **`fork`** saves the caller's state from the registers into the child — it
  is the caller's system call, so that is where the state is — and **`exec`**
  gives the task the clean state again.
- **Every other processor** is given the first one's CR4 as it is started
  and writes the same XCR0 (`fpu::init_processor`): which components are on
  is a register of each processor's own, and a task moved to a processor
  that had fewer would fault on its first wide instruction — or, had it
  more, be saved short.

## Eager, not lazy

Lazy switching sets `CR0.TS` and waits for the `#NM` a task takes when it first
touches the registers, saving the cost for tasks that never do. It is also how
the LazyFP vulnerability happened. One save and one restore per switch is
cheap enough that Linux gave up on lazy switching in 2016, and it is what
happens here. Where the processor has `XSAVEOPT` that is what saves: it
leaves out a component a task has not touched — noting in the header that it
is as the processor first had it — so a task that never used a wide register
is saved as `FXSAVE` would have saved it.

## What is on and what is saved are one decision

`FXSAVE` covers x87, MMX and SSE, and nothing wider. For a long time
`CR4.OSXSAVE` was left clear because of that: an AVX instruction faulted in
user space, and there was no wider state to lose.

**Turning a component on without saving it hands one task the upper halves
of another's registers**, and nothing faults to say so. `fpu::enable` is the
one place both are decided, and the save and the restore take their list of
components from it. `dtest fpu` loads a pattern into every wide register,
lets another task do the same, and looks at its own again: with AVX on and
`FXSAVE` doing the saving, three of its checks fail — which was tried.

A component that is not turned on: AMX, whose tile registers are eight
kilobytes a task. The area is sized for what is turned on (`AREA_SIZE`), and
a processor that asks for more room than that for the same components is
given x87 and SSE alone.

## References

- Intel SDM Vol. 1, Chapter 10 (Programming with SSE) and Chapter 13 (XSAVE)
- Linux commit `58122bf1d856`, which made eager switching the only kind
