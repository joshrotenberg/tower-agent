use std::collections::BTreeMap;

use serde::Serialize;

use crate::diagnostic::{Diagnostic, codes};
use crate::partial::{PartialTurn, Profile};
use crate::provider::ProviderId;
use crate::requirement::{Answer, Requirement, RequirementReason, ValueKind, ids};

/// Provider baseline defaults, the lowest-precedence layer.
///
/// A provider's baseline applies only after that provider is selected by a
/// higher layer or a provider answer. The baseline's own `provider` field is
/// ignored during merge: a baseline cannot select or change the provider.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProviderDefaults {
    map: BTreeMap<ProviderId, PartialTurn>,
}

impl ProviderDefaults {
    /// Set one provider's baseline, returning any it replaced.
    pub fn insert(&mut self, provider: ProviderId, defaults: PartialTurn) -> Option<PartialTurn> {
        self.map.insert(provider, defaults)
    }

    /// One provider's baseline, if set.
    pub fn get(&self, provider: ProviderId) -> Option<&PartialTurn> {
        self.map.get(&provider)
    }
}

/// The resolution layers for one planning pass, lowest precedence first.
///
/// Precedence from lowest to highest: provider baseline defaults, application
/// defaults, the selected profile, the explicit request, and elicited answers
/// for paths still unbound.
#[derive(Clone, Copy, Debug)]
pub struct Layers<'a> {
    provider_defaults: Option<&'a ProviderDefaults>,
    application_defaults: Option<&'a PartialTurn>,
    profile: Option<&'a Profile>,
    explicit: &'a PartialTurn,
    answers: &'a [Answer],
}

impl<'a> Layers<'a> {
    /// Layers with only the caller's explicit values bound.
    pub fn new(explicit: &'a PartialTurn) -> Self {
        Self {
            provider_defaults: None,
            application_defaults: None,
            profile: None,
            explicit,
            answers: &[],
        }
    }

    /// Add provider baselines, the lowest-precedence layer.
    pub fn with_provider_defaults(mut self, defaults: &'a ProviderDefaults) -> Self {
        self.provider_defaults = Some(defaults);
        self
    }

    /// Add application defaults, above provider baselines.
    pub fn with_application_defaults(mut self, defaults: &'a PartialTurn) -> Self {
        self.application_defaults = Some(defaults);
        self
    }

    /// Add a saved profile, above application defaults.
    pub fn with_profile(mut self, profile: &'a Profile) -> Self {
        self.profile = Some(profile);
        self
    }

    /// Add elicited answers, which fill only still-unbound paths.
    pub fn with_answers(mut self, answers: &'a [Answer]) -> Self {
        self.answers = answers;
        self
    }
}

/// The outcome of one resolution pass.
///
/// Invalid bound values take priority over eliciting more values: a pass
/// returns `Invalid` whenever any diagnostic exists, `Missing` when the data
/// is coherent but incomplete, and `Complete` otherwise.
#[derive(Clone, Debug, PartialEq)]
pub enum Resolution {
    /// Every shared requirement is bound and structurally valid.
    Complete(ResolvedTurn),
    /// Coherent but incomplete. Elicit the requirements and resolve again.
    Missing {
        /// The merged view the requirements were derived from.
        resolved: PartialTurn,
        /// Unresolved values in deterministic order.
        requirements: Vec<Requirement>,
    },
    /// Refused. Invalid values take priority over eliciting more.
    Invalid {
        /// What was wrong, in deterministic order.
        diagnostics: Vec<Diagnostic>,
    },
}

/// A merged turn whose shared requirements are satisfied.
///
/// Constructed only by [`resolve`]. The provider and prompt are bound and the
/// bound values passed the shared structural checks. Provider-conditional
/// validation and the fold into a concrete `Turn<O>` belong to the provider
/// planners, and the adapters re-validate at launch.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ResolvedTurn {
    merged: PartialTurn,
}

impl ResolvedTurn {
    /// The bound provider.
    pub fn provider(&self) -> ProviderId {
        self.merged
            .provider
            .expect("resolve guarantees a bound provider")
    }

