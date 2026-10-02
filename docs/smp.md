# More than one processor

Quark starts every processor the machine's firmware lists and runs tasks on
all of them. The kernel itself runs on one at a time.

This is what was done, why it was done that way, and what would have to
change.

## One processor in the kernel

Everything this kernel knows about being safe it knows in one form: with
interrupts off, nothing else runs. Twenty thousand lines lean on it — every
`irq_save` in the scheduler, every table that is searched and then filled,
`IrqSpinLock` panicking when it finds a lock taken because on one processor
that can only be the same code coming round again.

On more than one processor that stays true of the *kernel* if only one of
them is ever in it, and `klock.rs` is the lock that makes it so. Programs
run on every processor at once; whichever of them makes a system call, takes
an interrupt or faults waits at the door until the kernel is empty.

It is the oldest way to put a kernel on a second processor, and it costs a
microkernel less than most: the file server, the network stack, the
compositor and every driver are programs, and run side by side. What is
serialised is what is left — message passing, page tables, the scheduler.
Two programs computing run in parallel. Two programs making calls take turns
at the calls.

The rules, each of which is kept in one place:

- **Taken on the way in from ring 3**, by whoever arrives: the system
  call's dispatch, and the interrupt and exception handlers. A handler that
  interrupted the kernel itself finds the lock held by its own processor and
  takes nothing. Each remembers, in its own frame, whether it took the lock,
  and gives back exactly that.
- **Carried across a switch.** The lock is the processor's and not the
  task's. A processor that switches tasks in the kernel goes on holding it,
  and the frames of the task it switches *to* say what to give back on the
  way out — they were written when that task came in. A task that is
  switched back in on another processor is switched in by one that holds the
  lock, so its frames are still right.
- **Given up before ring 3**, every way there is to get there, and by a
  processor with nothing to do, around its `hlt`.

Waiting for it is spinning with interrupts off. It is not a queue: a queue
is fairer, and worse where the machine underneath may stop running a
processor that is standing in it, which a virtual machine does. Instead a
processor that had the lock last, and finds somebody waiting, stands aside
for a moment before asking again — without that, a program making one call
after another gets it back every time, and the clock, which is an interrupt
on one processor, never gets in.

## What each processor has

`percpu.rs`: the task it is running, where that task's kernel stack ends,
a descriptor table and a task state segment (the stack an interrupt from
ring 3 is taken on is in it, and two processors run two tasks), and where
its idle loop left off. A processor finds its own through GS.

A task can be moved to another processor anywhere it can be preempted, which
in a system call is anywhere interrupts are on. So "which processor is this"
has an answer only while they are off — but one instruction through GS reads
or writes the processor the task is on at that instruction, and that is all
`percpu::current()` is.

The idle loop is task 0 on every processor, with a saved context each. It is
in no ready queue; it is what runs when the queues are empty.

## Starting the others

`acpi.rs` reads the firmware's table of processors. `smp::start` copies a
page of code (`ap_boot.s`) into the first megabyte, and for each processor
sends INIT and then STARTUP through the local APIC (`lapic.rs`). The
processor begins in real mode at that page, climbs to long mode on the first
processor's page tables, and arrives in `smp::arrive` on a stack of its own.
There it loads what the first loaded, turns on what the first turned on,
waits for the kernel lock and goes to the idle loop.

A processor that arrives after the first has stopped waiting for it halts.
Nothing would know it was there.

A machine with no ACPI tables, one processor, or no usable local APIC is one
processor with an 8259, as every machine was.

## Interrupts

Devices and the clock interrupt the first processor. What time it is, is a
counter any processor reads (`clock.rs`); seeing to what is due is the
first processor's, on its tick and between ticks by its own local APIC's
timer. They come in through the I/O APIC where the firmware lists
one (`ioapic.rs`) — which could send each to any processor, and sends all
of them to the first, because nothing yet gives a reason for another — and
through the 8259 where it does not. `intc.rs` is whichever there is.

Every other processor has its local APIC's timer, at the same hundred times
a second, and it does one thing: ends a task's turn.

