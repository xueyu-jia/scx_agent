// SPDX-License-Identifier: GPL-2.0

use std::collections::BTreeSet;
use std::mem::MaybeUninit;
use std::sync::atomic::{fence, AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use crossbeam::channel::RecvTimeoutError;
use libbpf_rs::skel::OpenSkel;
use libbpf_rs::{MapCore, MapFlags, OpenObject, RingBufferBuilder};
use log::{info, warn};
use scx_stats::prelude::*;
use scx_utils::build_id;
use scx_utils::compat;
use scx_utils::Topology;
use scx_utils::{
    scx_ops_attach, scx_ops_load, scx_ops_open, try_set_rlimit_infinity, uei_exited, uei_report,
    UserExitInfo,
};

use crate::activation::{
    ActivationBatcher, ActivationCompletion, ActivationNotifier, RuleMissActivation,
};
use crate::bpf_intf;
use crate::cli::{Opts, WorkloadClass};
use crate::control::ControlServer;
use crate::control_wire;
use crate::control_wire::{
    ControlOp, ControlRequest, ControlResponse, ControlStats, ControlStatus, RuleObservation,
    RuleSource as WireRuleSource,
};
use crate::policy::{
    checked_ns, comm_from_key, effective_digest, hex_digest, load_activation_allowlist,
    load_rule_table, relay_rule_miss, validate_control_request, BatchEpochPolicy, COMM_LEN,
};
use crate::rules::{CasStatus, Comm, RuleClass, RuleSource, RuleState, RuleStore, RuleTable};
use crate::stats::{self, Metrics};
use crate::{BpfSkel, BpfSkelBuilder, SCHEDULER_NAME};

const MAX_STEAL_SCAN: u32 = bpf_intf::agent_consts_AGENT_STEAL_SCAN_MAX;
const IDLE_POLL_INTERVAL: Duration = Duration::from_secs(1);
const CONTROL_POLL_INTERVAL: Duration = Duration::from_millis(100);
const ACTIVATION_COALESCE: Duration = Duration::from_millis(250);
const RULE_MISS_RESCAN_INTERVAL: Duration = Duration::from_secs(30);
const MAX_ACTIVATION_COMMS: usize = 128;
const _: () = assert!(std::mem::size_of::<bpf_intf::agent_cpu_topology>() == 16);
pub(crate) struct Scheduler<'a> {
    skel: BpfSkel<'a>,
    struct_ops: Option<libbpf_rs::Link>,
    stats_server: StatsServer<(), Metrics>,
    rule_store: Option<RuleStore>,
    control_server: Option<ControlServer>,
    rule_miss_ring: libbpf_rs::RingBuffer<'static>,
    rule_miss_rx: crossbeam::channel::Receiver<[u8; COMM_LEN]>,
    activation_notifier: Option<ActivationNotifier>,
    activation_allowlist: Option<BTreeSet<Comm>>,
    activation_batcher: ActivationBatcher,
    last_rule_miss_rescan: Instant,
    scheduler_instance_id: String,
    default_class: RuleClass,
    fatal_control_error: Option<String>,
    rule_miss_collection_enabled: bool,
    poll_interval: Duration,
}

