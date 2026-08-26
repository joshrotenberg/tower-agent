//! Typed Claude planner cases.
//!
//! The JSON corpus specifies resolution; these cases specify the fold from a
//! complete resolution into a concrete `Turn<ClaudeOptions>`, asserting the
//! typed output the adapter will actually receive.

#![cfg(feature = "claude")]

use std::path::{Path, PathBuf};

use tower_agent_claude::{
    ClaudeAmbientContext, ClaudeEffort, ClaudeHermetic, ClaudePermissionMode,
};
use tower_agent_plan::claude::{AmbientContextChoice, EffortChoice, PermissionModeChoice};
use tower_agent_plan::{
    Answer, FilesystemChoice, Layers, PartialClaudeOptions, PartialTurn, Prepared, Profile,
    ProviderId, ReadyTurn, ResumeBinding, diagnostic_codes, prepare,
};

fn claude_turn(explicit: &PartialTurn) -> tower_agent::Turn<tower_agent_claude::ClaudeOptions> {
    match prepare(Layers::new(explicit)) {
        Prepared::Ready(ReadyTurn::Claude(turn)) => turn,
        other => panic!("expected a ready claude turn, got {other:?}"),
    }
}

fn explicit_claude(prompt: &str) -> PartialTurn {
    PartialTurn {
        provider: Some(ProviderId::Claude),
        prompt: Some(prompt.to_string()),
        ..Default::default()
    }
}

#[test]
fn minimal_fold_uses_adapter_defaults() {
    let turn = claude_turn(&explicit_claude("inspect this repository"));
    assert_eq!(turn.prompt, "inspect this repository");
    assert_eq!(turn.working_directory, None);
    assert!(turn.session.is_none());
    assert_eq!(turn.options.system_prompt, None);
    assert_eq!(turn.options.append_system_prompt, None);
    assert_eq!(turn.options.model, None);
    assert_eq!(turn.options.fallback_model, None);
    assert_eq!(turn.options.effort, None);
    assert!(turn.options.allowed_tools.is_empty());
    assert!(turn.options.disallowed_tools.is_empty());
    assert!(turn.options.additional_directories.is_empty());
    assert_eq!(turn.options.max_turns, None);
    assert_eq!(turn.options.max_budget_usd, None);
    assert_eq!(turn.options.permission_mode, None);
    assert_eq!(turn.options.json_schema, None);
    assert!(!turn.options.strict_mcp_config);
    assert_eq!(turn.options.ambient_context, ClaudeAmbientContext::Inherit);
}

#[test]
fn shared_vocabulary_folds_into_concrete_fields() {
    let mut explicit = explicit_claude("review the current branch");
    explicit.model.name = Some("claude-sonnet-5".to_string());
    explicit.context.cwd = Some(PathBuf::from("/repos/example"));
    explicit.context.additional_directories = Some(vec![PathBuf::from("/repos/shared")]);

    let turn = claude_turn(&explicit);
    assert_eq!(turn.options.model.as_deref(), Some("claude-sonnet-5"));
    assert_eq!(
        turn.working_directory.as_deref(),
        Some(Path::new("/repos/example"))
    );
    assert_eq!(
        turn.options.additional_directories,
        vec![PathBuf::from("/repos/shared")]
    );
}

