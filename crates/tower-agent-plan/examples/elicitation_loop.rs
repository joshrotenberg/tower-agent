//! Resolve a partly specified turn, ask for what is missing, then compile it.
//!
//! This is the crate's whole shape in one file. An application holds defaults
//! at several precedences, a user supplies a fragment, and the planner reports
//! what is still unbound as data rather than prompting for it. The application
//! decides how to ask; a CLI renders missing flags, an MCP adapter elicits, a
//! form renders fields. None of that belongs in this crate, which is why the
//! requirements come back as values.
//!
//! Nothing here compiles to argv, and nothing launches. The output is a typed
//! `tower_agent::Turn<ClaudeOptions>` a host hands to a service.
//!
//! Run with
//! `cargo run -p tower-agent-plan --features claude --example elicitation_loop`.

use std::path::PathBuf;

use tower_agent_plan::{
    Answer, Layers, PartialClaudeOptions, PartialTurn, Prepared, ProviderDefaults, ProviderId,
    ReadyTurn, Requirement, prepare,
};

fn main() {
    // Lowest precedence: what each provider gets when it is the one selected.
    // A baseline never selects the provider, it only applies once one is.
    let mut provider_defaults = ProviderDefaults::default();
    provider_defaults.insert(ProviderId::Claude, claude_baseline());

    // Above that: this application's own defaults, provider-independent.
    let application_defaults = PartialTurn {
        context: tower_agent_plan::PartialContext {
            cwd: Some(PathBuf::from(".")),
            ..Default::default()
        },
        ..Default::default()
    };

    // What the user actually typed. Not enough to run: no provider, no prompt.
    let explicit = PartialTurn::default();

    // Pass one. The planner reports the gap instead of guessing at it.
    let layers = Layers::new(&explicit)
        .with_provider_defaults(&provider_defaults)
        .with_application_defaults(&application_defaults);

    let requirements = match prepare(layers) {
        Prepared::Missing { requirements, .. } => {
            println!("pass 1: incomplete, {} requirement(s)", requirements.len());
            for requirement in &requirements {
                println!("  {}", render(requirement));
            }
            requirements
        }
        other => panic!("expected an incomplete plan, got {other:?}"),
    };

    // The application answers however it likes. Answers fill only paths that
    // are still unbound; one naming an already-bound path is a diagnostic, not
    // a silent override, so a default cannot be clobbered by accident.
    let answers = vec![
        Answer {
            id: requirements[0].id.clone(),
            value: "claude".to_string(),
        },
        Answer {
            id: "prompt".to_string(),
            value: "summarize the open issues".to_string(),
        },
    ];

    // Pass two. Same layers, plus the answers.
    let layers = Layers::new(&explicit)
        .with_provider_defaults(&provider_defaults)
        .with_application_defaults(&application_defaults)
        .with_answers(&answers);

    match prepare(layers) {
        Prepared::Ready(ReadyTurn::Claude(turn)) => {
            println!("\npass 2: ready");
            println!("  prompt:   {}", turn.prompt);
            println!("  cwd:      {:?}", turn.working_directory);
            // From the provider baseline, which applied only once the answer
            // selected Claude.
            println!("  model:    {:?}", turn.options.model);
            println!("  max_turns:{:?}", turn.options.max_turns);
            // `turn` is an ordinary tower-agent turn now. A host would hand it
            // to a ClaudeService, optionally through `claude::preflight` first
            // to check the configured service will not refuse it.
        }
        other => panic!("expected a ready turn, got {other:?}"),
    }

    // Diagnostics are data too. A provider answer that names nothing this
    // build supports is refused with a stable code, not a panic or a guess.
    let bad = vec![Answer {
        id: "provider".to_string(),
        value: "not-a-provider".to_string(),
    }];
    let layers = Layers::new(&explicit).with_answers(&bad);
    match prepare(layers) {
        Prepared::Invalid { diagnostics } => {
            println!("\nrefused, with reasons:");
            for diagnostic in diagnostics {
                println!(
                    "  [{}] {}{}",
                    diagnostic.code,
                    diagnostic.message,
                    diagnostic
                        .path
                        .map(|path| format!(" (at {path})"))
                        .unwrap_or_default()
                );
            }
        }
        other => panic!("expected refusal, got {other:?}"),
    }
}

/// What Claude turns get when Claude is the selected provider.
fn claude_baseline() -> PartialTurn {
    // The provider groups are themselves feature-gated, so each field is named
    // under the same cfg the struct uses. That keeps this exact under a
    // claude-only build and an all-provider one, where a `..Default::default()`
    // tail would be redundant in the first and required in the second.
    let provider_options = tower_agent_plan::PartialProviderOptions {
        claude: Some(PartialClaudeOptions {
            max_turns: Some(8),
            ..Default::default()
        }),
        #[cfg(feature = "codex")]
        codex: None,
    };

    PartialTurn {
        model: tower_agent_plan::PartialModel {
            name: Some("claude-opus-5".to_string()),
        },
        provider_options,
        ..Default::default()
    }
}

/// Requirements carry enough structure for any front end to render them.
fn render(requirement: &Requirement) -> String {
    format!(
        "{} ({:?}) at {} because {:?}",
        requirement.label, requirement.kind, requirement.path, requirement.reason
    )
}
