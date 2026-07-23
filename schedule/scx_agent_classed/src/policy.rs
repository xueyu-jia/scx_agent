// SPDX-License-Identifier: GPL-2.0

use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};

use crate::bpf_intf;
use crate::cli::Opts;
use crate::control_wire::ControlRequest;
use crate::rules::{Comm, RuleClass, RuleTable};

pub(crate) const COMM_LEN: usize = crate::rules::COMM_KEY_LEN;
pub(crate) fn parse_rule(spec: &str, origin: &str) -> Result<(Comm, RuleClass)> {
    let (comm, class) = spec
        .split_once('=')
        .with_context(|| format!("invalid rule '{spec}' in {origin}: expected COMM=CLASS"))?;
    let comm =
        Comm::new(comm.trim()).map_err(|error| anyhow!("invalid rule in {origin}: {error}"))?;
    let class =
        RuleClass::try_from(class).map_err(|error| anyhow!("invalid rule in {origin}: {error}"))?;
    Ok((comm, class))
}

pub(crate) fn load_activation_allowlist(opts: &Opts) -> Result<Option<BTreeSet<Comm>>> {
    if opts.activation_comm.is_empty() {
        return Ok(None);
    }

    let mut allowlist = BTreeSet::new();
    for value in &opts.activation_comm {
        let comm = Comm::new(value.clone())
            .with_context(|| format!("invalid --activation-comm '{value}'"))?;
        if !allowlist.insert(comm) {
            bail!("duplicate --activation-comm '{value}'");
        }
    }
    Ok(Some(allowlist))
}

pub(crate) fn load_rule_table(opts: &Opts) -> Result<RuleTable> {
    let mut rules = RuleTable::new();

    if let Some(path) = &opts.rules {
        let contents = fs::read_to_string(path)
            .with_context(|| format!("reading rule file {}", path.display()))?;
        for (index, raw_line) in contents.lines().enumerate() {
            let line = raw_line.split('#').next().unwrap_or_default().trim();
            if line.is_empty() {
                continue;
            }
            let origin = format!("{}:{}", path.display(), index + 1);
            let (key, class_id) = parse_rule(line, &origin)?;
            rules.insert(key, class_id);
        }
    }

    for (index, spec) in opts.rule.iter().enumerate() {
        let origin = format!("--rule #{}", index + 1);
        let (key, class_id) = parse_rule(spec, &origin)?;
        rules.insert(key, class_id);
    }

    if rules.len() > bpf_intf::agent_consts_AGENT_MAX_RULES as usize {
        bail!(
            "rule table contains {} entries, maximum is {}",
            rules.len(),
            bpf_intf::agent_consts_AGENT_MAX_RULES
        );
    }
    Ok(rules)
}

pub(crate) fn checked_ns(value_us: u64, name: &str) -> Result<u64> {
    if value_us == 0 {
        bail!("{name} must be greater than zero");
    }
    value_us
        .checked_mul(1000)
        .with_context(|| format!("{name} is too large"))
}

pub(crate) fn relay_rule_miss(
    data: &[u8],
    sender: &crossbeam::channel::Sender<[u8; COMM_LEN]>,
) -> i32 {
    if data.len() < std::mem::size_of::<bpf_intf::agent_rule_miss_event>() {
        return 0;
    }
    let event = unsafe {
        std::ptr::read_unaligned(data.as_ptr() as *const bpf_intf::agent_rule_miss_event)
    };
    if event.version != bpf_intf::agent_consts_AGENT_EVENT_ABI_VERSION
        || event.type_ != bpf_intf::agent_event_type_AGENT_EVENT_RULE_MISS
    {
        return 0;
    }

    let mut key = [0_u8; COMM_LEN];
    unsafe {
        std::ptr::copy_nonoverlapping(
            event.key.comm.as_ptr().cast::<u8>(),
            key.as_mut_ptr(),
            COMM_LEN,
        );
    }
    let _ = sender.try_send(key);
    0
}

pub(crate) fn validate_control_request(request: &ControlRequest) -> Result<()> {
    request.validate().map_err(anyhow::Error::msg)
}

pub(crate) fn effective_digest(rules: &RuleTable) -> String {
    let mut hasher = Sha256::new();
    for (comm, class) in rules {
        hasher.update(comm.as_bpf_key());
        hasher.update(class.as_bpf_id().to_le_bytes());
    }
    format!("sha256:{:x}", hasher.finalize())
}

pub(crate) fn hex_digest(value: &[u8]) -> String {
    format!("{:x}", Sha256::digest(value))
}

pub(crate) fn comm_from_key(key: &[u8]) -> Result<Comm> {
    if key.len() != COMM_LEN {
        bail!("rule key has {} bytes, expected {COMM_LEN}", key.len());
    }
    let visible_len = key.iter().position(|byte| *byte == 0).unwrap_or(COMM_LEN);
    let comm = std::str::from_utf8(&key[..visible_len]).context("comm is not valid UTF-8")?;
    Comm::new(comm)
}

pub(crate) fn normalize_cli_path(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .context("resolving dynamic-rule paths from the current directory")?
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    Ok(normalized)
}

pub(crate) fn validate_dynamic_paths(opts: &Opts) -> Result<()> {
    let paths = [
        ("--learned-rules", opts.learned_rules.as_deref()),
        ("--control-socket", opts.control_socket.as_deref()),
        ("--tuning-agent-socket", opts.tuning_agent_socket.as_deref()),
    ];
    for (index, (left_name, left)) in paths.iter().enumerate() {
        let Some(left) = left else {
            continue;
        };
        let left = normalize_cli_path(left)?;
        for (right_name, right) in &paths[index + 1..] {
            let Some(right) = right else {
                continue;
            };
            if left == normalize_cli_path(right)? {
                bail!("{left_name} and {right_name} must use distinct paths");
            }
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BatchEpochPolicy {
    pub(crate) min_epoch_ns: u64,
    pub(crate) max_epoch_ns: u64,
    pub(crate) round_ns: u64,
    pub(crate) min_run_ns: u64,
    pub(crate) preempt_granularity_ns: u64,
}

impl BatchEpochPolicy {
    pub(crate) fn from_opts(opts: &Opts) -> Result<Self> {
        let policy = Self {
            min_epoch_ns: checked_ns(opts.batch_min_epoch_us, "batch-min-epoch-us")?,
            max_epoch_ns: checked_ns(opts.batch_max_epoch_us, "batch-max-epoch-us")?,
            round_ns: checked_ns(opts.batch_round_us, "batch-round-us")?,
            min_run_ns: checked_ns(opts.batch_min_run_us, "batch-min-run-us")?,
            preempt_granularity_ns: checked_ns(
                opts.batch_preempt_granularity_us,
                "batch-preempt-granularity-us",
            )?,
        };

        policy
            .min_epoch_ns
            .checked_mul(8)
            .context("batch-min-epoch-us is too large for epoch levels")?;
        if policy.max_epoch_ns < policy.min_epoch_ns {
            bail!("batch-max-epoch-us must be at least batch-min-epoch-us");
        }
        if policy.max_epoch_ns > policy.min_epoch_ns * 8 {
            bail!("batch-max-epoch-us must not exceed eight times batch-min-epoch-us");
        }
        if policy.round_ns < policy.min_epoch_ns {
            bail!("batch-round-us must be at least batch-min-epoch-us");
        }
        if policy.min_run_ns > policy.min_epoch_ns {
            bail!("batch-min-run-us must not exceed batch-min-epoch-us");
        }
        Ok(policy)
    }
}
