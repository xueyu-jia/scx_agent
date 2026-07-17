#ifndef BATCH_MICROBENCH_H
#define BATCH_MICROBENCH_H

#include <stdatomic.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <sys/resource.h>
#include <sys/types.h>

#define BM_POINTER_WORKERS 4
#define BM_POINTER_MAX_CHAINS 64U
#define BM_LOCK_ROLES 4
#define BM_LOCK_DEFAULT_RPS 100U
#define BM_LOCK_MAX_RPS 1000000000U

enum bm_chain_kind {
	BM_CHAIN_CACHELINE,
	BM_CHAIN_PAGE,
};

enum bm_lock_role {
	BM_LOCK_HOLDER,
	BM_LOCK_BACKGROUND,
	BM_LOCK_WAITER,
	BM_LOCK_COORDINATOR,
};

struct bm_common_options {
	double warmup_seconds;
	double duration_seconds;
	int cpu0;
	int cpu1;
};

struct bm_pointer_options {
	struct bm_common_options common;
	enum bm_chain_kind chain_kind;
	size_t working_set_kb;
	size_t pages;
	size_t chains_per_worker;
	uint64_t seed;
};

struct bm_lock_options {
	struct bm_common_options common;
	uint64_t critical_us;
	uint32_t rps;
	bool write_raw_samples;
};

struct bm_rusage_delta {
	uint64_t voluntary_switches;
	uint64_t involuntary_switches;
	uint64_t minor_faults;
	uint64_t major_faults;
};

struct bm_pointer_worker_result {
	uint64_t operations;
	uint64_t segment_operations[BM_POINTER_MAX_CHAINS];
	uint64_t elapsed_ns;
	uint64_t cpu_time_ns;
	uint64_t checksum;
	uint64_t affinity_violations;
	uint64_t affinity_errors;
	struct bm_rusage_delta usage;
};

struct bm_pointer_summary {
	struct bm_pointer_worker_result workers[BM_POINTER_WORKERS];
	uint64_t total_operations;
	uint64_t elapsed_ns;
	uint64_t total_cpu_time_ns;
	uint64_t checksum;
	uint64_t affinity_violations;
	uint64_t affinity_errors;
	struct bm_rusage_delta usage;
	double aggregate_ops_per_sec;
	double aggregate_ops_per_cpu_second;
	double per_worker_cv;
};

struct bm_lock_sample {
	uint64_t scheduled_ns;
	uint64_t launch_ns;
	uint64_t launch_lateness_ns;
	uint64_t request_ns;
	uint64_t unlock_ns;
	uint64_t acquire_ns;
	uint64_t holder_cpu_request_ns;
	uint64_t holder_cpu_unlock_ns;
	uint64_t holder_cpu_after_request_ns;
	uint64_t holder_burn_start_ns;
	uint64_t holder_burn_end_ns;
	uint64_t holder_service_ns;
	uint64_t total_wait_ns;
	uint64_t holder_descheduled_ns;
	uint64_t handoff_ns;
	uint64_t service_error_ns;
	uint32_t launched;
	uint32_t dropped;
	uint32_t attempted;
	uint32_t parked;
	uint32_t contended;
	uint32_t deadline_valid;
	uint32_t service_valid;
	uint32_t valid;
};

struct bm_lock_role_result {
	uint64_t operations;
	uint64_t elapsed_ns;
	uint64_t started_ns;
	uint64_t finished_ns;
	uint64_t checksum;
	uint64_t affinity_violations;
	uint64_t affinity_errors;
	uint64_t clock_errors;
	uint64_t missed_deadlines;
	uint32_t completed;
	struct bm_rusage_delta usage;
};

struct bm_lock_summary {
	struct bm_lock_role_result roles[BM_LOCK_ROLES];
	struct bm_rusage_delta usage;
	uint32_t valid;
	uint64_t scheduled_sample_count;
	uint64_t launched_sample_count;
	uint64_t contended_sample_count;
	uint64_t sample_count;
	uint64_t invalid_sample_count;
	uint64_t dropped_slots;
	uint64_t missed_contentions;
	uint64_t missed_deadlines;
	uint64_t handshake_errors;
	uint64_t deadline_violations;
	uint64_t affinity_violations;
	uint64_t affinity_errors;
	uint64_t service_error_count;
	uint64_t clock_errors;
	uint64_t bg_operations;
	uint64_t bg_elapsed_ns;
	double bg_ops_per_sec;
	double service_error_max_pct;
	double total_wait_us[4];
	double holder_descheduled_us[4];
	double handoff_us[4];
	double launch_lateness_us[4];
	double launch_lateness_max_us;
	double contended_rate;
	char raw_samples_path[4096];
};

uint64_t bm_now_ns(clockid_t clock_id);
uint64_t bm_seconds_to_ns(double seconds);
int bm_set_role(const char *name, int cpu, uint64_t *affinity_errors);
void bm_check_affinity(int expected_cpu, uint64_t *violations);
struct bm_rusage_delta bm_rusage_diff(const struct rusage *before,
				      const struct rusage *after);
void bm_rusage_add(struct bm_rusage_delta *total,
			  const struct bm_rusage_delta *value);
int bm_wait_counter(_Atomic uint32_t *counter, uint32_t target,
		    _Atomic uint32_t *fatal, double timeout_seconds);
void bm_signal_u32(_Atomic uint32_t *value, uint32_t next);
uint32_t bm_increment_u32(_Atomic uint32_t *value);
void bm_wait_u32_change(_Atomic uint32_t *value, uint32_t expected);
void bm_kill_and_reap(pid_t *children, size_t nr_children);
int bm_reap_children(pid_t *children, size_t nr_children);

int bm_run_pointer(const struct bm_pointer_options *options,
		   struct bm_pointer_summary *summary);
int bm_run_lock(const struct bm_lock_options *options,
		struct bm_lock_summary *summary);

#endif
