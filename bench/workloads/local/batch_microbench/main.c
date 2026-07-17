#define _GNU_SOURCE

#include "batch_microbench.h"

#include <errno.h>
#include <getopt.h>
#include <inttypes.h>
#include <math.h>
#include <sched.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
#include <unistd.h>

enum benchmark_mode {
	MODE_NONE,
	MODE_POINTER_CHASE,
	MODE_LOCK_CONVOY,
};

static void usage(FILE *output, const char *program)
{
	(void)fprintf(output,
		"Usage: %s --mode pointer-chase|lock-convoy [options]\n"
		"       %s --self-test\n\n"
		"Common options:\n"
		"  --warmup-seconds N       in-process warmup (default: 5)\n"
		"  --duration-seconds N     measured duration (default: 30)\n"
		"  --cpu0 N / --cpu1 N      guest CPUs (default: 0 and 1)\n"
		"Pointer options:\n"
		"  --chain cacheline|page   chain layout (default: cacheline)\n"
		"  --working-set-kb N       cacheline bytes per worker (default: 1024)\n"
		"  --pages N                pages per worker (default: 4096)\n"
		"  --chains-per-worker N    independent mappings per worker (default: 1, max: 64)\n"
		"  --seed N                 deterministic seed\n"
		"Lock options:\n"
		"  --critical-us N          holder CPU service (default: 1000)\n"
		"  --rps N                  open-loop request rate (default: 100)\n",
		program, program);
}

static int parse_u64(const char *text, uint64_t *value)
{
	char *end = NULL;
	unsigned long long parsed;

	errno = 0;
	parsed = strtoull(text, &end, 0);
	if (errno != 0 || end == text || *end != '\0')
		return -1;
	*value = (uint64_t)parsed;
	return 0;
}

static int parse_size(const char *text, size_t *value)
{
	uint64_t parsed;

	if (parse_u64(text, &parsed) != 0 || parsed > SIZE_MAX)
		return -1;
	*value = (size_t)parsed;
	return 0;
}

static int parse_cpu(const char *text, int *value)
{
	uint64_t parsed;

	if (parse_u64(text, &parsed) != 0 || parsed >= CPU_SETSIZE)
		return -1;
	*value = (int)parsed;
	return 0;
}

static int parse_rps(const char *text, uint32_t *value)
{
	uint64_t parsed;

	if (parse_u64(text, &parsed) != 0 || parsed == 0 ||
	    parsed > BM_LOCK_MAX_RPS)
		return -1;
	*value = (uint32_t)parsed;
	return 0;
}

static int parse_double_value(const char *text, double *value)
{
	char *end = NULL;
	double parsed;

	errno = 0;
	parsed = strtod(text, &end);
	if (errno != 0 || end == text || *end != '\0' || !isfinite(parsed))
		return -1;
	*value = parsed;
	return 0;
}

static enum benchmark_mode parse_mode(const char *value)
{
	if (strcmp(value, "pointer-chase") == 0)
		return MODE_POINTER_CHASE;
	if (strcmp(value, "lock-convoy") == 0)
		return MODE_LOCK_CONVOY;
	return MODE_NONE;
}

static void print_json_string(const char *value)
{
	const unsigned char *cursor = (const unsigned char *)value;

	(void)putchar('"');
	while (*cursor != '\0') {
		switch (*cursor) {
		case '"':
			(void)fputs("\\\"", stdout);
			break;
		case '\\':
			(void)fputs("\\\\", stdout);
			break;
		case '\n':
			(void)fputs("\\n", stdout);
			break;
		case '\r':
			(void)fputs("\\r", stdout);
			break;
		case '\t':
			(void)fputs("\\t", stdout);
			break;
		default:
			if (*cursor < 0x20U)
				(void)printf("\\u%04x", *cursor);
			else
				(void)putchar((int)*cursor);
			break;
		}
		cursor++;
	}
	(void)putchar('"');
}

