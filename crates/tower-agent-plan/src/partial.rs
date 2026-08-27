use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tower_agent::{FilesystemAuthority, SessionHandle};

use crate::ProviderId;

/// A possibly incomplete set of turn bindings.
///
/// Defaults, profiles, explicit requests, and elicited answers all use this
/// one shape. Every field distinguishes "unbound at this layer" from a bound
/// value: an empty string, an empty list, and `false` are real bindings, and
/// only `None` means a layer says nothing about a path.
///
/// This is the wire-shaped planning DTO, not the portable execution body. A
/// complete resolution folds into the kernel's `Turn<O>` through a provider
/// planner; the kernel types never gain serde obligations from this crate.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PartialTurn {
    /// Selected provider. A profile/explicit mismatch is invalid, never an
    /// implicit conversion.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<ProviderId>,
    /// Primary user input for the turn.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// Shared model options.
    #[serde(skip_serializing_if = "PartialModel::is_unbound")]
    pub model: PartialModel,
    /// Shared execution-context options.
    #[serde(skip_serializing_if = "PartialContext::is_unbound")]
    pub context: PartialContext,
    /// Shared permission options.
    #[serde(skip_serializing_if = "PartialPermissions::is_unbound")]
    pub permissions: PartialPermissions,
    /// Provider-specific option groups.
    #[serde(skip_serializing_if = "PartialProviderOptions::is_unbound")]
    pub provider_options: PartialProviderOptions,
}

impl PartialTurn {
    /// Fold a higher-precedence layer into this one.
    ///
    /// Scalars replace when the layer binds them, groups merge by field, and
    /// bound lists replace lower layers whole.
    pub(crate) fn merge_from(&mut self, layer: &Self) {
        if layer.provider.is_some() {
            self.provider = layer.provider;
        }
        if layer.prompt.is_some() {
            self.prompt.clone_from(&layer.prompt);
        }
        self.model.merge_from(&layer.model);
        self.context.merge_from(&layer.context);
        self.permissions.merge_from(&layer.permissions);
        self.provider_options.merge_from(&layer.provider_options);
    }
}

/// Provider-specific option groups.
///
/// Each group exists only when its provider feature is enabled, and each
/// group merges fieldwise like the shared groups. A setting the shared
/// vocabulary already carries never appears in a group, so no concrete
/// option field is reachable from two planning paths.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PartialProviderOptions {
    #[cfg(feature = "claude")]
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Claude-specific values, merged only when Claude is selected.
    pub claude: Option<crate::claude::PartialClaudeOptions>,
    #[cfg(feature = "codex")]
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Codex-specific values, merged only when Codex is selected.
    pub codex: Option<crate::codex::PartialCodexOptions>,
}

impl PartialProviderOptions {
    /// Whether no group in this layer is bound.
    pub fn is_unbound(&self) -> bool {
        #[cfg(feature = "claude")]
        if self.claude.is_some() {
            return false;
        }
        #[cfg(feature = "codex")]
        if self.codex.is_some() {
            return false;
        }
        true
    }

    fn merge_from(&mut self, layer: &Self) {
        #[cfg(feature = "claude")]
        match (&mut self.claude, &layer.claude) {
            (Some(current), Some(incoming)) => current.merge_from(incoming),
            (current @ None, Some(incoming)) => *current = Some(incoming.clone()),
            _ => {}
        }
        #[cfg(feature = "codex")]
        match (&mut self.codex, &layer.codex) {
            (Some(current), Some(incoming)) => current.merge_from(incoming),
            (current @ None, Some(incoming)) => *current = Some(incoming.clone()),
            _ => {}
        }
        #[cfg(not(any(feature = "claude", feature = "codex")))]
        let _ = layer;
    }
}

/// Shared model options common to every provider.
///
/// Provider-specific model behavior such as reasoning effort stays in the
/// provider option mirrors rather than pretending to be portable.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PartialModel {
    /// Provider-native model name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl PartialModel {
    /// Whether no field in this group is bound.
    pub fn is_unbound(&self) -> bool {
        self.name.is_none()
    }

    fn merge_from(&mut self, layer: &Self) {
        if layer.name.is_some() {
            self.name.clone_from(&layer.name);
        }
    }
}

/// Shared execution-context options.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PartialContext {
    /// Working directory for the turn.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    /// Extra directories made available to the turn. A bound list replaces
    /// lower layers whole; a bound empty list is a real value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub additional_directories: Option<Vec<PathBuf>>,
    /// Provider-tagged continuation of a prior turn.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume: Option<ResumeBinding>,
}

impl PartialContext {
    /// Whether no field in this group is bound.
    pub fn is_unbound(&self) -> bool {
        self.cwd.is_none() && self.additional_directories.is_none() && self.resume.is_none()
    }

