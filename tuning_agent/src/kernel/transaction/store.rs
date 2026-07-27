use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::domain::{
    content_digest, ChangeId, CommitAuthorization, EvaluationIntentPin, ResourceKey, TransactionId,
};
use crate::kernel::transaction::wal::load_file;
use crate::kernel::transaction::{
    ChangeState, FileWal, TransactionSeal, WalEntry, WalError, WalEvent,
    MAX_CHANGES_PER_TRANSACTION,
};

const FILE_PREFIX: &str = "tx-";
const FILE_SUFFIX: &str = ".jsonl";
const RUNTIME_LOCK_FILENAME: &str = ".runtime.lock";
const MAX_ENCODED_ID_BYTES: usize = 120;

pub struct TransactionStore {
    root: PathBuf,
    _root_directory: File,
    _runtime_lock: RuntimeDirectoryLock,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecoveryInventory {
    pub pending: Vec<PendingTransaction>,
    pub sealed: Vec<SealedTransaction>,
    pub unstarted: Vec<UnstartedTransaction>,
    pub corrupt: Vec<CorruptTransactionLog>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnstartedTransaction {
    pub transaction_id: TransactionId,
    pub path: PathBuf,
    identity: FileIdentity,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingTransaction {
    pub transaction_id: TransactionId,
    pub intent_pin: EvaluationIntentPin,
    pub capability_generation: u64,
    pub path: PathBuf,
    pub entry_count: usize,
    pub next_sequence: u64,
    pub changes: Vec<RecoveryChange>,
    pub pending_operation_count: usize,
    pub has_applied_unknown: bool,
    identity: FileIdentity,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecoveryChange {
    pub change_id: ChangeId,
    pub supersedes: Option<ChangeId>,
    pub capability_id: crate::domain::CapabilityId,
    pub resource: ResourceKey,
    pub experiment_verified: bool,
    pub state: ChangeState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SealedTransaction {
    pub transaction_id: TransactionId,
    pub intent_pin: EvaluationIntentPin,
    pub path: PathBuf,
    pub outcome: TransactionSeal,
    pub authorization: Option<CommitAuthorization>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CorruptTransactionLog {
    pub path: PathBuf,
    pub error: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

impl FileIdentity {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }
}

impl TransactionStore {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, WalError> {
        let root = root.into();
        match fs::symlink_metadata(&root) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(WalError::new(format!(
                    "transaction store root '{}' must not be a symlink",
                    root.display()
                )));
            }
            Ok(metadata) if !metadata.is_dir() => {
                return Err(WalError::new(format!(
                    "transaction store root '{}' is not a directory",
                    root.display()
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir_all(&root).map_err(|error| {
                    WalError::new(format!(
                        "failed to create transaction store '{}': {error}",
                        root.display()
                    ))
                })?;
            }
            Err(error) => {
                return Err(WalError::new(format!(
                    "failed to inspect transaction store '{}': {error}",
                    root.display()
                )));
            }
        }
        let root = fs::canonicalize(&root).map_err(|error| {
            WalError::new(format!(
                "failed to canonicalize transaction store '{}': {error}",
                root.display()
            ))
        })?;
        let root_directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(&root)
            .map_err(|error| {
                WalError::new(format!(
                    "failed to securely open transaction store '{}': {error}",
                    root.display()
                ))
            })?;
        let metadata = root_directory.metadata().map_err(|error| {
            WalError::new(format!(
                "failed to inspect transaction store '{}': {error}",
                root.display()
            ))
        })?;
        if !metadata.is_dir() {
            return Err(WalError::new(format!(
                "transaction store '{}' is not a directory",
                root.display()
            )));
        }
        // SAFETY: geteuid has no arguments and no memory-safety preconditions.
        let effective_uid = unsafe { libc::geteuid() };
        if metadata.uid() != effective_uid {
            return Err(WalError::new(format!(
                "transaction store '{}' must be owned by uid {effective_uid}",
                root.display()
            )));
        }
        if metadata.mode() & 0o777 != 0o700 {
            root_directory
                .set_permissions(fs::Permissions::from_mode(0o700))
                .map_err(|error| {
                    WalError::new(format!(
                        "failed to secure transaction store '{}': {error}",
                        root.display()
                    ))
                })?;
            root_directory.sync_all().map_err(|error| {
                WalError::new(format!(
                    "failed to sync transaction store permissions '{}': {error}",
                    root.display()
                ))
            })?;
        }
        let runtime_lock = RuntimeDirectoryLock::acquire(&root)?;
        Ok(Self {
            root,
            _root_directory: root_directory,
            _runtime_lock: runtime_lock,
        })
    }

    #[cfg(test)]
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn wal_path(&self, transaction_id: &TransactionId) -> Result<PathBuf, WalError> {
        Ok(self.root.join(filename_for(transaction_id)?))
    }

    pub fn create(&self, transaction_id: &TransactionId) -> Result<FileWal, WalError> {
        let path = self.wal_path(transaction_id)?;
        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&path)
            .map_err(|error| {
                WalError::new(format!(
                    "refusing to create transaction WAL '{}': {error}",
                    path.display()
                ))
            })?;
        if let Err(error) = file.set_permissions(fs::Permissions::from_mode(0o600)) {
            drop(file);
            let _ = fs::remove_file(&path);
            let _ = sync_directory(&self.root);
            return Err(WalError::new(format!(
                "failed to secure new transaction WAL '{}': {error}",
                path.display()
            )));
        }
        file.sync_all().map_err(|error| {
            WalError::new(format!(
                "failed to sync new transaction WAL '{}': {error}",
                path.display()
            ))
        })?;
        sync_directory(&self.root)?;
        FileWal::from_open_file(path, file)
    }

    pub fn open_existing(&self, pending: &PendingTransaction) -> Result<FileWal, WalError> {
        let path = self.wal_path(&pending.transaction_id)?;
        if path != pending.path {
            return Err(WalError::new(
                "pending WAL path does not match its transaction id",
            ));
        }
        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&path)
            .map_err(|error| {
                WalError::new(format!(
                    "failed to open existing transaction WAL '{}': {error}",
                    path.display()
                ))
            })?;
        let metadata = file.metadata().map_err(|error| {
            WalError::new(format!(
                "failed to inspect existing transaction WAL '{}': {error}",
                path.display()
            ))
        })?;
        if !metadata.is_file() {
            return Err(WalError::new(format!(
                "transaction WAL '{}' is not a regular file",
                path.display()
            )));
        }
        if FileIdentity::from_metadata(&metadata) != pending.identity {
            return Err(WalError::new(format!(
                "transaction WAL '{}' changed after discovery",
                path.display()
            )));
        }
        FileWal::from_open_file(path, file)
    }

    pub fn discard_unstarted(&self, unstarted: &UnstartedTransaction) -> Result<(), WalError> {
        let expected_path = self.wal_path(&unstarted.transaction_id)?;
        if expected_path != unstarted.path {
            return Err(WalError::new(
                "unstarted WAL path does not match its transaction id",
            ));
        }
        let opened = load_read_only(&expected_path)?;
        if opened.len != 0 || !opened.entries.is_empty() {
            return Err(WalError::new(format!(
                "refusing to discard unstarted WAL '{}' because it is no longer an empty regular file",
                expected_path.display()
            )));
        }
        if opened.identity != unstarted.identity {
            return Err(WalError::new(format!(
                "refusing to discard unstarted WAL '{}' because it changed after discovery",
                expected_path.display()
            )));
        }
        fs::remove_file(&expected_path).map_err(|error| {
            WalError::new(format!(
                "failed to discard unstarted transaction WAL '{}': {error}",
                expected_path.display()
            ))
        })?;
        sync_directory(&self.root)
    }

    pub fn discover(&self) -> Result<RecoveryInventory, WalError> {
        let mut inventory = RecoveryInventory {
            pending: Vec::new(),
            sealed: Vec::new(),
            unstarted: Vec::new(),
            corrupt: Vec::new(),
        };
        let entries = fs::read_dir(&self.root).map_err(|error| {
            WalError::new(format!(
                "failed to list transaction store '{}': {error}",
                self.root.display()
            ))
        })?;

        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    inventory.corrupt.push(CorruptTransactionLog {
                        path: self.root.clone(),
                        error: format!("failed to read transaction store entry: {error}"),
                    });
                    continue;
                }
            };
            let path = entry.path();
            if path.extension() != Some(OsStr::new("jsonl")) {
                continue;
            }
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(error) => {
                    inventory.corrupt.push(CorruptTransactionLog {
                        path,
                        error: format!("failed to inspect transaction log: {error}"),
                    });
                    continue;
                }
            };
            if !file_type.is_file() {
                inventory.corrupt.push(CorruptTransactionLog {
                    path,
                    error: "transaction log is not a regular file".into(),
                });
                continue;
            }

            let transaction_id = match transaction_id_from_path(&path) {
                Ok(transaction_id) => transaction_id,
                Err(error) => {
                    inventory.corrupt.push(CorruptTransactionLog {
                        path,
                        error: error.to_string(),
                    });
                    continue;
                }
            };
            let opened = match load_read_only(&path) {
                Ok(opened) => opened,
                Err(error) => {
                    inventory.corrupt.push(CorruptTransactionLog {
                        path,
                        error: error.to_string(),
                    });
                    continue;
                }
            };
            if opened.entries.is_empty() {
                if opened.len == 0 {
                    inventory.unstarted.push(UnstartedTransaction {
                        transaction_id,
                        path,
                        identity: opened.identity,
                    });
                } else {
                    inventory.corrupt.push(CorruptTransactionLog {
                        path,
                        error: "transaction WAL contains no records".into(),
                    });
                }
                continue;
            }
            match inspect_log(&path, &transaction_id, &opened.entries, opened.identity) {
                Ok(LogState::Pending(metadata)) => inventory.pending.push(metadata),
                Ok(LogState::Sealed(metadata)) => inventory.sealed.push(metadata),
                Err(error) => inventory.corrupt.push(CorruptTransactionLog {
                    path,
                    error: error.to_string(),
                }),
            }
        }

        inventory
            .pending
            .sort_by(|left, right| left.transaction_id.cmp(&right.transaction_id));
        inventory
            .sealed
            .sort_by(|left, right| left.transaction_id.cmp(&right.transaction_id));
        inventory
            .unstarted
            .sort_by(|left, right| left.transaction_id.cmp(&right.transaction_id));
        inventory
            .corrupt
            .sort_by(|left, right| left.path.cmp(&right.path));
        Ok(inventory)
    }
}

struct RuntimeDirectoryLock {
    file: File,
}

impl RuntimeDirectoryLock {
    fn acquire(root: &Path) -> Result<Self, WalError> {
        let path = root.join(RUNTIME_LOCK_FILENAME);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&path)
            .map_err(|error| {
                WalError::new(format!(
                    "failed to open transaction store lock '{}': {error}",
                    path.display()
                ))
            })?;
        if !file
            .metadata()
            .map_err(|error| {
                WalError::new(format!(
                    "failed to inspect transaction store lock '{}': {error}",
                    path.display()
                ))
            })?
            .is_file()
        {
            return Err(WalError::new(format!(
                "transaction store lock '{}' is not a regular file",
                path.display()
            )));
        }

