use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::Deserialize;

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub llm: LlmConfig,
    pub reasoning: ReasoningConfig,
    pub safety: SafetyConfig,
    pub activation: ActivationConfig,
    pub audit: AuditConfig,
    pub transaction: TransactionConfig,
    pub skills: SkillConfig,
    pub capabilities: CapabilityConfig,
    pub mcp: McpConfig,
}

impl Config {
    pub fn validate(&self) -> Result<(), String> {
        if self.reasoning.max_rounds == 0 || self.reasoning.max_rounds > 64 {
            return Err("reasoning.max_rounds must be between 1 and 64".to_string());
        }
        if self.llm.timeout_ms == 0 || self.llm.timeout_ms > 300_000 {
            return Err("llm.timeout_ms must be between 1 and 300000".to_string());
        }
        if self.llm.retry_count > 10 {
            return Err("llm.retry_count must not exceed 10".to_string());
        }
        self.safety.validate()?;
        if self.activation.socket_path.as_os_str().is_empty() {
            return Err("activation.socket_path must not be empty".to_string());
        }
        if self.audit.path.as_os_str().is_empty() {
            return Err("audit.path must not be empty".to_string());
        }
        self.transaction.validate()?;
        self.skills.validate()?;
        self.capabilities.validate()?;
        self.mcp.validate()
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SafetyConfig {
    /// Total monotonic budget for the trusted A/B evaluation protocol.
    pub evaluation_timeout_ms: u64,
    /// Minimum delay before the Activation Kernel accepts another episode.
    pub cooldown_ms: u64,
}

impl Default for SafetyConfig {
    fn default() -> Self {
        Self {
            evaluation_timeout_ms: 600_000,
            cooldown_ms: 30_000,
        }
    }
}

impl SafetyConfig {
    fn validate(&self) -> Result<(), String> {
        if self.evaluation_timeout_ms == 0 || self.evaluation_timeout_ms > 3_600_000 {
            return Err("safety.evaluation_timeout_ms must be between 1 and 3600000".to_string());
        }
        if self.cooldown_ms > 86_400_000 {
            return Err("safety.cooldown_ms must not exceed 86400000".to_string());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
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
#[serde(default, deny_unknown_fields)]
pub struct ReasoningConfig {
    pub max_rounds: usize,
}

impl Default for ReasoningConfig {
    fn default() -> Self {
        Self { max_rounds: 4 }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ActivationConfig {
    pub socket_path: PathBuf,
    pub timer_interval_ms: Option<u64>,
}

impl Default for ActivationConfig {
    fn default() -> Self {
        Self {
            socket_path: PathBuf::from("/tmp/tuning-agent.sock"),
            timer_interval_ms: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
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
#[serde(default, deny_unknown_fields)]
pub struct TransactionConfig {
    pub wal_dir: PathBuf,
}

impl Default for TransactionConfig {
    fn default() -> Self {
        Self {
            wal_dir: PathBuf::from("state/transactions"),
        }
    }
}

impl TransactionConfig {
    fn validate(&self) -> Result<(), String> {
        if self.wal_dir.as_os_str().is_empty() {
            return Err("transaction.wal_dir must not be empty".to_string());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SkillConfig {
    pub enabled: bool,
    pub roots: Vec<PathBuf>,
    pub max_skills: usize,
    pub max_catalog_chars: usize,
    pub max_loaded_skills: usize,
    pub max_skill_rounds: usize,
    pub max_reference_reads: usize,
    pub max_skill_bytes: usize,
    pub max_reference_bytes: usize,
    pub max_references_per_skill: usize,
    pub max_registry_bytes: usize,
}

impl Default for SkillConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            roots: Vec::new(),
            max_skills: 64,
            max_catalog_chars: 8_000,
            max_loaded_skills: 4,
            max_skill_rounds: 4,
            max_reference_reads: 8,
            max_skill_bytes: 128 * 1024,
            max_reference_bytes: 256 * 1024,
            max_references_per_skill: 128,
            max_registry_bytes: 16 * 1024 * 1024,
        }
    }
}

impl SkillConfig {
    fn validate(&self) -> Result<(), String> {
        if self.enabled && self.roots.is_empty() {
            return Err("skills.roots must not be empty when skills are enabled".to_string());
        }
        let mut roots = BTreeSet::new();
        for root in &self.roots {
            if !root.is_absolute() {
                return Err(format!(
                    "skills root '{}' must be an absolute path",
                    root.display()
                ));
            }
            if !roots.insert(root) {
                return Err(format!(
                    "skills.roots contains duplicate path '{}'",
                    root.display()
                ));
            }
        }
        validate_bounded("skills.max_skills", self.max_skills, 1, 256)?;
        validate_bounded(
            "skills.max_catalog_chars",
            self.max_catalog_chars,
            256,
            65_536,
        )?;
        validate_bounded("skills.max_loaded_skills", self.max_loaded_skills, 1, 32)?;
        validate_bounded("skills.max_skill_rounds", self.max_skill_rounds, 1, 32)?;
        validate_bounded(
            "skills.max_reference_reads",
            self.max_reference_reads,
            1,
            256,
        )?;
        validate_bounded(
            "skills.max_skill_bytes",
            self.max_skill_bytes,
            1_024,
            1024 * 1024,
        )?;
        validate_bounded(
            "skills.max_reference_bytes",
            self.max_reference_bytes,
            1_024,
            2 * 1024 * 1024,
        )?;
        validate_bounded(
            "skills.max_references_per_skill",
            self.max_references_per_skill,
            1,
            1_024,
        )?;
        validate_bounded(
            "skills.max_registry_bytes",
            self.max_registry_bytes,
            self.max_skill_bytes,
            256 * 1024 * 1024,
        )
    }
}

fn validate_bounded(
    field: &str,
    value: usize,
    minimum: usize,
    maximum: usize,
) -> Result<(), String> {
    if value < minimum || value > maximum {
        return Err(format!("{field} must be between {minimum} and {maximum}"));
    }
    Ok(())
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CapabilityConfig {
    /// Empty means that all capabilities permitted by the runtime policy are eligible.
    pub allowed_capabilities: Vec<String>,
    pub denied_capabilities: Vec<String>,
    pub local_mutations: Vec<LocalMutationConfig>,
}

impl CapabilityConfig {
    fn validate(&self) -> Result<(), String> {
        validate_names(
            "capabilities.allowed_capabilities",
            &self.allowed_capabilities,
        )?;
        validate_names(
            "capabilities.denied_capabilities",
            &self.denied_capabilities,
        )?;

        let allowed = self.allowed_capabilities.iter().collect::<BTreeSet<_>>();
        if let Some(capability) = self
            .denied_capabilities
            .iter()
            .find(|capability| allowed.contains(capability))
        {
            return Err(format!(
                "capability '{capability}' is present in both allow and deny lists"
            ));
        }
        let mut mutation_ids = BTreeSet::new();
        if self.local_mutations.len() > 256 {
            return Err("capabilities.local_mutations must not exceed 256 entries".to_string());
        }
        for mutation in &self.local_mutations {
            mutation.validate()?;
            if !mutation_ids.insert(&mutation.id) {
                return Err(format!(
                    "capabilities.local_mutations contains duplicate id '{}'",
                    mutation.id
                ));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct LocalMutationConfig {
    pub id: String,
    pub description: String,
    #[serde(flatten)]
    pub target: LocalMutationTargetConfig,
}

impl LocalMutationConfig {
    fn validate(&self) -> Result<(), String> {
        validate_name("local mutation id", &self.id)?;
        if self.description.trim().is_empty()
            || self.description.len() > 4096
            || self.description.chars().any(char::is_control)
        {
            return Err(format!(
                "local mutation '{}' requires a valid description of at most 4096 bytes",
                self.id
            ));
        }
        self.target.validate(&self.id)
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum LocalMutationTargetConfig {
    Sysctl { key: String },
    ProcSys { path: PathBuf },
    Sysfs { path: PathBuf },
    Cgroup { path: PathBuf },
}

impl LocalMutationTargetConfig {
    fn validate(&self, id: &str) -> Result<(), String> {
        match self {
            Self::Sysctl { key } => {
                if key.is_empty()
                    || key.contains('/')
                    || key.split('.').any(|part| {
                        part.is_empty()
                            || !part.bytes().all(|byte| {
                                byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')
                            })
                    })
                {
                    return Err(format!("local mutation '{id}' has an invalid sysctl key"));
                }
            }
            Self::ProcSys { path } => validate_target_path(id, path, "/proc/sys")?,
            Self::Sysfs { path } => validate_target_path(id, path, "/sys")?,
            Self::Cgroup { path } => validate_target_path(id, path, "/sys/fs/cgroup")?,
        }
        Ok(())
    }
}

fn validate_target_path(id: &str, path: &std::path::Path, root: &str) -> Result<(), String> {
    if !path.is_absolute() || !path.starts_with(root) {
        return Err(format!(
            "local mutation '{id}' path must be absolute and beneath '{root}'"
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct McpConfig {
    pub enabled: bool,
    pub servers: Vec<McpServerConfig>,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            servers: Vec::new(),
        }
    }
}

impl McpConfig {
    fn validate(&self) -> Result<(), String> {
        if self.servers.len() > 32 {
            return Err("mcp.servers must not exceed 32 entries".to_string());
        }
        let mut ids = BTreeSet::new();
        for server in &self.servers {
            validate_name("mcp server id", &server.id)?;
            if !ids.insert(server.id.as_str()) {
                return Err(format!("duplicate mcp server id '{}'", server.id));
            }
            server.validate()?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct McpServerConfig {
    pub id: String,
    pub enabled: bool,
    /// Executable for the MCP stdio transport. It is never passed through a shell.
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub request_timeout_ms: u64,
    /// Empty means that the server manifest is filtered only by global policy.
    pub allowed_capabilities: Vec<String>,
    /// MCP mutation providers require an explicit per-server opt-in.
    pub allow_mutations: bool,
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            id: String::new(),
            enabled: true,
            command: String::new(),
            args: Vec::new(),
            env: BTreeMap::new(),
            request_timeout_ms: 30_000,
            allowed_capabilities: Vec::new(),
            allow_mutations: false,
        }
    }
}

impl McpServerConfig {
    fn validate(&self) -> Result<(), String> {
        if self.enabled && self.command.trim().is_empty() {
            return Err(format!(
                "mcp server '{}' requires a non-empty command when enabled",
                self.id
            ));
        }
        if self.enabled && !std::path::Path::new(&self.command).is_absolute() {
            return Err(format!(
                "mcp server '{}' command must be an absolute path",
                self.id
            ));
        }
        if self.enabled && (self.request_timeout_ms == 0 || self.request_timeout_ms > 300_000) {
            return Err(format!(
                "mcp server '{}'.request_timeout_ms must be between 1 and 300000",
                self.id
            ));
        }
        if self.args.len() > 256 || self.env.len() > 128 {
            return Err(format!(
                "mcp server '{}' has too many arguments or environment variables",
                self.id
            ));
        }
        validate_names(
            "mcp server allowed_capabilities",
            &self.allowed_capabilities,
        )?;
        for key in self.env.keys() {
            if key.is_empty() || key.contains('=') || key.chars().any(char::is_control) {
                return Err(format!(
                    "mcp server '{}' contains invalid environment key '{key}'",
                    self.id
                ));
            }
        }
        Ok(())
    }
}

fn validate_names(field: &str, names: &[String]) -> Result<(), String> {
    let mut seen = BTreeSet::new();
    for name in names {
        validate_name(field, name)?;
        if !seen.insert(name) {
            return Err(format!("{field} contains duplicate value '{name}'"));
        }
    }
    Ok(())
}

fn validate_name(field: &str, name: &str) -> Result<(), String> {
    if name.is_empty()
        || name.len() > 256
        || name.trim() != name
        || name.chars().any(char::is_control)
    {
        return Err(format!("{field} contains invalid value '{name}'"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_configuration_keeps_new_sections_at_safe_defaults() {
        let config: Config = toml::from_str(
            r#"
                [reasoning]
                max_rounds = 8

            "#,
        )
        .unwrap();

        assert_eq!(config.reasoning.max_rounds, 8);
        assert_eq!(
            config.transaction.wal_dir,
            PathBuf::from("state/transactions")
        );
        assert!(config.mcp.enabled);
        assert!(config.mcp.servers.is_empty());
        assert!(!config.skills.enabled);
        assert!(config.skills.roots.is_empty());
        assert_eq!(config.safety.evaluation_timeout_ms, 600_000);
        assert_eq!(config.safety.cooldown_ms, 30_000);
        config.validate().unwrap();
    }

    #[test]
    fn zero_reasoning_rounds_are_rejected() {
        let mut config = Config::default();
        config.reasoning.max_rounds = 0;
        assert_eq!(
            config.validate().unwrap_err(),
            "reasoning.max_rounds must be between 1 and 64"
        );
    }

    #[test]
    fn zero_evaluation_budget_is_rejected() {
        let mut config = Config::default();
        config.safety.evaluation_timeout_ms = 0;

        assert_eq!(
            config.validate().unwrap_err(),
            "safety.evaluation_timeout_ms must be between 1 and 3600000"
        );
    }

    #[test]
    fn capability_cannot_be_both_allowed_and_denied() {
        let mut config = Config::default();
        config.capabilities.allowed_capabilities = vec!["linux/probe.psi".to_string()];
        config.capabilities.denied_capabilities = vec!["linux/probe.psi".to_string()];

        assert!(config
            .validate()
            .unwrap_err()
            .contains("both allow and deny"));
    }

    #[test]
    fn enabled_mcp_server_requires_command_and_unique_id() {
        let mut config = Config::default();
        config.mcp.servers = vec![McpServerConfig {
            id: "scxtop".to_string(),
            ..McpServerConfig::default()
        }];
        assert!(config.validate().unwrap_err().contains("non-empty command"));

        config.mcp.servers = vec![
            McpServerConfig {
                id: "scxtop".to_string(),
                command: "/usr/bin/scxtop-mcp".to_string(),
                ..McpServerConfig::default()
            },
            McpServerConfig {
                id: "scxtop".to_string(),
                command: "/usr/bin/other-mcp".to_string(),
                ..McpServerConfig::default()
            },
        ];
        assert!(config
            .validate()
            .unwrap_err()
            .contains("duplicate mcp server id"));
    }

    #[test]
    fn obsolete_v1_sections_are_rejected_instead_of_silently_ignored() {
        let error = toml::from_str::<Config>(
            r#"
                [command]
                timeout_ms = 1000
            "#,
        )
        .unwrap_err();
        assert!(error.to_string().contains("unknown field"));
    }

    #[test]
    fn enabled_skills_require_absolute_unique_roots() {
        let mut config = Config::default();
        config.skills.enabled = true;
        assert!(config.validate().unwrap_err().contains("must not be empty"));

        config.skills.roots = vec![PathBuf::from("relative")];
        assert!(config.validate().unwrap_err().contains("absolute path"));

        config.skills.roots = vec![PathBuf::from("/skills"), PathBuf::from("/skills")];
        assert!(config.validate().unwrap_err().contains("duplicate path"));
    }

    #[test]
    fn local_mutation_requires_an_explicit_bounded_target() {
        let config: Config = toml::from_str(
            r#"
                [[capabilities.local_mutations]]
                id = "local/vm-dirty-ratio"
                description = "Tune VM dirty ratio"
                kind = "sysctl"
                key = "vm.dirty_ratio"
            "#,
        )
        .unwrap();
        config.validate().unwrap();
        assert_eq!(config.capabilities.local_mutations.len(), 1);
    }

    #[test]
    fn local_mutation_rejects_unknown_target_fields() {
        let error = toml::from_str::<Config>(
            r#"
                [[capabilities.local_mutations]]
                id = "local/vm-dirty-ratio"
                description = "Tune VM dirty ratio"
                kind = "sysctl"
                key = "vm.dirty_ratio"
                command = "unsafe fallback"
            "#,
        )
        .unwrap_err();

        assert!(error.to_string().contains("unknown field `command`"));
    }
}
