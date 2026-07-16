use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};

use crate::capability::ProbeProvider;
use crate::domain::{
    CapabilityId, CapabilityKind, CapabilityMeta, Digest, EffectClass, EpisodePhase, ProbeEvidence,
    ProbeRequest, ProviderClass, ProviderError, ProviderErrorKind, ProviderId, ProviderPin,
    ProviderVersion,
};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Section {
    Loadavg,
    Memory,
    Pressure,
    Cpu,
    Scheduler,
}

impl Section {
    const ALL: [Self; 5] = [
        Self::Loadavg,
        Self::Memory,
        Self::Pressure,
        Self::Cpu,
        Self::Scheduler,
    ];

    fn parse(value: &str) -> Option<Self> {
        match value {
            "loadavg" => Some(Self::Loadavg),
            "memory" => Some(Self::Memory),
            "pressure" => Some(Self::Pressure),
            "cpu" => Some(Self::Cpu),
            "scheduler" => Some(Self::Scheduler),
            _ => None,
        }
    }
}

pub struct LinuxProcSnapshotProbe {
    meta: CapabilityMeta,
    proc_root: PathBuf,
}

impl LinuxProcSnapshotProbe {
    pub fn new() -> Self {
        Self::with_proc_root("/proc")
    }

    /// The root is fixed when the trusted provider is constructed and is never
    /// accepted from a probe invocation.
    pub fn with_proc_root(proc_root: impl Into<PathBuf>) -> Self {
        let provider = ProviderPin {
            provider_id: ProviderId::new("builtin.linux-proc-snapshot")
                .expect("static provider id is valid"),
            provider_version: ProviderVersion::new("1").expect("static version is valid"),
            provider_class: ProviderClass::Builtin,
            manifest_digest: Digest::new("builtin-linux-proc-snapshot-v1")
                .expect("static digest is valid"),
        };
        let mut meta = CapabilityMeta::new(
            CapabilityId::new("builtin/probe.linux-proc-snapshot.v1")
                .expect("static capability id is valid"),
            CapabilityKind::Probe,
            EffectClass::ReadOnly,
            provider,
            "Collect a bounded structured Linux procfs diagnostic snapshot",
            json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "sections": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": 5,
                        "uniqueItems": true,
                        "items": {
                            "type": "string",
                            "enum": ["loadavg", "memory", "pressure", "cpu", "scheduler"]
                        }
                    }
                }
            }),
            json!({
                "type": "object",
                "required": ["schema_version", "sections"],
                "properties": {
                    "schema_version": { "const": 1 },
                    "sections": { "type": "object" }
                }
            }),
        )
        .with_allowed_phases([EpisodePhase::Clean, EpisodePhase::Experimenting]);
        meta.idempotent = true;

        Self {
            meta,
            proc_root: proc_root.into(),
        }
    }

    fn collect(&self, requested: &BTreeSet<Section>) -> ProbeEvidence {
        let mut warnings = Vec::new();
        let mut sections = Map::new();

        if requested.contains(&Section::Loadavg) {
            collect_section(
                &mut sections,
                &mut warnings,
                "loadavg",
                self.proc_root.join("loadavg"),
                parse_loadavg,
            );
        }
        if requested.contains(&Section::Memory) {
            collect_section(
                &mut sections,
                &mut warnings,
                "memory",
                self.proc_root.join("meminfo"),
                parse_meminfo_summary,
            );
        }
        if requested.contains(&Section::Pressure) {
            let mut pressure = Map::new();
            for resource in ["cpu", "io", "memory"] {
                let path = self.proc_root.join("pressure").join(resource);
                match read_and_parse(&path, parse_psi) {
                    Ok(value) => {
                        pressure.insert(resource.to_string(), value);
                    }
                    Err(error) => warnings.push(format!("pressure.{resource}: {error}")),
                }
            }
            if !pressure.is_empty() {
                sections.insert("pressure".to_string(), Value::Object(pressure));
            }
        }
        if requested.contains(&Section::Cpu) || requested.contains(&Section::Scheduler) {
            let path = self.proc_root.join("stat");
            match fs::read_to_string(&path) {
                Ok(content) => {
                    if requested.contains(&Section::Cpu) {
                        match parse_cpu(&content) {
                            Ok(value) => {
                                sections.insert("cpu".to_string(), value);
                            }
                            Err(error) => warnings.push(format!("cpu: {error}")),
                        }
                    }
                    if requested.contains(&Section::Scheduler) {
                        match parse_scheduler(&content) {
                            Ok(value) => {
                                sections.insert("scheduler".to_string(), value);
                            }
                            Err(error) => warnings.push(format!("scheduler: {error}")),
                        }
                    }
                }
                Err(error) => {
                    if requested.contains(&Section::Cpu) {
                        warnings.push(format!("cpu: failed to read '{}': {error}", path.display()));
                    }
                    if requested.contains(&Section::Scheduler) {
                        warnings.push(format!(
                            "scheduler: failed to read '{}': {error}",
                            path.display()
                        ));
                    }
                }
            }
        }

        ProbeEvidence {
            observed_at_ns: now_ns(),
            data: json!({
                "schema_version": 1,
                "sections": sections,
            }),
            warnings,
        }
    }
}

