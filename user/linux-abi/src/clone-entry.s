/* Where a new thread begins.
 *
 * Entered with RDI holding the thread function's argument, and RSP pointing at
 * two words the creator planted: the thread pointer, then the function.
 */
	.text
	.global __quark_thread_entry
	.type   __quark_thread_entry,@function
__quark_thread_entry:
	mov %rdi,%rbx           /* the argument, before anything clobbers it */
	pop %rdi                /* the thread pointer */
	pop %r12                /* the function */
	pop %r13                /* the word to clear when this thread exits */
	and $-16,%rsp           /* a call needs RSP aligned; below here is ours */
	test %rdi,%rdi
	jz 1f
	call __quark_set_fs     /* FS base, which is where thread-locals hang */
1:
	test %r13,%r13
	jz 2f
	mov %r13,%rdi
	call __quark_set_clear_tid
2:
	mov %rbx,%rdi
	call *%r12
	mov %eax,%edi
	call __quark_thread_exit
	hlt
	.size __quark_thread_entry,.-__quark_thread_entry
	.section .note.GNU-stack,"",@progbits
