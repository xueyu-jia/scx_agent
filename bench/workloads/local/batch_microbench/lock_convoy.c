#define _GNU_SOURCE

#include "batch_microbench.h"

#include <errno.h>
#include <math.h>
#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <time.h>
#include <unistd.h>

enum lock_phase {
	LOCK_INIT,
	LOCK_WARMUP,
	LOCK_MEASURE,
	LOCK_STOP,
};

struct lock_shared {
	pthread_mutex_t convoy_mutex;
	_Atomic uint32_t phase;
	_Atomic uint32_t ready;
	_Atomic uint32_t fatal;
	_Atomic uint32_t start_seq;
	_Atomic uint32_t locked_seq;
	_Atomic uint32_t waiter_attempted_seq;
	_Atomic uint32_t waiter_parked_seq;
	_Atomic uint32_t waiter_gate_seq;
	_Atomic uint32_t completed_seq;
	_Atomic uint32_t warmup_done;
	_Atomic uint32_t measure_done;
	_Atomic uint32_t background_measure_ready;
	uint32_t warmup_trials;
	uint32_t measure_trials;
	uint64_t period_ns;
	uint64_t critical_ns;
	uint64_t warmup_start_ns;
	uint64_t measure_start_ns;
	uint64_t measure_deadline_ns;
	pid_t holder_pid;
	struct bm_lock_role_result roles[BM_LOCK_ROLES];
	struct bm_lock_sample samples[];
};

static void sleep_until(uint64_t deadline_ns)
{
	struct timespec deadline = {
		.tv_sec = (time_t)(deadline_ns / 1000000000ULL),
		.tv_nsec = (long)(deadline_ns % 1000000000ULL),
	};

	while (clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME, &deadline, NULL) != 0 &&
	       errno == EINTR)
		;
}

static uint64_t abs_diff_u64(uint64_t left, uint64_t right)
{
	return left > right ? left - right : right - left;
}

static uint64_t burn_thread_cpu(uint64_t service_ns, int expected_cpu,
				uint64_t *affinity_violations,
				uint64_t *state, uint64_t *operations)
{
	uint64_t started = bm_now_ns(CLOCK_THREAD_CPUTIME_ID);
	uint64_t deadline = started + service_ns;
	uint64_t current = started;
	uint64_t value = *state;
	uint64_t count = 0;

	while (current < deadline) {
		unsigned int index;

		for (index = 0; index < 64U; index++) {
			value ^= value << 13;
			value ^= value >> 7;
			value ^= value << 17;
		}
		count += 64U;
		current = bm_now_ns(CLOCK_THREAD_CPUTIME_ID);
		bm_check_affinity(expected_cpu, affinity_violations);
	}
	*state = value;
	*operations += count;
	return current - started;
}

static bool is_measurement_trial(const struct lock_shared *shared, uint32_t seq)
{
	return seq > shared->warmup_trials;
}

static size_t sample_index(const struct lock_shared *shared, uint32_t seq)
{
	return (size_t)(seq - shared->warmup_trials - 1U);
}

static void role_failed(struct lock_shared *shared)
{
	atomic_store_explicit(&shared->fatal, 1, memory_order_release);
}

static void wait_for_phase_change(struct lock_shared *shared, uint32_t phase)
{
	while (atomic_load_explicit(&shared->phase, memory_order_acquire) == phase)
		bm_wait_u32_change(&shared->phase, phase);
}

static void holder_process(const struct bm_lock_options *options,
			   struct lock_shared *shared)
{
	struct bm_lock_role_result result = { 0 };
	struct rusage before = { 0 }, after = { 0 };
	uint64_t state = 0x243f6a8885a308d3ULL;
	uint32_t last_seq = 0;
	bool measuring = false;

	(void)bm_set_role("bm-holder", options->common.cpu0,
			  &result.affinity_errors);
	shared->holder_pid = getpid();
	(void)bm_increment_u32(&shared->ready);
	wait_for_phase_change(shared, LOCK_INIT);

