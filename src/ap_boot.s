// Where a processor other than the first begins.
//
// A processor that has been reset and told to start is in real mode, at the
// beginning of a page in the first megabyte, with nothing: no stack, no
// tables, and the address of that page as its only clue to where it is.
// This is the code it finds there. It is copied to the page by `smp.rs`,
// which fills in the words at the end; from here the processor climbs to
// long mode on the first processor's page tables, takes the stack it was
// given and calls the address it was given, which is `smp::arrive`.
//
// It runs wherever it was copied to, so nothing in it is an address: the
// real-mode part reaches its data through DS, which it sets to its own
// segment, and the rest through EBX, which it sets to the page. The three
// places an address cannot be avoided — the descriptor table's, and the
// two far jumps' — are words `smp.rs` writes once it knows the page.
//
// Selectors are the kernel's own for 64-bit code and for data (0x08 and
// 0x10), so that nothing has to be reloaded when the processor takes its
// own descriptor table; 32-bit code, which only this needs, is 0x18.
//
// It is assembled into .rodata and never run from there.

.section .rodata
.balign 16
.global ap_start
.global ap_start_end
.global ap_protected
.global ap_long
.global ap_gdt
.global ap_gdt_base
.global ap_far_protected
.global ap_far_long
.global ap_word_cr0
.global ap_word_cr3
.global ap_word_cr4
.global ap_word_efer
.global ap_word_stack
.global ap_word_index
.global ap_word_entry

.code16
ap_start:
    cli
    cld
    // Where this is: the segment it was started in, as an address.
    xorl %ebx, %ebx
    movw %cs, %bx
    shll $4, %ebx
    movw %cs, %ax
    movw %ax, %ds
    lgdtl ap_gdt_ptr - ap_start
    movl %cr0, %eax
    orl $1, %eax
    movl %eax, %cr0
    // DS still has the base it had: the far pointer is read through it.
    ljmpl *(ap_far_protected - ap_start)

.code32
ap_protected:
    movw $0x10, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    // What long mode needs, and the first processor's page tables. The
    // rest of what the first processor has turned on waits until this one
    // is in Rust: some of it cannot be turned on from here.
    movl (ap_word_cr4 - ap_start)(%ebx), %eax
    movl %eax, %cr4
    movl (ap_word_cr3 - ap_start)(%ebx), %eax
    movl %eax, %cr3
    movl $0xC0000080, %ecx
    movl (ap_word_efer - ap_start)(%ebx), %eax
    xorl %edx, %edx
    wrmsr
    movl (ap_word_cr0 - ap_start)(%ebx), %eax
    movl %eax, %cr0
    ljmpl *(ap_far_long - ap_start)(%ebx)

.code64
ap_long:
    xorl %eax, %eax
    movw %ax, %fs
    movw %ax, %gs
    movl %ebx, %ebx
    movq (ap_word_stack - ap_start)(%rbx), %rsp
    movq (ap_word_index - ap_start)(%rbx), %rdi
    movq (ap_word_entry - ap_start)(%rbx), %rax
    xorl %ebp, %ebp
    callq *%rax
1:
    cli
    hlt
    jmp 1b

.balign 8
ap_gdt:
    .quad 0x0000000000000000    // [0x00] null
    .quad 0x00AF9A000000FFFF    // [0x08] 64-bit code, as the kernel's
    .quad 0x00CF92000000FFFF    // [0x10] data, as the kernel's
    .quad 0x00CF9A000000FFFF    // [0x18] 32-bit code, for the step between
ap_gdt_end:

ap_gdt_ptr:
    .short ap_gdt_end - ap_gdt - 1
ap_gdt_base:
    .long 0                     // where ap_gdt was copied to

.balign 4
ap_far_protected:
    .long 0                     // where ap_protected was copied to
    .short 0x18
.balign 4
ap_far_long:
    .long 0                     // where ap_long was copied to
    .short 0x08

.balign 8
ap_word_cr0:   .long 0
ap_word_cr3:   .long 0
ap_word_cr4:   .long 0
ap_word_efer:  .long 0
ap_word_stack: .quad 0
ap_word_index: .quad 0
ap_word_entry: .quad 0
ap_start_end:

.code64
.text
