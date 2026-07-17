#define _GNU_SOURCE

#include "batch_microbench.h"

#include <errno.h>
#include <math.h>
#include <sched.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

enum pointer_phase {
	POINTER_INIT,
	POINTER_WARMUP,
	POINTER_MEASURE,
	POINTER_STOP,
};

struct pointer_node {
	struct pointer_node *next;
	uint64_t value;
	unsigned char padding[64 - sizeof(void *) - sizeof(uint64_t)];
} __attribute__((aligned(64)));

_Static_assert(sizeof(struct pointer_node) == 64,
	       "pointer nodes must occupy exactly one cache line");

struct pointer_chain {
	void *mapping;
	size_t mapping_size;
	size_t node_count;
	size_t page_size;
	enum bm_chain_kind kind;
	struct pointer_node *first;
};

struct pointer_shared {
	_Atomic uint32_t phase;
	_Atomic uint32_t ready;
	_Atomic uint32_t measure_ready;
	_Atomic uint32_t done;
	_Atomic uint32_t fatal;
	uint64_t warmup_start_ns;
	uint64_t warmup_deadline_ns;
	uint64_t measure_start_ns;
	uint64_t measure_deadline_ns;
	struct bm_pointer_worker_result results[BM_POINTER_WORKERS];
};

static uint64_t prng_next(uint64_t *state)
{
	uint64_t value = *state;

	value ^= value >> 12;
	value ^= value << 25;
	value ^= value >> 27;
	*state = value;
	return value * 2685821657736338717ULL;
}

static struct pointer_node *chain_node(const struct pointer_chain *chain,
				       size_t index)
{
	unsigned char *base = chain->mapping;

	if (chain->kind == BM_CHAIN_CACHELINE)
		return (struct pointer_node *)(base + index * sizeof(struct pointer_node));

	/* Rotate page offsets so the TLB test is not also a single-set cache test. */
	return (struct pointer_node *)(base + index * chain->page_size +
				       ((index * 17U) & 63U) * 64U);
}

static void destroy_chain(struct pointer_chain *chain)
{
	if (chain->mapping != NULL && chain->mapping != MAP_FAILED)
		(void)munmap(chain->mapping, chain->mapping_size);
	memset(chain, 0, sizeof(*chain));
}

static int create_chain(const struct bm_pointer_options *options,
			uint32_t worker_index, size_t chain_index,
			struct pointer_chain *chain)
{
	uint64_t state = options->seed ^
		(0x9e3779b97f4a7c15ULL * ((uint64_t)worker_index + 1ULL)) ^
		(0xd1b54a32d192ed03ULL * ((uint64_t)chain_index + 1ULL));
	size_t *permutation = NULL;
	size_t index;
	long page_size = sysconf(_SC_PAGESIZE);

	if (state == 0)
		state = 0x94d049bb133111ebULL ^ (uint64_t)worker_index ^
			((uint64_t)chain_index << 32);

	memset(chain, 0, sizeof(*chain));
	if (page_size <= 0 || (size_t)page_size < sizeof(struct pointer_node))
		return -1;
	chain->page_size = (size_t)page_size;
	chain->kind = options->chain_kind;
	if (chain->kind == BM_CHAIN_CACHELINE) {
		if (options->working_set_kb > SIZE_MAX / 1024U)
			return -1;
		chain->mapping_size = options->working_set_kb * 1024U;
		chain->node_count = chain->mapping_size / sizeof(struct pointer_node);
		chain->mapping_size = chain->node_count * sizeof(struct pointer_node);
	} else {
		if (options->pages > SIZE_MAX / chain->page_size)
			return -1;
		chain->node_count = options->pages;
		chain->mapping_size = options->pages * chain->page_size;
	}
	if (chain->node_count < 2 || chain->mapping_size == 0)
		return -1;

