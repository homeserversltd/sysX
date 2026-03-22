/* Hostile oracle: drive memory cgroup OOM (Phase 4 install.py). */
#include <stdlib.h>
#include <unistd.h>

int main(void) {
    size_t chunk = 1024 * 1024;
    for (;;) {
        void *p = malloc(chunk);
        if (p)
            ((volatile char *)p)[0] = 1;
        usleep(1000);
    }
}