static void print_pointer_json(const struct bm_pointer_options *options,
			       const struct bm_pointer_summary *summary)
{
	long page_size = sysconf(_SC_PAGESIZE);
	unsigned int index;
	size_t segment;

	(void)printf("{\"metadata\":{\"tool\":\"batch_microbench\","
		     "\"mode\":\"pointer-chase\",\"chain\":\"%s\","
		     "\"workers\":%u,\"cpu_affinity\":[%d,%d,%d,%d],"
		     "\"warmup_seconds\":%.6f,\"duration_seconds\":%.6f,"
		     "\"working_set_kb\":%zu,\"pages_per_worker\":%zu,"
		     "\"chains_per_worker\":%zu,"
		     "\"page_size\":%ld,\"seed\":%" PRIu64 "},\"metrics\":{"
		     "\"throughput\":%.6f,\"aggregate_ops_per_sec\":%.6f,"
		     "\"aggregate_ops_per_cpu_second\":%.6f,"
		     "\"total_operations\":%" PRIu64 ",\"per_worker_cv\":%.9f,"
		     "\"measurement_cpu_time_ns\":%" PRIu64 ","
		     "\"voluntary_context_switches\":%" PRIu64 ","
		     "\"involuntary_context_switches\":%" PRIu64 ","
		     "\"nvcsw\":%" PRIu64 ",\"nivcsw\":%" PRIu64 ","
		     "\"minor_page_faults\":%" PRIu64 ","
		     "\"major_page_faults\":%" PRIu64 ","
		     "\"page_faults\":%" PRIu64 ","
		     "\"affinity_violations\":%" PRIu64 ","
		     "\"affinity_errors\":%" PRIu64 ",\"checksum\":%" PRIu64,
		     options->chain_kind == BM_CHAIN_CACHELINE ? "cacheline" : "page",
		     BM_POINTER_WORKERS, options->common.cpu0, options->common.cpu0,
		     options->common.cpu1, options->common.cpu1,
		     options->common.warmup_seconds, options->common.duration_seconds,
		     options->working_set_kb, options->pages, options->chains_per_worker,
		     page_size, options->seed,
		     summary->aggregate_ops_per_sec, summary->aggregate_ops_per_sec,
		     summary->aggregate_ops_per_cpu_second,
		     summary->total_operations, summary->per_worker_cv,
		     summary->total_cpu_time_ns,
		     summary->usage.voluntary_switches,
		     summary->usage.involuntary_switches,
		     summary->usage.voluntary_switches,
		     summary->usage.involuntary_switches,
		     summary->usage.minor_faults, summary->usage.major_faults,
		     summary->usage.minor_faults + summary->usage.major_faults,
		     summary->affinity_violations, summary->affinity_errors,
		     summary->checksum);
	for (index = 0; index < BM_POINTER_WORKERS; index++) {
		const struct bm_pointer_worker_result *worker = &summary->workers[index];
		double rate = worker->elapsed_ns == 0 ? 0.0 :
			(double)worker->operations * 1000000000.0 /
			(double)worker->elapsed_ns;
		double cpu_rate = worker->cpu_time_ns == 0 ? 0.0 :
			(double)worker->operations * 1000000000.0 /
			(double)worker->cpu_time_ns;

		(void)printf(",\"worker_%u_ops_per_sec\":%.6f,"
			     "\"worker_%u_operations\":%" PRIu64 ","
			     "\"worker_%u_cpu_time_ns\":%" PRIu64 ","
			     "\"worker_%u_ops_per_cpu_second\":%.6f",
			     index, rate, index, worker->operations,
			     index, worker->cpu_time_ns, index, cpu_rate);
	}
	(void)fputs("},\"raw\":{\"per_worker_segment_operations\":[", stdout);
	for (index = 0; index < BM_POINTER_WORKERS; index++) {
		if (index != 0U)
			(void)putchar(',');
		(void)putchar('[');
		for (segment = 0; segment < options->chains_per_worker; segment++) {
			if (segment != 0)
				(void)putchar(',');
			(void)printf("%" PRIu64,
				     summary->workers[index].segment_operations[segment]);
		}
		(void)putchar(']');
	}
	(void)fputs("],\"aggregate_segment_operations\":[", stdout);
	for (segment = 0; segment < options->chains_per_worker; segment++) {
		uint64_t aggregate = 0;

		if (segment != 0)
			(void)putchar(',');
		for (index = 0; index < BM_POINTER_WORKERS; index++)
			aggregate += summary->workers[index].segment_operations[segment];
		(void)printf("%" PRIu64, aggregate);
	}
	(void)fputs("]}}\n", stdout);
}