        // LOCK_NB makes ownership failure explicit instead of hanging startup.
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            return Err(WalError::new(format!(
                "transaction store '{}' is already owned by another runtime or cannot be locked: {error}",
                root.display()
            )));
        }
        Ok(Self { file })
    }
}

impl Drop for RuntimeDirectoryLock {
    fn drop(&mut self) {
        // Closing the file also releases flock; the explicit unlock documents
        // ownership and releases it before the descriptor is dropped.
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

enum LogState {
    Pending(PendingTransaction),
    Sealed(SealedTransaction),
}

fn inspect_log(
    path: &Path,
    expected_transaction_id: &TransactionId,
    entries: &[WalEntry],
    identity: FileIdentity,
) -> Result<LogState, WalError> {
    if entries.is_empty() {
        return Err(WalError::new("transaction WAL is empty"));
    }
    let mut intent_pin = None;
    let mut capability_generation = None;
    let mut changes: BTreeMap<ChangeId, RecoveryChange> = BTreeMap::new();
    let mut resource_heads = BTreeMap::new();
    let mut resource_baselines = BTreeMap::new();
    let mut resource_providers = BTreeMap::new();
    let mut known_changes = BTreeSet::new();
    let mut pending_operations = BTreeMap::new();
    let mut seal = None;
    let mut commit_authorization = None;

    for (index, entry) in entries.iter().enumerate() {
        if &entry.transaction_id != expected_transaction_id {
            return Err(WalError::new(format!(
                "WAL entry {} transaction id does not match its filename",
                index + 1
            )));
        }
        if entry.sequence != index as u64 {
            return Err(WalError::new(format!(
                "WAL sequence gap at entry {}: expected {}, got {}",
                index + 1,
                index,
                entry.sequence
            )));
        }
        match &entry.event {
            WalEvent::Started {
                intent_pin: event_intent_pin,
                capability_generation: event_generation,
            } => {
                if index != 0 || intent_pin.replace(event_intent_pin.clone()).is_some() {
                    return Err(WalError::new(
                        "transaction WAL must contain exactly one leading start record",
                    ));
                }
                capability_generation = Some(*event_generation);
            }
            WalEvent::ChangeUpsert { change } => {
                if intent_pin.is_none() {
                    return Err(WalError::new("change appears before transaction start"));
                }
                if &change.transaction_id != expected_transaction_id {
                    return Err(WalError::new(format!(
                        "change '{}' belongs to a different transaction",
                        change.change_id
                    )));
                }
                if !content_digest(&change.prepared.baseline.value)
                    .is_ok_and(|digest| digest == change.prepared.baseline.digest)
                    || !content_digest(&change.prepared.desired.value)
                        .is_ok_and(|digest| digest == change.prepared.desired.digest)
                {
                    return Err(WalError::new(format!(
                        "change '{}' contains a state digest that does not match its value",
                        change.change_id
                    )));
                }
                if change.prepared.baseline.value == change.prepared.desired.value {
                    return Err(WalError::new(format!(
                        "change '{}' has identical baseline and desired states",
                        change.change_id
                    )));
                }
                if let Some(previous) = changes.get(&change.change_id) {
                    if previous.resource != change.resource
                        || previous.capability_id != change.capability_id
                        || previous.supersedes != change.supersedes
                    {
                        return Err(WalError::new(format!(
                            "change '{}' changed capability or resource identity in the WAL",
                            change.change_id
                        )));
                    }
                    if previous.experiment_verified && !change.experiment_verified {
                        return Err(WalError::new(format!(
                            "change '{}' cleared durable experiment verification",
                            change.change_id
                        )));
                    }
                    if !previous.experiment_verified
                        && change.experiment_verified
                        && change.state != ChangeState::AppliedVerified
                    {
                        return Err(WalError::new(format!(
                            "change '{}' gained experiment verification outside initial apply",
                            change.change_id
                        )));
                    }
                } else if changes.len() >= MAX_CHANGES_PER_TRANSACTION {
                    return Err(WalError::new(format!(
                        "transaction WAL contains more than {MAX_CHANGES_PER_TRANSACTION} changes"
                    )));
                } else if change.experiment_verified {
                    return Err(WalError::new(format!(
                        "first WAL record for change '{}' is already experiment-verified",
                        change.change_id
                    )));
                } else if change.state != ChangeState::IntentDurable {
                    return Err(WalError::new(format!(
                        "first WAL record for change '{}' is not an apply intent",
                        change.change_id
                    )));
                } else {
                    match &change.supersedes {
                        None => {
                            if let Some(owner) = resource_heads.get(&change.resource) {
                                return Err(WalError::new(format!(
                                    "resource '{}' is already owned by '{}' but '{}' has no revision link",
                                    change.resource, owner, change.change_id
                                )));
                            }
                            resource_baselines.insert(
                                change.resource.clone(),
                                change.prepared.baseline.digest.clone(),
                            );
                            resource_providers
                                .insert(change.resource.clone(), change.prepared.provider.clone());
                        }
                        Some(previous_id) => {
                            let previous = changes.get(previous_id).ok_or_else(|| {
                                WalError::new(format!(
                                    "change '{}' supersedes unknown change '{}'",
                                    change.change_id, previous_id
                                ))
                            })?;
                            if resource_heads.get(&change.resource) != Some(previous_id)
                                || previous.state != ChangeState::BaselineRestored
                                || !previous.experiment_verified
                                || previous.capability_id != change.capability_id
                                || resource_baselines.get(&change.resource)
                                    != Some(&change.prepared.baseline.digest)
                                || resource_providers.get(&change.resource)
                                    != Some(&change.prepared.provider)
                            {
                                return Err(WalError::new(format!(
                                    "change '{}' has an invalid predecessor '{}'",
                                    change.change_id, previous_id
                                )));
                            }
                        }
                    }
                    resource_heads.insert(change.resource.clone(), change.change_id.clone());
                }
                if matches!(
                    change.state,
                    ChangeState::AppliedVerified
                        | ChangeState::CandidateApplied
                        | ChangeState::Finalized
                ) && !change.experiment_verified
                {
                    return Err(WalError::new(format!(
                        "change '{}' has a verified state without experiment evidence",
                        change.change_id
                    )));
                }
                known_changes.insert(change.change_id.clone());
                if pending_operations
                    .get(&change.change_id)
                    .is_some_and(|(operation_id, _)| operation_id == &change.last_operation_id)
                {
                    pending_operations.remove(&change.change_id);
                }
                changes.insert(
                    change.change_id.clone(),
                    RecoveryChange {
                        change_id: change.change_id.clone(),
                        supersedes: change.supersedes.clone(),
                        capability_id: change.capability_id.clone(),
                        resource: change.resource.clone(),
                        experiment_verified: change.experiment_verified,
                        state: change.state,
                    },
                );
            }
            WalEvent::OperationIntent {
                change_id,
                operation_id,
                operation,
            } => {
                if !known_changes.contains(change_id) {
                    return Err(WalError::new(format!(
                        "operation intent references unknown change '{change_id}'"
                    )));
                }
                if *operation == crate::kernel::transaction::OperationIntentKind::Finalize {
                    let change = &changes[change_id];
                    if change.state != ChangeState::CandidateApplied || !change.experiment_verified
                    {
                        return Err(WalError::new(format!(
                            "finalize intent references non-candidate change '{change_id}'"
                        )));
                    }
                }
                pending_operations.insert(change_id.clone(), (operation_id.clone(), *operation));
            }
            WalEvent::Sealed { outcome } => {
                if *outcome == TransactionSeal::Committed {
                    return Err(WalError::new(
                        "committed transaction seal is missing commit authorization",
                    ));
                }
                if index + 1 != entries.len() || seal.replace(*outcome).is_some() {
                    return Err(WalError::new(
                        "transaction seal must be the final and only seal record",
                    ));
                }
            }
            WalEvent::CommitSealed {
                authorization,
                changes: terminal_changes,
            } => {
                if index + 1 != entries.len()
                    || seal.replace(TransactionSeal::Committed).is_some()
                    || !pending_operations.is_empty()
                    || terminal_changes.is_empty()
                    || terminal_changes.len() != changes.len()
                {
                    return Err(WalError::new(
                        "commit seal is not a complete, acknowledged terminal record",
                    ));
                }
                let mut seen = BTreeSet::new();
                for terminal in terminal_changes {
                    if &terminal.transaction_id != expected_transaction_id
                        || !matches!(
                            terminal.state,
                            ChangeState::Finalized | ChangeState::RolledBack
                        )
                        || (terminal.state == ChangeState::Finalized
                            && !terminal.experiment_verified)
                        || !seen.insert(terminal.change_id.clone())
                    {
                        return Err(WalError::new(
                            "commit seal contains invalid terminal changes",
                        ));
                    }
                    let previous = changes.get(&terminal.change_id).ok_or_else(|| {
                        WalError::new(format!(
                            "commit seal references unknown change '{}'",
                            terminal.change_id
                        ))
                    })?;
                    if previous.capability_id != terminal.capability_id
                        || previous.resource != terminal.resource
                        || previous.supersedes != terminal.supersedes
                        || previous.experiment_verified != terminal.experiment_verified
                        || (terminal.state == ChangeState::Finalized
                            && previous.state != ChangeState::CandidateApplied)
                        || (terminal.state == ChangeState::RolledBack
                            && previous.state != ChangeState::BaselineRestored)
                    {
                        return Err(WalError::new(format!(
                            "commit seal changed identity or experiment evidence for '{}'",
                            terminal.change_id
                        )));
                    }
                    if terminal.state == ChangeState::Finalized
                        && resource_heads.get(&terminal.resource) != Some(&terminal.change_id)
                    {
                        return Err(WalError::new(format!(
                            "commit seal finalized superseded change '{}'",
                            terminal.change_id
                        )));
                    }
                }
                let committed_candidate = crate::domain::Candidate::new(
                    terminal_changes
                        .iter()
                        .filter(|change| change.state == ChangeState::Finalized)
                        .map(|change| change.change_id.clone())
                        .collect(),
                )
                .map_err(|error| {
                    WalError::new(format!("commit seal has an invalid candidate: {error}"))
                })?;
                if authorization.candidate_digest() != committed_candidate.digest() {
                    return Err(WalError::new(
                        "commit authorization does not match the committed candidate",
                    ));
                }
                if Some(authorization.intent_pin()) != intent_pin.as_ref() {
                    return Err(WalError::new(
                        "commit authorization does not match the transaction evaluation intent",
                    ));
                }
                commit_authorization = Some(authorization.clone());
                for terminal in terminal_changes {
                    changes.insert(
                        terminal.change_id.clone(),
                        RecoveryChange {
                            change_id: terminal.change_id.clone(),
                            supersedes: terminal.supersedes.clone(),
                            capability_id: terminal.capability_id.clone(),
                            resource: terminal.resource.clone(),
                            experiment_verified: terminal.experiment_verified,
                            state: terminal.state,
                        },
                    );
                }
            }
        }
    }

    let intent_pin =
        intent_pin.ok_or_else(|| WalError::new("transaction WAL has no start record"))?;
    if let Some(outcome) = seal {
        if !pending_operations.is_empty() {
            return Err(WalError::new(
                "sealed transaction WAL contains unresolved operation intents",
            ));
        }
        let states_are_terminal = changes.values().all(|change| match outcome {
            TransactionSeal::Committed => {
                matches!(
                    change.state,
                    ChangeState::Finalized | ChangeState::RolledBack
                ) && (change.state != ChangeState::Finalized || change.experiment_verified)
            }
            TransactionSeal::RolledBack => change.state == ChangeState::RolledBack,
        });
        if !states_are_terminal {
            return Err(WalError::new(
                "sealed transaction WAL contains non-terminal change state",
            ));
        }
        return Ok(LogState::Sealed(SealedTransaction {
            transaction_id: expected_transaction_id.clone(),
            intent_pin,
            path: path.to_path_buf(),
            outcome,
            authorization: match outcome {
                TransactionSeal::Committed => Some(commit_authorization.ok_or_else(|| {
                    WalError::new("committed transaction seal has no authorization")
                })?),
                TransactionSeal::RolledBack => None,
            },
        }));
    }
    let mut changes = changes.into_values().collect::<Vec<_>>();
    changes.sort_by(|left, right| left.change_id.cmp(&right.change_id));
    let has_applied_unknown = pending_operations.values().any(|(_, operation)| {
        *operation != crate::kernel::transaction::OperationIntentKind::Finalize
    }) || changes.iter().any(|change| {
        matches!(
            change.state,
            ChangeState::IntentDurable | ChangeState::AppliedUnknown
        )
    });
    Ok(LogState::Pending(PendingTransaction {
        transaction_id: expected_transaction_id.clone(),
        intent_pin,
        capability_generation: capability_generation
            .ok_or_else(|| WalError::new("transaction WAL start has no generation"))?,
        path: path.to_path_buf(),
        entry_count: entries.len(),
        next_sequence: entries.len() as u64,
        changes,
        pending_operation_count: pending_operations.len(),
        has_applied_unknown,
        identity,
    }))
}

fn filename_for(transaction_id: &TransactionId) -> Result<String, WalError> {
    let bytes = transaction_id.as_str().as_bytes();
    if bytes.len() > MAX_ENCODED_ID_BYTES {
        return Err(WalError::new(format!(
            "transaction id exceeds store filename limit of {MAX_ENCODED_ID_BYTES} bytes"
        )));
    }
    Ok(format!("{FILE_PREFIX}{}{FILE_SUFFIX}", encode_hex(bytes)))
}

fn transaction_id_from_path(path: &Path) -> Result<TransactionId, WalError> {
    let filename = path
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| WalError::new("transaction WAL filename is not valid UTF-8"))?;
    let encoded = filename
        .strip_prefix(FILE_PREFIX)
        .and_then(|name| name.strip_suffix(FILE_SUFFIX))
        .ok_or_else(|| WalError::new("transaction WAL filename has an invalid shape"))?;
    let bytes = decode_hex(encoded)?;
    let value = String::from_utf8(bytes)
        .map_err(|error| WalError::new(format!("transaction filename is not UTF-8: {error}")))?;
    let transaction_id = TransactionId::new(value)
        .map_err(|error| WalError::new(format!("invalid transaction id in filename: {error}")))?;
    if filename_for(&transaction_id)? != filename {
        return Err(WalError::new("transaction WAL filename is not canonical"));
    }
    Ok(transaction_id)
}

fn encode_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(DIGITS[(byte >> 4) as usize] as char);
        encoded.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn decode_hex(encoded: &str) -> Result<Vec<u8>, WalError> {
    if encoded.is_empty() || !encoded.len().is_multiple_of(2) {
        return Err(WalError::new(
            "transaction WAL filename has invalid hex encoding",
        ));
    }
    encoded
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let high = hex_digit(pair[0])?;
            let low = hex_digit(pair[1])?;
            Ok((high << 4) | low)
        })
        .collect()
}

