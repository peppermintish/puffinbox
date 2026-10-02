# SPDX-License-Identifier: MIT OR Apache-2.0
# Original AMD64 bridges to the public LSB binary interfaces.
.section .text.puffinbox_atexit,"ax",@progbits
.globl __wrap_atexit
.hidden __wrap_atexit
.type __wrap_atexit,@function
__wrap_atexit:
    xorl %esi, %esi
    xorl %edx, %edx
    jmp *__cxa_atexit@GOTPCREL(%rip)
.size __wrap_atexit, .-__wrap_atexit

.section .text.puffinbox_atfork,"ax",@progbits
.globl __wrap_pthread_atfork
.hidden __wrap_pthread_atfork
.type __wrap_pthread_atfork,@function
__wrap_pthread_atfork:
    leaq __dso_handle(%rip), %rcx
    jmp *__register_atfork@GOTPCREL(%rip)
.size __wrap_pthread_atfork, .-__wrap_pthread_atfork
.section .note.GNU-stack,"",@progbits
