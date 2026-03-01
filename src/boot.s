// Quark microkernel - boot assembly
// Multiboot2 header + 32-bit to 64-bit long mode bootstrap

// ============================================================
// Multiboot2 Header
// ============================================================
.section .multiboot_header, "a"
.align 8
mb2_header_start:
    .long 0xE85250D6                                                // magic
    .long 0                                                          // architecture: i386
    .long mb2_header_end - mb2_header_start                         // header length
    .long -(0xE85250D6 + 0 + (mb2_header_end - mb2_header_start))  // checksum

    // Address tag - allows non-ELF-aware bootloaders to load the kernel
    .align 8
    .short 2        // type: address
    .short 0        // flags: required
    .long 24        // size
    .long __mb2_header     // header_addr
    .long __mb2_header     // load_addr (load from start of image)
    .long __data_end       // load_end_addr (end of data segment)
    .long __bss_end        // bss_end_addr (end of BSS)

    // Entry address tag - 32-bit entry point
    .align 8
    .short 3        // type: entry address
    .short 0        // flags: required
    .long 12        // size
    .long _start    // entry_addr

    // Framebuffer tag - request 1024x768x32 (optional)
    // GRUB picks text mode on BIOS if pixel mode is unavailable
    .align 8
    .short 5        // type: framebuffer
    .short 1        // flags: optional
    .long 20        // size
    .long 1024      // width
    .long 768       // height
    .long 32        // depth (32 bpp)

    // End tag
    .align 8
    .short 0
    .short 0
    .long 8
mb2_header_end:

// ============================================================
// BSS: page tables and stack
// ============================================================
.section .bss
.align 4096
pml4:
    .skip 4096
pdpt:
    .skip 4096
pd0:
    .skip 4096
pd1:
    .skip 4096
pd2:
    .skip 4096
pd3:
    .skip 4096

.align 16
stack_bottom:
    .skip 65536         // 64 KiB kernel stack
stack_top:

// ============================================================
// 32-bit bootstrap (entered by multiboot2 bootloader)
// ============================================================
.section .text.boot
.code32
.global _start
_start:
    cli
    mov $stack_top, %esp

    // Save multiboot2 info pointer (ebx) on the stack
    push %ebx

    // --- Identity-map first 4 GiB using 2 MiB huge pages ---

    // PML4[0] -> PDPT
    mov $pdpt, %eax
    or $0x3, %eax               // present + writable
    mov %eax, pml4

    // PDPT[0..3] -> PD0..PD3
    mov $pd0, %eax
    or $0x3, %eax
    mov %eax, pdpt

    mov $pd1, %eax
    or $0x3, %eax
    mov %eax, pdpt + 8

    mov $pd2, %eax
    or $0x3, %eax
    mov %eax, pdpt + 16

    mov $pd3, %eax
    or $0x3, %eax
    mov %eax, pdpt + 24

    // Fill PD0..PD3 (2048 entries total) -> 2 MiB pages covering 0..4 GiB
    mov $pd0, %edi              // base of contiguous PD array
    mov $0, %ecx
    mov $0x83, %ebx             // present + writable + huge (2 MiB)
1:
    mov %ebx, (%edi,%ecx,8)
    add $0x200000, %ebx
    inc %ecx
    cmp $2048, %ecx
    jne 1b

    // Restore multiboot2 info pointer into edi (for kernel_main's rdi arg)
    pop %edi

    // --- Switch to long mode ---

    // Load PML4 into CR3
    mov $pml4, %eax
    mov %eax, %cr3

    // Enable PAE
    mov %cr4, %eax
    or $(1 << 5), %eax
    mov %eax, %cr4

    // Set long mode enable in EFER MSR
    mov $0xC0000080, %ecx
    rdmsr
    or $(1 << 8), %eax
    wrmsr

    // Enable paging (enters IA-32e compatibility mode)
    mov %cr0, %eax
    or $(1 << 31), %eax
    mov %eax, %cr0

    // Load 64-bit GDT
    lgdt gdt64_ptr

    // Far jump to 64-bit code segment
    ljmp $0x08, $_start64

// ============================================================
// 64-bit entry point
// ============================================================
.code64
.global _start64
_start64:
    // Reload data segments
    mov $0x10, %ax
    mov %ax, %ds
    mov %ax, %es
    mov %ax, %fs
    mov %ax, %gs
    mov %ax, %ss

    // Set up 64-bit stack
    mov $stack_top, %rsp

    // edi still holds multiboot2 info pointer (zero-extended to rdi)
    call kernel_main

    // Halt if kernel_main returns
2:
    cli
    hlt
    jmp 2b

// ============================================================
// GDT for 64-bit mode
// ============================================================
.section .rodata
.align 16
gdt64:
    .quad 0x0000000000000000    // null descriptor
    .quad 0x00AF9A000000FFFF    // code: 64-bit, present, ring 0, executable
    .quad 0x00CF92000000FFFF    // data: present, ring 0, writable
gdt64_end:

gdt64_ptr:
    .short gdt64_end - gdt64 - 1
    .long gdt64
