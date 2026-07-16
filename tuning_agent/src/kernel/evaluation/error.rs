use std::error::Error;
use std::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EvaluationErrorKind {
    InvalidContract,
    BudgetExceeded,
    MissingCapability,
    Transaction,
    Measurement,
    Cleanup,
    Comparison,
    Protocol,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EvaluationError {
    pub kind: EvaluationErrorKind,
    pub message: String,
}

impl EvaluationError {
    pub fn new(kind: EvaluationErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl fmt::Display for EvaluationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for EvaluationError {}
