// SPDX-License-Identifier: GPL-2.0

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use super::{
    CasResult, CasStatus, Comm, EffectiveRule, RuleClass, RuleSet, RuleSource, RuleState,
    RuleTable, RULES_SCHEMA_VERSION,
};
#[derive(Debug)]
pub struct RuleStore {
    path: PathBuf,
    rules: RuleSet,
    persistence_uncertain: bool,
}

impl RuleStore {
    pub fn open(path: impl Into<PathBuf>, base: RuleTable) -> Result<Self> {
        let path = path.into();
        ensure_parent_directory(&path)?;
        let (learned, revision) = load_document(&path)?;
        let rules = RuleSet::new(base, learned, revision)
            .with_context(|| format!("validating learned rules from {}", path.display()))?;
        Ok(Self {
            path,
            rules,
            persistence_uncertain: false,
        })
    }

    #[cfg(test)]
    fn path(&self) -> &Path {
        &self.path
    }

    pub fn revision(&self) -> u64 {
        self.rules.revision()
    }

    pub fn base(&self) -> &RuleTable {
        self.rules.base()
    }

    pub fn learned(&self) -> &RuleTable {
        self.rules.learned()
    }

    pub fn effective(&self) -> &RuleTable {
        self.rules.effective()
    }

    pub fn learned_state(&self, comm: &Comm) -> RuleState {
        self.rules.learned_state(comm)
    }

    pub fn effective_rule(&self, comm: &Comm) -> Option<EffectiveRule> {
        self.rules.effective_rule(comm)
    }

    pub fn read_persisted(&self) -> Result<(RuleTable, u64)> {
        load_document(&self.path)
    }

    pub fn persistence_uncertain(&self) -> bool {
        self.persistence_uncertain
    }

    pub fn compare_and_set(
        &mut self,
        comm: Comm,
        expected: RuleState,
        desired: RuleState,
    ) -> Result<CasResult> {
        if self.rules.base.contains_key(&comm) {
            bail!("comm '{comm}' is owned by a read-only base rule");
        }

        let previous = self.rules.learned_state(&comm);
        if previous != expected {
            return Ok(self.cas_result(CasStatus::Conflict, comm, previous, previous));
        }
        if previous == desired {
            return Ok(self.cas_result(CasStatus::Noop, comm, previous, desired));
        }

        let revision = self
            .rules
            .revision
            .checked_add(1)
            .context("learned rule revision overflow")?;
        let mut learned = self.rules.learned.clone();
        match desired {
            RuleState::Absent => {
                learned.remove(&comm);
            }
            RuleState::Present(class) => {
                learned.insert(comm.clone(), class);
            }
        }
        let next = RuleSet::new(self.rules.base.clone(), learned, revision)?;
        let encoded = next.canonical_learned_json()?;
        match atomic_replace(&self.path, &encoded)? {
            ReplaceOutcome::Durable => {}
            ReplaceOutcome::RenamedButUnsynced(error) => {
                self.rules = next;
                self.persistence_uncertain = true;
                return Err(error);
            }
        }
        self.rules = next;

        Ok(self.cas_result(CasStatus::Applied, comm, previous, desired))
    }

    fn cas_result(
        &self,
        status: CasStatus,
        comm: Comm,
        previous: RuleState,
        current: RuleState,
    ) -> CasResult {
        let effective = RuleState::from_option(self.rules.effective.get(&comm).copied());
        CasResult {
            status,
            comm,
            previous,
            current,
            effective,
            revision: self.rules.revision,
        }
    }
}

fn ensure_parent_directory(path: &Path) -> Result<()> {
    path.file_name()
        .with_context(|| format!("learned rules path '{}' has no file name", path.display()))?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .with_context(|| format!("creating learned rules directory {}", parent.display()))?;
    if !fs::metadata(parent)
        .with_context(|| format!("inspecting learned rules directory {}", parent.display()))?
        .is_dir()
    {
        bail!(
            "learned rules parent '{}' is not a directory",
            parent.display()
        );
    }
    Ok(())
}

