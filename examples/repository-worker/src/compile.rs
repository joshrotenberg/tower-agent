//! Compile application configuration into an immutable workflow definition.
//!
//! Every refusal here happens before any step launches, and names a location
//! a person can open. A workflow that is going to fail because a profile is
//! misspelled should fail in the compiler, not three steps into a run that
//! has already spent money.

use std::collections::BTreeSet;

use tower_agent_plan::{Layers, PartialTurn, Resolution, resolve};
use tower_agent_workflow::{DagBuilder, StepSpec, WorkflowDefinition};

use crate::job::{Job, MechanicalOp, ProfileCatalog};
use crate::schema::{JobEntry, Location, SCHEMA_VERSION, StepEntry, WorkflowFile};

/// One reason a configuration could not be compiled.
#[derive(Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub location: Location,
    pub message: String,
}

impl std::fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.location, self.message)
    }
}

/// Compile one file into a definition, or report everything wrong with it.
///
/// Diagnostics accumulate rather than short-circuiting: a person fixing a
/// config wants the whole list, not one error per edit-run cycle.
pub fn compile(
    file_name: &str,
    document: &str,
    catalog: &ProfileCatalog,
) -> Result<WorkflowDefinition<Job>, Vec<Diagnostic>> {
    let parsed: WorkflowFile = match toml::from_str(document) {
        Ok(parsed) => parsed,
        Err(error) => {
            return Err(vec![Diagnostic {
                location: Location::new(file_name, "<document>"),
                message: format!("configuration is not valid: {error}"),
            }]);
        }
    };

    let mut diagnostics = Vec::new();
    if parsed.schema_version != SCHEMA_VERSION {
        diagnostics.push(Diagnostic {
            location: Location::new(file_name, "schema_version"),
            message: format!(
                "unsupported schema version {}, this build reads {SCHEMA_VERSION}",
                parsed.schema_version
            ),
        });
    }

    // Flatten the two authoring forms into one list of (step, needs). A phase
    // list is sugar: every step in a phase depends on every step of the
    // previous phase, which is exactly a fan-in followed by a fan-out.
    let mut entries: Vec<(&StepEntry, Vec<String>, String)> = Vec::new();
    if !parsed.step.is_empty() && !parsed.phase.is_empty() {
        diagnostics.push(Diagnostic {
            location: Location::new(file_name, "<document>"),
            message: "a workflow uses either `step` or `phase`, not both".to_string(),
        });
    }
    for (index, step) in parsed.step.iter().enumerate() {
        entries.push((step, step.needs.clone(), format!("step[{index}]")));
    }
    let mut previous_phase: Vec<String> = Vec::new();
    for (phase_index, phase) in parsed.phase.iter().enumerate() {
        let mut this_phase = Vec::new();
        for (step_index, step) in phase.step.iter().enumerate() {
            let mut needs = step.needs.clone();
            needs.extend(previous_phase.iter().cloned());
            entries.push((
                step,
                needs,
                format!("phase[{phase_index}].step[{step_index}]"),
            ));
            this_phase.push(step.id.clone());
        }
        previous_phase = this_phase;
    }

    if entries.is_empty() && diagnostics.is_empty() {
        diagnostics.push(Diagnostic {
            location: Location::new(file_name, "<document>"),
            message: "a workflow needs at least one step".to_string(),
        });
    }

    // Known ids first, so a missing dependency can be reported against the
    // step that names it rather than as a topology failure with no location.
    let declared: BTreeSet<&str> = entries
        .iter()
        .map(|(step, _, _)| step.id.as_str())
        .collect();

    let mut builder = DagBuilder::new(&parsed.workflow.id, &parsed.workflow.version);
    for (step, needs, path) in &entries {
        for (need_index, need) in needs.iter().enumerate() {
            if !declared.contains(need.as_str()) {
                diagnostics.push(Diagnostic {
                    location: Location::new(file_name, format!("{path}.needs[{need_index}]")),
                    message: format!("step `{}` depends on unknown step `{need}`", step.id),
                });
            }
        }
        match compile_job(file_name, step, path, catalog) {
            Ok(job) => {
                builder = builder.step(StepSpec::new(&step.id, job).needs(needs.clone()));
            }
            Err(mut found) => diagnostics.append(&mut found),
        }
    }

    if !diagnostics.is_empty() {
        return Err(diagnostics);
    }

    // Identity, duplicate ids, and cycles are the workflow crate's own
    // invariants, so they are checked there rather than restated here.
    builder.build().map_err(|error| {
        vec![Diagnostic {
            location: Location::new(file_name, "<workflow>"),
            message: error.to_string(),
        }]
    })
}

fn compile_job(
    file_name: &str,
    step: &StepEntry,
    path: &str,
    catalog: &ProfileCatalog,
) -> Result<Job, Vec<Diagnostic>> {
    match &step.job {
        JobEntry::Agent { profile, prompt } => {
            let Some(saved) = catalog.get(profile) else {
                let known: Vec<&str> = catalog.names().collect();
                return Err(vec![Diagnostic {
                    location: Location::new(file_name, format!("{path}.profile")),
                    message: format!(
                        "unknown profile `{profile}`, this host defines {}",
                        known.join(", ")
                    ),
                }]);
            };
            // Resolve the profile now, with the prompt this step supplies, so
            // a configuration that cannot produce a runnable turn fails here.
            // The planning crate answers with requirements or diagnostics as
            // data, which is what makes that check possible at compile time.
            let explicit = PartialTurn {
                prompt: Some(prompt.clone()),
                ..Default::default()
            };
            match resolve(Layers::new(&explicit).with_profile(saved)) {
                Resolution::Complete(resolved) => Ok(Job::Agent {
                    profile: profile.clone(),
                    prompt: prompt.clone(),
                    provider: resolved.provider(),
                }),
                Resolution::Missing { requirements, .. } => Err(requirements
                    .into_iter()
                    .map(|requirement| Diagnostic {
                        location: Location::new(file_name, format!("{path}.{}", requirement.path)),
                        message: format!(
                            "profile `{profile}` still needs {} ({})",
                            requirement.id, requirement.label
                        ),
                    })
                    .collect()),
                Resolution::Invalid { diagnostics } => Err(diagnostics
                    .into_iter()
                    .map(|diagnostic| Diagnostic {
                        location: Location::new(
                            file_name,
                            format!("{path}.{}", diagnostic.path.as_deref().unwrap_or("profile")),
                        ),
                        message: diagnostic.message,
                    })
                    .collect()),
            }
        }
        JobEntry::Mechanical { op, args } => match MechanicalOp::parse(op) {
            Some(op) => Ok(Job::Mechanical {
                op,
                args: args.clone(),
            }),
            None => Err(vec![Diagnostic {
                location: Location::new(file_name, format!("{path}.op")),
                message: format!("unknown mechanical operation `{op}`"),
            }]),
        },
    }
}