	chain->mapping = mmap(NULL, chain->mapping_size, PROT_READ | PROT_WRITE,
				     MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	if (chain->mapping == MAP_FAILED)
		return -1;
#ifdef MADV_NOHUGEPAGE
	if (madvise(chain->mapping, chain->mapping_size, MADV_NOHUGEPAGE) != 0 &&
	    errno != EINVAL) {
		destroy_chain(chain);
		return -1;
	}
#endif

	permutation = calloc(chain->node_count, sizeof(*permutation));
	if (permutation == NULL) {
		destroy_chain(chain);
		return -1;
	}
	for (index = 0; index < chain->node_count; index++) {
		struct pointer_node *node = chain_node(chain, index);

		permutation[index] = index;
		node->next = NULL;
		node->value = (uint64_t)index ^ state;
	}
	for (index = chain->node_count - 1; index > 0; index--) {
		size_t other = (size_t)(prng_next(&state) % (index + 1U));
		size_t temporary = permutation[index];

		permutation[index] = permutation[other];
		permutation[other] = temporary;
	}
	for (index = 0; index < chain->node_count; index++) {
		struct pointer_node *node = chain_node(chain, permutation[index]);
		struct pointer_node *next =
			chain_node(chain, permutation[(index + 1U) % chain->node_count]);

		node->next = next;
	}
	chain->first = chain_node(chain, permutation[0]);
	free(permutation);
	return 0;
}

static void destroy_chains(struct pointer_chain *chains, size_t count)
{
	size_t index;

	for (index = 0; index < count; index++)
		destroy_chain(&chains[index]);
}

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

static uint64_t chase_until(struct pointer_node **cursor, uint64_t deadline_ns,
			    int expected_cpu, uint64_t *affinity_violations)
{
	struct pointer_node *node = *cursor;
	uint64_t operations = 0;

	while (bm_now_ns(CLOCK_MONOTONIC) < deadline_ns) {
		unsigned int index;

		for (index = 0; index < 4096U; index++)
			node = node->next;
		operations += 4096U;
		bm_check_affinity(expected_cpu, affinity_violations);
	}
	*cursor = node;
	return operations;
}

static uint64_t segment_deadline(uint64_t start_ns, uint64_t deadline_ns,
				 size_t segment, size_t segment_count)
{
	uint64_t duration_ns = deadline_ns - start_ns;
	uint64_t completed = (uint64_t)segment + 1ULL;

	return start_ns + duration_ns / segment_count * completed +
		(duration_ns % segment_count) * completed / segment_count;
}

static uint64_t chase_segments(struct pointer_node **cursors, size_t count,
			       uint64_t start_ns, uint64_t deadline_ns,
			       int expected_cpu, uint64_t *affinity_violations,
			       uint64_t *segment_operations)
{
	uint64_t operations = 0;
	size_t index;

	for (index = 0; index < count; index++) {
		uint64_t deadline = segment_deadline(start_ns, deadline_ns,
						     index, count);
		uint64_t segment_result;

		segment_result = chase_until(&cursors[index], deadline, expected_cpu,
					     affinity_violations);
		if (segment_operations != NULL)
			segment_operations[index] = segment_result;
		operations += segment_result;
	}
	return operations;
}

static uint64_t rusage_cpu_time_ns(const struct rusage *usage)
{
	return (uint64_t)usage->ru_utime.tv_sec * 1000000000ULL +
		(uint64_t)usage->ru_utime.tv_usec * 1000ULL +
		(uint64_t)usage->ru_stime.tv_sec * 1000000000ULL +
		(uint64_t)usage->ru_stime.tv_usec * 1000ULL;
}

static void pointer_child(const struct bm_pointer_options *options,
			  struct pointer_shared *shared, uint32_t worker_index)
{
	struct bm_pointer_worker_result result = { 0 };
	struct pointer_chain *chains = NULL;
	struct pointer_node **cursors = NULL;
	struct rusage before = { 0 }, after = { 0 };
	char name[16];
	int expected_cpu = worker_index < 2U ? options->common.cpu0 : options->common.cpu1;
	size_t created = 0;
	size_t index;
	uint64_t started_ns;
	uint64_t before_cpu_ns;
	uint64_t after_cpu_ns;
	uint32_t phase;