static void print_lock_json(const struct bm_lock_options *options,
			    const struct bm_lock_summary *summary)
{
	static const char *const role_names[BM_LOCK_ROLES] = {
		"holder", "background", "waiter", "coordinator"
	};
	unsigned int role;

	(void)printf("{\"metadata\":{\"tool\":\"batch_microbench\","
		     "\"mode\":\"lock-convoy\",\"rps\":%u,"
		     "\"critical_us\":%" PRIu64 ","
		     "\"warmup_seconds\":%.6f,\"duration_seconds\":%.6f,"
		     "\"holder_cpu\":%d,\"background_cpu\":%d,"
		     "\"waiter_cpu\":%d,\"coordinator_cpu\":%d},\"metrics\":{"
		     "\"valid\":%s,\"scheduled_sample_count\":%" PRIu64 ","
		     "\"launched_sample_count\":%" PRIu64 ","
		     "\"contended_sample_count\":%" PRIu64 ","
		     "\"contended_rate\":%.9f,"
		     "\"sample_count\":%" PRIu64 ","
		     "\"invalid_sample_count\":%" PRIu64 ","
		     "\"dropped_slots\":%" PRIu64 ","
		     "\"total_wait_p50_us\":%.6f,\"total_wait_p90_us\":%.6f,"
		     "\"total_wait_p99_us\":%.6f,\"total_wait_p999_us\":%.6f,"
		     "\"holder_descheduled_p50_us\":%.6f,"
		     "\"holder_descheduled_p90_us\":%.6f,"
		     "\"holder_descheduled_p99_us\":%.6f,"
		     "\"holder_descheduled_p999_us\":%.6f,"
		     "\"handoff_p50_us\":%.6f,\"handoff_p90_us\":%.6f,"
		     "\"handoff_p99_us\":%.6f,\"handoff_p999_us\":%.6f,"
		     "\"launch_lateness_p50_us\":%.6f,"
		     "\"launch_lateness_p90_us\":%.6f,"
		     "\"launch_lateness_p99_us\":%.6f,"
		     "\"launch_lateness_p999_us\":%.6f,"
		     "\"launch_lateness_max_us\":%.6f,"
		     "\"bg_ops_per_sec\":%.6f,\"background_ops_per_sec\":%.6f,"
		     "\"background_operations\":%" PRIu64 ","
		     "\"missed_contentions\":%" PRIu64 ","
		     "\"missed_deadlines\":%" PRIu64 ","
		     "\"handshake_errors\":%" PRIu64 ","
		     "\"deadline_violations\":%" PRIu64 ","
		     "\"affinity_violations\":%" PRIu64 ","
		     "\"affinity_errors\":%" PRIu64 ","
		     "\"service_error_count\":%" PRIu64 ","
		     "\"service_error_max_pct\":%.9f,"
		     "\"clock_errors\":%" PRIu64 ","
		     "\"voluntary_context_switches\":%" PRIu64 ","
		     "\"involuntary_context_switches\":%" PRIu64 ","
		     "\"nvcsw\":%" PRIu64 ",\"nivcsw\":%" PRIu64,
		     options->rps, options->critical_us,
		     options->common.warmup_seconds, options->common.duration_seconds,
		     options->common.cpu0, options->common.cpu0,
		     options->common.cpu1, options->common.cpu1,
		     summary->valid ? "true" : "false",
		     summary->scheduled_sample_count,
		     summary->launched_sample_count,
		     summary->contended_sample_count, summary->contended_rate,
		     summary->sample_count,
		     summary->invalid_sample_count, summary->dropped_slots,
		     summary->total_wait_us[0], summary->total_wait_us[1],
		     summary->total_wait_us[2], summary->total_wait_us[3],
		     summary->holder_descheduled_us[0],
		     summary->holder_descheduled_us[1],
		     summary->holder_descheduled_us[2],
		     summary->holder_descheduled_us[3],
		     summary->handoff_us[0], summary->handoff_us[1],
		     summary->handoff_us[2], summary->handoff_us[3],
		     summary->launch_lateness_us[0],
		     summary->launch_lateness_us[1],
		     summary->launch_lateness_us[2],
		     summary->launch_lateness_us[3],
		     summary->launch_lateness_max_us,
		     summary->bg_ops_per_sec, summary->bg_ops_per_sec,
		     summary->bg_operations, summary->missed_contentions,
		     summary->missed_deadlines, summary->handshake_errors,
		     summary->deadline_violations, summary->affinity_violations,
		     summary->affinity_errors, summary->service_error_count,
		     summary->service_error_max_pct, summary->clock_errors,
		     summary->usage.voluntary_switches,
		     summary->usage.involuntary_switches,
		     summary->usage.voluntary_switches,
		     summary->usage.involuntary_switches);
	for (role = 0; role < BM_LOCK_ROLES; role++) {
		const struct bm_lock_role_result *result = &summary->roles[role];

		(void)printf(",\"%s_nvcsw\":%" PRIu64 ",\"%s_nivcsw\":%" PRIu64,
			     role_names[role], result->usage.voluntary_switches,
			     role_names[role], result->usage.involuntary_switches);
	}
	(void)fputs("},\"raw\":{\"lock_samples_path\":", stdout);
	print_json_string(summary->raw_samples_path);
	(void)fputs("}}\n", stdout);
}

