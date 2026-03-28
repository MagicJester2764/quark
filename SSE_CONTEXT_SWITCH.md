# SSE Context Switch Support Required

## Problem

The hosted Rust target (`x86_64-unknown-quark`) allows SSE2 instructions.
Previously, all user programs were compiled with `+soft-float` which prevented
the compiler from emitting SSE/AVX instructions. Newer versions of rustc
reject `+soft-float` on x86_64 because SSE2 is part of the ABI, so the hosted
target uses `"features": ""` (default x86_64 features, which include SSE2).

If two tasks are preempted mid-SSE-operation, the XMM/YMM registers will be
clobbered because the kernel's context switch does not save or restore them.

## What needs to change

The scheduler's context switch (`schedule_inner` / `enter_user_inner`) must
save and restore the x87/SSE/AVX state. Options:

1. **FXSAVE/FXRESTORE** — saves x87 + SSE (XMM0-15) to a 512-byte region.
   Simplest, sufficient if no AVX code is generated.

2. **XSAVE/XRSTOR** — saves x87 + SSE + AVX + extensions. More future-proof
   but requires detecting supported components via CPUID.

Each task's state struct needs an additional 512-byte (FXSAVE) or larger
(XSAVE) aligned buffer for the FPU/SSE state.

The kernel must also set CR0.TS and CR4.OSFXSR appropriately:
- `CR4.OSFXSR = 1` — enables FXSAVE/FXRESTORE
- `CR0.EM = 0` — do not emulate FPU
- `CR0.TS` — optionally used for lazy FPU switching (set TS, catch #NM on
  first FPU/SSE use, save old state and restore new state, clear TS)

## Lazy vs eager switching

- **Eager**: FXSAVE/FXRESTORE on every context switch. Simple, predictable.
  ~50-100 cycle overhead per switch.
- **Lazy**: Set CR0.TS, catch #NM (Device Not Available) exception on first
  SSE use, then save/restore. Avoids the cost when a task doesn't use SSE.
  More complex, and modern CPUs make eager switching cheap enough that Linux
  switched to eager in 2016.

Recommendation: start with eager FXSAVE/FXRESTORE.

## Current risk

Simple hosted programs (like hello) may not trigger SSE codegen in practice,
but any use of floating point, SIMD, or optimized memcpy/memmove could. The
`compiler-builtins-mem` feature provides `memcpy`/`memmove`/`memset` in
software, but the compiler may still emit SSE instructions for other reasons
(e.g., passing/returning structs, auto-vectorization).

No_std programs compiled with `x86_64-unknown-none` still use `+soft-float`
and are unaffected.

## References

- Intel SDM Vol. 1, Chapter 10 (Programming with SSE)
- Intel SDM Vol. 3, Section 13.1 (XSAVE-Supported Features)
- Linux commit switching to eager FPU: `58122bf1d856` (2016)
