use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::control_wire::RuleState;

use super::schema::PROVIDER_VERSION;
use super::{sha256_hex, validate_comm, validate_id};

const MAX_JOURNAL_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MutationState {
    pub value: RuleState,
    pub digest: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MutationDriverData {
    pub schema_version: u32,
    pub comm: String,
    pub baseline: RuleState,
    pub desired: RuleState,
}

impl MutationDriverData {
    fn validate(&self) -> Result<()> {
        if self.schema_version != 1 {
            bail!("unsupported mutation driver_data schema_version");
        }
        validate_comm(&self.comm)?;
        if !self.baseline.is_valid() || !self.desired.is_valid() {
            bail!("mutation driver_data contains an invalid rule state");
        }
        if !self.desired.present {
            bail!("upsert desired state must be present");
        }
        if self.baseline == self.desired {
            bail!("mutation baseline and desired states are identical");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedMutation {
    pub capability_id: String,
    pub provider: ProviderPin,
    pub resource: String,
    pub baseline: MutationState,
    pub desired: MutationState,
    pub driver_data: MutationDriverData,
}

impl PreparedMutation {
    pub fn validate(&self) -> Result<()> {
        self.driver_data.validate()?;
        validate_id("capability_id", &self.capability_id)?;
        validate_id("resource", &self.resource)?;
        validate_id("baseline digest", &self.baseline.digest)?;
        validate_id("desired digest", &self.desired.digest)?;
        self.provider.validate()?;
        if !self.capability_id.ends_with("/rule.upsert.v1") {
            bail!("prepared capability_id is not rule.upsert.v1");
        }
        validate_state_digest(&self.baseline)?;
        validate_state_digest(&self.desired)?;
        if self.baseline.value != self.driver_data.baseline
            || self.desired.value != self.driver_data.desired
        {
            bail!("prepared states do not match mutation driver_data");
        }
        let expected_resource = format!(
            "learned-rule/{}",
            sha256_hex(self.driver_data.comm.as_bytes())
        );
        if self.resource != expected_resource
            && !self.resource.ends_with(&format!("/{expected_resource}"))
        {
            bail!("prepared resource does not match comm");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderPin {
    pub provider_id: String,
    pub provider_version: String,
    pub provider_class: String,
    pub manifest_digest: String,
}

impl ProviderPin {
    fn validate(&self) -> Result<()> {
        validate_id("provider_id", &self.provider_id)?;
        validate_id("provider_version", &self.provider_version)?;
        validate_id("manifest_digest", &self.manifest_digest)?;
        if self.provider_class != "mcp" || self.provider_version != PROVIDER_VERSION {
            bail!("prepared mutation provider pin does not match this MCP provider");
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MutationOperationRequest {
    pub operation_id: String,
    pub prepared: PreparedMutation,
}

impl MutationOperationRequest {
    pub fn validate(&self) -> Result<()> {
        validate_id("operation_id", &self.operation_id)?;
        self.prepared.validate()
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MutationStatusRequest {
    pub operation_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MutationVerifyRequest {
    pub operation_id: String,
    pub prepared: PreparedMutation,
    pub expected: MutationState,
}

impl MutationVerifyRequest {
    pub fn validate(&self) -> Result<()> {
        validate_id("operation_id", &self.operation_id)?;
        self.prepared.validate()?;
        if !self.expected.value.is_valid() {
            bail!("verify expected state is invalid");
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalKind {
    Apply,
    Restore,
    Finalize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum JournalPhase {
    Started,
    Completed,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationRecord {
    kind: JournalKind,
    phase: JournalPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    result_state: Option<String>,
    pub comm: String,
    pub baseline: RuleState,
    pub desired: RuleState,
    prepared: PreparedMutation,
}

impl OperationRecord {
    pub fn new(
        kind: JournalKind,
        comm: String,
        baseline: RuleState,
        desired: RuleState,
        prepared: PreparedMutation,
    ) -> Self {
        Self {
            kind,
            phase: JournalPhase::Started,
            result_state: None,
            comm,
            baseline,
            desired,
            prepared,
        }
    }

    pub fn completed_as(&self, state: &str) -> bool {
        self.phase == JournalPhase::Completed && self.result_state.as_deref() == Some(state)
    }

    pub fn infer_state(&self, observed: &RuleState, integrity_matches: bool) -> &'static str {
        let baseline = observed == &self.baseline && integrity_matches;
        let desired = observed == &self.desired && integrity_matches;
        match self.kind {
            JournalKind::Apply if desired => "applied",
            JournalKind::Apply if baseline => "not_applied",
            JournalKind::Restore if baseline => "restored",
            JournalKind::Restore if desired => "applied",
            JournalKind::Finalize if desired => "finalized",
            _ => "unknown",
        }
    }

    pub fn is_completion(&self, state: &str) -> bool {
        matches!(
            (self.kind, state),
            (JournalKind::Apply, "applied")
                | (JournalKind::Restore, "restored")
                | (JournalKind::Finalize, "finalized")
        )
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct JournalData {
    version: u32,
    operations: BTreeMap<String, OperationRecord>,
}

impl Default for JournalData {
    fn default() -> Self {
        Self {
            version: 1,
            operations: BTreeMap::new(),
        }
    }
}

pub struct Journal {
    path: PathBuf,
}

impl Journal {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn get(&self, operation_id: &str) -> Result<Option<OperationRecord>> {
        Ok(self.load()?.operations.get(operation_id).cloned())
    }

    pub fn begin(&self, operation_id: &str, record: OperationRecord) -> Result<OperationRecord> {
        validate_id("operation_id", operation_id)?;
        let mut data = self.load()?;
        if let Some(existing) = data.operations.get(operation_id) {
            let mut expected = record;
            expected.phase = existing.phase;
            expected.result_state = existing.result_state.clone();
            if existing != &expected {
                bail!("operation_id '{operation_id}' was reused with different input");
            }
            return Ok(existing.clone());
        }
        data.operations
            .insert(operation_id.to_string(), record.clone());
        self.save(&data)?;
        Ok(record)
    }

    pub fn complete(&self, operation_id: &str, result_state: &str) -> Result<()> {
        let mut data = self.load()?;
        let record = data
            .operations
            .get_mut(operation_id)
            .ok_or_else(|| anyhow!("operation_id '{operation_id}' is not journaled"))?;
        if record.phase == JournalPhase::Completed {
            if record.result_state.as_deref() != Some(result_state) {
                bail!("operation_id '{operation_id}' completed with a different state");
            }
            return Ok(());
        }
        record.phase = JournalPhase::Completed;
        record.result_state = Some(result_state.to_string());
        self.save(&data)
    }

    fn load(&self) -> Result<JournalData> {
        if !self.path.exists() {
            return Ok(JournalData::default());
        }
        let file = File::open(&self.path)
            .with_context(|| format!("failed to open journal '{}'", self.path.display()))?;
        if file.metadata()?.len() > MAX_JOURNAL_BYTES {
            bail!("operation journal exceeds {MAX_JOURNAL_BYTES} bytes");
        }
        let data: JournalData = serde_json::from_reader(file)
            .with_context(|| format!("invalid operation journal '{}'", self.path.display()))?;
        if data.version != 1 {
            bail!("unsupported operation journal version {}", data.version);
        }
        for (operation_id, record) in &data.operations {
            validate_id("journal operation_id", operation_id)?;
            record.prepared.validate()?;
            if record.comm != record.prepared.driver_data.comm
                || record.baseline != record.prepared.driver_data.baseline
                || record.desired != record.prepared.driver_data.desired
            {
                bail!("operation journal record '{operation_id}' is inconsistent");
            }
        }
        Ok(data)
    }

    fn save(&self, data: &JournalData) -> Result<()> {
        let parent = self
            .path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        fs::create_dir_all(parent).with_context(|| {
            format!("failed to create journal directory '{}'", parent.display())
        })?;
        let payload = serde_json::to_vec(data)?;
        if payload.len() as u64 > MAX_JOURNAL_BYTES {
            bail!("operation journal exceeds {MAX_JOURNAL_BYTES} bytes");
        }
        let temporary =
            self.path
                .with_extension(format!("tmp.{}.{}", std::process::id(), now_ns()?));
        let result = (|| -> Result<()> {
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(&temporary)
                .with_context(|| {
                    format!(
                        "failed to create journal temporary '{}'",
                        temporary.display()
                    )
                })?;
            file.write_all(&payload)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            drop(file);
            fs::rename(&temporary, &self.path).with_context(|| {
                format!(
                    "failed to replace journal '{}' with '{}'",
                    self.path.display(),
                    temporary.display()
                )
            })?;
            File::open(parent)?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
}

fn validate_state_digest(state: &MutationState) -> Result<()> {
    // tuning-agent digests the JSON Value returned by prepare. Convert back to
    // Value here so object keys use the same canonical ordering.
    let encoded = serde_json::to_vec(&serde_json::to_value(&state.value)?)?;
    let expected = format!("sha256:{}", sha256_hex(&encoded));
    if state.digest != expected {
        bail!("mutation state digest does not match its value");
    }
    Ok(())
}

fn now_ns() -> Result<u128> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_nanos())
}
