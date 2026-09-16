/* Time.
 *
 * Quark has a 100 Hz timer, and a date the kernel read from the machine's
 * clock at boot: `time` is that date plus the ticks since, in seconds since
 * 1970. A machine with no clock reports seconds since boot instead — an
 * obviously small number rather than a plausible lie.
 */
#ifndef _TIME_H
#define _TIME_H

#include <stddef.h>
#include <sys/types.h>

#define CLOCKS_PER_SEC 100L

struct timespec {
    time_t tv_sec;
    long   tv_nsec;
};

/* Seconds since boot. Stores the value through `t` as well, if given. */
time_t time(time_t *t);

/* Timer ticks since boot, at CLOCKS_PER_SEC. */
clock_t clock(void);

/* Sleep for `req`. `rem` is unused: nothing here interrupts a sleep. */
int nanosleep(const struct timespec *req, struct timespec *rem);

#endif