One processor tells another something by interrupting it, and there are
three things to say — and a fourth that only the first processor is told,
that something is due before its timer is set to look (`idt::VEC_CLOCK`),
which is heard like the first:

- **Look at what you are running, and at what is waiting.** To a processor
  asleep with nothing to do when a task is made ready, and to the processor
  running a task that has just been ended or stopped. An ordinary interrupt:
  it takes the lock like any other way in.
- **Forget your translations.** Answered without the lock, because whoever
  asks is holding it and waiting for the answer.
- **Stop.** The kernel has faulted, and the machine halts — all of it.

A processor waiting for the lock has interrupts off and hears nothing, so it
looks for the last two each time round the wait.

## What a second processor changes

Only one thing, and everything else that had to be written follows from it:
**a task that is not the caller may be running**, in ring 3, on another
processor, at the moment the kernel does something to it.

**Ending it, and stopping it.** The task is marked — dead, or held — and its
processor is interrupted. It may run a little longer in ring 3. It never
runs in the kernel again: at each of the three doors (a system call, a
fault, an interrupt taken in ring 3) the task is looked at once the lock is
held, and one that was ended is switched away from there and then
(`scheduler::arrived`).

In between, it is dead and still standing on its kernel stack and in its
address space. So "this task is not running" is not something its state can
say, and the scheduler keeps which processor each task is on
(`scheduler::ON_CPU`). Two things wait for a dead task to be on none: its
parent being told, so that it is not collected while it runs; and its being
taken apart. The second is why throwing away an address space asks whether
any task is *on a processor* in it, and not only whether any is alive.

**Taking a page away.** A processor remembers the translations it has used.
A thread on another processor, with the same address space loaded, would go
on reading and writing a frame that has been unmapped — and then given to
somebody else. `tlb.rs`: a change that takes something away is noted, and
settled — every other processor with that address space loaded is asked to
forget, and answers — before a frame is given out again and before the
kernel lock is given up. Between those, nothing can come of it. A call that
unmaps a thousand pages interrupts the program's other threads once.

**Taking leave to write away.** A `fork` leaves the parent's pages in the
child as well, and neither may write one until it has a copy. The parent's
other threads, on other processors, remember that they may. For a page
unmapped it is enough that they forget before the frame is anybody else's;
here the frame is somebody else's already — the child's — so they are asked
at once, in the fork, before it does anything else. Until they have
forgotten, what they write is in the child too, and that is harmless only
while the child has not run: it is a write that came just before the fork
rather than just after.

**Two threads touching a new page at once.** Both fault; one is given the
page; by the time the kernel hears the other's fault the page is there. A
fault the page tables no longer agree with is no fault, and the instruction
is run again (`paging::permits`). Before there was a second processor, the
fault and its handling were one step, and the second thread would have been
told that nothing was promised at that address.

## Who runs what

The ready queues are the machine's. Each processor takes the best task
waiting: on its tick, when it has nothing to do, and when it is woken.

A task made ready wakes a processor that is asleep, if there is one. Not for
a reply to a call, though: whoever answers is about to wait for the next
call, and the caller runs there in its place. Waking another processor for
it would send the two of them back and forth between processors, with an
interrupt each way for every call.

Nothing says which processor a task runs on, and nothing keeps one where its
cache is.

## What would have to change

This is a kernel that runs on several processors. It is not yet one that
uses them well, and the difference is a list:

- **The one lock.** Four programs making calls make no more calls than one.
  Taking it apart means a lock for the scheduler, one for each address
  space's tables, one for the frame allocator, and a call between two tasks
  that does not stop a third — and an order to take them in, written down,
  because a kernel with more than one lock has a way to deadlock that a
  kernel with one has not.
- **A queue for each processor**, with work moved from a busy one to an idle
  one, and a task otherwise left where it was.
- **A better task waking that interrupts the processor running the worst**,
  instead of waiting for a tick.
- **A processor that takes no ticks while it has nothing to do.**
- **Interrupts from devices on any processor.** The I/O APIC can send one
  anywhere; every one still goes to the first.
- **More than sixteen processors**, and x2APIC ids above 255.
