# More than one processor

Quark starts every processor the machine's firmware lists and runs tasks on
all of them. The kernel itself runs on one at a time.

This is what was done, why it was done that way, and what would have to
change.

## One processor in the kernel

Everything this kernel knows about being safe it knows in one form: with
interrupts off, nothing else runs. Twenty thousand lines lean on it — every
`irq_save` in the scheduler, every table that is searched and then filled,
and, until the locks under it were made to wait, `IrqSpinLock` panicking
when it found a lock taken, because on one processor that could only be
the same code coming round again.

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

## The order locks are taken in

Below the one lock are locks of their own (`sync.rs`): each keeps
interrupts off while it is held, waits for whoever has it — reading until
it looks free, then trying, a little longer between reads each time, and
answering what other processors ask meanwhile — and has a rank. A
processor takes a lock only above every rank it holds, and anything else
stops the machine on the spot, naming both: a lock order kept by checking
every use finds a deadlock the first time the code that could make one
runs, rather than the first time two processors are unlucky together.

Two of one kind — two tasks' records for a call, two run queues for a move,
two buckets for a requeue — are taken in the order of where they are in
memory, which every processor agrees on; the second is held at the rank
after its kind's, which is its alone.

| Rank | Lock | Keeps |
|---|---|---|
| 0 | the kernel (`klock.rs`) | what has not come out from under it; taken first or not at all |
| 2 | the descriptor tables (`fdtable.rs`) | every program's descriptors, their marks and limits, and which tasks use which table |
| 4 | the poll sets (`pollset.rs`) | the sets, what each watches and who is parked on each; a set asks each thing it watches whether it is ready |
| 6 | the local sockets (`local.rs`) | the sockets and those waiting to accept; a connection makes a stream |
| 7 | the streams (`stream.rs`) | the streams and the descriptors in flight with them |
| 8 | the terminals (`pty.rs`) | the pairs, their rings and their waiters |
| 9 | the pipes (`pipe.rs`) | the pipes, their names and their waiters |
| 10 | the counters (`eventfd.rs`) | the counters and their waiters |
| 11 | the timers (`timerfd.rs`) | the timers and their waiters |
| 12 | the signal descriptors (`sigfd.rs`) | the descriptors |
| 13 | the served descriptors (`served.rs`) | the objects servers named |
| 14 | the shared regions (`shmem.rs`) | the regions, and who may map and has mapped each |
| 16, 17 | the futex's waiters (`futex.rs`) | a list of waiters, of 256; a requeue takes two |
| 18, 19 | a task's record | its calls, its wait, what others change of how it is scheduled; a call takes the caller's and the callee's |
| 20, 21 | a capability space (`cap.rs`: one of 64, the one its number picks) | its slots, its bits and how many tasks use it; a thread joining its program's, or a fork's copy, takes two |
| 22 | the programs' records (`fdtable.rs`) | a program's signals and what waits with them, its alarm and timers, what it has used, its name and limits: asked about under whatever a wait holds |
| 23 | the capability spaces' table (`cap.rs`) | which numbers have a space, and the making of a number's counts of revocations; the counts are read and moved on with no lock |
| 24, 25 | an address space (`paging::space_lock`) | its tables and reservations, and what is forgotten of them: one of 64 locks, the one its root hashes to; a fork's copy or a move between two takes both |
| 28, 29 | a processor's run queues (`runq.rs`) | what waits to run there, and the links of the tasks waiting; a move is two steps, never two held |
| 32 | the clock | what is due, and when the timer is set to look |
| 36 | interrupts (`irq_dispatch.rs`) | who is told of which |
| 40 | the displays (`display.rs`) | memory given to display drivers, which takes frames |
| 44 | the heap (`heap.rs`) | the kernel's own allocations, which take frames when it grows |
| 48 | who owns which frame (`pmm.rs`) | the owners; never held with the frames' lock |
| 52 | the frames (`pmm.rs`) | which are free |
| 60 | the console's screen | what the kernel prints, from anywhere: innermost |

The one lock's place is the reason for the rest: whatever comes out from
under it does so by taking the locks of what it touches, in this order,
and the one lock is never taken by a processor that holds any of them.
The table is what the paths that come out first take: a call takes the two
tasks' records, then a run queue; a fault takes the address space, then
the frames; a futex wake takes a bucket, then the woken task's record, then
a run queue; a pipe's read takes the pipe, then a waiter's record. What a
program makes has a lock for each kind — its table and every one of it —
rather than one for each thing: a lock for each pipe would want another
for the table's slots, and nothing yet says two programs' pipes contend.
Of these, the tasks' and the clock's have their ranks here before their
locks.

A processor that has waited thirty seconds for a lock says which, and
which processor has it, and the machine stops; each processor that was
waiting for a lock when it was stopped says which, with what it held.

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
Nothing would know it was there. And one that has not arrived by then is
sent INIT again, which resets it wherever it is, to wait for a STARTUP it
is never sent: the page's words — its stack, its number — are the next
processor's, and one that came late would read them. Then the next is
started, with its number.

The local APICs are in x2APIC mode wherever the processor has it, which
the kernel turns on itself — on the first processor, and on each other as
it arrives (`lapic::init_other`), since one that has been reset is in
xAPIC mode. In x2APIC mode a processor is named in thirty-two bits rather
than eight, and an interrupt to another is one write. There may be 256
processors (`percpu::MAX_CPUS`, declared once), listed once each though
the firmware lists one as an APIC and as an x2APIC. A processor the table
says is not enabled is not started, whether or not it says it could be
brought online — those are counted, and said.

A machine with no ACPI tables, one processor, or no usable local APIC is one
processor with an 8259, as every machine was.

## Offline and back

