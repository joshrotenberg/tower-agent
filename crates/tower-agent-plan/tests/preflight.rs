//! Preflight closes the gap between planner `Ready` and adapter refusal.
//!
//! `compile` proves a fold is representable; preflight proves one configured
//! service will not refuse it during the validation phase. These cases show
//! both halves: a Ready turn a stricter service still refuses, and a Ready
//! turn that passes.

#[cfg(feature = "codex")]
mod codex {
    use tower_agent_codex::CodexService;
    use tower_agent_plan::{
        FilesystemChoice, Layers, PartialTurn, Prepared, ProviderId, ReadyTurn, diagnostic_codes,
        prepare,
    };

    fn ready_codex(explicit: &PartialTurn) -> tower_agent::Turn<tower_agent_codex::CodexOptions> {
        match prepare(Layers::new(explicit)) {
            Prepared::Ready(ReadyTurn::Codex(turn)) => turn,
            other => panic!("expected a ready codex turn, got {other:?}"),
        }
    }

    #[test]
    fn ready_turn_can_still_be_refused_by_a_stricter_service() {
        let mut explicit = PartialTurn {
            provider: Some(ProviderId::Codex),
            prompt: Some("write the report".to_string()),
            ..Default::default()
        };
        explicit.permissions.filesystem = Some(FilesystemChoice::WorkspaceWrite);
        let turn = ready_codex(&explicit);

        let service = CodexService::new();
        let diagnostic =
            tower_agent_plan::codex::preflight(&service, &turn).expect_err("ceiling is read-only");
        assert_eq!(diagnostic.code, diagnostic_codes::ADAPTER_REFUSAL);
    }

    #[test]
    fn ready_turn_passes_preflight_for_a_matching_service() {
        let explicit = PartialTurn {
            provider: Some(ProviderId::Codex),
            prompt: Some("inspect this repository".to_string()),
            ..Default::default()
        };
        let turn = ready_codex(&explicit);
        assert!(tower_agent_plan::codex::preflight(&CodexService::new(), &turn).is_ok());
    }
}

#[cfg(feature = "claude")]
mod claude {
    use tower_agent_claude::ClaudeService;
    use tower_agent_plan::{
        Layers, PartialClaudeOptions, PartialTurn, Prepared, ProviderId, ReadyTurn,
        diagnostic_codes, prepare,
    };

    #[test]
    fn ready_turn_can_still_be_refused_by_the_adapter_checks() {
        let mut explicit = PartialTurn {
            provider: Some(ProviderId::Claude),
            prompt: Some("inspect this repository".to_string()),
            ..Default::default()
        };
        explicit.provider_options.claude = Some(PartialClaudeOptions {
            max_turns: Some(0),
            ..Default::default()
        });
        let Prepared::Ready(ReadyTurn::Claude(turn)) = prepare(Layers::new(&explicit)) else {
            panic!("expected a ready claude turn");
        };

        let diagnostic = tower_agent_plan::claude::preflight(&ClaudeService::new(), &turn)
            .expect_err("zero max turns is refused");
        assert_eq!(diagnostic.code, diagnostic_codes::ADAPTER_REFUSAL);
    }

    #[test]
    fn ready_turn_passes_preflight_for_a_matching_service() {
        let explicit = PartialTurn {
            provider: Some(ProviderId::Claude),
            prompt: Some("inspect this repository".to_string()),
            ..Default::default()
        };
        let Prepared::Ready(ReadyTurn::Claude(turn)) = prepare(Layers::new(&explicit)) else {
            panic!("expected a ready claude turn");
        };
        assert!(tower_agent_plan::claude::preflight(&ClaudeService::new(), &turn).is_ok());
    }
}