static int choose_self_test_cpus(int *cpu0, int *cpu1)
{
	cpu_set_t allowed;
	int cpu;

	if (sched_getaffinity(0, sizeof(allowed), &allowed) != 0)
		return -1;
	*cpu0 = -1;
	*cpu1 = -1;
	for (cpu = 0; cpu < CPU_SETSIZE; cpu++) {
		if (!CPU_ISSET(cpu, &allowed))
			continue;
		if (*cpu0 < 0)
			*cpu0 = cpu;
		else {
			*cpu1 = cpu;
			break;
		}
	}
	return *cpu0 >= 0 && *cpu1 >= 0 ? 0 : -1;
}

static int run_self_test(void)
{
	struct bm_pointer_options pointer = {
		.common = { .warmup_seconds = 0.03, .duration_seconds = 0.05 },
		.chain_kind = BM_CHAIN_CACHELINE,
		.working_set_kb = 64,
		.pages = 32,
		.chains_per_worker = 2,
		.seed = 0x4d595df4d0f33173ULL,
	};
	struct bm_lock_options lock = {
		.common = { .warmup_seconds = 0.03, .duration_seconds = 0.05 },
		.critical_us = 100,
		.rps = BM_LOCK_DEFAULT_RPS,
		.write_raw_samples = false,
	};
	struct bm_pointer_summary cache_summary;
	struct bm_pointer_summary page_summary;
	struct bm_lock_summary lock_summary;
	int cache_rc;
	int page_rc;
	int lock_rc;
	bool passed;

	if (choose_self_test_cpus(&pointer.common.cpu0, &pointer.common.cpu1) != 0) {
		(void)fprintf(stderr, "self-test requires at least two allowed CPUs\n");
		return 1;
	}
	lock.common.cpu0 = pointer.common.cpu0;
	lock.common.cpu1 = pointer.common.cpu1;
	cache_rc = bm_run_pointer(&pointer, &cache_summary);
	pointer.chain_kind = BM_CHAIN_PAGE;
	page_rc = bm_run_pointer(&pointer, &page_summary);
	lock_rc = bm_run_lock(&lock, &lock_summary);
	passed = cache_rc == 0 && page_rc == 0 && lock_rc == 0 &&
		 cache_summary.total_operations > 0 && page_summary.total_operations > 0 &&
		 lock_summary.valid && lock_summary.sample_count > 0 &&
		 cache_summary.affinity_errors == 0 &&
		 page_summary.affinity_errors == 0 && lock_summary.affinity_errors == 0 &&
		 lock_summary.clock_errors == 0;
	(void)printf("{\"metadata\":{\"tool\":\"batch_microbench\","
		     "\"mode\":\"self-test\",\"cpu0\":%d,\"cpu1\":%d},"
		     "\"metrics\":{\"passed\":%s,\"cacheline_rc\":%d,"
		     "\"page_rc\":%d,\"lock_rc\":%d,"
		     "\"cacheline_operations\":%" PRIu64 ","
		     "\"page_operations\":%" PRIu64 ","
		     "\"lock_samples\":%" PRIu64 "},\"raw\":{}}\n",
		     pointer.common.cpu0, pointer.common.cpu1,
		     passed ? "true" : "false", cache_rc, page_rc, lock_rc,
		     cache_rc == 0 ? cache_summary.total_operations : 0,
		     page_rc == 0 ? page_summary.total_operations : 0,
		     lock_rc == 0 ? lock_summary.sample_count : 0);
	return passed ? 0 : 1;
}