	(void)snprintf(name, sizeof(name), "bm-ptr%u", worker_index);
	(void)bm_set_role(name, expected_cpu, &result.affinity_errors);
	chains = calloc(options->chains_per_worker, sizeof(*chains));
	cursors = calloc(options->chains_per_worker, sizeof(*cursors));
	if (chains == NULL || cursors == NULL)
		goto init_fail;
	for (index = 0; index < options->chains_per_worker; index++) {
		if (create_chain(options, worker_index, index, &chains[index]) != 0)
			goto init_fail;
		cursors[index] = chains[index].first;
		created++;
	}
	(void)bm_increment_u32(&shared->ready);

	phase = atomic_load_explicit(&shared->phase, memory_order_acquire);
	while (phase == POINTER_INIT) {
		bm_wait_u32_change(&shared->phase, phase);
		phase = atomic_load_explicit(&shared->phase, memory_order_acquire);
	}
	if (phase != POINTER_WARMUP)
		goto child_fail;
	sleep_until(shared->warmup_start_ns);
	(void)chase_segments(cursors, options->chains_per_worker,
			     shared->warmup_start_ns, shared->warmup_deadline_ns,
			     expected_cpu, &result.affinity_violations, NULL);
	result.affinity_violations = 0;
	(void)bm_increment_u32(&shared->measure_ready);

	while (atomic_load_explicit(&shared->phase, memory_order_acquire) ==
	       POINTER_WARMUP)
		bm_wait_u32_change(&shared->phase, POINTER_WARMUP);
	if (atomic_load_explicit(&shared->phase, memory_order_acquire) !=
	    POINTER_MEASURE)
		goto child_fail;
	sleep_until(shared->measure_start_ns);
	(void)getrusage(RUSAGE_SELF, &before);
	before_cpu_ns = rusage_cpu_time_ns(&before);
	started_ns = bm_now_ns(CLOCK_MONOTONIC);
	result.operations = chase_segments(cursors, options->chains_per_worker,
					   shared->measure_start_ns,
					   shared->measure_deadline_ns,
					   expected_cpu,
					   &result.affinity_violations,
					   result.segment_operations);
	result.elapsed_ns = bm_now_ns(CLOCK_MONOTONIC) - started_ns;
	(void)getrusage(RUSAGE_SELF, &after);
	after_cpu_ns = rusage_cpu_time_ns(&after);
	result.cpu_time_ns = after_cpu_ns >= before_cpu_ns ?
		after_cpu_ns - before_cpu_ns : 0;
	result.usage = bm_rusage_diff(&before, &after);
	result.checksum = result.operations ^ ((uint64_t)worker_index << 56);
	for (index = 0; index < options->chains_per_worker; index++)
		result.checksum ^= cursors[index]->value + 0x9e3779b97f4a7c15ULL +
				   (result.checksum << 6) + (result.checksum >> 2);
	shared->results[worker_index] = result;
	(void)bm_increment_u32(&shared->done);
	destroy_chains(chains, created);
	free(cursors);
	free(chains);
	_exit(0);

child_fail:
	atomic_store_explicit(&shared->fatal, 1, memory_order_release);
	destroy_chains(chains, created);
	free(cursors);
	free(chains);
	_exit(1);

init_fail:
	atomic_store_explicit(&shared->fatal, 1, memory_order_release);
	(void)bm_increment_u32(&shared->ready);
	destroy_chains(chains, created);
	free(cursors);
	free(chains);
	_exit(1);
}

static double coefficient_of_variation(const double *values, size_t count)
{
	double mean = 0.0;
	double variance = 0.0;
	size_t index;

	for (index = 0; index < count; index++)
		mean += values[index];
	mean /= (double)count;
	if (mean == 0.0)
		return 0.0;
	for (index = 0; index < count; index++) {
		double difference = values[index] - mean;

		variance += difference * difference;
	}
	variance /= (double)count;
	return sqrt(variance) / mean;
}

int bm_run_pointer(const struct bm_pointer_options *options,
		   struct bm_pointer_summary *summary)
{
	struct pointer_shared *shared;
	pid_t children[BM_POINTER_WORKERS] = { 0 };
	uint64_t warmup_ns = bm_seconds_to_ns(options->common.warmup_seconds);
	uint64_t measure_ns = bm_seconds_to_ns(options->common.duration_seconds);
	double rates[BM_POINTER_WORKERS] = { 0 };
	uint64_t now;
	size_t index;
	int failed = 0;

