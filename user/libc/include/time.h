/* Time, such as there is.
 *
 * Quark has a 100 Hz timer and no real-time clock, so there is no wall clock
 * to report: `time` counts from boot rather than from 1970. Saying so is
 * better than returning a confident wrong epoch — code that measures an
 * interval works, and code that wants a date gets an obviously small number
 * rather than a plausible lie.
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