	for (;;) {
		uint32_t seq = atomic_load_explicit(&shared->start_seq,
						    memory_order_acquire);
		bool measured;
		struct bm_lock_sample *sample = NULL;
		uint64_t service_ns;
		int rc;

		if (seq == UINT32_MAX)
			break;
		if (seq <= last_seq) {
			if (atomic_load_explicit(&shared->phase,
						 memory_order_acquire) == LOCK_STOP)
				break;
			bm_wait_u32_change(&shared->start_seq, seq);
			continue;
		}
		measured = is_measurement_trial(shared, seq);
		if (measured && !measuring) {
			result.affinity_violations = 0;
			(void)getrusage(RUSAGE_SELF, &before);
			result.started_ns = shared->measure_start_ns;
			measuring = true;
		}
		if (measured)
			sample = &shared->samples[sample_index(shared, seq)];

		rc = pthread_mutex_lock(&shared->convoy_mutex);
		if (rc != 0) {
			role_failed(shared);
			_exit(1);
		}
		bm_check_affinity(options->common.cpu0, &result.affinity_violations);
		bm_signal_u32(&shared->locked_seq, seq);
		if (bm_wait_counter(&shared->waiter_attempted_seq, seq,
				    &shared->fatal, 5.0) != 0 ||
		    bm_wait_counter(&shared->waiter_parked_seq, seq,
				    &shared->fatal, 5.0) != 0) {
			(void)pthread_mutex_unlock(&shared->convoy_mutex);
			role_failed(shared);
			_exit(1);
		}

		/* Only the CPU-time service itself is included in this measurement. */
		if (sample != NULL)
			sample->holder_burn_start_ns = bm_now_ns(CLOCK_MONOTONIC);
		service_ns = burn_thread_cpu(shared->critical_ns,
					     options->common.cpu0,
					     &result.affinity_violations,
					     &state, &result.operations);
		if (sample != NULL)
			sample->holder_burn_end_ns = bm_now_ns(CLOCK_MONOTONIC);
		rc = pthread_mutex_unlock(&shared->convoy_mutex);
		if (rc != 0) {
			role_failed(shared);
			_exit(1);
		}
		if (sample != NULL) {
			sample->holder_service_ns = service_ns;
			sample->unlock_ns = bm_now_ns(CLOCK_MONOTONIC);
			sample->holder_cpu_unlock_ns =
				bm_now_ns(CLOCK_THREAD_CPUTIME_ID);
			atomic_thread_fence(memory_order_release);
		}
		/* The explicit futex gate prevents userspace mutex spinning. */
		bm_signal_u32(&shared->waiter_gate_seq, seq);
		last_seq = seq;
	}

	if (measuring) {
		(void)getrusage(RUSAGE_SELF, &after);
		result.usage = bm_rusage_diff(&before, &after);
	}
	result.finished_ns = bm_now_ns(CLOCK_MONOTONIC);
	result.checksum = state;
	result.elapsed_ns = shared->measure_deadline_ns - shared->measure_start_ns;
	result.completed = 1U;
	shared->roles[BM_LOCK_HOLDER] = result;
	_exit(0);
}

static void waiter_process(const struct bm_lock_options *options,
			   struct lock_shared *shared)
{
	struct bm_lock_role_result result = { 0 };
	struct rusage before = { 0 }, after = { 0 };
	clockid_t holder_clock = CLOCK_MONOTONIC;
	uint32_t last_seq = 0;
	bool measuring = false;

	(void)bm_set_role("bm-waiter", options->common.cpu1,
			  &result.affinity_errors);
	(void)bm_increment_u32(&shared->ready);
	wait_for_phase_change(shared, LOCK_INIT);
	if (clock_getcpuclockid(shared->holder_pid, &holder_clock) != 0)
		result.clock_errors++;