impl Default for LinuxProcSnapshotProbe {
    fn default() -> Self {
        Self::new()
    }
}

impl ProbeProvider for LinuxProcSnapshotProbe {
    fn meta(&self) -> &CapabilityMeta {
        &self.meta
    }

    fn probe(&self, request: &ProbeRequest) -> Result<ProbeEvidence, ProviderError> {
        let sections = parse_arguments(&request.arguments)?;
        Ok(self.collect(&sections))
    }
}

fn parse_arguments(arguments: &Value) -> Result<BTreeSet<Section>, ProviderError> {
    let object = arguments.as_object().ok_or_else(|| {
        ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            "linux proc snapshot arguments must be an object",
        )
    })?;
    if let Some(key) = object.keys().find(|key| key.as_str() != "sections") {
        return Err(ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            format!("unsupported linux proc snapshot argument '{key}'"),
        ));
    }
    let Some(raw_sections) = object.get("sections") else {
        return Ok(Section::ALL.into_iter().collect());
    };
    let raw_sections = raw_sections.as_array().ok_or_else(|| {
        ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            "linux proc snapshot sections must be an array",
        )
    })?;
    if raw_sections.is_empty() || raw_sections.len() > Section::ALL.len() {
        return Err(ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            "linux proc snapshot requires between 1 and 5 unique sections",
        ));
    }
    let mut sections = BTreeSet::new();
    for value in raw_sections {
        let value = value.as_str().ok_or_else(|| {
            ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "linux proc snapshot section names must be strings",
            )
        })?;
        let section = Section::parse(value).ok_or_else(|| {
            ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                format!("unsupported linux proc snapshot section '{value}'"),
            )
        })?;
        if !sections.insert(section) {
            return Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                format!("duplicate linux proc snapshot section '{value}'"),
            ));
        }
    }
    Ok(sections)
}

fn collect_section(
    sections: &mut Map<String, Value>,
    warnings: &mut Vec<String>,
    name: &str,
    path: PathBuf,
    parse: fn(&str) -> Result<Value, String>,
) {
    match read_and_parse(&path, parse) {
        Ok(value) => {
            sections.insert(name.to_string(), value);
        }
        Err(error) => warnings.push(format!("{name}: {error}")),
    }
}

fn read_and_parse(path: &Path, parse: fn(&str) -> Result<Value, String>) -> Result<Value, String> {
    let content = fs::read_to_string(path)
        .map_err(|error| format!("failed to read '{}': {error}", path.display()))?;
    parse(&content)
}

fn parse_loadavg(content: &str) -> Result<Value, String> {
    let fields = content.split_whitespace().collect::<Vec<_>>();
    if fields.len() < 5 {
        return Err("loadavg has fewer than five fields".to_string());
    }
    let one = parse_f64(fields[0], "loadavg 1m")?;
    let five = parse_f64(fields[1], "loadavg 5m")?;
    let fifteen = parse_f64(fields[2], "loadavg 15m")?;
    let (runnable, entities) = fields[3]
        .split_once('/')
        .ok_or_else(|| "loadavg runnable field is malformed".to_string())?;
    Ok(json!({
        "one_minute": one,
        "five_minutes": five,
        "fifteen_minutes": fifteen,
        "runnable_entities": parse_u64(runnable, "loadavg runnable entities")?,
        "total_entities": parse_u64(entities, "loadavg total entities")?,
        "last_pid": parse_u64(fields[4], "loadavg last pid")?,
    }))
}

