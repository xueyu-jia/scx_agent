use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

#[derive(Clone, Debug)]
pub struct CommandRequest {
    pub command: String,
    pub working_dir: Option<String>,
    pub timeout: Option<Duration>,
}

impl CommandRequest {
    pub fn new(command: String) -> Self {
        Self {
            command,
            working_dir: None,
            timeout: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WriteTarget {
    Sysctl {
        key: String,
    },
    ProcSys {
        path: PathBuf,
    },
    Sysfs {
        path: PathBuf,
    },
    Cgroup {
        path: PathBuf,
    },
    #[cfg(test)]
    File {
        path: PathBuf,
    },
}

impl WriteTarget {
    pub fn from_json(value: &Value) -> Result<Self, String> {
        let kind = value
            .get("kind")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "write target.kind is required".to_string())?;
        match kind {
            "sysctl" => {
                let key = value
                    .get("key")
                    .and_then(|v| v.as_str())
                    .filter(|v| !v.trim().is_empty())
                    .ok_or_else(|| "write target.key is required for sysctl".to_string())?;
                Ok(Self::Sysctl {
                    key: key.to_string(),
                })
            }
            "proc_sys" => {
                let path = parse_path(value, "path", "proc_sys")?;
                Ok(Self::ProcSys { path })
            }
            "sysfs" => {
                let path = parse_path(value, "path", "sysfs")?;
                Ok(Self::Sysfs { path })
            }
            "cgroup" => {
                let path = parse_path(value, "path", "cgroup")?;
                Ok(Self::Cgroup { path })
            }
            #[cfg(test)]
            "file" => {
                let path = parse_path(value, "path", "file")?;
                Ok(Self::File { path })
            }
            _ => Err(format!("unsupported write target kind '{kind}'")),
        }
    }

    pub fn path(&self) -> PathBuf {
        match self {
            Self::Sysctl { key } => PathBuf::from("/proc/sys").join(key.replace('.', "/")),
            Self::ProcSys { path } | Self::Sysfs { path } | Self::Cgroup { path } => path.clone(),
            #[cfg(test)]
            Self::File { path } => path.clone(),
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Sysctl { key } => validate_sysctl_key(key),
            Self::ProcSys { path } => validate_path_prefix(path, Path::new("/proc/sys")),
            Self::Sysfs { path } => validate_path_prefix(path, Path::new("/sys")),
            Self::Cgroup { path } => validate_path_prefix(path, Path::new("/sys/fs/cgroup")),
            #[cfg(test)]
            Self::File { path } => validate_path_prefix(path, Path::new("/tmp")),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ExperimentWriteRequest {
    pub target: WriteTarget,
    pub value: String,
    pub reason: String,
}

impl ExperimentWriteRequest {
    pub fn from_json(value: &Value) -> Result<Self, String> {
        let target = value
            .get("target")
            .ok_or_else(|| "experiment_write.target is required".to_string())
            .and_then(WriteTarget::from_json)?;
        let new_value = parse_write_value(value, "experiment_write.value")?;
        let reason = value
            .get("reason")
            .and_then(|v| v.as_str())
            .unwrap_or("unspecified")
            .to_string();
        Ok(Self {
            target,
            value: new_value,
            reason,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CommitWrite {
    pub target: WriteTarget,
    pub value: String,
}

impl CommitWrite {
    pub fn from_json(value: &Value) -> Result<Self, String> {
        let target = value
            .get("target")
            .ok_or_else(|| "keep_writes.target is required".to_string())
            .and_then(WriteTarget::from_json)?;
        let value = parse_write_value(value, "keep_writes.value")?;
        Ok(Self { target, value })
    }
}

fn parse_write_value(value: &Value, field: &str) -> Result<String, String> {
    value
        .get("value")
        .and_then(|v| v.as_str())
        .filter(|v| !v.trim().is_empty())
        .map(ToString::to_string)
        .ok_or_else(|| format!("{field} is required"))
}

fn parse_path(value: &Value, field: &str, kind: &str) -> Result<PathBuf, String> {
    let path = value
        .get(field)
        .and_then(|v| v.as_str())
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| format!("write target.{field} is required for {kind}"))?;
    Ok(PathBuf::from(path))
}

fn validate_sysctl_key(key: &str) -> Result<(), String> {
    if key.starts_with('.') || key.ends_with('.') || key.contains("..") {
        return Err(format!("invalid sysctl key '{key}'"));
    }
    if !key
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || ch == '.')
    {
        return Err(format!("invalid sysctl key '{key}'"));
    }
    Ok(())
}

fn validate_path_prefix(path: &Path, prefix: &Path) -> Result<(), String> {
    if !path.is_absolute() {
        return Err(format!("write path '{}' must be absolute", path.display()));
    }
    if !path.starts_with(prefix) {
        return Err(format!(
            "write path '{}' must be under '{}'",
            path.display(),
            prefix.display()
        ));
    }
    Ok(())
}
