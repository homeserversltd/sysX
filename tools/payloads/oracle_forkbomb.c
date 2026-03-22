/* Hostile oracle: fork storm; pids.max should contain (Phase 4). */
#include <unistd.h>

int main(void) {
    for (;;)
        fork();
}
