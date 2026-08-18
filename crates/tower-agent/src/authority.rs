use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use crate::{AgentError, EffectState, ErrorKind, FailurePhase, Turn};

/// Portable filesystem authority requested by one turn.
///
/// The ordering is intentional: a host ceiling may authorize a request only
/// when the requested level is less than or equal to the ceiling.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum FilesystemAuthority {
    /// The provider may inspect the filesystem but must not modify it.
    #[default]
    ReadOnly,
    /// The provider may modify its workspace and explicitly granted roots.
    WorkspaceWrite,
    /// The provider is not constrained by a filesystem sandbox.
    FullAccess,
}

impl FilesystemAuthority {
    /// Whether this host ceiling permits the requested authority.
    pub const fn allows(self, requested: Self) -> bool {
        requested as u8 <= self as u8
    }

    /// The authority no broader than either input.
    pub const fn intersect(self, other: Self) -> Self {
        if (self as u8) <= (other as u8) {
            self
        } else {
            other
        }
    }
}

impl fmt::Display for FilesystemAuthority {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ReadOnly => "read_only",
            Self::WorkspaceWrite => "workspace_write",
            Self::FullAccess => "full_access",
        })
    }
}

/// Provider options that carry a portable filesystem-authority request.
pub trait RequestsFilesystemAuthority {
    fn filesystem_authority(&self) -> FilesystemAuthority;

    /// Extra roots requested for the provider invocation.
    fn additional_filesystem_roots(&self) -> &[PathBuf] {
        &[]
    }
}

/// Host-owned ceiling and writable-root policy.
///
/// Explicit roots are canonicalized when configured. Under
/// [`FilesystemAuthority::WorkspaceWrite`], an explicit working directory or
/// additional root must resolve beneath one of them. An omitted working
/// directory remains host-owned ambient configuration. Full access is never
/// path-contained and therefore requires an explicit full-access ceiling.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthorityPolicy {
    filesystem_ceiling: FilesystemAuthority,
    writable_roots: Vec<PathBuf>,
}

impl AuthorityPolicy {
    pub const fn new(filesystem_ceiling: FilesystemAuthority) -> Self {
        Self {
            filesystem_ceiling,
            writable_roots: Vec::new(),
        }
    }

    pub const fn read_only() -> Self {
        Self::new(FilesystemAuthority::ReadOnly)
    }

    pub const fn filesystem_ceiling(&self) -> FilesystemAuthority {
        self.filesystem_ceiling
    }

    pub fn writable_roots(&self) -> &[PathBuf] {
        &self.writable_roots
    }

    /// Add a host-approved writable root.
    pub fn allow_writable_root(mut self, root: impl AsRef<Path>) -> io::Result<Self> {
        let root = std::fs::canonicalize(root)?;
        if !self.writable_roots.contains(&root) {
            self.writable_roots.push(root);
        }
        Ok(self)
    }

    /// Validate a portable request without broadening it.
    pub fn authorize<O>(&self, turn: &Turn<O>) -> Result<(), AgentError>
    where
        O: RequestsFilesystemAuthority,
    {
        let requested = turn.options.filesystem_authority();
        if !self.filesystem_ceiling.allows(requested) {
            return Err(unauthorized(format!(
                "requested filesystem authority {requested} exceeds host ceiling {}",
                self.filesystem_ceiling
            )));
        }

        if requested != FilesystemAuthority::WorkspaceWrite {
            return Ok(());
        }

        if let Some(directory) = turn.working_directory.as_deref() {
            self.authorize_writable_root(directory)?;
        }
        for directory in turn.options.additional_filesystem_roots() {
            self.authorize_writable_root(directory)?;
        }
        Ok(())
    }

    fn authorize_writable_root(&self, requested: &Path) -> Result<(), AgentError> {
        let canonical = std::fs::canonicalize(requested).map_err(|error| {
            AgentError::invalid_request(format!(
                "requested filesystem root {} cannot be resolved: {error}",
                requested.display()
            ))
        })?;
        if self
            .writable_roots
            .iter()
            .any(|allowed| canonical.starts_with(allowed))
        {
            Ok(())
        } else {
            Err(unauthorized(format!(
                "requested writable root {} is outside host policy",
                requested.display()
            )))
        }
    }
}

impl Default for AuthorityPolicy {
    fn default() -> Self {
        Self::read_only()
    }
}

fn unauthorized(message: impl Into<String>) -> AgentError {
    AgentError::new(
        ErrorKind::Unauthorized,
        message,
        FailurePhase::Validation,
        EffectState::None,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug)]
    struct Options {
        authority: FilesystemAuthority,
        roots: Vec<PathBuf>,
    }

    impl RequestsFilesystemAuthority for Options {
        fn filesystem_authority(&self) -> FilesystemAuthority {
            self.authority
        }

        fn additional_filesystem_roots(&self) -> &[PathBuf] {
            &self.roots
        }
    }

    #[test]
    fn intersection_never_broadens_either_side() {
        assert_eq!(
            FilesystemAuthority::WorkspaceWrite.intersect(FilesystemAuthority::ReadOnly),
            FilesystemAuthority::ReadOnly
        );
        assert_eq!(
            FilesystemAuthority::FullAccess.intersect(FilesystemAuthority::WorkspaceWrite),
            FilesystemAuthority::WorkspaceWrite
        );
    }

    #[test]
    fn workspace_write_is_contained_to_host_roots() {
        let crate_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let allowed = crate_root.join("src");
        let policy = AuthorityPolicy::new(FilesystemAuthority::WorkspaceWrite)
            .allow_writable_root(&allowed)
            .expect("source directory exists");
        let allowed_turn = Turn::new("edit")
            .with_options(Options {
                authority: FilesystemAuthority::WorkspaceWrite,
                roots: vec![allowed.join("layer")],
            })
            .in_directory(&allowed);

        policy
            .authorize(&allowed_turn)
            .expect("a descendant of the approved root is authorized");

        let denied_turn = Turn::new("edit")
            .with_options(Options {
                authority: FilesystemAuthority::WorkspaceWrite,
                roots: Vec::new(),
            })
            .in_directory(crate_root.join("../tower-agent-codex"));
        let error = policy
            .authorize(&denied_turn)
            .expect_err("a sibling crate is outside the approved root");
        assert_eq!(error.kind, ErrorKind::Unauthorized);
        assert_eq!(error.effects, EffectState::None);
    }
}
