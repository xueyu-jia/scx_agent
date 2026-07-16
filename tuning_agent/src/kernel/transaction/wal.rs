use std::fs::File;
#[cfg(test)]
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
#[cfg(test)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::domain::{CommitAuthorization, EvaluationIntentPin};
use crate::kernel::transaction::{
    ChangeRecord, OperationIntentKind, TransactionId, TransactionSeal, WalError,
};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WalEntry {
    pub sequence: u64,
    pub transaction_id: TransactionId,
    pub event: WalEvent,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WalEvent {
    Started {
        intent_pin: EvaluationIntentPin,
        capability_generation: u64,
    },
    ChangeUpsert {
        change: Box<ChangeRecord>,
    },
    OperationIntent {
        change_id: crate::domain::ChangeId,
        operation_id: crate::domain::OperationId,
        operation: OperationIntentKind,
    },
    Sealed {
        outcome: TransactionSeal,
    },
    CommitSealed {
        authorization: CommitAuthorization,
        changes: Vec<ChangeRecord>,
    },
}

pub trait TransactionWal: Send {
    fn append_durable(&mut self, entry: &WalEntry) -> Result<(), WalError>;

    fn load(&self) -> Result<Vec<WalEntry>, WalError>;

    fn seal(&mut self, entry: &WalEntry) -> Result<(), WalError>;
}

pub struct FileWal {
    path: PathBuf,
    file: File,
    sealed: bool,
}

impl FileWal {
    #[cfg(test)]
    pub(crate) fn open(path: impl Into<PathBuf>) -> Result<Self, WalError> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                WalError::new(format!(
                    "failed to create WAL directory '{}': {error}",
                    parent.display()
                ))
            })?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&path)
            .map_err(|error| {
                WalError::new(format!("failed to open WAL '{}': {error}", path.display()))
            })?;
        Self::from_open_file(path, file)
    }

    pub(super) fn from_open_file(path: PathBuf, file: File) -> Result<Self, WalError> {
        if !file
            .metadata()
            .map_err(|error| {
                WalError::new(format!(
                    "failed to inspect WAL '{}': {error}",
                    path.display()
                ))
            })?
            .is_file()
        {
            return Err(WalError::new(format!(
                "WAL '{}' is not a regular file",
                path.display()
            )));
        }
        let sealed = load_file(&file, &path)?.last().is_some_and(|entry| {
            matches!(
                entry.event,
                WalEvent::Sealed { .. } | WalEvent::CommitSealed { .. }
            )
        });
        Ok(Self { path, file, sealed })
    }

    fn write_synced(&mut self, entry: &WalEntry) -> Result<(), WalError> {
        if self.sealed {
            return Err(WalError::new("cannot append to a sealed WAL"));
        }
        let encoded = serde_json::to_vec(entry)
            .map_err(|error| WalError::new(format!("failed to encode WAL entry: {error}")))?;
        self.file
            .write_all(&encoded)
            .and_then(|_| self.file.write_all(b"\n"))
            .and_then(|_| self.file.flush())
            .and_then(|_| self.file.sync_all())
            .map_err(|error| {
                WalError::new(format!(
                    "failed to durably append WAL '{}': {error}",
                    self.path.display()
                ))
            })
    }
}

impl TransactionWal for FileWal {
    fn append_durable(&mut self, entry: &WalEntry) -> Result<(), WalError> {
        if matches!(
            entry.event,
            WalEvent::Sealed { .. } | WalEvent::CommitSealed { .. }
        ) {
            return Err(WalError::new("use seal() for a terminal WAL entry"));
        }
        self.write_synced(entry)
    }

    fn load(&self) -> Result<Vec<WalEntry>, WalError> {
        load_file(&self.file, &self.path)
    }

    fn seal(&mut self, entry: &WalEntry) -> Result<(), WalError> {
        if !matches!(
            entry.event,
            WalEvent::Sealed { .. } | WalEvent::CommitSealed { .. }
        ) {
            return Err(WalError::new("WAL seal entry must contain a sealed event"));
        }
        self.write_synced(entry)?;
        self.sealed = true;
        Ok(())
    }
}

