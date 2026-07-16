use std::error::Error;
use std::fmt;

use crate::domain::{ProviderError, ProviderErrorKind};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum McpAdapterErrorKind {
    InvalidConfig,
    Spawn,
    Io,
    Timeout,
    Protocol,
    Rpc,
    Tool,
    Manifest,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct McpAdapterError {
    pub kind: McpAdapterErrorKind,
    pub message: String,
    pub retryable: bool,
}

impl McpAdapterError {
    pub(crate) fn new(kind: McpAdapterErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            retryable: false,
        }
    }

    pub(crate) fn retryable(mut self, retryable: bool) -> Self {
        self.retryable = retryable;
        self
    }

    pub(crate) fn provider_error(&self) -> ProviderError {
        let kind = match self.kind {
            McpAdapterErrorKind::InvalidConfig | McpAdapterErrorKind::Manifest => {
                ProviderErrorKind::InvalidRequest
            }
            McpAdapterErrorKind::Timeout => ProviderErrorKind::Timeout,
            McpAdapterErrorKind::Spawn | McpAdapterErrorKind::Io => ProviderErrorKind::Unavailable,
            McpAdapterErrorKind::Protocol | McpAdapterErrorKind::Rpc => ProviderErrorKind::Protocol,
            McpAdapterErrorKind::Tool => ProviderErrorKind::Internal,
        };
        ProviderError::new(kind, self.to_string()).retryable(self.retryable)
    }
}

impl fmt::Display for McpAdapterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for McpAdapterError {}