Any processor but the first can be taken offline and brought back while
the machine runs (`SYS_CPU_ONLINE`, by a holder of `Processors`). Offline,
it is no place a task may run (`percpu::online`, which `may_run_on` asks),
so nothing new comes to it; what it was running moves at its next door,
and its idle loop gives what waited for it to the others and parks it
(`smp::park`): in `hlt`, holding nothing, taking no tick, answering an
interrupt with nothing more than its end, and left out of shootdowns — it
forgets every translation as it comes back. Brought back, it takes the
kernel lock again and is a processor like any other. A task that may run
only where nothing is online is let run anywhere.

## Interrupts

Devices and the clock interrupt the first processor. What time it is, is a
counter any processor reads (`clock.rs`); seeing to what is due is the
first processor's, by its own local APIC's timer, set for the soonest thing
due however far off — and on its tick, in case something was not said. They come in through the I/O APIC where the firmware lists
one (`ioapic.rs`) — which could send each to any processor, and sends all
of them to the first, because nothing yet gives a reason for another — and
through the 8259 where it does not. `intc.rs` is whichever there is.

Every other processor has its local APIC's timer, at the same hundred times
a second, and it does one thing: ends a task's turn. A processor with
nothing to run takes no tick, the first included — its 8254's line is
masked while it sleeps, where the clock is the counter — and is woken by
whatever gives it something to run: a task put to wait there — one left
waiting on a busy processor among them, which that one's tick sends
(`runq::hand_to_sleeper`).
What the first processor's tick did besides is the clock's now, or an
interrupt's: an IOMMU says what it stops.

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

**Waiting in the kernel for somebody who is outside it.** On one processor
a loop in the kernel that waits without parking — try, ask whether to wait,
be told there is no need, try again — ends at the next tick, which runs
whoever it was waiting for. With the kernel one processor's at a time that
task is at the door, on another processor, with interrupts off, and the
tick finds nothing else to run: every processor is busy and the machine
has stopped. A terminal's write did this with one byte of room and a
newline, which goes out as two. Nothing can be done about such a loop from
outside it, so the lock at least says it has happened: a processor that
has waited half a minute for the kernel names the one that has it, that
one is stopped and says where it was, and the machine halts with a
sentence on the serial line rather than without one.

## Who runs what

Each processor has run queues of its own, a queue for each band
(`scheduler/runq.rs`), and runs the best of them: on its tick, when it has
nothing to do, and when it is woken. A task made ready waits on the
processor it last ran on — where what it uses may still be in the cache —
if it would run there at once: that one has nothing to do, or runs
something the task outranks. If not, it waits on one that is asleep, which
is woken — one whose core has nothing else to run first, then one in the
same package — or, if it outranks something running and none sleeps, on
the processor running the worst. Put to wait on another processor running
something worse, it interrupts that one, which switches to it at once
rather than at its tick. A task preempted waits where it was.

Not a reply to a call, though: the caller goes to the front of the queue
of the processor answering, which is about to wait for the next call, and
runs there in its place — on what was left of the turn it called in, which
its answerer ran on, and not a new one, or a pair calling each other
would never come to the end of a turn. Woken elsewhere, the two would go
back and forth between processors, with an interrupt each way for every
call.

Work moves only towards a processor that would otherwise have less: one
with nothing of its own takes the best task waiting on the busiest — of its
own package first, and of those as good one that has not run in the last
two milliseconds, whose cache it has lost anyway — and every fourth tick
one with less to run than the busiest takes a task waiting there that
weighs less than the difference, so that the two end nearer even. What a
processor has to run is weighed by how nice each task is, as a band's
share is: counted instead, two programs at nought on one processor and two
at nice 10 on another were even, and the nicer had half the machine where
they are owed a tenth of it. And each band is weighed alone, since fairness
is a question within one: weighed together, a server's turn on a processor
at the moment of the tick sent the computing programs there elsewhere. A task that has never run goes where there is
least to run. A task that moves keeps how far it has run, measured from
where its band has got to on each processor.

All of it is among the processors a task may run on (`SYS_AFFINITY`; all
of them unless it is told otherwise): where it is put to wait, what is
pulled and what balancing moves, a reply put at the front, a call handed
over and a turn put back each ask (`may_run_on`), and a task found running
where it may no longer — its set changed, or its processor taken offline —
moves as that processor next comes into the kernel. A task whose set has no
processor online in it may run on any.

A queue is found by band, and within one each part by what it holds: the
real-time tasks in order of priority, the tasks put at the front in the
order they came, and the rest by how far they have run — heaps linked
through the tasks' own records, so that choosing is never a walk of what
is waiting. It was, of one list for the machine, at every choice and every
wake: a program of four thousand threads waking ten times a second kept
the kernel choosing, and its own first thread waited minutes to run.

## What would have to change

This is a kernel that runs on several processors. It is not yet one that
uses them well, and the difference is a list:

- **The one lock.** Four programs making calls make no more calls than one.
  `callbench sweep 10` — pairs of threads calling each other, a pair to a
  processor — made 653,000 calls a second with one pair on eight
  processors under KVM, 563,000 with two, 506,000 with four and 421,000
  with eight: fewer the more there are, each waiting at the door for the
  rest. Taking it apart means a lock for the scheduler, one for each address
  space's tables, one for the frame allocator, and a call between two tasks
  that does not stop a third — and an order to take them in, written down,
  because a kernel with more than one lock has a way to deadlock that a
  kernel with one has not.
- **Interrupts from devices on any processor.** The I/O APIC can send one
  anywhere; every one still goes to the first.
- **A device's interrupt for a processor above 255**, which needs the
  IOMMU's interrupt remapping: a message names a processor in eight bits.