	memset(summary, 0, sizeof(*summary));
	if (options->chains_per_worker == 0 ||
	    options->chains_per_worker > BM_POINTER_MAX_CHAINS)
		return -1;
	shared = mmap(NULL, sizeof(*shared), PROT_READ | PROT_WRITE,
		      MAP_SHARED | MAP_ANONYMOUS, -1, 0);
	if (shared == MAP_FAILED)
		return -1;
	memset(shared, 0, sizeof(*shared));

	for (index = 0; index < BM_POINTER_WORKERS; index++) {
		children[index] = fork();
		if (children[index] == 0)
			pointer_child(options, shared, (uint32_t)index);
		if (children[index] < 0) {
			children[index] = 0;
			failed = -1;
			break;
		}
	}
	if (failed != 0 ||
	    bm_wait_counter(&shared->ready, BM_POINTER_WORKERS, &shared->fatal,
			    10.0) != 0)
		goto fail;

	now = bm_now_ns(CLOCK_MONOTONIC);
	shared->warmup_start_ns = now + 10000000ULL;
	shared->warmup_deadline_ns = shared->warmup_start_ns + warmup_ns;
	bm_signal_u32(&shared->phase, POINTER_WARMUP);
	if (bm_wait_counter(&shared->measure_ready, BM_POINTER_WORKERS,
			    &shared->fatal, options->common.warmup_seconds + 10.0) != 0)
		goto fail;

	now = bm_now_ns(CLOCK_MONOTONIC);
	shared->measure_start_ns = now + 10000000ULL;
	shared->measure_deadline_ns = shared->measure_start_ns + measure_ns;
	bm_signal_u32(&shared->phase, POINTER_MEASURE);
	if (bm_wait_counter(&shared->done, BM_POINTER_WORKERS, &shared->fatal,
			    options->common.duration_seconds + 10.0) != 0)
		goto fail;
	bm_signal_u32(&shared->phase, POINTER_STOP);
	if (bm_reap_children(children, BM_POINTER_WORKERS) != 0)
		goto fail_without_children;

	summary->elapsed_ns = measure_ns;
	for (index = 0; index < BM_POINTER_WORKERS; index++) {
		const struct bm_pointer_worker_result *result = &shared->results[index];

		summary->workers[index] = *result;
		summary->total_operations += result->operations;
		summary->total_cpu_time_ns += result->cpu_time_ns;
		summary->checksum ^= result->checksum;
		summary->affinity_violations += result->affinity_violations;
		summary->affinity_errors += result->affinity_errors;
		bm_rusage_add(&summary->usage, &result->usage);
		if (result->elapsed_ns != 0)
			rates[index] = (double)result->operations * 1000000000.0 /
				       (double)result->elapsed_ns;
	}
	if (measure_ns != 0)
		summary->aggregate_ops_per_sec =
			(double)summary->total_operations * 1000000000.0 /
			(double)measure_ns;
	if (summary->total_cpu_time_ns != 0)
		summary->aggregate_ops_per_cpu_second =
			(double)summary->total_operations * 1000000000.0 /
			(double)summary->total_cpu_time_ns;
	summary->per_worker_cv = coefficient_of_variation(rates,
							 BM_POINTER_WORKERS);
	(void)munmap(shared, sizeof(*shared));
	return 0;

fail:
	bm_signal_u32(&shared->phase, POINTER_STOP);
	bm_kill_and_reap(children, BM_POINTER_WORKERS);
fail_without_children:
	(void)munmap(shared, sizeof(*shared));
	return -1;
}