fn parse_meminfo_summary(content: &str) -> Result<Value, String> {
    const FIELDS: [(&str, &str); 10] = [
        ("MemTotal", "total_bytes"),
        ("MemFree", "free_bytes"),
        ("MemAvailable", "available_bytes"),
        ("Buffers", "buffers_bytes"),
        ("Cached", "cached_bytes"),
        ("Active", "active_bytes"),
        ("Inactive", "inactive_bytes"),
        ("SwapTotal", "swap_total_bytes"),
        ("SwapFree", "swap_free_bytes"),
        ("SReclaimable", "slab_reclaimable_bytes"),
    ];
    let mut summary = Map::new();
    for line in content.lines() {
        let Some((name, raw)) = line.split_once(':') else {
            continue;
        };
        let Some((_, output_name)) = FIELDS.iter().find(|(field, _)| *field == name) else {
            continue;
        };
        let mut parts = raw.split_whitespace();
        let kib = parts
            .next()
            .ok_or_else(|| format!("meminfo field '{name}' has no value"))
            .and_then(|value| parse_u64(value, name))?;
        if parts.next() != Some("kB") {
            return Err(format!("meminfo field '{name}' is not expressed in kB"));
        }
        let bytes = kib
            .checked_mul(1024)
            .ok_or_else(|| format!("meminfo field '{name}' overflows bytes"))?;
        summary.insert((*output_name).to_string(), json!(bytes));
    }
    for required in ["total_bytes", "available_bytes"] {
        if !summary.contains_key(required) {
            return Err(format!("meminfo is missing required field '{required}'"));
        }
    }
    let total = summary["total_bytes"]
        .as_u64()
        .expect("inserted memory total is numeric");
    let available = summary["available_bytes"]
        .as_u64()
        .expect("inserted available memory is numeric");
    summary.insert(
        "used_estimate_bytes".to_string(),
        json!(total.saturating_sub(available)),
    );
    Ok(Value::Object(summary))
}

fn parse_psi(content: &str) -> Result<Value, String> {
    let mut rows = Map::new();
    for line in content.lines() {
        let mut fields = line.split_whitespace();
        let Some(row) = fields.next() else {
            continue;
        };
        if row != "some" && row != "full" {
            continue;
        }
        let mut values = Map::new();
        for field in fields {
            let Some((name, value)) = field.split_once('=') else {
                continue;
            };
            match name {
                "avg10" | "avg60" | "avg300" => {
                    values.insert(name.to_string(), json!(parse_f64(value, name)?));
                }
                "total" => {
                    values.insert(
                        "total_microseconds".to_string(),
                        json!(parse_u64(value, name)?),
                    );
                }
                _ => {}
            }
        }
        for required in ["avg10", "avg60", "avg300", "total_microseconds"] {
            if !values.contains_key(required) {
                return Err(format!("PSI row '{row}' is missing '{required}'"));
            }
        }
        rows.insert(row.to_string(), Value::Object(values));
    }
    if rows.is_empty() {
        return Err("PSI file contains no some/full row".to_string());
    }
    Ok(Value::Object(rows))
}

fn parse_cpu(content: &str) -> Result<Value, String> {
    let aggregate = content
        .lines()
        .find(|line| line.starts_with("cpu "))
        .ok_or_else(|| "proc stat is missing aggregate cpu row".to_string())?;
    let fields = aggregate.split_whitespace().skip(1).collect::<Vec<_>>();
    if fields.len() < 8 {
        return Err("aggregate cpu row has fewer than eight counters".to_string());
    }
    let names = [
        "user",
        "nice",
        "system",
        "idle",
        "iowait",
        "irq",
        "softirq",
        "steal",
        "guest",
        "guest_nice",
    ];
    let mut ticks = Map::new();
    for (name, value) in names.iter().zip(fields.iter()) {
        ticks.insert((*name).to_string(), json!(parse_u64(value, name)?));
    }
    let logical_cpus = content
        .lines()
        .filter(|line| {
            line.strip_prefix("cpu").is_some_and(|suffix| {
                suffix
                    .as_bytes()
                    .first()
                    .is_some_and(|byte| byte.is_ascii_digit())
            })
        })
        .count();
    Ok(json!({
        "logical_cpus": logical_cpus,
        "time_unit": "USER_HZ_ticks",
        "aggregate_time_ticks": ticks,
    }))
}

fn parse_scheduler(content: &str) -> Result<Value, String> {
    let mut scheduler = Map::new();
    for line in content.lines() {
        let mut fields = line.split_whitespace();
        let Some(name) = fields.next() else {
            continue;
        };
        let output_name = match name {
            "ctxt" => "context_switches",
            "processes" => "processes_forked",
            "procs_running" => "runnable_tasks",
            "procs_blocked" => "blocked_tasks",
            _ => continue,
        };
        let value = fields
            .next()
            .ok_or_else(|| format!("proc stat field '{name}' has no value"))?;
        scheduler.insert(output_name.to_string(), json!(parse_u64(value, name)?));
    }
    for required in ["context_switches", "runnable_tasks", "blocked_tasks"] {
        if !scheduler.contains_key(required) {
            return Err(format!("proc stat is missing scheduler field '{required}'"));
        }
    }
    Ok(Value::Object(scheduler))
}

