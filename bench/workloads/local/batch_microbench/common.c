#define _GNU_SOURCE

#include "batch_microbench.h"

#include <errno.h>
#include <linux/futex.h>
#include <math.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

uint64_t bm_now_ns(clockid_t clock_id)
{
	struct timespec value;

	if (clock_gettime(clock_id, &value) != 0)
		return 0;
	return (uint64_t)value.tv_sec * 1000000000ULL + (uint64_t)value.tv_nsec;
}

uint64_t bm_seconds_to_ns(double seconds)
{
	if (!isfinite(seconds) || seconds <= 0.0)
		return 0;
	if (seconds >= (double)UINT64_MAX / 1000000000.0)
		return UINT64_MAX;
	return (uint64_t)llround(seconds * 1000000000.0);
}

int bm_set_role(const char *name, int cpu, uint64_t *affinity_errors)
{
	cpu_set_t cpuset;
	int error = 0;

	if (prctl(PR_SET_NAME, name, 0, 0, 0) != 0)
		error = errno;
	CPU_ZERO(&cpuset);
	CPU_SET(cpu, &cpuset);
	if (sched_setaffinity(0, sizeof(cpuset), &cpuset) != 0) {
		if (affinity_errors != NULL)
			(*affinity_errors)++;
		if (error == 0)
			error = errno;
	}
	return error == 0 ? 0 : -error;
}

void bm_check_affinity(int expected_cpu, uint64_t *violations)
{
	int current = sched_getcpu();

	if (current < 0 || current != expected_cpu)
		(*violations)++;
}

static uint64_t nonnegative_long_delta(long before, long after)
{
	return after > before ? (uint64_t)(after - before) : 0;
}

struct bm_rusage_delta bm_rusage_diff(const struct rusage *before,
				      const struct rusage *after)
{
	return (struct bm_rusage_delta) {
		.voluntary_switches = nonnegative_long_delta(before->ru_nvcsw,
							       after->ru_nvcsw),
		.involuntary_switches = nonnegative_long_delta(before->ru_nivcsw,
								 after->ru_nivcsw),
		.minor_faults = nonnegative_long_delta(before->ru_minflt,
						       after->ru_minflt),
		.major_faults = nonnegative_long_delta(before->ru_majflt,
						       after->ru_majflt),
	};
}

void bm_rusage_add(struct bm_rusage_delta *total,
			  const struct bm_rusage_delta *value)
{
	total->voluntary_switches += value->voluntary_switches;
	total->involuntary_switches += value->involuntary_switches;
	total->minor_faults += value->minor_faults;
	total->major_faults += value->major_faults;
}

static int futex_wait(_Atomic uint32_t *value, uint32_t expected,
		      const struct timespec *timeout)
{
	return (int)syscall(SYS_futex, (uint32_t *)value, FUTEX_WAIT, expected,
			    timeout, NULL, 0);
}

static void futex_wake_all(_Atomic uint32_t *value)
{
	(void)syscall(SYS_futex, (uint32_t *)value, FUTEX_WAKE, INT32_MAX,
		      NULL, NULL, 0);
}

void bm_signal_u32(_Atomic uint32_t *value, uint32_t next)
{
	atomic_store_explicit(value, next, memory_order_release);
	futex_wake_all(value);
}

uint32_t bm_increment_u32(_Atomic uint32_t *value)
{
	uint32_t next = atomic_fetch_add_explicit(value, 1, memory_order_release) + 1U;

	futex_wake_all(value);
	return next;
}

void bm_wait_u32_change(_Atomic uint32_t *value, uint32_t expected)
{
	while (atomic_load_explicit(value, memory_order_acquire) == expected) {
		if (futex_wait(value, expected, NULL) == 0)
			continue;
		if (errno != EAGAIN && errno != EINTR)
			break;
	}
}

int bm_wait_counter(_Atomic uint32_t *counter, uint32_t target,
		    _Atomic uint32_t *fatal, double timeout_seconds)
{
	uint64_t timeout_ns = bm_seconds_to_ns(timeout_seconds);
	uint64_t started = bm_now_ns(CLOCK_MONOTONIC);

	for (;;) {
		uint32_t current = atomic_load_explicit(counter, memory_order_acquire);
		struct timespec short_wait = { .tv_sec = 1, .tv_nsec = 0 };

		if (current >= target)
			return 0;
		if (fatal != NULL &&
		    atomic_load_explicit(fatal, memory_order_acquire) != 0)
			return -1;
		if (timeout_ns != 0 &&
		    bm_now_ns(CLOCK_MONOTONIC) - started >= timeout_ns)
			return -1;
		(void)futex_wait(counter, current, &short_wait);
	}
}

void bm_kill_and_reap(pid_t *children, size_t nr_children)
{
	size_t index;

	for (index = 0; index < nr_children; index++) {
		if (children[index] > 0)
			(void)kill(children[index], SIGKILL);
	}
	for (index = 0; index < nr_children; index++) {
		if (children[index] > 0) {
			while (waitpid(children[index], NULL, 0) < 0 && errno == EINTR)
				;
			children[index] = 0;
		}
	}
}

int bm_reap_children(pid_t *children, size_t nr_children)
{
	int failed = 0;
	size_t index;

	for (index = 0; index < nr_children; index++) {
		int status = 0;
		pid_t result;

		if (children[index] <= 0)
			continue;
		do {
			result = waitpid(children[index], &status, 0);
		} while (result < 0 && errno == EINTR);
		if (result < 0 || !WIFEXITED(status) || WEXITSTATUS(status) != 0)
			failed = -1;
		children[index] = 0;
	}
	return failed;
}