	for (;;) {
		uint32_t seq = atomic_load_explicit(&shared->locked_seq,
						    memory_order_acquire);
		bool measured;
		struct bm_lock_sample *sample = NULL;
		uint64_t request_ns;
		uint64_t holder_cpu_request = 0;
		uint64_t acquire_ns;
		int rc;
		bool contended = false;

		if (seq == UINT32_MAX)
			break;
		if (seq <= last_seq) {
			if (atomic_load_explicit(&shared->phase,
						 memory_order_acquire) == LOCK_STOP)
				break;
			bm_wait_u32_change(&shared->locked_seq, seq);
			continue;
		}
		measured = is_measurement_trial(shared, seq);
		if (measured && !measuring) {
			result.affinity_violations = 0;
			(void)getrusage(RUSAGE_SELF, &before);
			result.started_ns = shared->measure_start_ns;
			measuring = true;
		}
		if (measured)
			sample = &shared->samples[sample_index(shared, seq)];
		request_ns = bm_now_ns(CLOCK_MONOTONIC);
		if (result.clock_errors == 0)
			holder_cpu_request = bm_now_ns(holder_clock);
		bm_check_affinity(options->common.cpu1, &result.affinity_violations);

		rc = pthread_mutex_trylock(&shared->convoy_mutex);
		if (rc == 0) {
			/* Preserve mutex ownership semantics even for a missed sample. */
			(void)pthread_mutex_unlock(&shared->convoy_mutex);
		} else if (rc == EBUSY) {
			contended = true;
		} else {
			role_failed(shared);
			_exit(1);
		}
		if (sample != NULL) {
			sample->request_ns = request_ns;
			sample->holder_cpu_request_ns = holder_cpu_request;
			sample->attempted = 1U;
			sample->contended = contended ? 1U : 0U;
		}
		bm_signal_u32(&shared->waiter_attempted_seq, seq);
		if (sample != NULL)
			sample->parked = 1U;
		bm_signal_u32(&shared->waiter_parked_seq, seq);
		while (atomic_load_explicit(&shared->waiter_gate_seq,
					    memory_order_acquire) < seq) {
			uint32_t gate = atomic_load_explicit(&shared->waiter_gate_seq,
							     memory_order_acquire);

			bm_wait_u32_change(&shared->waiter_gate_seq, gate);
		}

		rc = pthread_mutex_lock(&shared->convoy_mutex);
		if (rc != 0) {
			role_failed(shared);
			_exit(1);
		}
		acquire_ns = bm_now_ns(CLOCK_MONOTONIC);
		if (measured) {
			uint64_t holder_cpu_after_request;
			uint64_t burn_wall_ns;
			double service_error_pct;

			atomic_thread_fence(memory_order_acquire);
			sample->acquire_ns = acquire_ns;
			holder_cpu_after_request =
				sample->holder_cpu_unlock_ns > holder_cpu_request ?
				sample->holder_cpu_unlock_ns - holder_cpu_request : 0;
			sample->holder_cpu_after_request_ns = holder_cpu_after_request;
			sample->total_wait_ns = acquire_ns > request_ns ?
				acquire_ns - request_ns : 0;
			sample->handoff_ns = acquire_ns > sample->unlock_ns ?
				acquire_ns - sample->unlock_ns : 0;
			burn_wall_ns = sample->holder_burn_end_ns >
				       sample->holder_burn_start_ns ?
				sample->holder_burn_end_ns -
					sample->holder_burn_start_ns : 0;
			sample->holder_descheduled_ns =
				burn_wall_ns > sample->holder_service_ns ?
				burn_wall_ns - sample->holder_service_ns : 0;
			sample->service_error_ns =
				abs_diff_u64(sample->holder_service_ns,
					     shared->critical_ns);
			service_error_pct = shared->critical_ns == 0 ? 100.0 :
				(double)sample->service_error_ns * 100.0 /
				(double)shared->critical_ns;
			sample->service_valid = service_error_pct < 5.0 ? 1U : 0U;
			sample->deadline_valid =
				request_ns >= shared->measure_start_ns &&
				sample->unlock_ns <= shared->measure_deadline_ns &&
				acquire_ns <= shared->measure_deadline_ns;
			sample->valid = sample->launched && sample->attempted &&
				sample->parked && sample->contended &&
				sample->deadline_valid && sample->service_valid;
		}
		rc = pthread_mutex_unlock(&shared->convoy_mutex);
		if (rc != 0) {
			role_failed(shared);
			_exit(1);
		}
		bm_signal_u32(&shared->completed_seq, seq);
		last_seq = seq;
	}

	if (measuring) {
		(void)getrusage(RUSAGE_SELF, &after);
		result.usage = bm_rusage_diff(&before, &after);
	}
	result.finished_ns = bm_now_ns(CLOCK_MONOTONIC);
	result.elapsed_ns = shared->measure_deadline_ns - shared->measure_start_ns;
	result.completed = 1U;
	shared->roles[BM_LOCK_WAITER] = result;
	_exit(0);
}

