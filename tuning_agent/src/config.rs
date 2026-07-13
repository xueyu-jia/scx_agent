use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub llm: LlmConfig,
    pub reasoning: ReasoningConfig,
    pub activation: ActivationConfig,
    pub audit: AuditConfig,
    pub command: CommandConfig,
    pub evaluation: EvaluationConfig,
}

impl Config {
    pub fn load(path: Option<&Path>) -> Result<Self, String> {
        match path {
            Some(path) => Self::load_file(path),
            None => {
                let default_path = Path::new("tuning-agent.toml");
                if default_path.exists() {
                    Self::load_file(default_path)
                } else {
                    Ok(Self::default())
                }
            }
        }
    }

    fn load_file(path: &Path) -> Result<Self, String> {
        let content = fs::read_to_string(path)
            .map_err(|err| format!("failed to read config '{}': {err}", path.display()))?;
        let config: Self = toml::from_str(&content)
            .map_err(|err| format!("failed to parse config '{}': {err}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), String> {
        if self.reasoning.max_rounds == 0 {
            return Err("reasoning.max_rounds must be greater than zero".to_string());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct LlmConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub timeout_ms: u64,
    pub retry_count: u32,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            api_key: String::new(),
            model: String::new(),
            timeout_ms: 30_000,
            retry_count: 3,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct ReasoningConfig {
    pub max_rounds: usize,
}

impl Default for ReasoningConfig {
    fn default() -> Self {
        Self { max_rounds: 4 }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct ActivationConfig {
    pub socket_path: PathBuf,
    pub timer_interval_ms: Option<u64>,
    pub ebpf_ringbuf_pin: Option<PathBuf>,
}

impl Default for ActivationConfig {
    fn default() -> Self {
        Self {
            socket_path: PathBuf::from("/tmp/tuning-agent.sock"),
            timer_interval_ms: None,
            ebpf_ringbuf_pin: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct AuditConfig {
    pub path: PathBuf,
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self {
            path: PathBuf::from("logs/audit.jsonl"),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct CommandConfig {
    pub timeout_ms: u64,
    pub output_limit_bytes: usize,
    pub evaluation_output_limit_bytes: usize,
}

impl Default for CommandConfig {
    fn default() -> Self {
        Self {
            timeout_ms: 30_000,
            output_limit_bytes: 65_536,
            evaluation_output_limit_bytes: 8_192,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct EvaluationConfig {
    pub default_window_seconds: u64,
    pub min_window_seconds: u64,
    pub max_window_seconds: u64,
    pub default_settle_seconds: u64,
    pub min_settle_seconds: u64,
    pub max_settle_seconds: u64,
}

impl Default for EvaluationConfig {
    fn default() -> Self {
        Self {
            default_window_seconds: 10,
            min_window_seconds: 3,
            max_window_seconds: 60,
            default_settle_seconds: 3,
            min_settle_seconds: 0,
            max_settle_seconds: 10,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasoning_rounds_default_to_four() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.reasoning.max_rounds, 4);
    }

    #[test]
    fn zero_reasoning_rounds_are_rejected() {
        let mut config = Config::default();
        config.reasoning.max_rounds = 0;
        assert_eq!(
            config.validate().unwrap_err(),
            "reasoning.max_rounds must be greater than zero"
        );
    }
}
