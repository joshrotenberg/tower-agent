//! A repository worker that compiles its own TOML into tower-agent workflows.
//!
//! Written to answer one question from #107: does the workflow library survive
//! contact with a host that builds definitions from configuration rather than
//! from Rust literals, and where does the boundary help or get in the way?
//!
//! The host owns the schema, the profile catalog, the mechanical operations,
//! and the dispatcher. The libraries own typed definitions and finite
//! execution. Nothing here serializes a request, a provider option, a
//! credential, a session, a cancellation token, or an `Instant`.
//!
//! Providers are in-process fakes. #107 is about composition, not live
//! provider reliability, and a fake registered in the router exercises the
//! same routing contract a real adapter would.

mod compile;
mod dispatch;
mod job;
mod schema;

use std::num::NonZeroUsize;

use tower::{Service, ServiceExt, service_fn};
use tower_agent::{AgentError, AgentRequest, Turn, TurnOutcome};
use tower_agent_plan::RoutedTurnService;
use tower_agent_workflow::{
    BoxStepService, WorkflowContext, WorkflowRequest, WorkflowRunId, WorkflowService,
};

use dispatch::{RepositoryWorker, RunInput, StepOutput};
use job::ProfileCatalog;

/// A provider that answers in process, so the example needs no credentials.
fn fake_provider<O: Send + 'static>(
    label: &'static str,
) -> impl Service<
    AgentRequest<Turn<O>>,
    Response = TurnOutcome,
    Error = AgentError,
    Future: Send + 'static,
> + Clone {
    service_fn(move |request: AgentRequest<Turn<O>>| async move {
        Ok::<_, AgentError>(TurnOutcome::new(format!(
            "{label} answered: {}",
            request.body.prompt
        )))
    })
}

fn router() -> RoutedTurnService {
    RoutedTurnService::new()
        .with_claude(fake_provider::<tower_agent_claude::ClaudeOptions>("claude"))
        .with_codex(fake_provider::<tower_agent_codex::CodexOptions>("codex"))
}

async fn run(file_name: &str, document: &str) -> Result<Vec<(String, String)>, String> {
    let catalog = ProfileCatalog::example();
    let definition = compile::compile(file_name, document, &catalog).map_err(|diagnostics| {
        diagnostics
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    })?;

    let dispatcher: BoxStepService<RunInput, job::Job, StepOutput, AgentError> =
        BoxStepService::new(RepositoryWorker::new(router(), ProfileCatalog::example()));
    let service = WorkflowService::new(dispatcher)
        .with_max_concurrency(NonZeroUsize::new(4).expect("nonzero"));

    let request = WorkflowRequest::new(
        WorkflowContext::new(WorkflowRunId::new("local-run").expect("valid run id")),
        definition,
        RunInput {
            repository: "joshrotenberg/tower-agent".to_string(),
            branch: "main".to_string(),
        },
    );

    let outcome = service
        .oneshot(request)
        .await
        .map_err(|failure| format!("{failure:?}"))?;

    let mut results: Vec<(String, String)> = outcome
        .outputs
        .iter()
        .map(|(id, output)| (id.as_str().to_string(), output.text.clone()))
        .collect();
    results.sort();
    Ok(results)
}