static int wait_for_completion(struct lock_shared *shared, uint32_t seq,
			       double timeout_seconds)
{
	return bm_wait_counter(&shared->completed_seq, seq, &shared->fatal,
			       timeout_seconds);
}

static int coordinate_slots(struct lock_shared *shared, uint32_t first_seq,
			    uint32_t count, uint64_t first_release_ns,
			    bool measured, struct bm_lock_role_result *result,
			    int expected_cpu, uint32_t *last_launched_out)
{
	const uint64_t period_ns = shared->period_ns;
	uint32_t last_launched = atomic_load_explicit(&shared->start_seq,
						    memory_order_acquire);
	uint32_t offset;

	for (offset = 0; offset < count; offset++) {
		uint32_t seq = first_seq + offset;
		uint64_t target_ns = first_release_ns + (uint64_t)offset * period_ns;
		uint64_t now;
		uint64_t lateness;
		bool previous_complete;
		bool expired;
		struct bm_lock_sample *sample = NULL;

		sleep_until(target_ns);
		now = bm_now_ns(CLOCK_MONOTONIC);
		lateness = now > target_ns ? now - target_ns : 0;
		previous_complete =
			atomic_load_explicit(&shared->completed_seq,
					     memory_order_acquire) >= last_launched;
		expired = lateness >= period_ns || !previous_complete ||
			  (measured && now >= shared->measure_deadline_ns);
		if (measured) {
			sample = &shared->samples[offset];
			sample->scheduled_ns = target_ns;
			sample->launch_ns = now;
			sample->launch_lateness_ns = lateness;
		}
		if (expired) {
			result->missed_deadlines++;
			if (sample != NULL)
				sample->dropped = 1U;
			continue;
		}
		bm_check_affinity(expected_cpu, &result->affinity_violations);
		if (sample != NULL)
			sample->launched = 1U;
		atomic_thread_fence(memory_order_release);
		bm_signal_u32(&shared->start_seq, seq);
		last_launched = seq;
	}
	*last_launched_out = last_launched;
	return 0;
}

static void coordinator_process(const struct bm_lock_options *options,
				struct lock_shared *shared)
{
	struct bm_lock_role_result result = { 0 };
	struct rusage before = { 0 }, after = { 0 };
	uint32_t last_launched = 0;

	(void)bm_set_role("bm-coord", options->common.cpu1,
			  &result.affinity_errors);
	(void)bm_increment_u32(&shared->ready);
	wait_for_phase_change(shared, LOCK_INIT);
	if (atomic_load_explicit(&shared->phase, memory_order_acquire) != LOCK_WARMUP)
		goto fail;
	if (coordinate_slots(shared, 1U, shared->warmup_trials,
			     shared->warmup_start_ns, false, &result,
			     options->common.cpu1, &last_launched) != 0)
		goto fail;
	if (last_launched != 0 &&
	    wait_for_completion(shared, last_launched, 5.0) != 0)
		goto fail;
	bm_signal_u32(&shared->warmup_done, 1U);
	wait_for_phase_change(shared, LOCK_WARMUP);
	if (atomic_load_explicit(&shared->phase, memory_order_acquire) != LOCK_MEASURE)
		goto fail;

	result.affinity_violations = 0;
	result.missed_deadlines = 0;
	if (bm_wait_counter(&shared->background_measure_ready, 1U,
			    &shared->fatal, 5.0) != 0)
		goto fail;
	(void)getrusage(RUSAGE_SELF, &before);
	result.started_ns = shared->measure_start_ns;
	if (coordinate_slots(shared, shared->warmup_trials + 1U,
			     shared->measure_trials, shared->measure_start_ns,
			     true, &result, options->common.cpu1,
			     &last_launched) != 0)
		goto fail;
	sleep_until(shared->measure_deadline_ns);
	(void)getrusage(RUSAGE_SELF, &after);
	result.usage = bm_rusage_diff(&before, &after);
	/* Cleanup may complete after the deadline, but such a sample is invalid. */
	if (last_launched > shared->warmup_trials &&
	    wait_for_completion(shared, last_launched, 5.0) != 0)
		goto fail;
	result.elapsed_ns = shared->measure_deadline_ns - shared->measure_start_ns;
	result.finished_ns = bm_now_ns(CLOCK_MONOTONIC);
	result.completed = 1U;
	shared->roles[BM_LOCK_COORDINATOR] = result;
	bm_signal_u32(&shared->measure_done, 1U);
	_exit(0);

fail:
	role_failed(shared);
	_exit(1);
}

