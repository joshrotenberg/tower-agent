use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// Host-owned policy for the environment inherited by provider subprocesses.
///
/// The compatibility default inherits the complete host environment. A
/// durable worker should normally start with [`Self::clear`] and rebuild only
/// the variables its provider and tool subprocesses require. Allowlisted
/// variables are copied from the host at call time; explicit variables replace
/// them and are never rendered by `Debug`.
#[derive(Clone, PartialEq, Eq)]
pub struct ChildEnvironmentPolicy {
    inherit_ambient: bool,
    allow_ambient: BTreeSet<String>,
    explicit: BTreeMap<String, String>,
}

impl ChildEnvironmentPolicy {
    /// Preserve the complete host environment, matching provider-wrapper
    /// compatibility behavior. Explicit variables may still override it.
    pub fn inherit() -> Self {
        Self {
            inherit_ambient: true,
            allow_ambient: BTreeSet::new(),
            explicit: BTreeMap::new(),
        }
    }

    /// Clear the inherited environment before rebuilding an allowlisted and
    /// explicit child environment.
    pub fn clear() -> Self {
        Self {
            inherit_ambient: false,
            allow_ambient: BTreeSet::new(),
            explicit: BTreeMap::new(),
        }
    }

    /// Copy one variable from the host environment when this policy is
    /// resolved. Missing variables remain absent. In inherit mode this is
    /// redundant but harmless.
    #[must_use]
    pub fn allow_ambient(mut self, key: impl Into<String>) -> Self {
        self.allow_ambient.insert(key.into());
        self
    }

    /// Set or replace one child variable. Values are redacted from `Debug`.
    #[must_use]
    pub fn with_variable(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.explicit.insert(key.into(), value.into());
        self
    }

    pub const fn inherits_ambient(&self) -> bool {
        self.inherit_ambient
    }

    /// Resolve allowlisted values from the current host environment and
    /// validate all keys and values before a provider is launched.
    pub fn resolve(&self) -> Result<ResolvedChildEnvironment, ChildEnvironmentError> {
        for key in self.allow_ambient.iter().chain(self.explicit.keys()) {
            validate_key(key)?;
        }
        for (key, value) in &self.explicit {
            if value.contains('\0') {
                return Err(ChildEnvironmentError::InvalidValue { key: key.clone() });
            }
        }

        let mut variables = BTreeMap::new();
        if !self.inherit_ambient {
            for key in &self.allow_ambient {
                match std::env::var(key) {
                    Ok(value) => {
                        variables.insert(key.clone(), value);
                    }
                    Err(std::env::VarError::NotPresent) => {}
                    Err(std::env::VarError::NotUnicode(_)) => {
                        return Err(ChildEnvironmentError::NonUnicodeAmbientValue {
                            key: key.clone(),
                        });
                    }
                }
            }
        }
        variables.extend(self.explicit.clone());

        Ok(ResolvedChildEnvironment {
            clear_inherited: !self.inherit_ambient,
            variables,
        })
    }
}

impl Default for ChildEnvironmentPolicy {
    fn default() -> Self {
        Self::inherit()
    }
}

impl fmt::Debug for ChildEnvironmentPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChildEnvironmentPolicy")
            .field("inherit_ambient", &self.inherit_ambient)
            .field("allow_ambient", &self.allow_ambient)
            .field("explicit_keys", &self.explicit.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// Validated environment projection consumed by provider adapters.
pub struct ResolvedChildEnvironment {
    clear_inherited: bool,
    variables: BTreeMap<String, String>,
}

impl ResolvedChildEnvironment {
    pub const fn clear_inherited(&self) -> bool {
        self.clear_inherited
    }

    pub fn variables(&self) -> impl Iterator<Item = (&str, &str)> {
        self.variables
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
    }
}

impl fmt::Debug for ResolvedChildEnvironment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResolvedChildEnvironment")
            .field("clear_inherited", &self.clear_inherited)
            .field("variable_keys", &self.variables.keys().collect::<Vec<_>>())
            .finish()
    }
}

#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
pub enum ChildEnvironmentError {
    #[error("child environment variable name is invalid")]
    InvalidKey,
    #[error("child environment variable {key} contains an invalid value")]
    InvalidValue { key: String },
    #[error("allowlisted host environment variable {key} is not valid UTF-8")]
    NonUnicodeAmbientValue { key: String },
}

fn validate_key(key: &str) -> Result<(), ChildEnvironmentError> {
    if key.is_empty() || key.contains('=') || key.contains('\0') {
        Err(ChildEnvironmentError::InvalidKey)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clear_policy_resolves_explicit_variables_and_redacts_values() {
        let policy = ChildEnvironmentPolicy::clear()
            .with_variable("VISIBLE_KEY", "private-value")
            .allow_ambient("MISSING_TOWER_AGENT_TEST_VALUE");
        let resolved = policy.resolve().expect("valid policy");

        assert!(resolved.clear_inherited());
        assert_eq!(
            resolved.variables().collect::<Vec<_>>(),
            [("VISIBLE_KEY", "private-value")]
        );
        assert!(!format!("{policy:?}").contains("private-value"));
        assert!(!format!("{resolved:?}").contains("private-value"));
    }

    #[test]
    fn invalid_entries_are_refused_before_launch() {
        let invalid_key = ChildEnvironmentPolicy::clear()
            .with_variable("BAD=KEY", "value")
            .resolve()
            .expect_err("invalid key must be refused");
        assert_eq!(invalid_key, ChildEnvironmentError::InvalidKey);

        let invalid_value = ChildEnvironmentPolicy::clear()
            .with_variable("GOOD_KEY", "bad\0value")
            .resolve()
            .expect_err("invalid value must be refused");
        assert_eq!(
            invalid_value,
            ChildEnvironmentError::InvalidValue {
                key: "GOOD_KEY".into()
            }
        );
    }
}
