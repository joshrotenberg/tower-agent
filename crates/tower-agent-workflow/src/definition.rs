use std::collections::{BTreeMap, BTreeSet};

use thiserror::Error;

use crate::{InvalidIdentifier, StepId, WorkflowId, WorkflowVersion};

/// An authoring-time step whose identifiers are validated by a workflow builder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepSpec<J> {
    id: String,
    needs: Vec<String>,
    job: J,
}

impl<J> StepSpec<J> {
    /// Construct an authoring-time step with no dependencies.
    ///
    /// The identifier is retained as text and validated when its workflow is
    /// built. Use [`Self::needs`] to declare direct dependencies in a DAG.
    pub fn new(id: impl Into<String>, job: J) -> Self {
        Self {
            id: id.into(),
            needs: Vec::new(),
            job,
        }
    }

    /// Declare direct dependencies. This is intended for [`DagBuilder`].
    pub fn needs(mut self, dependencies: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.needs = dependencies.into_iter().map(Into::into).collect();
        self
    }
}

/// One validated step in a normalized workflow DAG.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepDefinition<J> {
    id: StepId,
    needs: Vec<StepId>,
    job: J,
}

impl<J> StepDefinition<J> {
    /// Return the validated identity of this step.
    pub fn id(&self) -> &StepId {
        &self.id
    }

    /// Return this step's direct dependencies in stable step-id order.
    pub fn needs(&self) -> &[StepId] {
        &self.needs
    }

    /// Borrow the opaque host-owned job associated with this step.
    pub fn job(&self) -> &J {
        &self.job
    }
}

/// A validated, normalized DAG with an opaque host-owned job at each step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkflowDefinition<J> {
    id: WorkflowId,
    version: WorkflowVersion,
    steps: BTreeMap<StepId, StepDefinition<J>>,
    topological_order: Vec<StepId>,
}

impl<J> WorkflowDefinition<J> {
    /// Build a one-step workflow. A single shot is a one-node DAG.
    pub fn single(
        id: impl Into<String>,
        version: impl Into<String>,
        step: StepSpec<J>,
    ) -> Result<Self, WorkflowDefinitionError> {
        if !step.needs.is_empty() {
            return Err(WorkflowDefinitionError::SingleStepHasDependencies);
        }
        compile(id.into(), version.into(), vec![step])
    }

    /// Return the stable workflow identity.
    pub fn id(&self) -> &WorkflowId {
        &self.id
    }

    /// Return the host-defined version of this workflow.
    pub fn version(&self) -> &WorkflowVersion {
        &self.version
    }

    /// Iterate over all normalized steps in stable step-id order.
    pub fn steps(&self) -> impl ExactSizeIterator<Item = &StepDefinition<J>> {
        self.steps.values()
    }

    /// Find a normalized step by its validated identity.
    pub fn step(&self, id: &StepId) -> Option<&StepDefinition<J>> {
        self.steps.get(id)
    }

    /// Return a stable topological ordering of all step identities.
    ///
    /// Independent ready steps are ordered by step id, so equivalent
    /// definitions produce the same order regardless of authoring order.
    pub fn topological_order(&self) -> &[StepId] {
        &self.topological_order
    }

    /// Return all steps without dependencies in stable step-id order.
    pub fn roots(&self) -> Vec<&StepDefinition<J>> {
        self.steps
            .values()
            .filter(|step| step.needs.is_empty())
            .collect()
    }

    /// Return all steps without successors in stable step-id order.
    pub fn leaves(&self) -> Vec<&StepDefinition<J>> {
        let dependencies = self
            .steps
            .values()
            .flat_map(|step| step.needs.iter())
            .collect::<BTreeSet<_>>();
        self.steps
            .values()
            .filter(|step| !dependencies.contains(step.id()))
            .collect()
    }
}

/// Linear authoring syntax. Dependencies are inferred from insertion order.
#[derive(Clone, Debug)]
pub struct PipelineBuilder<J> {
    id: String,
    version: String,
    steps: Vec<StepSpec<J>>,
}

impl<J> PipelineBuilder<J> {
    /// Begin a linear workflow with the given stable identity and version.
    pub fn new(id: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            version: version.into(),
            steps: Vec::new(),
        }
    }

    /// Append a step after the previously appended step.
    ///
    /// Pipeline dependencies are inferred during [`Self::build`], so the step
    /// must not contain dependencies declared through [`StepSpec::needs`].
    pub fn then(mut self, step: StepSpec<J>) -> Self {
        self.steps.push(step);
        self
    }

    /// Validate and normalize this linear workflow into a DAG definition.
    pub fn build(mut self) -> Result<WorkflowDefinition<J>, WorkflowDefinitionError> {
        let mut previous: Option<String> = None;
        for step in &mut self.steps {
            if !step.needs.is_empty() {
                return Err(WorkflowDefinitionError::PipelineStepHasDependencies(
                    step.id.clone(),
                ));
            }
            step.needs = previous.iter().cloned().collect();
            previous = Some(step.id.clone());
        }
        compile(self.id, self.version, self.steps)
    }
}