#[test]
fn provider_options_fold_into_claude_fields() {
    let mut explicit = explicit_claude("summarize the diff");
    explicit.provider_options.claude = Some(PartialClaudeOptions {
        system_prompt: Some("respond in json".to_string()),
        append_system_prompt: Some("be brief".to_string()),
        fallback_model: Some("claude-opus-5".to_string()),
        effort: Some(EffortChoice::High),
        allowed_tools: Some(vec!["Read".to_string(), "Grep".to_string()]),
        disallowed_tools: Some(vec!["Bash".to_string()]),
        max_turns: Some(4),
        max_budget_usd: Some(2.5),
        permission_mode: Some(PermissionModeChoice::DontAsk),
        json_schema: Some(serde_json::json!({ "type": "object" })),
        strict_mcp_config: Some(true),
        ambient_context: None,
    });

    let turn = claude_turn(&explicit);
    assert_eq!(
        turn.options.system_prompt.as_deref(),
        Some("respond in json")
    );
    assert_eq!(
        turn.options.append_system_prompt.as_deref(),
        Some("be brief")
    );
    assert_eq!(
        turn.options.fallback_model.as_deref(),
        Some("claude-opus-5")
    );
    assert_eq!(turn.options.effort, Some(ClaudeEffort::High));
    assert_eq!(turn.options.allowed_tools, vec!["Read", "Grep"]);
    assert_eq!(turn.options.disallowed_tools, vec!["Bash"]);
    assert_eq!(turn.options.max_turns, Some(4));
    assert_eq!(turn.options.max_budget_usd, Some(2.5));
    assert_eq!(
        turn.options.permission_mode,
        Some(ClaudePermissionMode::DontAsk)
    );
    assert_eq!(
        turn.options.json_schema.as_deref(),
        Some(r#"{"type":"object"}"#)
    );
    assert!(turn.options.strict_mcp_config);
}

#[test]
fn ambient_context_modes_fold_exactly() {
    let cases = [
        (AmbientContextChoice::Inherit, ClaudeAmbientContext::Inherit),
        (
            AmbientContextChoice::HermeticProject,
            ClaudeAmbientContext::Hermetic(ClaudeHermetic::Project),
        ),
        (
            AmbientContextChoice::HermeticFull,
            ClaudeAmbientContext::Hermetic(ClaudeHermetic::Full),
        ),
        (AmbientContextChoice::Safe, ClaudeAmbientContext::Safe),
        (AmbientContextChoice::Bare, ClaudeAmbientContext::Bare),
    ];
    for (choice, expected) in cases {
        let mut explicit = explicit_claude("inspect this repository");
        explicit.provider_options.claude = Some(PartialClaudeOptions {
            ambient_context: Some(choice),
            ..Default::default()
        });
        let turn = claude_turn(&explicit);
        assert_eq!(turn.options.ambient_context, expected, "{choice:?}");
    }
}

#[test]
fn filesystem_permission_is_refused() {
    let mut explicit = explicit_claude("review the current branch");
    explicit.permissions.filesystem = Some(FilesystemChoice::WorkspaceWrite);

    let Prepared::Invalid { diagnostics } = prepare(Layers::new(&explicit)) else {
        panic!("expected invalid");
    };
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(
        diagnostics[0].code,
        diagnostic_codes::UNSUPPORTED_FILESYSTEM_PERMISSION
    );
}

#[test]
fn resume_folds_into_a_claude_tagged_session() {
    let mut explicit = explicit_claude("continue the review");
    explicit.context.resume = Some(ResumeBinding::new(
        ProviderId::Claude,
        "0199a213-81ef-7623-8004-e2f7469d612d",
    ));

    let turn = claude_turn(&explicit);
    let session = turn.session.expect("resumed turn carries a session");
    assert_eq!(session.provider(), "claude");
    assert_eq!(session.value(), "0199a213-81ef-7623-8004-e2f7469d612d");
}

#[cfg(feature = "codex")]
#[test]
fn compile_dispatches_by_provider() {
    let claude = prepare(Layers::new(&explicit_claude("inspect this repository")));
    assert!(matches!(claude, Prepared::Ready(ReadyTurn::Claude(_))));

    let codex = PartialTurn {
        provider: Some(ProviderId::Codex),
        prompt: Some("inspect this repository".to_string()),
        ..Default::default()
    };
    let codex = prepare(Layers::new(&codex));
    assert!(matches!(codex, Prepared::Ready(ReadyTurn::Codex(_))));
}

#[test]
fn prepare_composes_profile_and_answers_end_to_end() {
    let mut profile_turn = PartialTurn {
        provider: Some(ProviderId::Claude),
        ..Default::default()
    };
    profile_turn.provider_options.claude = Some(PartialClaudeOptions {
        permission_mode: Some(PermissionModeChoice::Plan),
        strict_mcp_config: Some(true),
        ..Default::default()
    });
    let profile = Profile {
        name: "careful-claude".to_string(),
        turn: profile_turn,
    };

    let explicit = PartialTurn::default();
    let missing = prepare(Layers::new(&explicit).with_profile(&profile));
    let Prepared::Missing { requirements, .. } = missing else {
        panic!("expected missing requirements");
    };
    assert_eq!(requirements.len(), 1);
    assert_eq!(requirements[0].id, "prompt");

    let answers = [Answer {
        id: "prompt".to_string(),
        value: "review the current branch".to_string(),
    }];
    let ready = prepare(
        Layers::new(&explicit)
            .with_profile(&profile)
            .with_answers(&answers),
    );
    let Prepared::Ready(ReadyTurn::Claude(turn)) = ready else {
        panic!("expected a ready claude turn, got {ready:?}");
    };
    assert_eq!(turn.prompt, "review the current branch");
    assert_eq!(
        turn.options.permission_mode,
        Some(ClaudePermissionMode::Plan)
    );
    assert!(turn.options.strict_mcp_config);
}