static void background_process(const struct bm_lock_options *options,
			       struct lock_shared *shared)
{
	struct bm_lock_role_result result = { 0 };
	struct rusage before = { 0 }, after = { 0 };
	uint64_t state = 0x13198a2e03707344ULL;
	uint32_t phase;

	(void)bm_set_role("bm-bg", options->common.cpu0, &result.affinity_errors);
	(void)bm_increment_u32(&shared->ready);
	wait_for_phase_change(shared, LOCK_INIT);

	phase = atomic_load_explicit(&shared->phase, memory_order_acquire);
	while (phase == LOCK_WARMUP) {
		uint64_t ignored = 0;

		(void)burn_thread_cpu(100000ULL, options->common.cpu0,
				      &result.affinity_violations, &state, &ignored);
		phase = atomic_load_explicit(&shared->phase, memory_order_acquire);
	}
	if (phase != LOCK_MEASURE)
		goto fail;
	result.affinity_violations = 0;
	bm_signal_u32(&shared->background_measure_ready, 1U);
	while (bm_now_ns(CLOCK_MONOTONIC) < shared->measure_start_ns) {
		uint64_t ignored = 0;

		(void)burn_thread_cpu(100000ULL, options->common.cpu0,
				      &result.affinity_violations, &state, &ignored);
	}
	(void)getrusage(RUSAGE_SELF, &before);
	result.started_ns = shared->measure_start_ns;
	while (bm_now_ns(CLOCK_MONOTONIC) < shared->measure_deadline_ns) {
		uint64_t ignored_service;

		ignored_service = burn_thread_cpu(100000ULL, options->common.cpu0,
						  &result.affinity_violations,
						  &state, &result.operations);
		(void)ignored_service;
	}
	(void)getrusage(RUSAGE_SELF, &after);
	result.usage = bm_rusage_diff(&before, &after);
	result.finished_ns = bm_now_ns(CLOCK_MONOTONIC);
	result.elapsed_ns = result.finished_ns > result.started_ns ?
		result.finished_ns - result.started_ns : 0;
	result.checksum = state;
	result.completed = 1U;
	shared->roles[BM_LOCK_BACKGROUND] = result;
	_exit(0);

fail:
	role_failed(shared);
	_exit(1);
}

static int compare_u64(const void *left, const void *right)
{
	uint64_t lhs = *(const uint64_t *)left;
	uint64_t rhs = *(const uint64_t *)right;

	return lhs < rhs ? -1 : lhs > rhs ? 1 : 0;
}

static uint64_t percentile(const uint64_t *sorted, size_t count, double fraction)
{
	size_t rank;

	if (count == 0)
		return 0;
	rank = (size_t)ceil(fraction * (double)count);
	if (rank == 0)
		rank = 1;
	if (rank > count)
		rank = count;
	return sorted[rank - 1U];
}

static int write_raw_samples(const struct lock_shared *shared,
			     struct bm_lock_summary *summary)
{
	const char *output_dir = getenv("SCX_BENCH_OUT");
	FILE *output;
	uint32_t index;

