/* SPDX-License-Identifier: MIT OR Apache-2.0 */
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
extern int shared_ready(void);
extern void *__dso_handle;
extern int __cxa_atexit(void (*)(void *), void *, void *);
static int initialized;
static _Thread_local int thread_value = 19;
__attribute__((constructor)) static void before_main(void) {
    initialized = 1;
    puts("exe-init");
}
__attribute__((destructor)) static void after_main(void) {
    puts("exe-fini");
}
static void cxa_cleanup(void *value) {
    if (value != &initialized) abort();
    puts("cxa-exit");
}
static void ordinary_cleanup(void) { puts("atexit"); }
static void *worker(void *unused) {
    (void) unused;
    if (thread_value != 19) abort();
    thread_value = 41;
    puts("worker-tls");
    return (void *)(uintptr_t)thread_value;
}
int main(int argc, char **argv, char **envp) {
    if (argc != 3 || strcmp(argv[1], "one") || strcmp(argv[2], "two")) return 90;
    if (!envp || !getenv("PUFFINBOX_STARTUP_PROBE") ||
        strcmp(getenv("PUFFINBOX_STARTUP_PROBE"), "original-entry")) return 91;
    if (!initialized || !shared_ready() || thread_value != 19) return 92;
    puts("main-args-env");
    pthread_t thread;
    void *result = NULL;
    if (pthread_create(&thread, NULL, worker, NULL) || pthread_join(thread, &result)) return 93;
    if ((uintptr_t)result != 41 || thread_value != 19) return 94;
    if (__cxa_atexit(cxa_cleanup, &initialized, __dso_handle) || atexit(ordinary_cleanup)) return 95;
    return 37;
}