/// General DAG authoring syntax.
#[derive(Clone, Debug)]
pub struct DagBuilder<J> {
    id: String,
    version: String,
    steps: Vec<StepSpec<J>>,
}

impl<J> DagBuilder<J> {
    /// Begin a DAG workflow with the given stable identity and version.
    pub fn new(id: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            version: version.into(),
            steps: Vec::new(),
        }
    }

    /// Add one authoring-time step to the DAG.
    pub fn step(mut self, step: StepSpec<J>) -> Self {
        self.steps.push(step);
        self
    }

    /// Validate identities and dependencies and produce a normalized DAG.
    pub fn build(self) -> Result<WorkflowDefinition<J>, WorkflowDefinitionError> {
        compile(self.id, self.version, self.steps)
    }
}

fn compile<J>(
    id: String,
    version: String,
    specs: Vec<StepSpec<J>>,
) -> Result<WorkflowDefinition<J>, WorkflowDefinitionError> {
    let id = WorkflowId::new(id)?;
    let version = WorkflowVersion::new(version)?;
    if specs.is_empty() {
        return Err(WorkflowDefinitionError::NoSteps);
    }

    let mut steps = BTreeMap::new();
    for spec in specs {
        let step_id = StepId::new(spec.id)?;
        let mut seen = BTreeSet::new();
        let mut needs = spec
            .needs
            .into_iter()
            .map(|dependency| {
                let dependency = StepId::new(dependency)?;
                if !seen.insert(dependency.clone()) {
                    return Err(WorkflowDefinitionError::DuplicateDependency {
                        step: step_id.clone(),
                        dependency,
                    });
                }
                Ok(dependency)
            })
            .collect::<Result<Vec<_>, _>>()?;
        needs.sort();
        let definition = StepDefinition {
            id: step_id.clone(),
            needs,
            job: spec.job,
        };
        if steps.insert(step_id.clone(), definition).is_some() {
            return Err(WorkflowDefinitionError::DuplicateStep(step_id));
        }
    }

    let mut indegree = steps
        .iter()
        .map(|(step_id, step)| (step_id.clone(), step.needs.len()))
        .collect::<BTreeMap<_, _>>();
    let mut outgoing = steps
        .keys()
        .cloned()
        .map(|step_id| (step_id, Vec::new()))
        .collect::<BTreeMap<_, _>>();

    for step in steps.values() {
        for dependency in &step.needs {
            if dependency == &step.id {
                return Err(WorkflowDefinitionError::SelfDependency(step.id.clone()));
            }
            let successors = outgoing.get_mut(dependency).ok_or_else(|| {
                WorkflowDefinitionError::MissingDependency {
                    step: step.id.clone(),
                    dependency: dependency.clone(),
                }
            })?;
            successors.push(step.id.clone());
        }
    }
    for successors in outgoing.values_mut() {
        successors.sort();
    }

    let mut ready = indegree
        .iter()
        .filter_map(|(step_id, count)| (*count == 0).then_some(step_id.clone()))
        .collect::<BTreeSet<_>>();
    let mut topological_order = Vec::with_capacity(steps.len());
    while let Some(step_id) = ready.pop_first() {
        topological_order.push(step_id.clone());
        for successor in &outgoing[&step_id] {
            let count = indegree
                .get_mut(successor)
                .expect("validated successor must have an indegree");
            *count -= 1;
            if *count == 0 {
                ready.insert(successor.clone());
            }
        }
    }

    if topological_order.len() != steps.len() {
        let cycle = indegree
            .into_iter()
            .filter_map(|(step_id, count)| (count > 0).then_some(step_id))
            .collect();
        return Err(WorkflowDefinitionError::Cycle(cycle));
    }

    Ok(WorkflowDefinition {
        id,
        version,
        steps,
        topological_order,
    })
}