pub(super) fn effective_rule(
    base: &RuleTable,
    learned: &RuleTable,
    comm: &Comm,
) -> Option<EffectiveRule> {
    base.get(comm)
        .copied()
        .map(|class| EffectiveRule {
            class,
            source: RuleSource::Base,
        })
        .or_else(|| {
            learned.get(comm).copied().map(|class| EffectiveRule {
                class,
                source: RuleSource::Learned,
            })
        })
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredDocument {
    schema_version: u32,
    revision: u64,
    rules: Vec<StoredRule>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredRule {
    comm: Comm,
    class: RuleClass,
}

fn load_document(path: &Path) -> Result<(RuleTable, u64)> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((RuleTable::new(), 0));
        }
        Err(error) => {
            return Err(error).with_context(|| format!("reading learned rules {}", path.display()));
        }
    };
    let document: StoredDocument = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing learned rules {}", path.display()))?;
    if document.schema_version != RULES_SCHEMA_VERSION {
        bail!(
            "unsupported learned rules schema_version {} in {}; expected {RULES_SCHEMA_VERSION}",
            document.schema_version,
            path.display()
        );
    }

    let mut learned = RuleTable::new();
    for rule in document.rules {
        if learned.insert(rule.comm.clone(), rule.class).is_some() {
            bail!(
                "duplicate learned rule for comm '{}' in {}",
                rule.comm,
                path.display()
            );
        }
    }
    Ok((learned, document.revision))
}

pub(super) fn canonical_document(revision: u64, learned: &RuleTable) -> Result<Vec<u8>> {
    let document = StoredDocument {
        schema_version: RULES_SCHEMA_VERSION,
        revision,
        rules: learned
            .iter()
            .map(|(comm, class)| StoredRule {
                comm: comm.clone(),
                class: *class,
            })
            .collect(),
    };
    let mut bytes = serde_json::to_vec_pretty(&document).context("serializing learned rules")?;
    bytes.push(b'\n');
    Ok(bytes)
}

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

enum ReplaceOutcome {
    Durable,
    RenamedButUnsynced(anyhow::Error),
}

fn atomic_replace(path: &Path, contents: &[u8]) -> Result<ReplaceOutcome> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .with_context(|| format!("learned rules path '{}' has no file name", path.display()))?;
    let (mut file, temp_path) = create_temp_file(parent, file_name)?;

    let replace_result = (|| -> Result<()> {
        file.write_all(contents)
            .with_context(|| format!("writing temporary rules file {}", temp_path.display()))?;
        file.sync_all()
            .with_context(|| format!("syncing temporary rules file {}", temp_path.display()))?;
        drop(file);

        fs::rename(&temp_path, path).with_context(|| {
            format!(
                "replacing learned rules {} with {}",
                path.display(),
                temp_path.display()
            )
        })
    })();

    if let Err(error) = replace_result {
        let _ = fs::remove_file(&temp_path);
        return Err(error);
    }
    match File::open(parent).and_then(|directory| directory.sync_all()) {
        Ok(()) => Ok(ReplaceOutcome::Durable),
        Err(error) => Ok(ReplaceOutcome::RenamedButUnsynced(
            anyhow::Error::new(error).context(format!(
                "syncing learned rules directory {}",
                parent.display()
            )),
        )),
    }
}

