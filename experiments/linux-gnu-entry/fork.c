/* SPDX-License-Identifier: MIT OR Apache-2.0 */
#include <pthread.h>
#include <stdio.h>
#include <sys/wait.h>
#include <unistd.h>
static char order[4];
static unsigned length;
static void prepare_one(void) { order[length++] = '1'; }
static void prepare_two(void) { order[length++] = '2'; }
static void parent_one(void) { order[length++] = '3'; }
static void parent_two(void) { order[length++] = '4'; }
static void child_one(void) { order[length++] = '5'; }
static void child_two(void) { order[length++] = '6'; }
int main(void) {
    if (pthread_atfork(prepare_one,parent_one,child_one) ||
        pthread_atfork(prepare_two,parent_two,child_two)) return 1;
    int channel[2];
    if (pipe(channel)) return 2;
    pid_t pid = fork();
    if (pid < 0) return 3;
    if (pid == 0) {
        close(channel[0]);
        if (length != 4 || write(channel[1],order,4) != 4) _exit(4);
        _exit(0);
    }
    close(channel[1]);
    char child_order[4];
    if (length != 4 || read(channel[0],child_order,4) != 4) return 5;
    int status;
    if (waitpid(pid,&status,0) != pid || !WIFEXITED(status) || WEXITSTATUS(status)) return 6;
    if (order[0]!='2' || order[1]!='1' || order[2]!='3' || order[3]!='4') return 7;
    if (child_order[0]!='2' || child_order[1]!='1' || child_order[2]!='5' || child_order[3]!='6') return 8;
    puts("prepare-LIFO-parent-child-FIFO");
    return 0;
}
