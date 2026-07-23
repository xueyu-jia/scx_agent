// SPDX-License-Identifier: GPL-2.0

use std::path::PathBuf;

use clap::{Parser, ValueEnum};
use scx_utils::libbpf_clap_opts::LibbpfOpts;

use crate::bpf_intf;
use crate::rules::RuleClass;
use crate::SCHEDULER_NAME;

const DEFAULT_BATCH_MIN_EPOCH_US: u64 = 1000;
const DEFAULT_BATCH_MAX_EPOCH_US: u64 = 8000;
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum WorkloadClass {
    Latency,
    Batch,
}

impl WorkloadClass {
    pub(crate) fn as_bpf(self) -> u32 {
        match self {
            Self::Latency => bpf_intf::workload_class_CLASS_LATENCY,
            Self::Batch => bpf_intf::workload_class_CLASS_BATCH,
        }
    }

    pub(crate) fn as_rule_class(self) -> RuleClass {
        match self {
            Self::Latency => RuleClass::Latency,
            Self::Batch => RuleClass::Batch,
        }
    }
}

#[derive(Debug, Parser)]
#[command(name = SCHEDULER_NAME, disable_version_flag = true)]
pub(crate) struct Opts {
    /// LATENCY task time slice in microseconds.
    #[clap(long, default_value = "1000")]
    pub(crate) latency_slice_us: u64,

    /// Initial and minimum adaptive BATCH epoch in microseconds.
    #[clap(long, default_value_t = DEFAULT_BATCH_MIN_EPOCH_US)]
    pub(crate) batch_min_epoch_us: u64,

    /// Maximum adaptive BATCH epoch in microseconds.
    #[clap(long, default_value_t = DEFAULT_BATCH_MAX_EPOCH_US)]
    pub(crate) batch_max_epoch_us: u64,

    /// Target maximum BATCH queue round in microseconds.
    #[clap(long, default_value = "16000")]
    pub(crate) batch_round_us: u64,

    /// Total LATENCY runtime budget for one wakeup, including continuation.
    #[clap(long, default_value = "2000")]
    pub(crate) latency_burst_us: u64,

    /// Maximum per-CPU LATENCY class debt used by all class arbitration paths.
    #[clap(long, default_value = "1000")]
    pub(crate) class_max_debt_us: u64,

    /// Minimum uninterrupted BATCH runtime before a class or vruntime handoff.
    #[clap(long, default_value = "500")]
    pub(crate) batch_min_run_us: u64,

    /// Vruntime gap required for a BATCH same-class handoff.
    #[clap(long, default_value = "500")]
    pub(crate) batch_preempt_granularity_us: u64,

    /// LATENCY class share weight.
    #[clap(long, default_value = "200")]
    pub(crate) latency_weight: u64,

    /// BATCH class share weight.
    #[clap(long, default_value = "100")]
    pub(crate) batch_weight: u64,

    /// Class assigned to a comm that does not match the rule table.
    #[clap(long, value_enum, default_value_t = WorkloadClass::Batch)]
    pub(crate) default_class: WorkloadClass,

    /// Static rule file containing COMM=latency or COMM=batch lines.
    #[clap(long, value_name = "PATH")]
    pub(crate) rules: Option<PathBuf>,

    /// Add or override one static COMM=latency|batch rule.
    #[clap(long = "rule", value_name = "COMM=CLASS", action = clap::ArgAction::Append)]
    pub(crate) rule: Vec<String>,

    /// Persistent JSON file containing Agent-learned rules.
    #[clap(long, value_name = "PATH")]
    pub(crate) learned_rules: Option<PathBuf>,

    /// Unix socket used by the scx_agent_classed MCP control client.
    #[clap(long, value_name = "PATH", requires = "learned_rules")]
    pub(crate) control_socket: Option<PathBuf>,

    /// tuning-agent activation socket notified on the first unknown comm.
    #[clap(
        long,
        value_name = "PATH",
        requires = "control_socket",
        requires = "learned_rules"
    )]
    pub(crate) tuning_agent_socket: Option<PathBuf>,

    /// Restrict tuning-agent activation to these comm values. May be repeated.
    #[clap(
        long = "activation-comm",
        value_name = "COMM",
        action = clap::ArgAction::Append,
        requires = "tuning_agent_socket"
    )]
    pub(crate) activation_comm: Vec<String>,

    /// Number of remote per-CPU class DSQs examined on an idle pull.
    #[clap(long, default_value = "8")]
    pub(crate) steal_scan: u32,

    /// Estimated migration cost within one LLC in microseconds.
    #[clap(long, default_value = "250")]
    pub(crate) same_llc_migration_cost_us: u64,

    /// Estimated migration cost within one NUMA node in microseconds.
    #[clap(long, default_value = "500")]
    pub(crate) same_node_migration_cost_us: u64,

    /// Estimated migration cost across NUMA nodes in microseconds.
    #[clap(long, default_value = "1000")]
    pub(crate) remote_node_migration_cost_us: u64,

    /// Track unmatched comm values and report them during a clean shutdown.
    #[clap(long, action = clap::ArgAction::SetTrue)]
    pub(crate) track_rule_misses: bool,

    /// Enable detailed per-CPU arbitration, placement, stealing, and lifecycle counters.
    #[clap(long, action = clap::ArgAction::SetTrue)]
    pub(crate) diagnostic_counters: bool,

    /// Exit debug dump buffer length. 0 uses the kernel default.
    #[clap(long, default_value = "0")]
    pub(crate) exit_dump_len: u32,

    /// Launch a statistics monitor at this interval in seconds.
    #[clap(long)]
    pub(crate) stats: Option<f64>,

    /// Monitor an already running scheduler without launching this one.
    #[clap(long)]
    pub(crate) monitor: Option<f64>,

    /// Enable BPF trace messages.
    #[clap(short = 'd', long, action = clap::ArgAction::SetTrue)]
    pub(crate) debug: bool,

    /// Enable verbose userspace and libbpf logging.
    #[clap(short = 'v', long, action = clap::ArgAction::SetTrue)]
    pub(crate) verbose: bool,

    /// Print version information and exit.
    #[clap(short = 'V', long, action = clap::ArgAction::SetTrue)]
    pub(crate) version: bool,

    /// Print descriptions for exported statistics.
    #[clap(long)]
    pub(crate) help_stats: bool,

    #[clap(flatten, next_help_heading = "Libbpf Options")]
    pub(crate) libbpf: LibbpfOpts,
}
