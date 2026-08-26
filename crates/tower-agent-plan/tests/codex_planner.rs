//! Typed Codex planner cases.
//!
//! The JSON corpus specifies resolution; these cases specify the fold from a
//! complete resolution into a concrete `Turn<CodexOptions>`, asserting the
//! typed output the adapter will actually receive.

#![cfg(feature = "codex")]

use std::path::{Path, PathBuf};

use tower_agent::FilesystemAuthority;
use tower_agent_plan::{
    Answer, FilesystemChoice, Layers, PartialCodexOptions, PartialTurn, Prepared, Profile,
    ProviderId, ReadyTurn, ResumeBinding, diagnostic_codes, prepare,
};

fn codex_turn(explicit: &PartialTurn) -> tower_agent::Turn<tower_agent_codex::CodexOptions> {
    match prepare(Layers::new(explicit)) {
        Prepared::Ready(ReadyTurn::Codex(turn)) => turn,
        other => panic!("expected a ready codex turn, got {other:?}"),
    }
}

fn explicit_codex(prompt: &str) -> PartialTurn {
    PartialTurn {
        provider: Some(ProviderId::Codex),
        prompt: Some(prompt.to_string()),
        ..Default::default()
    }
}

#[test]
fn minimal_fold_uses_adapter_defaults() {
    let turn = codex_turn(&explicit_codex("inspect this repository"));
    assert_eq!(turn.prompt, "inspect this repository");
    assert_eq!(turn.working_directory, None);
    assert!(turn.session.is_none());
    assert_eq!(turn.options.system_prompt, None);
    assert_eq!(turn.options.model, None);
    assert!(turn.options.additional_directories.is_empty());
    assert_eq!(turn.options.output_schema, None);
    assert_eq!(
        turn.options.filesystem_authority,
        FilesystemAuthority::ReadOnly
    );
    assert!(!turn.options.ephemeral);
}

#[test]
fn shared_vocabulary_folds_into_concrete_fields() {
    let mut explicit = explicit_codex("review the current branch");
    explicit.model.name = Some("gpt-5-codex".to_string());
    explicit.context.cwd = Some(PathBuf::from("/repos/example"));
    explicit.context.additional_directories = Some(vec![
        PathBuf::from("/repos/shared"),
        PathBuf::from("/tmp/a"),
    ]);
    explicit.permissions.filesystem = Some(FilesystemChoice::WorkspaceWrite);

    let turn = codex_turn(&explicit);
    assert_eq!(turn.options.model.as_deref(), Some("gpt-5-codex"));
    assert_eq!(
        turn.working_directory.as_deref(),
        Some(Path::new("/repos/example"))
    );
    assert_eq!(
        turn.options.additional_directories,
        vec![PathBuf::from("/repos/shared"), PathBuf::from("/tmp/a")]
    );
    assert_eq!(
        turn.options.filesystem_authority,
        FilesystemAuthority::WorkspaceWrite
    );
}

#[test]
fn provider_options_fold_into_codex_fields() {
    let mut explicit = explicit_codex("summarize the diff");
    explicit.provider_options.codex = Some(PartialCodexOptions {
        system_prompt: Some("respond in json".to_string()),
        output_schema: Some(serde_json::json!({ "type": "object" })),
        ephemeral: Some(true),
    });

    let turn = codex_turn(&explicit);
    assert_eq!(
        turn.options.system_prompt.as_deref(),
        Some("respond in json")
    );
    assert_eq!(
        turn.options.output_schema,
        Some(serde_json::json!({ "type": "object" }))
    );
    assert!(turn.options.ephemeral);
}

#[test]
fn resume_folds_into_a_codex_tagged_session() {
    let mut explicit = explicit_codex("continue the review");
    explicit.context.resume = Some(ResumeBinding::new(
        ProviderId::Codex,
        "0199a213-81ef-7623-8004-e2f7469d612d",
    ));

    let turn = codex_turn(&explicit);
    let session = turn.session.expect("resumed turn carries a session");
    assert_eq!(session.provider(), "codex");
    assert_eq!(session.value(), "0199a213-81ef-7623-8004-e2f7469d612d");
}

#[test]
fn resumed_additional_directories_are_refused() {
    let mut explicit = explicit_codex("continue the review");
    explicit.context.resume = Some(ResumeBinding::new(ProviderId::Codex, "abc"));
    explicit.context.additional_directories = Some(vec![PathBuf::from("/repos/shared")]);

    let Prepared::Invalid { diagnostics } = prepare(Layers::new(&explicit)) else {
        panic!("expected invalid");
    };
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(
        diagnostics[0].code,
        diagnostic_codes::RESUMED_ADDITIONAL_DIRECTORIES
    );
}

#[test]
fn resumed_with_empty_directory_list_is_ready() {
    let mut explicit = explicit_codex("continue the review");
    explicit.context.resume = Some(ResumeBinding::new(ProviderId::Codex, "abc"));
    explicit.context.additional_directories = Some(Vec::new());

    let turn = codex_turn(&explicit);
    assert!(turn.session.is_some());
    assert!(turn.options.additional_directories.is_empty());
}

/// With the claude feature enabled the claude planner handles this provider,
/// so the case only exists in codex-only builds.
#[cfg(not(feature = "claude"))]
#[test]
fn unplanned_provider_is_an_unsupported_diagnostic() {
    use tower_agent_plan::{Resolution, compile, resolve};

    let explicit = PartialTurn {
        provider: Some(ProviderId::Claude),
        prompt: Some("inspect this repository".to_string()),
        ..Default::default()
    };
    let Resolution::Complete(resolved) = resolve(Layers::new(&explicit)) else {
        panic!("expected complete resolution");
    };
    let Err(diagnostics) = compile(&resolved) else {
        panic!("expected compile refusal");
    };
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].code, diagnostic_codes::UNSUPPORTED_PROVIDER);
}

#[test]
fn prepare_composes_profile_and_answers_end_to_end() {
    let mut profile_turn = PartialTurn {
        provider: Some(ProviderId::Codex),
        permissions: tower_agent_plan::PartialPermissions {
            filesystem: Some(FilesystemChoice::WorkspaceWrite),
        },
        ..Default::default()
    };
    profile_turn.provider_options.codex = Some(PartialCodexOptions {
        ephemeral: Some(true),
        ..Default::default()
    });
    let profile = Profile {
        name: "careful-codex".to_string(),
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
    let Prepared::Ready(ReadyTurn::Codex(turn)) = ready else {
        panic!("expected a ready codex turn, got {ready:?}");
    };
    assert_eq!(turn.prompt, "review the current branch");
    assert_eq!(
        turn.options.filesystem_authority,
        FilesystemAuthority::WorkspaceWrite
    );
    assert!(turn.options.ephemeral);
}