fn parse_f64(value: &str, field: &str) -> Result<f64, String> {
    value
        .parse::<f64>()
        .ok()
        .filter(|number| number.is_finite())
        .ok_or_else(|| format!("field '{field}' is not a finite number"))
}

fn parse_u64(value: &str, field: &str) -> Result<u64, String> {
    value
        .parse::<u64>()
        .map_err(|_| format!("field '{field}' is not an unsigned integer"))
}

fn now_ns() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::domain::{EpisodeId, InvocationContext, OperationId};

    fn fixture() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tuning-agent-proc-probe-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(root.join("pressure")).unwrap();
        fs::write(root.join("loadavg"), "1.25 1.00 0.75 2/100 4242\n").unwrap();
        fs::write(
            root.join("meminfo"),
            "MemTotal:       1000 kB\nMemFree:         100 kB\nMemAvailable:    600 kB\nBuffers:          10 kB\nCached:          200 kB\nActive:          300 kB\nInactive:        100 kB\nSwapTotal:       500 kB\nSwapFree:        400 kB\nSReclaimable:     20 kB\n",
        )
        .unwrap();
        let psi = "some avg10=2.50 avg60=1.00 avg300=0.50 total=10\nfull avg10=0.25 avg60=0.10 avg300=0.05 total=2\n";
        fs::write(root.join("pressure/cpu"), psi).unwrap();
        fs::write(root.join("pressure/io"), psi).unwrap();
        fs::write(root.join("pressure/memory"), psi).unwrap();
        fs::write(
            root.join("stat"),
            "cpu  100 2 30 400 5 6 7 8 0 0\ncpu0 50 1 15 200 2 3 4 4 0 0\ncpu1 50 1 15 200 3 3 3 4 0 0\nctxt 12345\nprocesses 678\nprocs_running 3\nprocs_blocked 1\n",
        )
        .unwrap();
        root
    }

    fn request(arguments: Value) -> ProbeRequest {
        ProbeRequest {
            context: InvocationContext {
                episode_id: EpisodeId::new(1),
                operation_id: OperationId::new("probe-1").unwrap(),
            },
            arguments,
        }
    }

    #[test]
    fn collects_bounded_structured_proc_snapshot() {
        let root = fixture();
        let provider = LinuxProcSnapshotProbe::with_proc_root(&root);

        let evidence = provider.probe(&request(json!({}))).unwrap();

        assert!(evidence.warnings.is_empty(), "{:?}", evidence.warnings);
        assert_eq!(evidence.data["sections"]["loadavg"]["one_minute"], 1.25);
        assert_eq!(
            evidence.data["sections"]["memory"]["total_bytes"],
            1_024_000
        );
        assert_eq!(
            evidence.data["sections"]["pressure"]["io"]["full"]["avg10"],
            0.25
        );
        assert_eq!(evidence.data["sections"]["cpu"]["logical_cpus"], 2);
        assert_eq!(
            evidence.data["sections"]["scheduler"]["context_switches"],
            12_345
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn arguments_cannot_select_paths_or_commands() {
        let provider = LinuxProcSnapshotProbe::with_proc_root(fixture());

        for arguments in [
            json!({"path": "/etc/shadow"}),
            json!({"command": "cat /etc/shadow"}),
            json!({"sections": ["loadavg", "unknown"]}),
        ] {
            let error = provider.probe(&request(arguments)).unwrap_err();
            assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
        }
        let root = provider.proc_root.clone();
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn section_filter_does_not_read_unrequested_files() {
        let root = std::env::temp_dir().join(format!(
            "tuning-agent-proc-probe-filter-{}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("loadavg"), "0.10 0.20 0.30 1/10 7\n").unwrap();
        let provider = LinuxProcSnapshotProbe::with_proc_root(&root);

        let evidence = provider
            .probe(&request(json!({"sections": ["loadavg"]})))
            .unwrap();

        assert!(evidence.warnings.is_empty());
        assert_eq!(
            evidence.data["sections"]
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            vec!["loadavg"]
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn metadata_allows_only_agent_controlled_phases() {
        let provider = LinuxProcSnapshotProbe::new();

        assert_eq!(
            provider.meta().allowed_phases,
            vec![EpisodePhase::Clean, EpisodePhase::Experimenting]
        );
        assert!(!provider.meta().is_allowed_in(EpisodePhase::CommitPending));
    }
}