    fn merge_from(&mut self, layer: &Self) {
        if layer.cwd.is_some() {
            self.cwd.clone_from(&layer.cwd);
        }
        if layer.additional_directories.is_some() {
            self.additional_directories
                .clone_from(&layer.additional_directories);
        }
        if layer.resume.is_some() {
            self.resume.clone_from(&layer.resume);
        }
    }
}

/// Shared permission options.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PartialPermissions {
    /// Requested portable filesystem authority.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filesystem: Option<FilesystemChoice>,
}

impl PartialPermissions {
    /// Whether no field in this group is bound.
    pub fn is_unbound(&self) -> bool {
        self.filesystem.is_none()
    }

    fn merge_from(&mut self, layer: &Self) {
        if layer.filesystem.is_some() {
            self.filesystem = layer.filesystem;
        }
    }
}

/// Wire form of the kernel's portable filesystem-authority request.
///
/// Whether a provider honors the request stays honor-or-refuse in the
/// provider planner and the adapter; the planner never narrows it silently.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilesystemChoice {
    /// Inspect the filesystem without modifying it.
    ReadOnly,
    /// Write within the working directory.
    WorkspaceWrite,
    /// Write anywhere the host permits.
    FullAccess,
}

impl From<FilesystemChoice> for FilesystemAuthority {
    fn from(choice: FilesystemChoice) -> Self {
        match choice {
            FilesystemChoice::ReadOnly => Self::ReadOnly,
            FilesystemChoice::WorkspaceWrite => Self::WorkspaceWrite,
            FilesystemChoice::FullAccess => Self::FullAccess,
        }
    }
}

impl From<FilesystemAuthority> for FilesystemChoice {
    fn from(authority: FilesystemAuthority) -> Self {
        match authority {
            FilesystemAuthority::ReadOnly => Self::ReadOnly,
            FilesystemAuthority::WorkspaceWrite => Self::WorkspaceWrite,
            FilesystemAuthority::FullAccess => Self::FullAccess,
        }
    }
}

/// A provider-tagged resume value carried by planning layers.
///
/// This is deliberately the raw provider value in v1: profiles and explicit
/// requests may carry it, resolution checks its tag against the resolved
/// provider, and the adapters re-validate the value at launch. A host-minted
/// continuation reference is the intended refinement once host-managed
/// continuation exists. `Debug` redacts the value to match `SessionHandle`
/// discipline; serialization carries it by design.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResumeBinding {
    /// Provider that minted the resumed session.
    pub provider: ProviderId,
    value: String,
}

impl ResumeBinding {
    /// Bind a resume value to the provider that minted it.
    pub fn new(provider: ProviderId, value: impl Into<String>) -> Self {
        Self {
            provider,
            value: value.into(),
        }
    }

    /// The resume value. Redacted in `Debug`.
    pub fn value(&self) -> &str {
        &self.value
    }

    /// The kernel session handle this binding names.
    ///
    /// The adapters still enforce their own handle checks at launch.
    pub fn to_session_handle(&self) -> SessionHandle {
        SessionHandle::new(self.provider.as_str(), self.value.as_str())
    }
}

impl fmt::Debug for ResumeBinding {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResumeBinding")
            .field("provider", &self.provider)
            .field("value", &"[redacted]")
            .finish()
    }
}

/// A named, saved [`PartialTurn`].
///
/// A profile may be complete, but completeness is not part of its identity.
/// The requirements remaining after resolution are its effective signature.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    /// The name this profile is saved under.
    pub name: String,
    /// The partial turn this profile supplies.
    pub turn: PartialTurn,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_redacts_resume_value() {
        let binding = ResumeBinding::new(ProviderId::Codex, "0199a213-secret");
        let rendered = format!("{binding:?}");
        assert!(!rendered.contains("0199a213-secret"));
        assert!(rendered.contains("[redacted]"));
    }

    #[test]
    fn session_handle_carries_provider_tag() {
        let binding = ResumeBinding::new(ProviderId::Claude, "abc");
        let handle = binding.to_session_handle();
        assert_eq!(handle.provider(), "claude");
        assert_eq!(handle.value(), "abc");
    }

    #[test]
    fn groups_merge_by_field() {
        let mut base = PartialTurn {
            model: PartialModel {
                name: Some("base-model".to_string()),
            },
            context: PartialContext {
                cwd: Some(PathBuf::from("/repos/base")),
                ..Default::default()
            },
            ..Default::default()
        };
        let layer = PartialTurn {
            model: PartialModel {
                name: Some("layer-model".to_string()),
            },
            ..Default::default()
        };

        base.merge_from(&layer);
        assert_eq!(base.model.name.as_deref(), Some("layer-model"));
        assert_eq!(
            base.context.cwd.as_deref(),
            Some(std::path::Path::new("/repos/base"))
        );
    }
}
