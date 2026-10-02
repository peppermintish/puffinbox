# SPDX-License-Identifier: MIT OR Apache-2.0
# Original x86-64 dynamic process entry, derived from the public AMD64
# process stack/calling convention and LSB __libc_start_main interface.
# This probe requires glibc >= 2.34 for NULL-init constructor handling.
.section .text.puffinbox_entry,"ax",@progbits
.globl _start
.type _start,@function
_start:
    movq %rsp, %r10
    movq %rdx, %r9
    xorl %ebp, %ebp
    movl (%r10), %esi
    leaq 8(%r10), %rdx
    leaq main(%rip), %rdi
    xorl %ecx, %ecx
    xorl %r8d, %r8d
    andq $-16, %rsp
    subq $16, %rsp
    movq %r10, (%rsp)
    call *__libc_start_main@GOTPCREL(%rip)
    ud2
.size _start, .-_start

# A private address identifies this executable to __cxa_atexit.
.section .data.rel.ro.puffinbox_dso,"aw",@progbits
.p2align 3
.globl __dso_handle
.hidden __dso_handle
.type __dso_handle,@object
__dso_handle:
    .quad __dso_handle
.size __dso_handle, 8
.section .note.GNU-stack,"",@progbits