fn create_temp_file(parent: &Path, target_name: &std::ffi::OsStr) -> Result<(File, PathBuf)> {
    for _ in 0..32 {
        let mut temp_name = OsString::from(".");
        temp_name.push(target_name);
        temp_name.push(format!(
            ".{}.{}.tmp",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let temp_path = parent.join(temp_name);

        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&temp_path) {
            Ok(file) => return Ok((file, temp_path)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("creating temporary rules file {}", temp_path.display())
                });
            }
        }
    }
    bail!(
        "could not allocate a temporary rules file in {}",
        parent.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "scx-agent-classed-rules-{label}-{}-{nonce}-{}",
                std::process::id(),
                TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn join(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn comm(value: &str) -> Comm {
        Comm::new(value).unwrap()
    }

    fn table(entries: &[(&str, RuleClass)]) -> RuleTable {
        entries
            .iter()
            .map(|(name, class)| (comm(name), *class))
            .collect()
    }

    #[test]
    fn validates_comm_by_utf8_byte_length() {
        assert!(Comm::new("").is_err());
        assert!(Comm::new("123456789012345").is_ok());
        assert!(Comm::new("1234567890123456").is_err());
        assert!(Comm::new("123456789012中").is_ok());
        assert!(Comm::new("1234567890123中").is_err());
        assert!(Comm::new("worker\0child").is_err());
    }

    #[test]
    fn pads_bpf_comm_key() {
        let key = comm("worker").as_bpf_key();
        assert_eq!(&key[..6], b"worker");
        assert!(key[6..].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn rule_state_has_strict_wire_format() {
        assert_eq!(
            serde_json::to_string(&RuleState::Absent).unwrap(),
            r#"{"present":false}"#
        );
        assert_eq!(
            serde_json::to_string(&RuleState::Present(RuleClass::Latency)).unwrap(),
            r#"{"present":true,"class":"latency"}"#
        );
        assert_eq!(
            serde_json::from_str::<RuleState>(r#"{"present":false}"#).unwrap(),
            RuleState::Absent
        );
        assert!(serde_json::from_str::<RuleState>(r#"{"present":true}"#).is_err());
        assert!(serde_json::from_str::<RuleState>(r#"{"present":false,"class":"batch"}"#).is_err());
        assert!(serde_json::from_str::<RuleState>(r#"{"present":false,"extra":1}"#).is_err());
    }

    #[test]
    fn converts_shared_wire_state_only_when_valid() {
        let domain = RuleState::Present(RuleClass::Batch);
        let wire: crate::control_wire::RuleState = domain.into();
        assert_eq!(RuleState::try_from(wire).unwrap(), domain);

        let invalid = crate::control_wire::RuleState {
            present: false,
            class: Some(crate::control_wire::RuleClass::Latency),
        };
        assert!(RuleState::try_from(invalid).is_err());
    }

    #[test]
    fn rejects_unknown_class_during_deserialization() {
        assert!(serde_json::from_str::<RuleClass>(r#""interactive""#).is_err());
    }

    #[test]
    fn merges_disjoint_layers_and_reports_sources() {
        let base = table(&[("pipewire", RuleClass::Latency)]);
        let learned = table(&[("rustc", RuleClass::Batch)]);
        let rules = RuleSet::new(base, learned, 7).unwrap();

        assert_eq!(rules.revision(), 7);
        assert_eq!(rules.effective().len(), 2);
        assert_eq!(
            rules.effective_rule(&comm("pipewire")),
            Some(EffectiveRule {
                class: RuleClass::Latency,
                source: RuleSource::Base,
            })
        );
        assert_eq!(
            rules.effective_rule(&comm("rustc")),
            Some(EffectiveRule {
                class: RuleClass::Batch,
                source: RuleSource::Learned,
            })
        );
    }

    #[test]
    fn rejects_base_and_learned_conflict() {
        let base = table(&[("worker", RuleClass::Latency)]);
        let learned = table(&[("worker", RuleClass::Latency)]);
        let error = RuleSet::new(base, learned, 0).unwrap_err();

        assert!(error.to_string().contains("read-only base rule"));
    }

    #[test]
    fn canonical_json_has_stable_field_and_rule_order() {
        let learned = table(&[
            ("z-worker", RuleClass::Batch),
            ("a-worker", RuleClass::Latency),
        ]);
        let rules = RuleSet::new(RuleTable::new(), learned, 9).unwrap();
        let encoded = String::from_utf8(rules.canonical_learned_json().unwrap()).unwrap();

        assert_eq!(
            encoded,
            "{\n  \"schema_version\": 1,\n  \"revision\": 9,\n  \"rules\": [\n    {\n      \"comm\": \"a-worker\",\n      \"class\": \"latency\"\n    },\n    {\n      \"comm\": \"z-worker\",\n      \"class\": \"batch\"\n    }\n  ]\n}\n"
        );
    }

    #[test]
    fn missing_file_starts_at_revision_zero() {
        let directory = TestDir::new("missing");
        let store = RuleStore::open(directory.join("learned.json"), RuleTable::new()).unwrap();

        assert_eq!(store.revision(), 0);
        assert!(store.learned().is_empty());
        assert!(!store.path().exists());
    }

    #[test]
    fn creates_missing_parent_directories() {
        let directory = TestDir::new("parents");
        let path = directory.join("nested/state/learned.json");

        let store = RuleStore::open(&path, RuleTable::new()).unwrap();

        assert_eq!(store.path(), path);
        assert!(directory.join("nested/state").is_dir());
    }

    #[test]
    fn rejects_invalid_parent_path() {
        let directory = TestDir::new("invalid-parent");
        let parent = directory.join("not-a-directory");
        fs::write(&parent, b"data").unwrap();

        assert!(RuleStore::open(parent.join("learned.json"), RuleTable::new()).is_err());
    }

    #[test]
    fn applied_cas_persists_and_revision_is_monotonic() {
        let directory = TestDir::new("cas");
        let path = directory.join("learned.json");
        let worker = comm("worker");
        let mut store = RuleStore::open(&path, RuleTable::new()).unwrap();

        let applied = store
            .compare_and_set(
                worker.clone(),
                RuleState::Absent,
                RuleState::Present(RuleClass::Latency),
            )
            .unwrap();
        assert_eq!(applied.status, CasStatus::Applied);
        assert_eq!(applied.previous, RuleState::Absent);
        assert_eq!(applied.current, RuleState::Present(RuleClass::Latency));
        assert_eq!(applied.effective, applied.current);
        assert_eq!(applied.revision, 1);

        let restored = store
            .compare_and_set(
                worker.clone(),
                RuleState::Present(RuleClass::Latency),
                RuleState::Absent,
            )
            .unwrap();
        assert_eq!(restored.status, CasStatus::Applied);
        assert_eq!(restored.revision, 2);

        let reopened = RuleStore::open(&path, RuleTable::new()).unwrap();
        assert_eq!(reopened.revision(), 2);
        assert_eq!(reopened.learned_state(&worker), RuleState::Absent);
    }

    #[test]
    fn noop_and_conflict_do_not_write_or_advance_revision() {
        let directory = TestDir::new("no-write");
        let path = directory.join("learned.json");
        let worker = comm("worker");
        let mut store = RuleStore::open(&path, RuleTable::new()).unwrap();

        let noop = store
            .compare_and_set(worker.clone(), RuleState::Absent, RuleState::Absent)
            .unwrap();
        assert_eq!(noop.status, CasStatus::Noop);
        assert_eq!(noop.revision, 0);
        assert!(!path.exists());

        let conflict = store
            .compare_and_set(
                worker,
                RuleState::Present(RuleClass::Batch),
                RuleState::Present(RuleClass::Latency),
            )
            .unwrap();
        assert_eq!(conflict.status, CasStatus::Conflict);
        assert_eq!(conflict.revision, 0);
        assert!(!path.exists());
    }

    #[test]
    fn mutations_cannot_target_base_rules() {
        let directory = TestDir::new("base-mutation");
        let path = directory.join("learned.json");
        let worker = comm("worker");
        let mut store = RuleStore::open(path, table(&[("worker", RuleClass::Latency)])).unwrap();

        let error = store
            .compare_and_set(
                worker,
                RuleState::Absent,
                RuleState::Present(RuleClass::Batch),
            )
            .unwrap_err();
        assert!(error.to_string().contains("read-only base rule"));
    }

    #[test]
    fn load_rejects_duplicate_invalid_and_conflicting_rules() {
        let directory = TestDir::new("invalid-load");
        let path = directory.join("learned.json");
        fs::write(
            &path,
            r#"{"schema_version":1,"revision":3,"rules":[{"comm":"worker","class":"batch"},{"comm":"worker","class":"latency"}]}"#,
        )
        .unwrap();
        assert!(RuleStore::open(&path, RuleTable::new())
            .unwrap_err()
            .to_string()
            .contains("duplicate"));

        fs::write(
            &path,
            r#"{"schema_version":1,"revision":3,"rules":[{"comm":"worker","class":"interactive"}]}"#,
        )
        .unwrap();
        assert!(RuleStore::open(&path, RuleTable::new()).is_err());

        fs::write(
            &path,
            r#"{"schema_version":1,"revision":3,"rules":[{"comm":"worker","class":"batch"}]}"#,
        )
        .unwrap();
        assert!(RuleStore::open(&path, table(&[("worker", RuleClass::Latency)])).is_err());
    }

    #[test]
    fn load_rejects_unknown_schema_and_fields() {
        let directory = TestDir::new("schema");
        let path = directory.join("learned.json");
        fs::write(&path, r#"{"schema_version":2,"revision":0,"rules":[]}"#).unwrap();
        assert!(RuleStore::open(&path, RuleTable::new())
            .unwrap_err()
            .to_string()
            .contains("schema_version"));

        fs::write(
            &path,
            r#"{"schema_version":1,"revision":0,"rules":[],"extra":true}"#,
        )
        .unwrap();
        assert!(RuleStore::open(&path, RuleTable::new()).is_err());
    }

    #[test]
    fn atomic_write_leaves_only_the_target_file() {
        let directory = TestDir::new("atomic");
        let path = directory.join("learned.json");
        let mut store = RuleStore::open(&path, RuleTable::new()).unwrap();
        store
            .compare_and_set(
                comm("worker"),
                RuleState::Absent,
                RuleState::Present(RuleClass::Batch),
            )
            .unwrap();

        let entries: Vec<_> = fs::read_dir(&directory.0)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(entries, vec![OsString::from("learned.json")]);
        assert_eq!(
            RuleStore::open(&path, RuleTable::new()).unwrap().revision(),
            1
        );
    }
}
