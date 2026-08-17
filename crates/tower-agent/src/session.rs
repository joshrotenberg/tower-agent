use std::fmt;

/// Provider-tagged evidence that a later turn may resume prior context.
///
/// The kernel does not mint public session IDs or persist handles. A host may
/// translate this value into its own stable identifier before exposing it.
#[derive(Clone, PartialEq, Eq)]
pub struct SessionHandle {
    provider: String,
    value: String,
}

impl SessionHandle {
    pub fn new(provider: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            value: value.into(),
        }
    }

    pub fn provider(&self) -> &str {
        &self.provider
    }

    pub fn value(&self) -> &str {
        &self.value
    }
}

impl fmt::Debug for SessionHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionHandle")
            .field("provider", &self.provider)
            .field("value", &"[redacted]")
            .finish()
    }
}
