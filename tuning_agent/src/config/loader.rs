use std::fs;
use std::path::Path;

use super::model::Config;

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
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[test]
    fn load_validates_file_configuration() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "tuning-agent-config-{}-{unique}.toml",
            std::process::id()
        ));
        fs::write(&path, "[reasoning]\nmax_rounds = 0\n").unwrap();

        let error = Config::load(Some(&path)).unwrap_err();

        assert_eq!(error, "reasoning.max_rounds must be between 1 and 64");
        let _ = fs::remove_file(path);
    }
}