#[tokio::main]
async fn main() {
    for name in ["single", "pipeline", "dag", "phases"] {
        let document = std::fs::read_to_string(format!(
            "{}/workflows/{name}.toml",
            env!("CARGO_MANIFEST_DIR")
        ))
        .expect("workflow file");
        println!("== {name} ==");
        match run(&format!("{name}.toml"), &document).await {
            Ok(results) => {
                for (id, text) in results {
                    println!("  {id}: {text}");
                }
            }
            Err(report) => println!("  refused:\n{report}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workflow(name: &str) -> String {
        std::fs::read_to_string(format!(
            "{}/workflows/{name}.toml",
            env!("CARGO_MANIFEST_DIR")
        ))
        .expect("workflow file")
    }

    fn compile_error(document: &str) -> Vec<String> {
        let catalog = ProfileCatalog::example();
        compile::compile("test.toml", document, &catalog)
            .map(|_| Vec::<compile::Diagnostic>::new())
            .unwrap_err()
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    /// All four shapes run, and each preserves dependency results.
    #[tokio::test]
    async fn every_shape_runs_and_carries_dependency_results() {
        let single = run("single.toml", &workflow("single"))
            .await
            .expect("single");
        assert_eq!(single.len(), 1);
        assert!(single[0].1.contains("claude answered"));

        let pipeline = run("pipeline.toml", &workflow("pipeline"))
            .await
            .expect("pipeline");
        let summarize = &pipeline
            .iter()
            .find(|(id, _)| id == "summarize")
            .expect("summarize ran")
            .1;
        // The collector saw its direct dependency's result.
        assert!(summarize.contains("claude answered"), "{summarize}");

        let dag = run("dag.toml", &workflow("dag")).await.expect("dag");
        let join = &dag.iter().find(|(id, _)| id == "join").expect("join ran").1;
        // Both branches of the fan-out reached the join, and each went to the
        // provider its profile selected.
        assert!(join.contains("claude answered"), "{join}");
        assert!(join.contains("codex answered"), "{join}");
    }

    /// The phase form is sugar: it must produce the same run as the DAG it
    /// normalizes to, or the sugar is lying.
    #[tokio::test]
    async fn phases_normalize_to_the_same_run_as_the_dag() {
        let dag = run("dag.toml", &workflow("dag")).await.expect("dag");
        let phases = run("phases.toml", &workflow("phases"))
            .await
            .expect("phases");
        assert_eq!(dag, phases);
    }

    /// Same input, same output. A compiler that reordered steps would make
    /// run identity meaningless.
    #[test]
    fn compilation_is_deterministic() {
        let catalog = ProfileCatalog::example();
        let document = workflow("dag");
        let first = compile::compile("dag.toml", &document, &catalog).expect("compiles");
        let second = compile::compile("dag.toml", &document, &catalog).expect("compiles");
        assert_eq!(first.topological_order(), second.topological_order());
        assert_eq!(first.id(), second.id());
        assert_eq!(first.version(), second.version());
    }

    #[test]
    fn an_unknown_profile_names_the_step_and_the_known_profiles() {
        let errors = compile_error(
            r#"
schema_version = 1
[workflow]
id = "w"
version = "v1"
[[step]]
id = "review"
kind = "agent"
profile = "nonexistent"
prompt = "hello"
"#,
        );
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("step[0].profile"), "{}", errors[0]);
        assert!(errors[0].contains("nonexistent"), "{}", errors[0]);
        // Actionable: it says what is available.
        assert!(errors[0].contains("reviewer"), "{}", errors[0]);
    }

    #[test]
    fn a_missing_dependency_is_reported_against_the_step_that_names_it() {
        let errors = compile_error(
            r#"
schema_version = 1
[workflow]
id = "w"
version = "v1"
[[step]]
id = "second"
needs = ["first"]
kind = "mechanical"
op = "collect"
"#,
        );
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("step[0].needs[0]"), "{}", errors[0]);
        assert!(errors[0].contains("unknown step `first`"), "{}", errors[0]);
    }

    #[test]
    fn a_cycle_is_refused_before_anything_launches() {
        let errors = compile_error(
            r#"
schema_version = 1
[workflow]
id = "w"
version = "v1"
[[step]]
id = "a"
needs = ["b"]
kind = "mechanical"
op = "collect"
[[step]]
id = "b"
needs = ["a"]
kind = "mechanical"
op = "collect"
"#,
        );
        assert_eq!(errors.len(), 1);
        assert!(errors[0].to_lowercase().contains("cycle"), "{}", errors[0]);
    }

    #[test]
    fn an_old_schema_version_is_refused_rather_than_guessed() {
        let errors = compile_error(
            r#"
schema_version = 99
[workflow]
id = "w"
version = "v1"
[[step]]
id = "a"
kind = "mechanical"
op = "collect"
"#,
        );
        assert!(
            errors.iter().any(|e| e.contains("schema_version")),
            "{errors:?}"
        );
    }

    #[test]
    fn every_problem_is_reported_at_once() {
        // A person fixing configuration wants the whole list, not one error
        // per edit-and-rerun cycle.
        let errors = compile_error(
            r#"
schema_version = 1
[workflow]
id = "w"
version = "v1"
[[step]]
id = "a"
kind = "agent"
profile = "missing-one"
prompt = "hi"
[[step]]
id = "b"
needs = ["nope"]
kind = "mechanical"
op = "not_an_op"
"#,
        );
        assert!(errors.len() >= 3, "expected several, got {errors:?}");
    }
}