	if (output_dir == NULL || output_dir[0] == '\0')
		output_dir = ".";
	if (snprintf(summary->raw_samples_path, sizeof(summary->raw_samples_path),
		     "%s/lock_samples.csv", output_dir) >=
	    (int)sizeof(summary->raw_samples_path))
		return -1;
	output = fopen(summary->raw_samples_path, "w");
	if (output == NULL)
		return -1;
	(void)fprintf(output,
		"sample,scheduled_ns,launch_ns,launch_lateness_ns,launched,dropped,"
		"attempted,parked,contended,deadline_valid,service_valid,"
		"request_ns,unlock_ns,acquire_ns,total_wait_ns,"
		"holder_descheduled_ns,handoff_ns,holder_cpu_after_request_ns,"
		"holder_burn_start_ns,holder_burn_end_ns,holder_service_ns,"
		"service_error_ns,valid\n");
	for (index = 0; index < shared->measure_trials; index++) {
		const struct bm_lock_sample *sample = &shared->samples[index];

		(void)fprintf(output,
			"%u,%llu,%llu,%llu,%u,%u,%u,%u,%u,%u,%u,"
			"%llu,%llu,%llu,%llu,%llu,%llu,%llu,%llu,%llu,%llu,%llu,%u\n",
			index,
			(unsigned long long)sample->scheduled_ns,
			(unsigned long long)sample->launch_ns,
			(unsigned long long)sample->launch_lateness_ns,
			sample->launched, sample->dropped, sample->attempted,
			sample->parked, sample->contended, sample->deadline_valid,
			sample->service_valid,
			(unsigned long long)sample->request_ns,
			(unsigned long long)sample->unlock_ns,
			(unsigned long long)sample->acquire_ns,
			(unsigned long long)sample->total_wait_ns,
			(unsigned long long)sample->holder_descheduled_ns,
			(unsigned long long)sample->handoff_ns,
			(unsigned long long)sample->holder_cpu_after_request_ns,
			(unsigned long long)sample->holder_burn_start_ns,
			(unsigned long long)sample->holder_burn_end_ns,
			(unsigned long long)sample->holder_service_ns,
			(unsigned long long)sample->service_error_ns,
			sample->valid);
	}
	if (fclose(output) != 0)
		return -1;
	return 0;
}

static int summarize_lock(const struct lock_shared *shared,
			  struct bm_lock_summary *summary)
{
	uint64_t *total_wait;
	uint64_t *holder_descheduled;
	uint64_t *handoff;
	uint64_t *launch_lateness;
	const double fractions[4] = { 0.50, 0.90, 0.99, 0.999 };
	size_t valid = 0;
	size_t lateness_count = 0;
	bool roles_complete = true;
	uint32_t index;
	size_t percentile_index;

	memset(summary, 0, sizeof(*summary));
	total_wait = calloc(shared->measure_trials, sizeof(*total_wait));
	holder_descheduled = calloc(shared->measure_trials,
				     sizeof(*holder_descheduled));
	handoff = calloc(shared->measure_trials, sizeof(*handoff));
	launch_lateness = calloc(shared->measure_trials, sizeof(*launch_lateness));
	if (total_wait == NULL || holder_descheduled == NULL || handoff == NULL ||
	    launch_lateness == NULL) {
		free(total_wait);
		free(holder_descheduled);
		free(handoff);
		free(launch_lateness);
		return -1;
	}