impl<'a> Scheduler<'a> {
    pub(crate) fn init(opts: &Opts, open_object: &'a mut MaybeUninit<OpenObject>) -> Result<Self> {
        let latency_slice_ns = checked_ns(opts.latency_slice_us, "latency-slice-us")?;
        let batch = BatchEpochPolicy::from_opts(opts)?;
        let latency_burst_budget_ns = checked_ns(opts.latency_burst_us, "latency-burst-us")?;
        let class_max_debt_ns = checked_ns(opts.class_max_debt_us, "class-max-debt-us")?;
        let same_llc_migration_cost_ns = checked_ns(
            opts.same_llc_migration_cost_us,
            "same-llc-migration-cost-us",
        )?;
        let same_node_migration_cost_ns = checked_ns(
            opts.same_node_migration_cost_us,
            "same-node-migration-cost-us",
        )?;
        let remote_node_migration_cost_ns = checked_ns(
            opts.remote_node_migration_cost_us,
            "remote-node-migration-cost-us",
        )?;
        latency_slice_ns
            .checked_mul(2)
            .context("latency-slice-us is too large for latency accounting")?;
        if latency_burst_budget_ns < latency_slice_ns {
            bail!("latency-burst-us must be at least latency-slice-us");
        }
        if latency_burst_budget_ns > latency_slice_ns * 2 {
            bail!("latency-burst-us must not exceed twice latency-slice-us");
        }
        if class_max_debt_ns > i64::MAX as u64 {
            bail!("class-max-debt-us exceeds the signed vruntime comparison range");
        }
        if opts.latency_weight == 0 || opts.batch_weight == 0 {
            bail!("class weights must be greater than zero");
        }
        if opts.steal_scan > MAX_STEAL_SCAN {
            bail!("steal-scan must be in the range 0..={MAX_STEAL_SCAN}");
        }
        if same_llc_migration_cost_ns > same_node_migration_cost_ns
            || same_node_migration_cost_ns > remote_node_migration_cost_ns
        {
            bail!("migration costs must be ordered same-LLC <= same-node <= remote-node");
        }
        let base_rules = load_rule_table(opts)?;
        let activation_allowlist = load_activation_allowlist(opts)?;
        let rule_store = opts
            .learned_rules
            .as_ref()
            .map(|path| RuleStore::open(path, base_rules.clone()))
            .transpose()
            .context("loading learned rules")?;
        let effective_rules = rule_store
            .as_ref()
            .map(RuleStore::effective)
            .unwrap_or(&base_rules);
        let topology = Topology::new().context("reading CPU topology")?;
        try_set_rlimit_infinity();
        info!(
            "{} {}",
            SCHEDULER_NAME,
            build_id::full_version(env!("CARGO_PKG_VERSION"))
        );
        info!(
            "scheduler options: {}",
            std::env::args().collect::<Vec<_>>().join(" ")
        );
        info!(
            "effective BATCH policy: epoch={}..{} us round={} us min-run={} us preempt-granularity={} us",
            batch.min_epoch_ns / 1000,
            batch.max_epoch_ns / 1000,
            batch.round_ns / 1000,
            batch.min_run_ns / 1000,
            batch.preempt_granularity_ns / 1000,
        );

        let mut skel_builder = BpfSkelBuilder::default();
        skel_builder.obj_builder.debug(opts.verbose);
        let open_opts = opts.libbpf.clone().into_bpf_open_opts();
        let mut skel = scx_ops_open!(skel_builder, open_object, agent_classed_ops, open_opts)?;
        if opts.verbose {
            for mut program in skel.open_object_mut().progs_mut() {
                program.set_log_level(1);
            }
        }

        skel.struct_ops.agent_classed_ops_mut().exit_dump_len = opts.exit_dump_len;
        skel.struct_ops.agent_classed_ops_mut().flags = *compat::SCX_OPS_ENQ_EXITING
            | *compat::SCX_OPS_ENQ_LAST
            | *compat::SCX_OPS_ENQ_MIGRATION_DISABLED
            | *compat::SCX_OPS_ALLOW_QUEUED_WAKEUP
            | *compat::SCX_OPS_BUILTIN_IDLE_PER_NODE;

        let rodata = skel
            .maps
            .rodata_data
            .as_mut()
            .context("missing BPF rodata")?;
        rodata.debug = opts.debug;
        rodata.latency_slice_ns = latency_slice_ns;
        rodata.batch_min_epoch_ns = batch.min_epoch_ns;
        rodata.batch_max_epoch_ns = batch.max_epoch_ns;
        rodata.batch_round_ns = batch.round_ns;
        rodata.latency_burst_budget_ns = latency_burst_budget_ns;
        rodata.class_max_debt_ns = class_max_debt_ns;
        rodata.batch_min_run_ns = batch.min_run_ns;
        rodata.batch_preempt_granularity_ns = batch.preempt_granularity_ns;
        rodata.latency_weight = opts.latency_weight;
        rodata.batch_weight = opts.batch_weight;
        rodata.default_class = opts.default_class.as_bpf();
        rodata.steal_scan = opts.steal_scan;
        rodata.same_llc_migration_cost_ns = same_llc_migration_cost_ns;
        rodata.same_node_migration_cost_ns = same_node_migration_cost_ns;
        rodata.remote_node_migration_cost_ns = remote_node_migration_cost_ns;
        let rule_miss_collection_enabled =
            opts.track_rule_misses || opts.tuning_agent_socket.is_some();
        rodata.track_rule_misses = rule_miss_collection_enabled;
        rodata.diagnostic_counters = opts.diagnostic_counters;

        let mut skel = scx_ops_load!(skel, agent_classed_ops, uei)?;
        for (comm, class) in effective_rules {
            let key = comm.as_bpf_key();
            skel.maps
                .rules_map
                .update(&key, &class.as_bpf_id().to_ne_bytes(), MapFlags::ANY)?;
        }
        let max_capacity = topology
            .all_cpus
            .values()
            .map(|cpu| cpu.cpu_capacity)
            .max()
            .unwrap_or(1)
            .max(1);
        for cpu in topology.all_cpus.values() {
            let cpu_id = u32::try_from(cpu.id).context("CPU ID exceeds u32")?;
            if cpu_id >= bpf_intf::agent_consts_AGENT_MAX_CPUS {
                bail!("CPU ID {cpu_id} exceeds scheduler topology map capacity");
            }
            let llc_id = u32::try_from(cpu.llc_id).context("LLC ID exceeds u32")?;
            let node_id = u32::try_from(cpu.node_id).context("NUMA node ID exceeds u32")?;
            let capacity = (cpu.cpu_capacity * 1024 / max_capacity).clamp(1, 1024) as u32;
            let mut value = [0u8; 16];
            value[0..4].copy_from_slice(&llc_id.to_ne_bytes());
            value[4..8].copy_from_slice(&node_id.to_ne_bytes());
            value[8..12].copy_from_slice(&capacity.to_ne_bytes());
            skel.maps
                .cpu_topology_map
                .update(&cpu_id.to_ne_bytes(), &value, MapFlags::ANY)?;
        }
        info!(
            "loaded {} base and {} learned rules; unmatched tasks use {:?}",
            base_rules.len(),
            rule_store.as_ref().map_or(0, |store| store.learned().len()),
            opts.default_class
        );

        skel.maps
            .bss_data
            .as_mut()
            .context("missing BPF bss")?
            .rules_seq = 2;

        let (rule_miss_tx, rule_miss_rx) = crossbeam::channel::bounded(1024);
        let mut ring_builder = RingBufferBuilder::new();
        ring_builder
            .add(&skel.maps.rule_miss_events, move |data| {
                relay_rule_miss(data, &rule_miss_tx)
            })
            .context("registering rule-miss ring buffer")?;
        let rule_miss_ring = ring_builder
            .build()
            .context("building rule-miss ring buffer")?;

        let control_server = opts
            .control_socket
            .as_ref()
            .map(ControlServer::bind)
            .transpose()?;
        if let Some(server) = &control_server {
            info!(
                "MCP control socket listening on {}",
                server.path().display()
            );
        }
        let now_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let scheduler_instance_id = format!("{}-{now_ns}", std::process::id());
        let activation_notifier = opts
            .tuning_agent_socket
            .as_ref()
            .map(|path| ActivationNotifier::start(path.clone(), scheduler_instance_id.clone()));
        if let Some(allowlist) = &activation_allowlist {
            info!(
                "tuning-agent activation restricted to {} comm values",
                allowlist.len()
            );
        }
        let poll_interval = if control_server.is_some() || activation_notifier.is_some() {
            CONTROL_POLL_INTERVAL
        } else {
            IDLE_POLL_INTERVAL
        };

        let struct_ops = Some(scx_ops_attach!(skel, agent_classed_ops, false)?);
        let stats_server = StatsServer::new(stats::server_data()).launch()?;
        let now = Instant::now();

        Ok(Self {
            skel,
            struct_ops,
            stats_server,
            rule_store,
            control_server,
            rule_miss_ring,
            rule_miss_rx,
            activation_notifier,
            activation_allowlist,
            activation_batcher: ActivationBatcher::new(ACTIVATION_COALESCE, MAX_ACTIVATION_COMMS),
            last_rule_miss_rescan: now,
            scheduler_instance_id,
            default_class: opts.default_class.as_rule_class(),
            fatal_control_error: None,
            rule_miss_collection_enabled,
            poll_interval,
        })
    }

