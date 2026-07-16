use std::error::Error;
use std::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransactionErrorKind {
    AppliedUnknown,
    CapabilityUnavailable,
    CommitOutcomeUnknown,
    CorruptWal,
    DuplicateChange,
    DuplicateResource,
    ExternalDrift,
    InvalidCandidate,
    InvalidState,
    PinMismatch,
    Provider,
    Sealed,
    Wal,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransactionError {
    pub kind: TransactionErrorKind,
    pub message: String,
}

impl TransactionError {
    pub fn new(kind: TransactionErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl fmt::Display for TransactionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for TransactionError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WalError {
    pub message: String,
}

impl WalError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for WalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for WalError {}