    /// The bound prompt.
    pub fn prompt(&self) -> &str {
        self.merged
            .prompt
            .as_deref()
            .expect("resolve guarantees a bound prompt")
    }

    /// The merged view behind this resolution.
    pub fn partial(&self) -> &PartialTurn {
        &self.merged
    }

    /// Take the merged view, consuming this resolution.
    pub fn into_partial(self) -> PartialTurn {
        self.merged
    }
}

/// Resolve one planning pass over the supplied layers.
///
/// The pass merges the layers under the documented precedence, applies
/// answers to still-unbound paths, validates every bound value it can, and
/// returns [`Resolution::Complete`], [`Resolution::Missing`] with the
/// requirements in deterministic order, or [`Resolution::Invalid`] with every
/// diagnostic found. It launches nothing, prompts no one, and reads no
/// ambient state.
pub fn resolve(layers: Layers<'_>) -> Resolution {
    let mut diagnostics = Vec::new();

    if let Some(profile) = layers.profile
        && let (Some(profile_provider), Some(explicit_provider)) =
            (profile.turn.provider, layers.explicit.provider)
        && profile_provider != explicit_provider
    {
        diagnostics.push(Diagnostic::error(
            codes::PROVIDER_MISMATCH,
            Some("provider"),
            format!(
                "profile {} selects provider {profile_provider} but the explicit \
                         request selects {explicit_provider}",
                profile.name
            ),
        ));
    }

    let mut upper = PartialTurn::default();
    if let Some(defaults) = layers.application_defaults {
        upper.merge_from(defaults);
    }
    if let Some(profile) = layers.profile {
        upper.merge_from(&profile.turn);
    }
    upper.merge_from(layers.explicit);

    // Provider selection may need the answers before the baseline layer can
    // apply. The answer is validated and bound with the other answers below.
    let answered_provider = layers
        .answers
        .iter()
        .filter(|answer| answer.id == ids::PROVIDER)
        .find_map(|answer| answer.value.parse::<ProviderId>().ok());
    let selected = upper.provider.or(answered_provider);

    let mut merged = match (selected, layers.provider_defaults) {
        (Some(provider), Some(defaults)) => {
            let mut baseline = defaults.get(provider).cloned().unwrap_or_default();
            baseline.provider = None;
            baseline
        }
        _ => PartialTurn::default(),
    };
    merged.merge_from(&upper);

    for answer in layers.answers {
        match answer.id.as_str() {
            ids::PROVIDER => {
                if merged.provider.is_some() {
                    diagnostics.push(answer_overrides_bound_path("provider"));
                } else {
                    match answer.value.parse::<ProviderId>() {
                        Ok(provider) => merged.provider = Some(provider),
                        Err(_) => diagnostics.push(Diagnostic::error(
                            codes::INVALID_PROVIDER_ANSWER,
                            Some("provider"),
                            "the provider answer does not name a known provider",
                        )),
                    }
                }
            }
            ids::PROMPT => {
                if merged.prompt.is_some() {
                    diagnostics.push(answer_overrides_bound_path("prompt"));
                } else {
                    merged.prompt = Some(answer.value.clone());
                }
            }
            unknown => diagnostics.push(Diagnostic::error(
                codes::UNKNOWN_REQUIREMENT,
                None,
                format!("answer refers to unknown requirement {unknown}"),
            )),
        }
    }

    if let Some(prompt) = &merged.prompt
        && prompt.trim().is_empty()
    {
        diagnostics.push(Diagnostic::error(
            codes::BLANK_PROMPT,
            Some("prompt"),
            "a bound prompt must not be blank",
        ));
    }
    if let Some(name) = &merged.model.name
        && name.trim().is_empty()
    {
        diagnostics.push(Diagnostic::error(
            codes::BLANK_MODEL,
            Some("model.name"),
            "a bound model name must not be blank",
        ));
    }
    if let Some(resume) = &merged.context.resume {
        if resume.value().trim().is_empty() {
            diagnostics.push(Diagnostic::error(
                codes::EMPTY_RESUME_VALUE,
                Some("context.resume"),
                "a resume value must not be empty",
            ));
        } else if resume.value().starts_with('-') {
            diagnostics.push(Diagnostic::error(
                codes::HYPHEN_RESUME_VALUE,
                Some("context.resume"),
                "a resume value must not begin with a hyphen",
            ));
        }
        if let Some(provider) = merged.provider
            && resume.provider != provider
        {
            diagnostics.push(Diagnostic::error(
                codes::RESUME_PROVIDER_MISMATCH,
                Some("context.resume"),
                format!(
                    "the resume binding is tagged {} but the resolved provider \
                         is {provider}",
                    resume.provider
                ),
            ));
        }
    }

    if !diagnostics.is_empty() {
        return Resolution::Invalid { diagnostics };
    }

    let mut requirements = Vec::new();
    if merged.provider.is_none() {
        requirements.push(Requirement {
            id: ids::PROVIDER.to_string(),
            path: "provider".to_string(),
            kind: ValueKind::Provider,
            label: "Provider".to_string(),
            reason: RequirementReason::ProviderSelection,
            sensitive: false,
            provider: None,
        });
    }
    if merged.prompt.is_none() {
        requirements.push(Requirement {
            id: ids::PROMPT.to_string(),
            path: "prompt".to_string(),
            kind: ValueKind::Text,
            label: "Prompt".to_string(),
            reason: RequirementReason::ProviderInput,
            sensitive: false,
            provider: merged.provider,
        });
    }
    if !requirements.is_empty() {
        return Resolution::Missing {
            resolved: merged,
            requirements,
        };
    }

    Resolution::Complete(ResolvedTurn { merged })
}