    fn rules_seq(&self) -> u64 {
        let bss = self.skel.maps.bss_data.as_ref().expect("missing BPF bss");
        let pointer = std::ptr::addr_of!(bss.rules_seq).cast_mut();
        unsafe { AtomicU64::from_ptr(pointer).load(Ordering::SeqCst) }
    }

    fn write_rules_seq(&mut self, value: u64) -> Result<()> {
        let bss = self
            .skel
            .maps
            .bss_data
            .as_mut()
            .context("missing BPF bss")?;
        let pointer = std::ptr::addr_of_mut!(bss.rules_seq);
        unsafe { AtomicU64::from_ptr(pointer).store(value, Ordering::SeqCst) };
        Ok(())
    }

    fn publish_rule(&mut self, comm: &Comm, state: RuleState) -> Result<()> {
        let stable = self.rules_seq() & !1;
        let updating = stable.wrapping_add(1);
        let published = stable.wrapping_add(2);

        self.write_rules_seq(updating)?;
        fence(Ordering::SeqCst);

        let key = comm.as_bpf_key();
        let update_result = (|| -> Result<()> {
            match state {
                RuleState::Present(class) => self
                    .skel
                    .maps
                    .rules_map
                    .update(&key, &class.as_bpf_id().to_ne_bytes(), MapFlags::ANY)
                    .context("updating BPF rule"),
                RuleState::Absent => {
                    if self
                        .skel
                        .maps
                        .rules_map
                        .lookup(&key, MapFlags::ANY)?
                        .is_some()
                    {
                        self.skel
                            .maps
                            .rules_map
                            .delete(&key)
                            .context("deleting BPF rule")
                    } else {
                        Ok(())
                    }
                }
            }
        })();

        fence(Ordering::SeqCst);
        self.write_rules_seq(published)?;
        update_result.context("publishing BPF rule")
    }

