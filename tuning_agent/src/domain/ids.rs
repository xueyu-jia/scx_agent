use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

macro_rules! string_id {
    ($name:ident, $label:literal, test_as_str) => {
        string_id!($name, $label);

        #[cfg(test)]
        impl $name {
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
    };
    ($name:ident, $label:literal, as_str) => {
        string_id!($name, $label);

        impl $name {
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
    };
    ($name:ident, $label:literal) => {
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, String> {
                let value = value.into();
                validate_identifier($label, &value, 256)?;
                Ok(Self(value))
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl FromStr for $name {
            type Err = String;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::new(value)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::new(value).map_err(serde::de::Error::custom)
            }
        }

        impl TryFrom<String> for $name {
            type Error = String;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl TryFrom<&str> for $name {
            type Error = String;

            fn try_from(value: &str) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }
    };
}

string_id!(CapabilityId, "capability id", as_str);
string_id!(ChangeId, "change id");
string_id!(ContractId, "contract id", as_str);
string_id!(Digest, "digest", test_as_str);
string_id!(MeasurementSessionId, "measurement session id");
string_id!(OperationId, "operation id", as_str);
string_id!(ProviderId, "provider id", as_str);
string_id!(ProviderVersion, "provider version");
string_id!(TransactionId, "transaction id", as_str);
string_id!(CommitId, "commit id");

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EpisodeId(u64);

impl EpisodeId {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for EpisodeId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ResourceKey(String);

impl ResourceKey {
    pub fn new(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        validate_identifier("resource key", &value, 4096)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ResourceKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for ResourceKey {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl<'de> Deserialize<'de> for ResourceKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

fn validate_identifier(label: &str, value: &str, max_len: usize) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("{label} must not be empty"));
    }
    if value.trim() != value {
        return Err(format!("{label} must not have surrounding whitespace"));
    }
    if value.len() > max_len {
        return Err(format!("{label} exceeds {max_len} bytes"));
    }
    if value.chars().any(char::is_control) {
        return Err(format!("{label} must not contain control characters"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn string_ids_reject_ambiguous_values() {
        assert!(CapabilityId::new("").is_err());
        assert!(CapabilityId::new(" probe").is_err());
        assert!(CapabilityId::new("probe\nother").is_err());
        assert_eq!(
            CapabilityId::new("linux/probe.psi").unwrap().as_str(),
            "linux/probe.psi"
        );
    }

    #[test]
    fn deserialization_enforces_identifier_validation() {
        assert!(serde_json::from_str::<CapabilityId>("\"\"").is_err());
        assert!(serde_json::from_str::<ChangeId>("\"bad\\nvalue\"").is_err());
        assert!(serde_json::from_value::<ResourceKey>(serde_json::json!(" x")).is_err());
        assert!(serde_json::from_value::<Digest>(serde_json::json!("x".repeat(257))).is_err());
    }
}