fn hex_digit(value: u8) -> Result<u8, WalError> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        _ => Err(WalError::new(
            "transaction WAL filename must use lowercase hex",
        )),
    }
}

struct OpenedWal {
    entries: Vec<WalEntry>,
    identity: FileIdentity,
    len: u64,
}

fn load_read_only(path: &Path) -> Result<OpenedWal, WalError> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|error| {
            WalError::new(format!(
                "failed to open transaction WAL '{}': {error}",
                path.display()
            ))
        })?;
    let metadata = file.metadata().map_err(|error| {
        WalError::new(format!(
            "failed to inspect transaction WAL '{}': {error}",
            path.display()
        ))
    })?;
    if !metadata.is_file() {
        return Err(WalError::new(format!(
            "transaction WAL '{}' is not a regular file",
            path.display()
        )));
    }
    Ok(OpenedWal {
        entries: load_file(&file, path)?,
        identity: FileIdentity::from_metadata(&metadata),
        len: metadata.len(),
    })
}

fn sync_directory(path: &Path) -> Result<(), WalError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| {
            WalError::new(format!(
                "failed to sync transaction store '{}': {error}",
                path.display()
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    use crate::domain::{
        Candidate, CapabilityId, CommitAuthorization, Digest, EpisodeId, EvaluationIntentPin,
        MutationState, OperationId, PreparedMutation, ProviderClass, ProviderId, ProviderPin,
        ProviderVersion,
    };
    use crate::kernel::transaction::TransactionWal;

    #[test]
    fn transaction_ids_map_to_distinct_safe_files() {
        let root = temp_store("safe-files");
        let store = TransactionStore::new(&root).unwrap();
        let first = TransactionId::new("../../episode/one").unwrap();
        let second = TransactionId::new("episode:two").unwrap();

        let first_path = store.wal_path(&first).unwrap();
        let second_path = store.wal_path(&second).unwrap();
        assert_eq!(first_path.parent(), Some(store.root()));
        assert_eq!(second_path.parent(), Some(store.root()));
        assert_ne!(first_path, second_path);
        assert!(!first_path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .contains('/'));

        let _first_wal = store.create(&first).unwrap();
        let _second_wal = store.create(&second).unwrap();
        assert!(first_path.is_file());
        assert!(second_path.is_file());
        cleanup(root);
    }

    #[test]
    fn new_store_and_wal_use_owner_only_permissions() {
        let root = temp_store("permissions");
        let store = TransactionStore::new(&root).unwrap();
        assert_eq!(
            fs::metadata(store.root()).unwrap().permissions().mode() & 0o777,
            0o700
        );

        let transaction_id = TransactionId::new("private").unwrap();
        drop(store.create(&transaction_id).unwrap());
        assert_eq!(
            fs::metadata(store.wal_path(&transaction_id).unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        cleanup(root);
    }

    #[test]
    fn discovery_separates_pending_sealed_and_corrupt_logs() {
        let root = temp_store("discovery");
        let store = TransactionStore::new(&root).unwrap();
        let pending_id = TransactionId::new("pending").unwrap();
        let sealed_id = TransactionId::new("sealed").unwrap();
        let corrupt_id = TransactionId::new("corrupt").unwrap();
        let unstarted_id = TransactionId::new("unstarted").unwrap();

        let mut pending = store.create(&pending_id).unwrap();
        append_start(&mut pending, pending_id.clone(), EpisodeId::new(11), 4);
        let pending_change = test_change(pending_id.clone());
        pending
            .append_durable(&WalEntry {
                sequence: 1,
                transaction_id: pending_id.clone(),
                event: WalEvent::ChangeUpsert {
                    change: Box::new(pending_change),
                },
            })
            .unwrap();

        let mut sealed = store.create(&sealed_id).unwrap();
        append_start(&mut sealed, sealed_id.clone(), EpisodeId::new(12), 5);
        sealed
            .seal(&WalEntry {
                sequence: 1,
                transaction_id: sealed_id.clone(),
                event: WalEvent::Sealed {
                    outcome: TransactionSeal::RolledBack,
                },
            })
            .unwrap();

        let corrupt_path = store.wal_path(&corrupt_id).unwrap();
        let corrupt = store.create(&corrupt_id).unwrap();
        drop(corrupt);
        fs::write(&corrupt_path, b"not-json\n").unwrap();
        let before = fs::read(&corrupt_path).unwrap();
        let unstarted_path = store.wal_path(&unstarted_id).unwrap();
        let unstarted = store.create(&unstarted_id).unwrap();
        drop(unstarted);

        let inventory = store.discover().unwrap();
        assert_eq!(inventory.pending.len(), 1);
        assert_eq!(inventory.pending[0].transaction_id, pending_id);
        assert_eq!(
            inventory.pending[0].intent_pin.episode_id(),
            EpisodeId::new(11)
        );
        assert_eq!(inventory.pending[0].capability_generation, 4);
        assert_eq!(inventory.pending[0].next_sequence, 2);
        assert_eq!(inventory.pending[0].changes.len(), 1);
        assert_eq!(
            inventory.pending[0].changes[0].change_id,
            ChangeId::new("change/pending").unwrap()
        );
        assert!(inventory.pending[0].has_applied_unknown);
        assert_eq!(inventory.sealed.len(), 1);
        assert_eq!(inventory.sealed[0].transaction_id, sealed_id);
        assert_eq!(
            inventory.sealed[0].intent_pin.episode_id(),
            EpisodeId::new(12)
        );
        assert!(inventory.sealed[0].authorization.is_none());
        assert_eq!(inventory.unstarted.len(), 1);
        assert_eq!(inventory.unstarted[0].transaction_id, unstarted_id);
        assert_eq!(inventory.unstarted[0].path, unstarted_path);
        assert_eq!(inventory.corrupt.len(), 1);
        assert_eq!(inventory.corrupt[0].path, corrupt_path);
        assert_eq!(fs::read(&corrupt_path).unwrap(), before);
        cleanup(root);
    }

    #[test]
    fn discovery_accepts_only_linear_resource_revisions() {
        let root = temp_store("resource-revisions");
        let store = TransactionStore::new(&root).unwrap();
        let transaction_id = TransactionId::new("revisions").unwrap();
        let mut wal = store.create(&transaction_id).unwrap();
        append_start(&mut wal, transaction_id.clone(), EpisodeId::new(21), 1);

        let mut first = test_change(transaction_id.clone());
        append_change(&mut wal, 1, &transaction_id, first.clone());
        first.experiment_verified = true;
        first.state = ChangeState::AppliedVerified;
        first.last_operation_id = OperationId::new("operation/first-applied").unwrap();
        append_change(&mut wal, 2, &transaction_id, first.clone());

        let restore_id = OperationId::new("operation/first-restore").unwrap();
        append_operation_intent(
            &mut wal,
            3,
            &transaction_id,
            &first.change_id,
            restore_id.clone(),
            crate::kernel::transaction::OperationIntentKind::Restore,
        );
        first.state = ChangeState::BaselineRestored;
        first.last_operation_id = restore_id;
        append_change(&mut wal, 4, &transaction_id, first.clone());

        let mut second = first.clone();
        second.change_id = ChangeId::new("change/revision-2").unwrap();
        second.supersedes = Some(first.change_id.clone());
        second.prepared.desired = MutationState {
            value: Value::String("other".into()),
            digest: content_digest(&Value::String("other".into())).unwrap(),
        };
        second.experiment_verified = false;
        second.state = ChangeState::IntentDurable;
        second.last_operation_id = OperationId::new("operation/second-apply").unwrap();
        append_change(&mut wal, 5, &transaction_id, second.clone());
        second.experiment_verified = true;
        second.state = ChangeState::AppliedVerified;
        append_change(&mut wal, 6, &transaction_id, second.clone());
        drop(wal);

        let inventory = store.discover().unwrap();
        assert!(inventory.corrupt.is_empty());
        assert_eq!(inventory.pending.len(), 1);
        assert_eq!(inventory.pending[0].changes.len(), 2);
        let recovered_second = inventory.pending[0]
            .changes
            .iter()
            .find(|change| change.change_id == second.change_id)
            .unwrap();
        assert_eq!(recovered_second.supersedes, Some(first.change_id));
        assert!(!inventory.pending[0].has_applied_unknown);
        cleanup(root);
    }

    #[test]
    fn create_never_reuses_or_overwrites_an_existing_log() {
        let root = temp_store("no-overwrite");
        let store = TransactionStore::new(&root).unwrap();
        let transaction_id = TransactionId::new("existing").unwrap();
        let path = store.wal_path(&transaction_id).unwrap();
        let mut wal = store.create(&transaction_id).unwrap();
        append_start(&mut wal, transaction_id.clone(), EpisodeId::new(2), 1);
        drop(wal);
        let before = fs::read(&path).unwrap();

        assert!(store.create(&transaction_id).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
        cleanup(root);
    }

    #[test]
    fn recovery_open_never_recreates_a_missing_log() {
        let root = temp_store("missing-recovery-log");
        let store = TransactionStore::new(&root).unwrap();
        let transaction_id = TransactionId::new("pending").unwrap();
        let path = store.wal_path(&transaction_id).unwrap();
        let mut wal = store.create(&transaction_id).unwrap();
        append_start(&mut wal, transaction_id, EpisodeId::new(2), 1);
        drop(wal);
        let pending = store.discover().unwrap().pending.pop().unwrap();
        fs::remove_file(&path).unwrap();

        assert!(store.open_existing(&pending).is_err());
        assert!(!path.exists());
        cleanup(root);
    }

    #[test]
    fn recovery_open_rejects_an_inode_replaced_after_discovery() {
        let root = temp_store("replaced-recovery-log");
        let store = TransactionStore::new(&root).unwrap();
        let transaction_id = TransactionId::new("pending").unwrap();
        let path = store.wal_path(&transaction_id).unwrap();
        let displaced = root.join("displaced.jsonl");
        let mut wal = store.create(&transaction_id).unwrap();
        append_start(&mut wal, transaction_id, EpisodeId::new(2), 1);
        drop(wal);
        let pending = store.discover().unwrap().pending.pop().unwrap();
        fs::rename(&path, &displaced).unwrap();
        fs::copy(&displaced, &path).unwrap();

        assert!(store.open_existing(&pending).is_err());
        cleanup(root);
    }

    #[test]
    fn read_only_loader_rejects_symlinks() {
        let root = temp_store("read-symlink");
        fs::create_dir_all(&root).unwrap();
        let target = root.join("target.jsonl");
        fs::write(&target, b"not-a-wal\n").unwrap();
        let link = root.join("tx-00.jsonl");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        assert!(load_read_only(&link).is_err());
        cleanup(root);
    }

    #[test]
    fn existing_store_permissions_are_tightened_before_use() {
        let root = temp_store("existing-permissions");
        fs::create_dir_all(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o777)).unwrap();

        let store = TransactionStore::new(&root).unwrap();

        assert_eq!(
            fs::metadata(store.root()).unwrap().permissions().mode() & 0o777,
            0o700
        );
        cleanup(root);
    }

    #[test]
    fn malformed_matching_filename_is_reported_without_deletion() {
        let root = temp_store("bad-name");
        let store = TransactionStore::new(&root).unwrap();
        let path = store.root().join("tx-not-hex.jsonl");
        fs::write(&path, b"preserve-me\n").unwrap();

        let inventory = store.discover().unwrap();
        assert_eq!(inventory.corrupt.len(), 1);
        assert_eq!(inventory.corrupt[0].path, path);
        assert_eq!(fs::read(&path).unwrap(), b"preserve-me\n");
        cleanup(root);
    }

    #[test]
    fn discovery_rejects_state_digests_that_do_not_bind_their_values() {
        let root = temp_store("forged-state-digest");
        let store = TransactionStore::new(&root).unwrap();
        let transaction_id = TransactionId::new("forged").unwrap();
        let path = store.wal_path(&transaction_id).unwrap();
        let mut wal = store.create(&transaction_id).unwrap();
        append_start(&mut wal, transaction_id.clone(), EpisodeId::new(3), 1);
        let mut change = test_change(transaction_id.clone());
        change.prepared.baseline.digest = Digest::new("sha256:forged").unwrap();
        wal.append_durable(&WalEntry {
            sequence: 1,
            transaction_id,
            event: WalEvent::ChangeUpsert {
                change: Box::new(change),
            },
        })
        .unwrap();
        drop(wal);

        let inventory = store.discover().unwrap();
        assert!(inventory.pending.is_empty());
        assert_eq!(inventory.corrupt.len(), 1);
        assert_eq!(inventory.corrupt[0].path, path);
        assert!(inventory.corrupt[0].error.contains("digest"));
        cleanup(root);
    }

    #[test]
    fn discovery_rejects_commit_authorization_for_another_evaluation_intent() {
        let root = temp_store("commit-intent-mismatch");
        let store = TransactionStore::new(&root).unwrap();
        let transaction_id = TransactionId::new("intent-mismatch").unwrap();
        let path = store.wal_path(&transaction_id).unwrap();
        let mut wal = store.create(&transaction_id).unwrap();
        append_start(&mut wal, transaction_id.clone(), EpisodeId::new(7), 1);

        let mut change = test_change(transaction_id.clone());
        append_change(&mut wal, 1, &transaction_id, change.clone());
        change.experiment_verified = true;
        change.state = ChangeState::AppliedVerified;
        append_change(&mut wal, 2, &transaction_id, change.clone());
        change.state = ChangeState::BaselineRestored;
        append_change(&mut wal, 3, &transaction_id, change.clone());

        let replay_id = OperationId::new("operation/replay").unwrap();
        append_operation_intent(
            &mut wal,
            4,
            &transaction_id,
            &change.change_id,
            replay_id.clone(),
            crate::kernel::transaction::OperationIntentKind::Apply,
        );
        change.state = ChangeState::CandidateApplied;
        change.last_operation_id = replay_id;
        append_change(&mut wal, 5, &transaction_id, change.clone());

        let finalize_id = OperationId::new("operation/finalize").unwrap();
        append_operation_intent(
            &mut wal,
            6,
            &transaction_id,
            &change.change_id,
            finalize_id.clone(),
            crate::kernel::transaction::OperationIntentKind::Finalize,
        );
        change.last_operation_id = finalize_id;
        append_change(&mut wal, 7, &transaction_id, change.clone());

        let candidate = Candidate::new(vec![change.change_id.clone()]).unwrap();
        let authorization = CommitAuthorization::issue(
            test_intent_pin(EpisodeId::new(8)),
            candidate.digest().clone(),
            Digest::new("decision-digest").unwrap(),
            Digest::new("evidence-digest").unwrap(),
        )
        .unwrap();
        change.state = ChangeState::Finalized;
        wal.seal(&WalEntry {
            sequence: 8,
            transaction_id: transaction_id.clone(),
            event: WalEvent::CommitSealed {
                authorization,
                changes: vec![change],
            },
        })
        .unwrap();
        drop(wal);

        let inventory = store.discover().unwrap();
        assert!(inventory.sealed.is_empty());
        assert_eq!(inventory.corrupt.len(), 1);
        assert_eq!(inventory.corrupt[0].path, path);
        assert!(inventory.corrupt[0].error.contains("evaluation intent"));
        cleanup(root);
    }

    #[test]
    fn overlong_ids_are_rejected_instead_of_truncated_or_colliding() {
        let root = temp_store("long-id");
        let store = TransactionStore::new(&root).unwrap();
        let transaction_id = TransactionId::new("x".repeat(MAX_ENCODED_ID_BYTES + 1)).unwrap();

        assert!(store.create(&transaction_id).is_err());
        assert!(store.discover().unwrap().pending.is_empty());
        cleanup(root);
    }

    #[test]
    fn transaction_store_has_exclusive_runtime_ownership() {
        let root = temp_store("exclusive-lock");
        let first = TransactionStore::new(&root).unwrap();

        let error = TransactionStore::new(&root)
            .err()
            .expect("a second store must not acquire the runtime lock");

        assert!(error.message.contains("already owned by another runtime"));
        assert!(first.discover().unwrap().corrupt.is_empty());
        drop(first);
        assert!(TransactionStore::new(&root).is_ok());
        cleanup(root);
    }

    #[test]
    fn discard_unstarted_revalidates_the_file_before_unlinking() {
        let root = temp_store("discard-unstarted");
        let store = TransactionStore::new(&root).unwrap();
        let transaction_id = TransactionId::new("unstarted").unwrap();
        drop(store.create(&transaction_id).unwrap());
        let unstarted = store.discover().unwrap().unstarted.pop().unwrap();
        fs::write(&unstarted.path, b"changed-after-discovery").unwrap();

        assert!(store.discard_unstarted(&unstarted).is_err());
        assert_eq!(
            fs::read(&unstarted.path).unwrap(),
            b"changed-after-discovery"
        );
        cleanup(root);
    }

    fn append_start(
        wal: &mut FileWal,
        transaction_id: TransactionId,
        episode_id: EpisodeId,
        capability_generation: u64,
    ) {
        wal.append_durable(&WalEntry {
            sequence: 0,
            transaction_id,
            event: WalEvent::Started {
                intent_pin: test_intent_pin(episode_id),
                capability_generation,
            },
        })
        .unwrap();
    }

    fn test_intent_pin(episode_id: EpisodeId) -> EvaluationIntentPin {
        EvaluationIntentPin::new(
            episode_id,
            Digest::new(format!("intent-{}", episode_id.get())).unwrap(),
            Digest::new(format!("contract-{}", episode_id.get())).unwrap(),
        )
    }

    fn append_change(
        wal: &mut FileWal,
        sequence: u64,
        transaction_id: &TransactionId,
        change: crate::kernel::transaction::ChangeRecord,
    ) {
        wal.append_durable(&WalEntry {
            sequence,
            transaction_id: transaction_id.clone(),
            event: WalEvent::ChangeUpsert {
                change: Box::new(change),
            },
        })
        .unwrap();
    }

    fn append_operation_intent(
        wal: &mut FileWal,
        sequence: u64,
        transaction_id: &TransactionId,
        change_id: &ChangeId,
        operation_id: OperationId,
        operation: crate::kernel::transaction::OperationIntentKind,
    ) {
        wal.append_durable(&WalEntry {
            sequence,
            transaction_id: transaction_id.clone(),
            event: WalEvent::OperationIntent {
                change_id: change_id.clone(),
                operation_id,
                operation,
            },
        })
        .unwrap();
    }

    fn test_change(transaction_id: TransactionId) -> crate::kernel::transaction::ChangeRecord {
        let resource = ResourceKey::new("test/resource").unwrap();
        let capability_id = CapabilityId::new("test/mutation").unwrap();
        let provider = ProviderPin {
            provider_id: ProviderId::new("test-provider").unwrap(),
            provider_version: ProviderVersion::new("1").unwrap(),
            provider_class: ProviderClass::Local,
            manifest_digest: Digest::new("test-manifest").unwrap(),
        };
        crate::kernel::transaction::ChangeRecord {
            transaction_id,
            change_id: ChangeId::new("change/pending").unwrap(),
            supersedes: None,
            capability_id: capability_id.clone(),
            resource: resource.clone(),
            prepared: PreparedMutation {
                capability_id,
                provider,
                resource,
                baseline: MutationState {
                    value: Value::String("old".into()),
                    digest: content_digest(&Value::String("old".into())).unwrap(),
                },
                desired: MutationState {
                    value: Value::String("new".into()),
                    digest: content_digest(&Value::String("new".into())).unwrap(),
                },
                driver_data: Value::Null,
            },
            experiment_verified: false,
            state: ChangeState::IntentDurable,
            last_operation_id: OperationId::new("operation/apply").unwrap(),
            last_receipt: None,
            message: None,
        }
    }

    fn temp_store(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "tuning-agent-transaction-store-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn cleanup(path: PathBuf) {
        let _ = fs::remove_dir_all(path);
    }
}