    fn active_rule(&self, comm: &Comm) -> Result<Option<RuleClass>> {
        let Some(value) = self
            .skel
            .maps
            .rules_map
            .lookup(&comm.as_bpf_key(), MapFlags::ANY)?
        else {
            return Ok(None);
        };
        if value.len() != std::mem::size_of::<u32>() {
            bail!("rules_map returned an invalid value size");
        }
        let id = u32::from_ne_bytes(value.try_into().expect("checked BPF rule value size"));
        match id {
            bpf_intf::workload_class_CLASS_LATENCY => Ok(Some(RuleClass::Latency)),
            bpf_intf::workload_class_CLASS_BATCH => Ok(Some(RuleClass::Batch)),
            other => bail!("rules_map returned invalid class id {other}"),
        }
    }

    fn persisted_snapshot(&self) -> Result<(RuleTable, bool)> {
        let store = self
            .rule_store
            .as_ref()
            .context("learned rule store is disabled")?;
        let (persisted, revision) = store.read_persisted()?;
        let matches_store = revision == store.revision() && &persisted == store.learned();
        Ok((persisted, matches_store))
    }

    fn rule_observation(&self, comm: &Comm) -> Result<RuleObservation> {
        let (persisted, matches_store) = self.persisted_snapshot()?;
        self.rule_observation_with_persisted(comm, &persisted, matches_store)
    }

    fn rule_observation_with_persisted(
        &self,
        comm: &Comm,
        persisted: &RuleTable,
        persisted_matches_store: bool,
    ) -> Result<RuleObservation> {
        let store = self
            .rule_store
            .as_ref()
            .context("learned rule store is disabled")?;
        let effective = store.effective_rule(comm);
        let (class, source) = match effective {
            Some(rule) => (
                rule.class,
                match rule.source {
                    RuleSource::Base => WireRuleSource::Base,
                    RuleSource::Learned => WireRuleSource::Learned,
                },
            ),
            None => (self.default_class, WireRuleSource::Default),
        };
        let active_class = self.active_rule(comm)?;
        let persisted_class = persisted.get(comm).copied();
        let expected_active = effective.map(|rule| rule.class);

        Ok(RuleObservation {
            comm: comm.as_str().to_string(),
            class: class.into(),
            source,
            active_class: active_class.map(Into::into),
            persisted_class: persisted_class.map(Into::into),
            consistent: active_class == expected_active && persisted_matches_store,
        })
    }

    fn control_response(
        &self,
        request_id: String,
        status: ControlStatus,
        current: Option<RuleState>,
        rules: Vec<RuleObservation>,
        message: Option<String>,
    ) -> ControlResponse {
        let bss = self.skel.maps.bss_data.as_ref().expect("missing BPF bss");
        let revision = self.rule_store.as_ref().map_or(0, RuleStore::revision);
        let digest = self
            .rule_store
            .as_ref()
            .map(|store| effective_digest(store.effective()))
            .unwrap_or_else(|| effective_digest(&RuleTable::new()));
        ControlResponse {
            version: control_wire::CONTROL_VERSION,
            request_id,
            status,
            current: current.map(Into::into),
            rules,
            revision,
            rules_seq: self.rules_seq(),
            effective_digest: digest,
            stats: Some(ControlStats {
                task_state_errors: bss.nr_task_state_errors,
                rule_refresh_deferred: bss.nr_rule_refresh_deferred,
            }),
            workload_fingerprint: Some(format!(
                "sha256:{}",
                hex_digest(self.scheduler_instance_id.as_bytes())
            )),
            message,
        }
    }