fn answer_overrides_bound_path(path: &str) -> Diagnostic {
    Diagnostic::error(
        codes::ANSWER_OVERRIDES_BOUND_PATH,
        Some(path),
        format!("an answer may not override the bound path {path}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::partial::PartialModel;

    fn explicit(provider: Option<ProviderId>, prompt: Option<&str>) -> PartialTurn {
        PartialTurn {
            provider,
            prompt: prompt.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn complete_when_provider_and_prompt_bound() {
        let request = explicit(Some(ProviderId::Codex), Some("inspect this repository"));
        let Resolution::Complete(resolved) = resolve(Layers::new(&request)) else {
            panic!("expected complete");
        };
        assert_eq!(resolved.provider(), ProviderId::Codex);
        assert_eq!(resolved.prompt(), "inspect this repository");
    }

    #[test]
    fn requirements_are_ordered_provider_first() {
        let request = explicit(None, None);
        let Resolution::Missing { requirements, .. } = resolve(Layers::new(&request)) else {
            panic!("expected missing");
        };
        let actual: Vec<&str> = requirements
            .iter()
            .map(|requirement| requirement.id.as_str())
            .collect();
        assert_eq!(actual, [ids::PROVIDER, ids::PROMPT]);
    }

    #[test]
    fn invalid_takes_priority_over_missing() {
        let request = PartialTurn {
            model: PartialModel {
                name: Some("   ".to_string()),
            },
            ..explicit(Some(ProviderId::Codex), None)
        };
        let Resolution::Invalid { diagnostics } = resolve(Layers::new(&request)) else {
            panic!("expected invalid");
        };
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, codes::BLANK_MODEL);
    }

    #[test]
    fn provider_baseline_cannot_select_the_provider() {
        let baseline = PartialTurn {
            provider: Some(ProviderId::Codex),
            model: PartialModel {
                name: Some("baseline-model".to_string()),
            },
            ..Default::default()
        };
        let mut defaults = ProviderDefaults::default();
        defaults.insert(ProviderId::Codex, baseline);

        let request = explicit(None, Some("inspect this repository"));
        let answers = [Answer {
            id: ids::PROVIDER.to_string(),
            value: "codex".to_string(),
        }];
        let outcome = resolve(
            Layers::new(&request)
                .with_provider_defaults(&defaults)
                .with_answers(&answers),
        );
        let Resolution::Complete(resolved) = outcome else {
            panic!("expected complete, got {outcome:?}");
        };
        assert_eq!(resolved.provider(), ProviderId::Codex);
        assert_eq!(
            resolved.partial().model.name.as_deref(),
            Some("baseline-model")
        );
    }
}
