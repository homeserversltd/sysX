/* Hostile oracle: ignore SIGTERM/SIGINT; FD3 readiness then spin (autotest stop ladder). */
#include <signal.h>
#include <unistd.h>

int main(void) {
    signal(SIGTERM, SIG_IGN);
    signal(SIGINT, SIG_IGN);
    {
        const char b[] = "R";
        (void)write(3, b, 1);
    }
    for (;;)
        pause();
}
