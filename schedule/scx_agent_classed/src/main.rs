// SPDX-License-Identifier: GPL-2.0

mod bpf_skel;
pub use bpf_skel::*;
pub mod bpf_intf;
pub use bpf_intf::*;

mod activation;
mod cli;
mod control;
mod policy;
mod rules;
mod scheduler;
mod stats;

pub(crate) use scx_agent_classed_control as control_wire;

use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::Parser;
use libbpf_rs::PrintLevel;
use log::{debug, warn};
use scx_utils::build_id;
use scx_utils::init_libbpf_logging;

use cli::Opts;
use policy::validate_dynamic_paths;
use scheduler::Scheduler;

#[cfg(test)]
use cli::WorkloadClass;
#[cfg(test)]
use policy::{load_activation_allowlist, parse_rule, BatchEpochPolicy};
#[cfg(test)]
use rules::Comm;

pub(crate) const SCHEDULER_NAME: &str = "scx_agent_classed";
fn init_logging(verbose: bool) -> Result<()> {
    let mut config = simplelog::ConfigBuilder::new();
    config
        .set_time_offset_to_local()
        .expect("failed to set local time offset")
        .set_time_level(simplelog::LevelFilter::Error)
        .set_location_level(simplelog::LevelFilter::Off)
        .set_target_level(simplelog::LevelFilter::Off)
        .set_thread_level(simplelog::LevelFilter::Off);
    simplelog::TermLogger::init(
        if verbose {
            simplelog::LevelFilter::Debug
        } else {
            simplelog::LevelFilter::Info
        },
        config.build(),
        simplelog::TerminalMode::Stderr,
        simplelog::ColorChoice::Auto,
    )?;
    Ok(())
}

fn start_monitor(interval: f64, shutdown: Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(
        move || match stats::monitor(Duration::from_secs_f64(interval), shutdown) {
            Ok(()) => debug!("statistics monitor stopped"),
            Err(error) => warn!("statistics monitor stopped: {error}"),
        },
    )
}

