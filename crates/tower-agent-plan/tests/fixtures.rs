//! Executable planning specification.
//!
//! Every JSON file under `tests/fixtures/planning` is one resolution case:
//! layers in, expected outcome out. Missing cases assert exact requirement
//! ids and order, invalid cases assert exact diagnostic codes and order, and
//! complete cases assert the full merged turn.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use serde::Deserialize;
use tower_agent_plan::{
    Answer, Layers, PartialTurn, Profile, ProviderDefaults, ProviderId, Resolution, resolve,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    #[serde(default)]
    application_defaults: Option<PartialTurn>,
    #[serde(default)]
    provider_defaults: BTreeMap<String, PartialTurn>,
    #[serde(default)]
    profiles: BTreeMap<String, PartialTurn>,
    #[serde(default)]
    profile: Option<String>,
    #[serde(default)]
    explicit: PartialTurn,
    #[serde(default)]
    answers: Vec<Answer>,
    expect: Expect,
}

#[derive(Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
enum Expect {
    Complete {
        resolved: serde_json::Value,
    },
    Missing {
        requirements: Vec<String>,
        #[serde(default)]
        resolved: Option<serde_json::Value>,
    },
    Invalid {
        diagnostics: Vec<String>,
    },
}

#[test]
fn planning_fixtures() {
    run_directory("tests/fixtures/planning");
}

/// Codex resolution fixtures parse provider option groups that only exist
/// under the `codex` feature, so they live in their own gated directory.
#[cfg(feature = "codex")]
#[test]
fn planning_codex_fixtures() {
    run_directory("tests/fixtures/planning_codex");
}

fn run_directory(relative: &str) {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(relative);
    let mut paths: Vec<PathBuf> = fs::read_dir(&directory)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", directory.display()))
        .map(|entry| entry.expect("readable directory entry").path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect();
    paths.sort();
    assert!(
        !paths.is_empty(),
        "no fixtures found in {}",
        directory.display()
    );

    for path in paths {
        let name = path
            .file_name()
            .expect("fixture file name")
            .to_string_lossy()
            .into_owned();
        let text = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("{name}: cannot read fixture: {error}"));
        let case: Case = serde_json::from_str(&text)
            .unwrap_or_else(|error| panic!("{name}: cannot parse fixture: {error}"));
        run_case(&name, case);
    }
}

fn run_case(name: &str, case: Case) {
    let mut provider_defaults = ProviderDefaults::default();
    for (provider, defaults) in &case.provider_defaults {
        let provider: ProviderId = provider
            .parse()
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        provider_defaults.insert(provider, defaults.clone());
    }

    let profile = case.profile.as_ref().map(|selected| {
        let turn = case
            .profiles
            .get(selected)
            .unwrap_or_else(|| panic!("{name}: profile {selected} is not defined"))
            .clone();
        Profile {
            name: selected.clone(),
            turn,
        }
    });

    let mut layers = Layers::new(&case.explicit)
        .with_provider_defaults(&provider_defaults)
        .with_answers(&case.answers);
    if let Some(defaults) = &case.application_defaults {
        layers = layers.with_application_defaults(defaults);
    }
    if let Some(profile) = &profile {
        layers = layers.with_profile(profile);
    }

    let outcome = resolve(layers);
    match case.expect {
        Expect::Complete { resolved: expected } => {
            let Resolution::Complete(resolved) = outcome else {
                panic!("{name}: expected complete, got {outcome:?}");
            };
            let actual = serde_json::to_value(resolved.partial()).expect("serializable turn");
            assert_eq!(actual, expected, "{name}: resolved turn mismatch");
        }
        Expect::Missing {
            requirements: expected_ids,
            resolved: expected_resolved,
        } => {
            let Resolution::Missing {
                resolved,
                requirements,
            } = outcome
            else {
                panic!("{name}: expected missing, got {outcome:?}");
            };
            let actual_ids: Vec<String> = requirements
                .iter()
                .map(|requirement| requirement.id.clone())
                .collect();
            assert_eq!(actual_ids, expected_ids, "{name}: requirement ids mismatch");
            if let Some(expected) = expected_resolved {
                let actual = serde_json::to_value(&resolved).expect("serializable turn");
                assert_eq!(actual, expected, "{name}: resolved view mismatch");
            }
        }
        Expect::Invalid {
            diagnostics: expected_codes,
        } => {
            let Resolution::Invalid { diagnostics } = outcome else {
                panic!("{name}: expected invalid, got {outcome:?}");
            };
            let actual_codes: Vec<String> = diagnostics
                .iter()
                .map(|diagnostic| diagnostic.code.clone())
                .collect();
            assert_eq!(
                actual_codes, expected_codes,
                "{name}: diagnostic codes mismatch"
            );
        }
    }
}