/// An authoring or validation error in a workflow definition.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum WorkflowDefinitionError {
    /// A workflow, version, step, or dependency identifier is invalid.
    #[error(transparent)]
    InvalidIdentifier(#[from] InvalidIdentifier),
    /// The workflow contains no steps.
    #[error("workflow must contain at least one step")]
    NoSteps,
    /// A definition declared as a single shot contains a dependency.
    #[error("a single-step workflow cannot declare dependencies")]
    SingleStepHasDependencies,
    /// A pipeline step declared an explicit dependency instead of using insertion order.
    #[error("pipeline step `{0}` cannot declare explicit dependencies")]
    PipelineStepHasDependencies(String),
    /// More than one step uses the same validated identity.
    #[error("duplicate step id `{0}`")]
    DuplicateStep(StepId),
    /// A step names the same direct dependency more than once.
    #[error("step `{step}` repeats dependency `{dependency}`")]
    DuplicateDependency {
        /// The step containing the repeated edge.
        step: StepId,
        /// The dependency named more than once.
        dependency: StepId,
    },
    /// A step refers to a dependency that is absent from the definition.
    #[error("step `{step}` refers to missing dependency `{dependency}`")]
    MissingDependency {
        /// The step containing the unresolved edge.
        step: StepId,
        /// The dependency not present in the workflow.
        dependency: StepId,
    },
    /// A step directly depends on itself.
    #[error("step `{0}` depends on itself")]
    SelfDependency(StepId),
    /// At least one dependency cycle prevents a complete topological ordering.
    ///
    /// The contained identities are the steps left blocked by the cycle, not
    /// necessarily a minimal cycle path.
    #[error("workflow contains a cycle involving {0:?}")]
    Cycle(Vec<StepId>),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipeline_normalizes_to_one_dag_representation() {
        let workflow = PipelineBuilder::new("review", "v1")
            .then(StepSpec::new("research", "research-job"))
            .then(StepSpec::new("implement", "implement-job"))
            .then(StepSpec::new("review", "review-job"))
            .build()
            .expect("pipeline is valid");

        let implement = workflow
            .step(&StepId::new("implement").expect("valid id"))
            .expect("step exists");
        assert_eq!(implement.needs(), &[StepId::new("research").unwrap()]);
        assert_eq!(workflow.roots().len(), 1);
        assert_eq!(workflow.leaves().len(), 1);
    }

    #[test]
    fn rejects_missing_dependencies_and_cycles() {
        let missing = DagBuilder::new("missing", "v1")
            .step(StepSpec::new("a", ()).needs(["absent"]))
            .build();
        assert!(matches!(
            missing,
            Err(WorkflowDefinitionError::MissingDependency { .. })
        ));

        let cycle = DagBuilder::new("cycle", "v1")
            .step(StepSpec::new("a", ()).needs(["b"]))
            .step(StepSpec::new("b", ()).needs(["a"]))
            .build();
        assert!(matches!(cycle, Err(WorkflowDefinitionError::Cycle(_))));
    }

    #[test]
    fn rejects_every_ambiguous_edge_or_identity() {
        let empty = DagBuilder::<()>::new("empty", "v1").build();
        assert_eq!(empty, Err(WorkflowDefinitionError::NoSteps));

        let duplicate_step = DagBuilder::new("duplicate", "v1")
            .step(StepSpec::new("same", 1))
            .step(StepSpec::new("same", 2))
            .build();
        assert!(matches!(
            duplicate_step,
            Err(WorkflowDefinitionError::DuplicateStep(_))
        ));

        let duplicate_dependency = DagBuilder::new("duplicate-edge", "v1")
            .step(StepSpec::new("root", ()))
            .step(StepSpec::new("child", ()).needs(["root", "root"]))
            .build();
        assert!(matches!(
            duplicate_dependency,
            Err(WorkflowDefinitionError::DuplicateDependency { .. })
        ));

        let self_dependency = DagBuilder::new("self-edge", "v1")
            .step(StepSpec::new("self", ()).needs(["self"]))
            .build();
        assert!(matches!(
            self_dependency,
            Err(WorkflowDefinitionError::SelfDependency(_))
        ));

        let blank_id = WorkflowDefinition::single("valid", "v1", StepSpec::new(" ", ()))
            .expect_err("blank step is invalid");
        assert!(matches!(
            blank_id,
            WorkflowDefinitionError::InvalidIdentifier(_)
        ));
    }

    #[test]
    fn topological_order_is_stable_for_independent_roots() {
        let workflow = DagBuilder::new("stable", "v1")
            .step(StepSpec::new("z", ()))
            .step(StepSpec::new("a", ()))
            .step(StepSpec::new("join", ()).needs(["z", "a"]))
            .build()
            .expect("dag is valid");

        assert_eq!(
            workflow.topological_order(),
            [
                StepId::new("a").unwrap(),
                StepId::new("z").unwrap(),
                StepId::new("join").unwrap(),
            ]
        );
        assert_eq!(
            workflow
                .step(&StepId::new("join").unwrap())
                .expect("join exists")
                .needs(),
            [StepId::new("a").unwrap(), StepId::new("z").unwrap()]
        );
    }
}
