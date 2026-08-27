use std::fmt;

use thiserror::Error;

macro_rules! identifier {
    ($name:ident, $kind:literal, $description:literal) => {
        #[doc = $description]
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(String);

        impl $name {
            /// Validate and construct this identifier.
            ///
            /// Identifiers must be non-empty and contain no surrounding
            /// whitespace. Their remaining syntax is deliberately host-defined.
            pub fn new(value: impl Into<String>) -> Result<Self, InvalidIdentifier> {
                let value = value.into();
                if value.trim().is_empty() || value.trim() != value {
                    return Err(InvalidIdentifier { kind: $kind });
                }
                Ok(Self(value))
            }

            /// Borrow the validated identifier text.
            pub fn as_str(&self) -> &str {
                &self.0
            }

            /// Consume this identifier and return its owned text.
            pub fn into_string(self) -> String {
                self.0
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                self.as_str()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
            }
        }

        impl TryFrom<String> for $name {
            type Error = InvalidIdentifier;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl TryFrom<&str> for $name {
            type Error = InvalidIdentifier;

            fn try_from(value: &str) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }
    };
}

identifier!(
    WorkflowId,
    "workflow id",
    "A stable host-defined identity for a workflow."
);
identifier!(
    WorkflowVersion,
    "workflow version",
    "A host-defined version identifying an immutable workflow definition."
);
identifier!(
    WorkflowRunId,
    "workflow run id",
    "A host-defined identity for one workflow execution."
);
identifier!(
    StepId,
    "step id",
    "A stable identity for one workflow step."
);

/// The error returned when a workflow identifier violates its basic contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[error("{kind} must be non-empty and contain no surrounding whitespace")]
pub struct InvalidIdentifier {
    kind: &'static str,
}

impl InvalidIdentifier {
    /// Return the kind of identifier that failed validation.
    pub const fn kind(self) -> &'static str {
        self.kind
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_reject_blank_values() {
        let error = StepId::new("  ").expect_err("blank step id is invalid");
        assert_eq!(error.kind(), "step id");

        assert!(StepId::new(" padded").is_err());
        assert!(StepId::new("padded ").is_err());
    }

    #[test]
    fn every_conversion_agrees_with_the_constructor() {
        // The conversions are the ergonomic surface a host actually uses, so
        // none of them may accept a value `new` would reject or alter one it
        // accepts.
        let from_new = StepId::new("compile").expect("valid");
        let from_str = StepId::try_from("compile").expect("valid");
        let from_string = StepId::try_from(String::from("compile")).expect("valid");
        assert_eq!(from_new, from_str);
        assert_eq!(from_new, from_string);

        assert_eq!(from_new.as_str(), "compile");
        assert_eq!(from_new.as_ref() as &str, "compile");
        assert_eq!(from_new.to_string(), "compile");
        assert_eq!(from_new.into_string(), String::from("compile"));

        assert!(StepId::try_from(" compile").is_err());
        assert!(StepId::try_from(String::from("")).is_err());
    }

    #[test]
    fn each_identifier_names_itself_when_it_refuses_a_value() {
        // A host surfacing this error needs to know which field was wrong.
        assert_eq!(
            WorkflowId::new("").expect_err("blank").kind(),
            "workflow id"
        );
        assert_eq!(
            WorkflowVersion::new("").expect_err("blank").kind(),
            "workflow version"
        );
        assert_eq!(
            WorkflowRunId::new("").expect_err("blank").kind(),
            "workflow run id"
        );
    }
}
