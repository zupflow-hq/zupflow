//! Sandbox and Production: data, keys and configurations of one never serve the other.

use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Environment {
    Sandbox,
    Production,
}

impl Environment {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "sandbox" => Some(Self::Sandbox),
            "production" => Some(Self::Production),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sandbox => "sandbox",
            Self::Production => "production",
        }
    }

    /// The environment as people read it: `Sandbox`, `Production`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Sandbox => "Sandbox",
            Self::Production => "Production",
        }
    }
}

impl fmt::Display for Environment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