int main(int argc, char **argv)
{
	struct bm_pointer_options pointer = {
		.common = {
			.warmup_seconds = 5.0,
			.duration_seconds = 30.0,
			.cpu0 = 0,
			.cpu1 = 1,
		},
		.chain_kind = BM_CHAIN_CACHELINE,
		.working_set_kb = 1024,
		.pages = 4096,
		.chains_per_worker = 1,
		.seed = 0x4d595df4d0f33173ULL,
	};
	struct bm_lock_options lock = {
		.common = {
			.warmup_seconds = 5.0,
			.duration_seconds = 30.0,
			.cpu0 = 0,
			.cpu1 = 1,
		},
		.critical_us = 1000,
		.rps = BM_LOCK_DEFAULT_RPS,
		.write_raw_samples = true,
	};
	static const struct option long_options[] = {
		{ "mode", required_argument, NULL, 'm' },
		{ "warmup-seconds", required_argument, NULL, 'w' },
		{ "duration-seconds", required_argument, NULL, 'd' },
		{ "cpu0", required_argument, NULL, 1000 },
		{ "cpu1", required_argument, NULL, 1001 },
		{ "chain", required_argument, NULL, 'c' },
		{ "working-set-kb", required_argument, NULL, 1002 },
		{ "pages", required_argument, NULL, 'p' },
		{ "chains-per-worker", required_argument, NULL, 1005 },
		{ "seed", required_argument, NULL, 's' },
		{ "critical-us", required_argument, NULL, 1003 },
		{ "rps", required_argument, NULL, 1004 },
		{ "self-test", no_argument, NULL, 't' },
		{ "help", no_argument, NULL, 'h' },
		{ NULL, 0, NULL, 0 },
	};
	enum benchmark_mode mode = MODE_NONE;
	bool self_test = false;
	int option;

	(void)prctl(PR_SET_NAME, "bm-parent", 0, 0, 0);

	while ((option = getopt_long(argc, argv, "m:w:d:c:p:s:th",
				  long_options, NULL)) != -1) {
		switch (option) {
		case 'm':
			mode = parse_mode(optarg);
			if (mode == MODE_NONE)
				goto invalid;
			break;
		case 'w':
			if (parse_double_value(optarg,
					       &pointer.common.warmup_seconds) != 0)
				goto invalid;
			lock.common.warmup_seconds = pointer.common.warmup_seconds;
			break;
		case 'd':
			if (parse_double_value(optarg,
					       &pointer.common.duration_seconds) != 0)
				goto invalid;
			lock.common.duration_seconds = pointer.common.duration_seconds;
			break;
		case 'c':
			if (strcmp(optarg, "cacheline") == 0)
				pointer.chain_kind = BM_CHAIN_CACHELINE;
			else if (strcmp(optarg, "page") == 0)
				pointer.chain_kind = BM_CHAIN_PAGE;
			else
				goto invalid;
			break;
		case 'p':
			if (parse_size(optarg, &pointer.pages) != 0)
				goto invalid;
			break;
		case 's':
			if (parse_u64(optarg, &pointer.seed) != 0)
				goto invalid;
			break;
		case 't':
			self_test = true;
			break;
		case 'h':
			usage(stderr, argv[0]);
			return 0;
		case 1000:
			if (parse_cpu(optarg, &pointer.common.cpu0) != 0)
				goto invalid;
			lock.common.cpu0 = pointer.common.cpu0;
			break;
		case 1001:
			if (parse_cpu(optarg, &pointer.common.cpu1) != 0)
				goto invalid;
			lock.common.cpu1 = pointer.common.cpu1;
			break;
		case 1002:
			if (parse_size(optarg, &pointer.working_set_kb) != 0)
				goto invalid;
			break;
		case 1003:
			if (parse_u64(optarg, &lock.critical_us) != 0)
				goto invalid;
			break;
		case 1004:
			if (parse_rps(optarg, &lock.rps) != 0)
				goto invalid;
			break;
		case 1005:
			if (parse_size(optarg, &pointer.chains_per_worker) != 0)
				goto invalid;
			break;
		default:
			goto invalid;
		}
	}
	if (optind != argc)
		goto invalid;
	if (self_test)
		return run_self_test();
	if (mode == MODE_NONE || pointer.common.warmup_seconds < 0.0 ||
	    pointer.common.duration_seconds <= 0.0 ||
	    pointer.common.cpu0 == pointer.common.cpu1 ||
	    pointer.working_set_kb < 1 || pointer.pages < 2 || pointer.seed == 0 ||
	    pointer.chains_per_worker == 0 ||
	    pointer.chains_per_worker > BM_POINTER_MAX_CHAINS ||
	    lock.critical_us == 0 || lock.critical_us > UINT64_MAX / 1000ULL ||
	    lock.rps == 0 || lock.rps > BM_LOCK_MAX_RPS)
		goto invalid;

	if (mode == MODE_POINTER_CHASE) {
		struct bm_pointer_summary summary;

		if (bm_run_pointer(&pointer, &summary) != 0) {
			(void)fprintf(stderr, "pointer-chase execution failed\n");
			return 1;
		}
		print_pointer_json(&pointer, &summary);
		return 0;
	}
	if (mode == MODE_LOCK_CONVOY) {
		struct bm_lock_summary summary;

		if (bm_run_lock(&lock, &summary) != 0) {
			(void)fprintf(stderr, "lock-convoy execution failed\n");
			return 1;
		}
		print_lock_json(&lock, &summary);
		return summary.valid ? 0 : 1;
	}

invalid:
	usage(stderr, argv[0]);
	return 2;
}