pub(super) fn load_file(file: &File, path: &Path) -> Result<Vec<WalEntry>, WalError> {
    let mut file = file.try_clone().map_err(|error| {
        WalError::new(format!(
            "failed to clone WAL handle '{}': {error}",
            path.display()
        ))
    })?;
    file.seek(SeekFrom::Start(0)).map_err(|error| {
        WalError::new(format!("failed to seek WAL '{}': {error}", path.display()))
    })?;
    let mut entries = Vec::new();
    for (index, line) in BufReader::new(file).lines().enumerate() {
        let line = line.map_err(|error| {
            WalError::new(format!(
                "failed to read WAL '{}' line {}: {error}",
                path.display(),
                index + 1
            ))
        })?;
        if line.trim().is_empty() {
            continue;
        }
        let entry = serde_json::from_str(&line).map_err(|error| {
            WalError::new(format!(
                "invalid WAL '{}' line {}: {error}",
                path.display(),
                index + 1
            ))
        })?;
        entries.push(entry);
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{symlink, PermissionsExt};

    use super::*;

    fn intent_pin(episode: u64) -> EvaluationIntentPin {
        EvaluationIntentPin::new(
            crate::domain::EpisodeId::new(episode),
            crate::domain::Digest::new(format!("intent-{episode}")).unwrap(),
            crate::domain::Digest::new(format!("contract-{episode}")).unwrap(),
        )
    }

    #[test]
    fn file_wal_loads_synced_entries_and_rejects_writes_after_seal() {
        let path = std::env::temp_dir().join(format!(
            "tuning-agent-wal-{}-{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let transaction_id = TransactionId::new("tx/file-wal").unwrap();
        let mut wal = FileWal::open(&path).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o077, 0);
        wal.append_durable(&WalEntry {
            sequence: 0,
            transaction_id: transaction_id.clone(),
            event: WalEvent::Started {
                intent_pin: intent_pin(7),
                capability_generation: 3,
            },
        })
        .unwrap();
        wal.seal(&WalEntry {
            sequence: 1,
            transaction_id: transaction_id.clone(),
            event: WalEvent::Sealed {
                outcome: TransactionSeal::RolledBack,
            },
        })
        .unwrap();

        let entries = wal.load().unwrap();
        assert_eq!(entries.len(), 2);
        assert!(matches!(
            entries.last().unwrap().event,
            WalEvent::Sealed {
                outcome: TransactionSeal::RolledBack
            }
        ));
        assert!(wal
            .append_durable(&WalEntry {
                sequence: 2,
                transaction_id,
                event: WalEvent::Started {
                    intent_pin: intent_pin(7),
                    capability_generation: 3,
                },
            })
            .is_err());
        let _ = fs::remove_file(path);
    }

    #[test]
    fn file_wal_rejects_symlink_paths() {
        let root = std::env::temp_dir().join(format!(
            "tuning-agent-wal-symlink-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let target = root.join("target");
        File::create(&target).unwrap();
        let link = root.join("wal.jsonl");
        symlink(&target, &link).unwrap();

        assert!(FileWal::open(&link).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn file_wal_loads_from_its_open_handle_after_path_replacement() {
        let root = std::env::temp_dir().join(format!(
            "tuning-agent-wal-replaced-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("wal.jsonl");
        let moved = root.join("original.jsonl");
        let replacement = root.join("replacement.jsonl");
        let transaction_id = TransactionId::new("tx/open-handle").unwrap();
        let mut wal = FileWal::open(&path).unwrap();
        wal.append_durable(&WalEntry {
            sequence: 0,
            transaction_id: transaction_id.clone(),
            event: WalEvent::Started {
                intent_pin: intent_pin(9),
                capability_generation: 1,
            },
        })
        .unwrap();

        fs::rename(&path, &moved).unwrap();
        File::create(&replacement).unwrap();
        symlink(&replacement, &path).unwrap();

        let entries = wal.load().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].transaction_id, transaction_id);
        let _ = fs::remove_dir_all(root);
    }
}