	summary->scheduled_sample_count = shared->measure_trials;
	for (index = 0; index < BM_LOCK_ROLES; index++) {
		summary->roles[index] = shared->roles[index];
		if (!shared->roles[index].completed)
			roles_complete = false;
		summary->affinity_violations +=
			shared->roles[index].affinity_violations;
		summary->affinity_errors += shared->roles[index].affinity_errors;
		summary->clock_errors += shared->roles[index].clock_errors;
		summary->missed_deadlines += shared->roles[index].missed_deadlines;
		bm_rusage_add(&summary->usage, &shared->roles[index].usage);
	}
	for (index = 0; index < shared->measure_trials; index++) {
		const struct bm_lock_sample *sample = &shared->samples[index];
		double error_pct;

		launch_lateness[lateness_count++] = sample->launch_lateness_ns;
		if (sample->launch_lateness_ns >
		    (uint64_t)(summary->launch_lateness_max_us * 1000.0))
			summary->launch_lateness_max_us =
				(double)sample->launch_lateness_ns / 1000.0;
		if (sample->dropped)
			summary->dropped_slots++;
		if (!sample->launched) {
			summary->invalid_sample_count++;
			continue;
		}
		summary->launched_sample_count++;
		if (!sample->attempted || !sample->parked)
			summary->handshake_errors++;
		if (sample->contended)
			summary->contended_sample_count++;
		else
			summary->missed_contentions++;
		if (!sample->deadline_valid)
			summary->deadline_violations++;
		error_pct = shared->critical_ns == 0 ? 100.0 :
			(double)sample->service_error_ns * 100.0 /
			(double)shared->critical_ns;
		if (!sample->service_valid)
			summary->service_error_count++;
		if (error_pct > summary->service_error_max_pct)
			summary->service_error_max_pct = error_pct;
		if (sample->valid) {
			total_wait[valid] = sample->total_wait_ns;
			holder_descheduled[valid] = sample->holder_descheduled_ns;
			handoff[valid] = sample->handoff_ns;
			valid++;
		} else {
			summary->invalid_sample_count++;
		}
	}
	qsort(total_wait, valid, sizeof(*total_wait), compare_u64);
	qsort(holder_descheduled, valid, sizeof(*holder_descheduled), compare_u64);
	qsort(handoff, valid, sizeof(*handoff), compare_u64);
	qsort(launch_lateness, lateness_count, sizeof(*launch_lateness), compare_u64);
	for (percentile_index = 0; percentile_index < 4; percentile_index++) {
		summary->total_wait_us[percentile_index] =
			(double)percentile(total_wait, valid,
					   fractions[percentile_index]) / 1000.0;
		summary->holder_descheduled_us[percentile_index] =
			(double)percentile(holder_descheduled, valid,
					   fractions[percentile_index]) / 1000.0;
		summary->handoff_us[percentile_index] =
			(double)percentile(handoff, valid,
					   fractions[percentile_index]) / 1000.0;
		summary->launch_lateness_us[percentile_index] =
			(double)percentile(launch_lateness, lateness_count,
					   fractions[percentile_index]) / 1000.0;
	}
	summary->sample_count = valid;
	if (summary->launched_sample_count != 0)
		summary->contended_rate =
			(double)summary->contended_sample_count /
			(double)summary->launched_sample_count;
	summary->bg_operations = shared->roles[BM_LOCK_BACKGROUND].operations;
	summary->bg_elapsed_ns = shared->roles[BM_LOCK_BACKGROUND].elapsed_ns;
	if (summary->bg_elapsed_ns != 0)
		summary->bg_ops_per_sec =
			(double)summary->bg_operations * 1000000000.0 /
			(double)summary->bg_elapsed_ns;
	summary->valid =
		roles_complete && summary->affinity_violations == 0 &&
		summary->affinity_errors == 0 && summary->clock_errors == 0 &&
		summary->launched_sample_count == summary->scheduled_sample_count &&
		summary->dropped_slots == 0 && summary->handshake_errors == 0 &&
		summary->deadline_violations == 0 &&
		summary->service_error_count == 0 &&
		summary->contended_rate >= 0.999 &&
		summary->sample_count == summary->contended_sample_count &&
		shared->roles[BM_LOCK_BACKGROUND].started_ns <=
			shared->measure_start_ns &&
		shared->roles[BM_LOCK_BACKGROUND].finished_ns >=
			shared->measure_deadline_ns &&
		summary->bg_operations > 0;
	free(total_wait);
	free(holder_descheduled);
	free(handoff);
	free(launch_lateness);
	return 0;
}

int bm_run_lock(const struct bm_lock_options *options,
		struct bm_lock_summary *summary)
{
	struct lock_shared *shared;
	pthread_mutexattr_t attributes;
	pid_t children[BM_LOCK_ROLES] = { 0 };
	uint64_t warmup_trials_u64;
	uint64_t measure_trials_u64;
	size_t mapping_size;
	uint64_t now;
	int rc;
	int failed = 0;
	bool attributes_initialized = false;
	uint32_t role;

	if (options->rps == 0 || options->rps > BM_LOCK_MAX_RPS)
		return -1;
	{
		double warmup_trials = options->common.warmup_seconds *
			(double)options->rps;
		double measure_trials = options->common.duration_seconds *
			(double)options->rps;

		if (!isfinite(warmup_trials) || !isfinite(measure_trials) ||
		    warmup_trials < 0.0 || warmup_trials > (double)UINT32_MAX ||
		    measure_trials <= 0.0 || measure_trials > 1000000.0)
			return -1;
		warmup_trials_u64 = (uint64_t)llround(warmup_trials);
		measure_trials_u64 = (uint64_t)llround(measure_trials);
	}
	if (warmup_trials_u64 > UINT32_MAX || measure_trials_u64 == 0 ||
	    measure_trials_u64 > 1000000ULL ||
	    warmup_trials_u64 + measure_trials_u64 > UINT32_MAX)
		return -1;
	if (measure_trials_u64 >
	    (SIZE_MAX - sizeof(*shared)) / sizeof(struct bm_lock_sample))
		return -1;
	mapping_size = sizeof(*shared) +
		       (size_t)measure_trials_u64 * sizeof(struct bm_lock_sample);
	shared = mmap(NULL, mapping_size, PROT_READ | PROT_WRITE,
		      MAP_SHARED | MAP_ANONYMOUS, -1, 0);
	if (shared == MAP_FAILED)
		return -1;
	memset(shared, 0, mapping_size);
	shared->warmup_trials = (uint32_t)warmup_trials_u64;
	shared->measure_trials = (uint32_t)measure_trials_u64;
	shared->period_ns = 1000000000ULL / options->rps;
	shared->critical_ns = options->critical_us * 1000ULL;