fn main() -> Result<()> {
    let opts = Opts::parse();

    if opts.version {
        println!(
            "{} {}",
            SCHEDULER_NAME,
            build_id::full_version(env!("CARGO_PKG_VERSION"))
        );
        return Ok(());
    }
    if opts.help_stats {
        stats::server_data().describe_meta(&mut std::io::stdout(), None)?;
        return Ok(());
    }

    validate_dynamic_paths(&opts)?;
    init_logging(opts.verbose)?;
    if opts.verbose {
        init_libbpf_logging(Some(PrintLevel::Debug));
    }
    if let Some(interval) = opts.monitor.or(opts.stats) {
        if !interval.is_finite() || interval <= 0.0 {
            bail!("statistics interval must be a positive finite number");
        }
    }
    let shutdown = Arc::new(AtomicBool::new(false));
    let signal_shutdown = shutdown.clone();
    ctrlc::set_handler(move || signal_shutdown.store(true, Ordering::Relaxed))
        .context("installing Ctrl-C handler")?;

    let monitor = opts
        .monitor
        .or(opts.stats)
        .map(|interval| start_monitor(interval, shutdown.clone()));
    if opts.monitor.is_some() {
        if let Some(handle) = monitor {
            let _ = handle.join();
        }
        return Ok(());
    }

    let mut open_object = MaybeUninit::uninit();
    loop {
        let mut scheduler = Scheduler::init(&opts, &mut open_object)?;
        if !scheduler.run(shutdown.clone())?.should_restart() {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_exact_comm_rule() {
        let (comm, class) = parse_rule("pipewire=latency", "test").unwrap();
        let key = comm.as_bpf_key();

        assert_eq!(&key[..8], b"pipewire");
        assert!(key[8..].iter().all(|byte| *byte == 0));
        assert_eq!(class.as_bpf_id(), WorkloadClass::Latency.as_bpf());
    }

    #[test]
    fn rejects_comm_longer_than_linux_limit() {
        let error = parse_rule("sixteen-byte-name=batch", "test").unwrap_err();

        assert!(error.to_string().contains("at most 15"));
    }

    #[test]
    fn rejects_unknown_class() {
        let error = parse_rule("worker=interactive", "test").unwrap_err();

        assert!(error.to_string().contains("expected latency or batch"));
    }

    #[test]
    fn activation_allowlist_is_optional_and_validated() {
        let unrestricted = Opts::try_parse_from(["scx_agent_classed"]).unwrap();
        assert!(load_activation_allowlist(&unrestricted).unwrap().is_none());

        let restricted = Opts::try_parse_from([
            "scx_agent_classed",
            "--learned-rules",
            "learned.json",
            "--control-socket",
            "control.sock",
            "--tuning-agent-socket",
            "activation.sock",
            "--activation-comm",
            "redis-server",
            "--activation-comm",
            "hackbench",
        ])
        .unwrap();
        let allowlist = load_activation_allowlist(&restricted).unwrap().unwrap();
        assert_eq!(
            allowlist.iter().map(Comm::as_str).collect::<Vec<_>>(),
            vec!["hackbench", "redis-server"]
        );

        let duplicate = Opts::try_parse_from([
            "scx_agent_classed",
            "--learned-rules",
            "learned.json",
            "--control-socket",
            "control.sock",
            "--tuning-agent-socket",
            "activation.sock",
            "--activation-comm",
            "worker",
            "--activation-comm",
            "worker",
        ])
        .unwrap();
        assert!(load_activation_allowlist(&duplicate)
            .unwrap_err()
            .to_string()
            .contains("duplicate"));
    }

    #[test]
    fn rejects_colliding_dynamic_paths() {
        let cases = [
            vec![
                "scx_agent_classed",
                "--learned-rules",
                "state/shared",
                "--control-socket",
                "state/shared",
            ],
            vec![
                "scx_agent_classed",
                "--learned-rules",
                "state/learned.json",
                "--control-socket",
                "state/control.sock",
                "--tuning-agent-socket",
                "state/./control.sock",
            ],
            vec![
                "scx_agent_classed",
                "--learned-rules",
                "state/agent/../learned.json",
                "--control-socket",
                "state/control.sock",
                "--tuning-agent-socket",
                "state/learned.json",
            ],
        ];

        for args in cases {
            let opts = Opts::try_parse_from(args).unwrap();
            assert!(validate_dynamic_paths(&opts)
                .unwrap_err()
                .to_string()
                .contains("must use distinct paths"));
        }
    }

    #[test]
    fn canonical_batch_epoch_defaults() {
        let opts = Opts::try_parse_from(["scx_agent_classed"]).unwrap();
        let policy = BatchEpochPolicy::from_opts(&opts).unwrap();

        assert_eq!(
            policy,
            BatchEpochPolicy {
                min_epoch_ns: 1_000_000,
                max_epoch_ns: 8_000_000,
                round_ns: 16_000_000,
                min_run_ns: 500_000,
                preempt_granularity_ns: 500_000,
            }
        );
    }

    #[test]
    fn canonical_class_debt_default() {
        let opts = Opts::try_parse_from(["scx_agent_classed"]).unwrap();

        assert_eq!(opts.class_max_debt_us, opts.latency_slice_us);
    }

    #[test]
    fn accepts_explicit_batch_epoch_policy() {
        let opts = Opts::try_parse_from([
            "scx_agent_classed",
            "--batch-min-epoch-us",
            "2000",
            "--batch-max-epoch-us",
            "8000",
            "--batch-round-us",
            "24000",
            "--batch-min-run-us",
            "750",
            "--batch-preempt-granularity-us",
            "1000",
        ])
        .unwrap();
        let policy = BatchEpochPolicy::from_opts(&opts).unwrap();

        assert_eq!(policy.min_epoch_ns, 2_000_000);
        assert_eq!(policy.max_epoch_ns, 8_000_000);
        assert_eq!(policy.round_ns, 24_000_000);
        assert_eq!(policy.min_run_ns, 750_000);
        assert_eq!(policy.preempt_granularity_ns, 1_000_000);
    }

    #[test]
    fn rejects_batch_epoch_range_over_eight_levels() {
        let opts = Opts::try_parse_from([
            "scx_agent_classed",
            "--batch-min-epoch-us",
            "1000",
            "--batch-max-epoch-us",
            "9000",
        ])
        .unwrap();

        assert!(BatchEpochPolicy::from_opts(&opts)
            .unwrap_err()
            .to_string()
            .contains("eight times"));
    }

    #[test]
    fn rejects_batch_min_run_over_min_epoch() {
        let opts = Opts::try_parse_from([
            "scx_agent_classed",
            "--batch-min-epoch-us",
            "1000",
            "--batch-min-run-us",
            "1500",
        ])
        .unwrap();

        assert!(BatchEpochPolicy::from_opts(&opts)
            .unwrap_err()
            .to_string()
            .contains("must not exceed"));
    }
}