    fn handle_control(&mut self, request: ControlRequest) -> ControlResponse {
        let request_id = request.request_id.clone();
        let result = match self.handle_control_inner(&request) {
            Ok(response) => return response,
            Err(error) => error,
        };
        self.control_response(
            request_id,
            ControlStatus::Error,
            None,
            Vec::new(),
            Some(format!("{result:#}")),
        )
    }

    fn handle_control_inner(&mut self, request: &ControlRequest) -> Result<ControlResponse> {
        validate_control_request(request)?;
        match request.op {
            ControlOp::GetRule => {
                let comm = Comm::new(request.comm.clone().expect("validated comm"))?;
                let current = self
                    .rule_store
                    .as_ref()
                    .context("learned rule store is disabled")?
                    .learned_state(&comm);
                let observation = self.rule_observation(&comm)?;
                Ok(self.control_response(
                    request.request_id.clone(),
                    ControlStatus::Ok,
                    Some(current),
                    vec![observation],
                    None,
                ))
            }
            ControlOp::Snapshot => {
                let comms = request
                    .comms
                    .as_ref()
                    .expect("validated comms")
                    .iter()
                    .map(|comm| Comm::new(comm.clone()))
                    .collect::<Result<Vec<_>>>()?;
                let (persisted, matches_store) = self.persisted_snapshot()?;
                let rules = comms
                    .iter()
                    .map(|comm| {
                        self.rule_observation_with_persisted(comm, &persisted, matches_store)
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok(self.control_response(
                    request.request_id.clone(),
                    ControlStatus::Ok,
                    None,
                    rules,
                    None,
                ))
            }
            ControlOp::CompareAndSetRule => {
                let comm = Comm::new(request.comm.clone().expect("validated comm"))?;
                let expected = RuleState::try_from(
                    request.expected.clone().expect("validated expected state"),
                )?;
                let desired =
                    RuleState::try_from(request.desired.clone().expect("validated desired state"))?;
                if self
                    .rule_store
                    .as_ref()
                    .context("learned rule store is disabled")?
                    .base()
                    .contains_key(&comm)
                {
                    bail!("comm '{comm}' is owned by a read-only base rule");
                }
                let cas_result = self
                    .rule_store
                    .as_mut()
                    .context("learned rule store is disabled")?
                    .compare_and_set(comm.clone(), expected, desired);
                let cas = match cas_result {
                    Ok(cas) => cas,
                    Err(error) => {
                        if self
                            .rule_store
                            .as_ref()
                            .is_some_and(RuleStore::persistence_uncertain)
                        {
                            self.fatal_control_error = Some(format!(
                                "learned-rule rename completed but durability is uncertain: {error:#}"
                            ));
                        }
                        return Err(error);
                    }
                };
                if cas.status == CasStatus::Conflict {
                    let observation = self.rule_observation(&comm)?;
                    return Ok(self.control_response(
                        request.request_id.clone(),
                        ControlStatus::Conflict,
                        Some(cas.current),
                        vec![observation],
                        Some("learned rule changed since prepare".to_string()),
                    ));
                }

                if cas.status == CasStatus::Noop {
                    if self.active_rule(&comm)? != cas.effective.class() {
                        if let Err(error) = self.publish_rule(&comm, cas.effective) {
                            self.fatal_control_error = Some(format!(
                                "failed to reconcile persistent rule with BPF state: {error:#}"
                            ));
                            return Err(error);
                        }
                    }
                    if matches!(cas.current, RuleState::Present(_)) {
                        self.clear_rule_miss(&comm);
                    }
                    let observation = self.rule_observation(&comm)?;
                    return Ok(self.control_response(
                        request.request_id.clone(),
                        ControlStatus::Noop,
                        Some(cas.current),
                        vec![observation],
                        None,
                    ));
                }

                if let Err(error) = self.publish_rule(&comm, cas.effective) {
                    let recovery = (|| -> Result<()> {
                        let rollback = self
                            .rule_store
                            .as_mut()
                            .context("learned rule store is disabled")?
                            .compare_and_set(comm.clone(), cas.current, cas.previous)
                            .context("rolling back persistent rule after BPF publish failure")?;
                        self.publish_rule(&comm, rollback.effective)
                            .context("restoring BPF rule after publish failure")
                    })();
                    if let Err(recovery_error) = recovery {
                        self.fatal_control_error = Some(format!(
                            "rule publish failed ({error:#}) and recovery failed ({recovery_error:#})"
                        ));
                    }
                    return Err(error);
                }
                if matches!(cas.current, RuleState::Present(_)) {
                    self.clear_rule_miss(&comm);
                }
                let observation = self.rule_observation(&comm)?;
                Ok(self.control_response(
                    request.request_id.clone(),
                    ControlStatus::Applied,
                    Some(cas.current),
                    vec![observation],
                    None,
                ))
            }
        }
    }

    fn clear_rule_miss(&mut self, comm: &Comm) {
        let key = comm.as_bpf_key();
        if self
            .skel
            .maps
            .rule_miss_comms
            .lookup(&key, MapFlags::ANY)
            .ok()
            .flatten()
            .is_some()
        {
            let _ = self.skel.maps.rule_miss_comms.delete(&key);
        }
        self.activation_batcher.remove_pending(comm);
    }

    fn record_rule_miss(&mut self, comm: Comm) {
        if self
            .activation_allowlist
            .as_ref()
            .is_some_and(|allowlist| !allowlist.contains(&comm))
        {
            return;
        }
        let already_classified = self
            .rule_store
            .as_ref()
            .and_then(|store| store.effective_rule(&comm))
            .is_some();
        if already_classified {
            self.clear_rule_miss(&comm);
        } else {
            self.activation_batcher.observe(comm, Instant::now());
        }
    }

    fn discard_resolved_rule_misses(&mut self) {
        let resolved = self
            .activation_batcher
            .pending()
            .filter(|comm| {
                self.rule_store
                    .as_ref()
                    .and_then(|store| store.effective_rule(comm))
                    .is_some()
            })
            .cloned()
            .collect::<Vec<_>>();
        for comm in resolved {
            self.clear_rule_miss(&comm);
        }
    }

    fn poll_control(&mut self) -> Result<()> {
        let connections = match &self.control_server {
            Some(server) => server.poll()?,
            None => return Ok(()),
        };
        for connection in connections {
            let response = self.handle_control(connection.request.clone());
            if let Err(error) = connection.respond(&response) {
                warn!("failed to respond to control request: {error:#}");
            }
            if let Some(error) = self.fatal_control_error.take() {
                bail!("fatal dynamic-rule control failure: {error}");
            }
        }
        Ok(())
    }

    fn poll_activation_completion(&mut self) -> Result<()> {
        let Some(notifier) = self.activation_notifier.as_ref() else {
            return Ok(());
        };
        let completion = match notifier.poll_completion() {
            Ok(Some(completion)) => completion,
            Ok(None) => return Ok(()),
            Err(error) => bail!("tuning-agent activation notifier failed: {error:?}"),
        };
        let Some(comms) = self.activation_batcher.finish() else {
            warn!("received a tuning-agent completion without an in-flight activation");
            return Ok(());
        };
        match completion {
            ActivationCompletion::Response(response) if response.accepted => info!(
                "tuning-agent activation completed with status={} for {} comms",
                response.status,
                comms.len()
            ),
            ActivationCompletion::Response(response) => warn!(
                "tuning-agent rejected activation for {} comms: status={} error={}",
                comms.len(),
                response.status,
                response.error.as_deref().unwrap_or("none")
            ),
            ActivationCompletion::Failed(error) => warn!(
                "tuning-agent activation failed for {} comms: {error}",
                comms.len()
            ),
        }
        for comm in comms {
            if self
                .rule_store
                .as_ref()
                .and_then(|store| store.effective_rule(&comm))
                .is_some()
            {
                self.clear_rule_miss(&comm);
            }
        }
        Ok(())
    }

    fn collect_rule_misses(&mut self) -> Result<()> {
        if !self.rule_miss_collection_enabled {
            return Ok(());
        }
        self.rule_miss_ring
            .consume()
            .context("consuming rule-miss ring buffer")?;
        if self.activation_notifier.is_none() {
            while self.rule_miss_rx.try_recv().is_ok() {}
            return Ok(());
        }
        while let Ok(key) = self.rule_miss_rx.try_recv() {
            match comm_from_key(&key) {
                Ok(comm) => self.record_rule_miss(comm),
                Err(error) => warn!("ignored invalid rule-miss comm: {error:#}"),
            }
        }

        if self.activation_notifier.is_some()
            && self.last_rule_miss_rescan.elapsed() >= RULE_MISS_RESCAN_INTERVAL
        {
            self.last_rule_miss_rescan = Instant::now();
            let keys = self.skel.maps.rule_miss_comms.keys().collect::<Vec<_>>();
            for key in keys {
                match comm_from_key(&key) {
                    Ok(comm) => self.record_rule_miss(comm),
                    Err(error) => warn!("ignored invalid persisted rule miss: {error:#}"),
                }
            }
        }

        self.discard_resolved_rule_misses();
        let Some(comms) = self.activation_batcher.take_ready(Instant::now()) else {
            return Ok(());
        };
        let activation = RuleMissActivation {
            comms: comms.iter().map(|comm| comm.as_str().to_string()).collect(),
            revision: self.rule_store.as_ref().map_or(0, RuleStore::revision),
        };
        let notifier = self
            .activation_notifier
            .as_ref()
            .expect("rule-miss collection checked the notifier");
        if let Err(error) = notifier.start_activation(activation) {
            bail!("failed to start tuning-agent activation: {error:?}");
        }
        Ok(())
    }

    fn get_metrics(&self) -> Metrics {
        let bss = self.skel.maps.bss_data.as_ref().expect("missing BPF bss");
        let latency = &bss.classes[WorkloadClass::Latency.as_bpf() as usize];
        let batch = &bss.classes[WorkloadClass::Batch.as_bpf() as usize];

        Metrics {
            nr_latency_queued: latency.nr_queued,
            nr_batch_queued: batch.nr_queued,
            nr_latency_running: latency.nr_running,
            nr_batch_running: batch.nr_running,
            latency_class_vruntime: latency.vruntime,
            batch_class_vruntime: batch.vruntime,
            nr_enqueues: bss.nr_enqueues,
            nr_latency_enqueues: bss.nr_latency_enqueues,
            nr_batch_enqueues: bss.nr_batch_enqueues,
            nr_direct_dispatches: bss.nr_direct_dispatches,
            nr_latency_preempts: bss.nr_latency_preempts,
            nr_latency_wakeup_enqueues: bss.nr_latency_wakeup_enqueues,
            nr_latency_handoffs: bss.nr_latency_handoffs,
            nr_latency_handoff_deferred: bss.nr_latency_handoff_deferred,
            nr_arbitration_slice_caps: bss.nr_arbitration_slice_caps,
            nr_latency_non_wakeup_enqueues: bss.nr_latency_non_wakeup_enqueues,
            nr_latency_continuations: bss.nr_latency_continuations,
            nr_latency_continuation_class_denied: bss.nr_latency_continuation_class_denied,
            nr_latency_continuation_budget_exhausted: bss.nr_latency_continuation_budget_exhausted,
            nr_latency_continuation_history_denied: bss.nr_latency_continuation_history_denied,
            nr_latency_stops_runnable: bss.nr_latency_stops_runnable,
            nr_latency_stops_quiescent: bss.nr_latency_stops_quiescent,
            nr_latency_slice_expirations: bss.nr_latency_slice_expirations,
            nr_batch_epochs: bss.nr_batch_epochs,
            nr_batch_epoch_exhaustions: bss.nr_batch_epoch_exhaustions,
            nr_batch_epoch_grows: bss.nr_batch_epoch_grows,
            nr_batch_epoch_resets: bss.nr_batch_epoch_resets,
            nr_batch_round_caps: bss.nr_batch_round_caps,
            nr_batch_grants_1x: bss.nr_batch_grants_1x,
            nr_batch_grants_2x: bss.nr_batch_grants_2x,
            nr_batch_grants_4x: bss.nr_batch_grants_4x,
            nr_batch_grants_8x: bss.nr_batch_grants_8x,
            nr_batch_vruntime_preempts: bss.nr_batch_vruntime_preempts,
            nr_local_dispatches: bss.nr_local_dispatches,
            nr_remote_dispatches: bss.nr_remote_dispatches,
            nr_latency_local_dispatches: bss.nr_latency_local_dispatches,
            nr_batch_local_dispatches: bss.nr_batch_local_dispatches,
            nr_latency_migrations: bss.nr_latency_migrations,
            nr_batch_migrations: bss.nr_batch_migrations,
            nr_fallback_dispatches: bss.nr_fallback_dispatches,
            nr_dequeues: bss.nr_dequeues,
            nr_task_state_errors: bss.nr_task_state_errors,
            nr_enqueue_ownership_reconciles: bss.nr_enqueue_ownership_reconciles,
            nr_running_queue_reconciles: bss.nr_running_queue_reconciles,
            nr_rule_matches: bss.nr_rule_matches,
            nr_rule_misses: bss.nr_rule_misses,
            rules_seq: self.rules_seq(),
            nr_rule_refreshes: bss.nr_rule_refreshes,
            nr_rule_refreshes_enqueue: bss.nr_rule_refreshes_enqueue,
            nr_rule_refreshes_runnable: bss.nr_rule_refreshes_runnable,
            nr_class_migrations: bss.nr_class_migrations,
            nr_class_migrations_enqueue: bss.nr_class_migrations_enqueue,
            nr_class_migrations_runnable: bss.nr_class_migrations_runnable,
            nr_rule_refresh_deferred: bss.nr_rule_refresh_deferred,
            nr_rule_refresh_conflicts: bss.nr_rule_refresh_conflicts,
            nr_stale_continuation_denied: bss.nr_stale_continuation_denied,
            nr_rule_miss_events: bss.nr_rule_miss_events,
            nr_rule_miss_event_drops: bss.nr_rule_miss_event_drops,
            latency_runtime_ns: bss.latency_runtime_ns,
            batch_runtime_ns: bss.batch_runtime_ns,
            nr_fallback_enqueues: bss.nr_fallback_enqueues,
            nr_single_class_fastpaths: bss.nr_single_class_fastpaths,
            nr_mixed_class_arbitrations: bss.nr_mixed_class_arbitrations,
            nr_class_decisions_latency: bss.nr_class_decisions_latency,
            nr_class_decisions_batch: bss.nr_class_decisions_batch,
            nr_class_decisions_batch_min_run: bss.nr_class_decisions_batch_min_run,
            mixed_class_lag_ns: bss.mixed_class_lag_ns,
            nr_gated_steal_attempts: bss.nr_gated_steal_attempts,
            nr_gated_steal_successes: bss.nr_gated_steal_successes,
            nr_gated_steal_local_busy: bss.nr_gated_steal_local_busy,
            nr_gated_steal_source_short: bss.nr_gated_steal_source_short,
            nr_gated_steal_load_gap: bss.nr_gated_steal_load_gap,
            nr_gated_steal_cooldown: bss.nr_gated_steal_cooldown,
            nr_gated_steal_claim_busy: bss.nr_gated_steal_claim_busy,
        }
    }
    fn exited(&mut self) -> bool {
        uei_exited!(&self.skel, uei)
    }

    fn log_rule_misses(&self) -> Result<()> {
        let map = &self.skel.maps.rule_miss_comms;
        let mut misses = Vec::new();

        for key in map.keys() {
            let Some(value) = map.lookup(&key, MapFlags::ANY)? else {
                continue;
            };
            if key.len() != COMM_LEN || value.len() != std::mem::size_of::<u64>() {
                continue;
            }
            let count = u64::from_ne_bytes(
                value
                    .as_slice()
                    .try_into()
                    .expect("rule miss count has a checked size"),
            );
            let visible_len = key.iter().position(|byte| *byte == 0).unwrap_or(COMM_LEN);
            let comm = String::from_utf8_lossy(&key[..visible_len]).into_owned();
            misses.push((count, comm));
        }

        misses.sort_unstable_by(|left, right| right.cmp(left));
        if !misses.is_empty() {
            info!(
                "tracked {} distinct rule-miss comm values; showing at most 32",
                misses.len()
            );
        }
        for (count, comm) in misses.into_iter().take(32) {
            info!("rule miss comm={comm:?} count={count}");
        }
        Ok(())
    }

    pub(crate) fn run(&mut self, shutdown: Arc<AtomicBool>) -> Result<UserExitInfo> {
        let (res_ch, req_ch) = self.stats_server.channels();
        while !shutdown.load(Ordering::Relaxed) && !self.exited() {
            match req_ch.recv_timeout(self.poll_interval) {
                Ok(()) => res_ch.send(self.get_metrics())?,
                Err(RecvTimeoutError::Timeout) => {}
                Err(error) => Err(error)?,
            }
            self.poll_control()?;
            self.poll_activation_completion()?;
            self.collect_rule_misses()?;
        }

        if let Err(error) = self.log_rule_misses() {
            warn!("failed to read rule miss comm counters: {error}");
        }
        let _ = self.struct_ops.take();
        uei_report!(&self.skel, uei)
    }
}

impl Drop for Scheduler<'_> {
    fn drop(&mut self) {
        info!("unregistering {SCHEDULER_NAME}");
    }
}
