/* SPDX-License-Identifier: MIT OR Apache-2.0 */
#include <stdio.h>
static int ready;
__attribute__((constructor)) static void start_shared(void) {
    ready = 1;
    puts("shared-init");
}
__attribute__((destructor)) static void end_shared(void) {
    puts("shared-fini");
}
int shared_ready(void) { return ready; }