	rc = pthread_mutexattr_init(&attributes);
	if (rc == 0)
		attributes_initialized = true;
	if (rc == 0)
		rc = pthread_mutexattr_setpshared(&attributes,
						   PTHREAD_PROCESS_SHARED);
	if (rc == 0)
		rc = pthread_mutex_init(&shared->convoy_mutex, &attributes);
	if (attributes_initialized)
		(void)pthread_mutexattr_destroy(&attributes);
	if (rc != 0) {
		(void)munmap(shared, mapping_size);
		return -1;
	}

	for (role = 0; role < BM_LOCK_ROLES; role++) {
		children[role] = fork();
		if (children[role] == 0) {
			switch (role) {
			case BM_LOCK_HOLDER:
				holder_process(options, shared);
				break;
			case BM_LOCK_BACKGROUND:
				background_process(options, shared);
				break;
			case BM_LOCK_WAITER:
				waiter_process(options, shared);
				break;
			case BM_LOCK_COORDINATOR:
				coordinator_process(options, shared);
				break;
			}
			_exit(1);
		}
		if (children[role] < 0) {
			children[role] = 0;
			failed = -1;
			break;
		}
	}
	if (failed != 0 ||
	    bm_wait_counter(&shared->ready, BM_LOCK_ROLES, &shared->fatal, 10.0) != 0)
		goto fail;

	now = bm_now_ns(CLOCK_MONOTONIC);
	shared->warmup_start_ns = now + 10000000ULL;
	bm_signal_u32(&shared->phase, LOCK_WARMUP);
	if (bm_wait_counter(&shared->warmup_done, 1U, &shared->fatal,
			    options->common.warmup_seconds + 10.0) != 0)
		goto fail;

	now = bm_now_ns(CLOCK_MONOTONIC);
	shared->measure_start_ns = now + 20000000ULL;
	shared->measure_deadline_ns = shared->measure_start_ns +
				      bm_seconds_to_ns(options->common.duration_seconds);
	bm_signal_u32(&shared->phase, LOCK_MEASURE);
	if (bm_wait_counter(&shared->measure_done, 1U, &shared->fatal,
			    options->common.duration_seconds + 10.0) != 0)
		goto fail;
	bm_signal_u32(&shared->phase, LOCK_STOP);
	bm_signal_u32(&shared->start_seq, UINT32_MAX);
	bm_signal_u32(&shared->locked_seq, UINT32_MAX);
	bm_signal_u32(&shared->waiter_gate_seq, UINT32_MAX);
	if (bm_reap_children(children, BM_LOCK_ROLES) != 0)
		goto fail_without_children;
	if (summarize_lock(shared, summary) != 0)
		goto fail_without_children;
	if (options->write_raw_samples && write_raw_samples(shared, summary) != 0)
		goto fail_without_children;
	(void)pthread_mutex_destroy(&shared->convoy_mutex);
	(void)munmap(shared, mapping_size);
	return 0;

fail:
	bm_signal_u32(&shared->phase, LOCK_STOP);
	bm_signal_u32(&shared->start_seq, UINT32_MAX);
	bm_signal_u32(&shared->locked_seq, UINT32_MAX);
	bm_signal_u32(&shared->waiter_gate_seq, UINT32_MAX);
	bm_kill_and_reap(children, BM_LOCK_ROLES);
fail_without_children:
	(void)pthread_mutex_destroy(&shared->convoy_mutex);
	(void)munmap(shared, mapping_size);
	return -1;
}
