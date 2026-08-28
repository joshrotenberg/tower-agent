use std::time::Duration;

use serde_json::{Value, json};
use tower_agent::{
    AgentError, Cost, EffectState, FailureEvidence, OperationId, SessionHandle, TokenUsage,
    TurnOutcome,
};

use crate::ContinuationId;

/// Whether provider-authored text may be published.
///
/// A provider decides what goes in its own error strings, and those strings
/// have been observed to contain session values. An adapter that forwards them
/// verbatim has delegated its redaction policy to the provider.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ProviderMessages {
    /// Replace provider text with the failure classification. The default,
    /// because it is safe without knowing who can read the output.
    #[default]
    Redacted,
    /// Publish provider text as written.
    ///
    /// Appropriate for a single-user local server whose operator already sees
    /// everything the provider sees. On a shared server this publishes
    /// whatever a provider chose to put in a message, to whoever called the
    /// tool.
    Verbatim,
}

/// Turns terminal results into transport-neutral structured content.
///
/// This produces [`serde_json::Value`] rather than a protocol type so the
/// rules it enforces, redaction and evidence projection, are testable without
/// a transport. Wrapping the value into a protocol response belongs to the
/// tool layer.
///
/// Two rules are inherited rather than decided here, because
/// `tower_mcp_composition` already fixed them: a provider session value never
/// crosses the boundary, and provider-authored strings are untrusted for
/// publication.
#[derive(Clone, Copy, Debug, Default)]
pub struct Projection {
    provider_messages: ProviderMessages,
}

impl Projection {
    /// A projection that redacts provider text.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Choose whether provider text is published.
    #[must_use]
    pub fn with_provider_messages(mut self, provider_messages: ProviderMessages) -> Self {
        self.provider_messages = provider_messages;
        self
    }

    /// The failure text this projection is willing to publish.
    #[must_use]
    pub fn message(&self, error: &AgentError) -> String {
        match self.provider_messages {
            ProviderMessages::Redacted => format!("agent operation failed ({})", error.kind),
            ProviderMessages::Verbatim => error.message.clone(),
        }
    }

    /// Project a settled turn.
    ///
    /// `continuation` is the public name for the outcome's session, when the
    /// host minted one. Absent means the turn is not resumable through this
    /// adapter, which is distinct from the turn having had no session at all.
    #[must_use]
    pub fn outcome(
        &self,
        outcome: &TurnOutcome,
        operation_id: OperationId,
        continuation: Option<&ContinuationId>,
    ) -> Value {
        json!({
            "operationId": operation_id.get(),
            "output": outcome.output,
            "structured": outcome.structured,
            "session": session_json(outcome.session.as_ref(), continuation),
            "usage": usage_json(outcome.usage),
            "cost": cost_json(outcome.cost.as_ref()),
            "durationMs": outcome.duration.map(duration_millis),
            "providerTurns": outcome.provider_turns,
        })
    }

    /// Project a failure, including whatever evidence it retained.
    ///
    /// A failed turn can still be resumable, because [`FailureEvidence`]
    /// carries a session. `continuation` names that session when the host
    /// minted an identifier for it.
    #[must_use]
    pub fn failure(
        &self,
        error: &AgentError,
        operation_id: OperationId,
        continuation: Option<&ContinuationId>,
    ) -> Value {
        let mut value = self.failure_body(error, continuation);
        value["operationId"] = json!(operation_id.get());
        value
    }

    fn failure_body(&self, error: &AgentError, continuation: Option<&ContinuationId>) -> Value {
        let evidence = error.evidence.as_deref();
        json!({
            "kind": error.kind.to_string(),
            "phase": error.phase.to_string(),
            "effects": error.effects.to_string(),
            "message": self.message(error),
            // Machine-readable form of the rule in docs/resilience.md. Only a
            // provably effect-free failure may be replayed automatically, and
            // a client holding a continuation id will otherwise assume the id
            // is how to try again. Continuing a conversation is not retrying
            // a turn, and this field is about the turn.
            "replaySafe": error.effects == EffectState::None,
            "evidence": evidence_json(evidence, continuation),
            // Causes are projected under the same policy, which matters:
            // the inner message is the one observed carrying a session value.
            "cause": error
                .cause
                .as_deref()
                .map(|cause| self.failure_body(cause, None)),
        })
    }
}

/// Names a session without revealing it.
///
/// `present` says a session exists. `continuation` is how a client refers to
/// it later, and is absent when the host minted no identifier. The provider
/// tag is published because a client needs it to know which provider it is
/// continuing with; the handle value is never published at all.
fn session_json(session: Option<&SessionHandle>, continuation: Option<&ContinuationId>) -> Value {
    match session {
        None => Value::Null,
        Some(session) => json!({
            "provider": session.provider(),
            "present": true,
            "continuation": continuation.map(ContinuationId::as_str),
        }),
    }
}

fn evidence_json(
    evidence: Option<&FailureEvidence>,
    continuation: Option<&ContinuationId>,
) -> Value {
    json!({
        "session": session_json(
            evidence.and_then(|evidence| evidence.session.as_ref()),
            continuation,
        ),
        "usage": usage_json(evidence.and_then(|evidence| evidence.usage)),
        "cost": cost_json(evidence.and_then(|evidence| evidence.cost.as_ref())),
        "durationMs": evidence.and_then(|evidence| evidence.duration).map(duration_millis),
        "providerTurns": evidence.and_then(|evidence| evidence.provider_turns),
    })
}

/// Absent buckets stay null rather than becoming zero.
///
/// The core keeps missing accounting absent instead of synthesizing it, and a
/// projection that emitted `0` here would undo that at the boundary, where it
/// is least recoverable.
fn usage_json(usage: Option<TokenUsage>) -> Value {
    match usage {
        None => Value::Null,
        Some(usage) => json!({
            "input": usage.input,
            "cachedInput": usage.cached_input,
            "cacheWriteInput": usage.cache_write_input,
            "output": usage.output,
            "reasoningOutput": usage.reasoning_output,
            "total": usage.total(),
        }),
    }
}

fn cost_json(cost: Option<&Cost>) -> Value {
    match cost {
        None => Value::Null,
        Some(cost) => json!({ "amount": cost.amount, "currency": cost.currency }),
    }
}

fn duration_millis(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests {
    use tower_agent::{ErrorKind, FailurePhase};

    use super::*;

    const PRIVATE: &str = "host-private-session";

    fn operation() -> OperationId {
        OperationId::from_u64(42)
    }

    fn settled() -> TurnOutcome {
        TurnOutcome {
            session: Some(SessionHandle::new("claude", PRIVATE)),
            usage: Some(TokenUsage {
                input: Some(13),
                output: Some(8),
                ..TokenUsage::default()
            }),
            cost: Some(Cost::usd(0.25)),
            duration: Some(Duration::from_millis(42)),
            provider_turns: Some(2),
            ..TurnOutcome::new("done")
        }
    }

    /// The failure shape observed in `tower_mcp_composition`: an outer
    /// deadline wrapping a provider cause whose message carries a session
    /// value.
    fn failed() -> AgentError {
        AgentError::deadline_exceeded(EffectState::Possible).with_cause(AgentError::new(
            ErrorKind::Provider,
            format!("provider reported a partial edit for {PRIVATE}"),
            FailurePhase::Settlement,
            EffectState::Reported,
        ))
    }

    #[test]
    fn a_settled_turn_projects_every_terminal_fact() {
        let value = Projection::new().outcome(&settled(), operation(), None);

        assert_eq!(value["operationId"], 42);
        assert_eq!(value["output"], "done");
        assert_eq!(value["usage"]["total"], 21);
        assert_eq!(value["cost"]["amount"], 0.25);
        assert_eq!(value["durationMs"], 42);
        assert_eq!(value["providerTurns"], 2);
    }

    #[test]
    fn a_session_is_named_but_never_revealed() {
        let id = ContinuationId::parse("public-name").expect("id");
        let value = Projection::new().outcome(&settled(), operation(), Some(&id));

        assert_eq!(value["session"]["provider"], "claude");
        assert_eq!(value["session"]["present"], true);
        assert_eq!(value["session"]["continuation"], "public-name");
        assert!(!value.to_string().contains(PRIVATE), "{value}");
    }

    #[test]
    fn a_session_with_no_continuation_is_present_but_unnameable() {
        let value = Projection::new().outcome(&settled(), operation(), None);

        // The distinction the composition test could not express: a session
        // exists, and this adapter minted no way to refer to it.
        assert_eq!(value["session"]["present"], true);
        assert_eq!(value["session"]["continuation"], Value::Null);
    }

    #[test]
    fn no_session_projects_null_rather_than_an_empty_object() {
        let value = Projection::new().outcome(&TurnOutcome::new("done"), operation(), None);
        assert_eq!(value["session"], Value::Null);
    }

    #[test]
    fn absent_accounting_stays_absent_rather_than_becoming_zero() {
        let value = Projection::new().outcome(&TurnOutcome::new("done"), operation(), None);

        assert_eq!(value["usage"], Value::Null);
        assert_eq!(value["cost"], Value::Null);
        assert_eq!(value["durationMs"], Value::Null);
        assert_eq!(value["providerTurns"], Value::Null);
    }

    #[test]
    fn an_explicitly_reported_zero_is_kept_as_a_measurement() {
        let outcome = TurnOutcome {
            usage: Some(TokenUsage {
                input: Some(0),
                ..TokenUsage::default()
            }),
            ..TurnOutcome::new("done")
        };
        let value = Projection::new().outcome(&outcome, operation(), None);

        assert_eq!(value["usage"]["input"], 0);
        assert_eq!(value["usage"]["output"], Value::Null);
    }

    #[test]
    fn a_failure_keeps_its_classification_and_its_cause() {
        let value = Projection::new().failure(&failed(), operation(), None);

        assert_eq!(value["operationId"], 42);
        assert_eq!(value["kind"], "deadline_exceeded");
        // with_cause raises the outer effect state to the inner one.
        assert_eq!(value["effects"], "reported");
        assert_eq!(value["cause"]["kind"], "provider");
        assert_eq!(value["cause"]["phase"], "settlement");
        assert_eq!(value["cause"]["effects"], "reported");
    }

    #[test]
    fn provider_text_is_redacted_by_default_through_the_whole_cause_chain() {
        let value = Projection::new().failure(&failed(), operation(), None);

        // The inner message is the one carrying a session value, so redacting
        // only the outer message would leak.
        assert_eq!(
            value["message"],
            "agent operation failed (deadline_exceeded)"
        );
        assert_eq!(
            value["cause"]["message"],
            "agent operation failed (provider)"
        );
        assert!(!value.to_string().contains(PRIVATE), "{value}");
    }

    #[test]
    fn verbatim_provider_text_publishes_what_the_provider_wrote() {
        let value = Projection::new()
            .with_provider_messages(ProviderMessages::Verbatim)
            .failure(&failed(), operation(), None);

        // This is the opt-in, and this assertion is what it costs: the
        // provider put a session value in its message and the projection
        // published it. Correct for a single-user local server, and the
        // reason the default is the other one.
        assert!(
            value["cause"]["message"]
                .as_str()
                .unwrap()
                .contains(PRIVATE)
        );
    }

    #[test]
    fn replay_safety_follows_the_effect_state_and_nothing_else() {
        let projection = Projection::new();

        let refused = AgentError::invalid_request("bad");
        assert_eq!(refused.effects, EffectState::None);
        assert_eq!(
            projection.failure(&refused, operation(), None)["replaySafe"],
            true
        );

        // Same kind of failure, different evidence about what already ran.
        let in_flight = AgentError::cancelled(EffectState::Possible);
        assert_eq!(
            projection.failure(&in_flight, operation(), None)["replaySafe"],
            false
        );
        assert_eq!(
            projection.failure(&failed(), operation(), None)["replaySafe"],
            false
        );
    }

    #[test]
    fn a_failed_turn_can_still_be_named_for_continuation() {
        let id = ContinuationId::parse("public-name").expect("id");
        let error =
            AgentError::deadline_exceeded(EffectState::Possible).with_evidence(FailureEvidence {
                session: Some(SessionHandle::new("codex", PRIVATE)),
                ..FailureEvidence::default()
            });

        let value = Projection::new().failure(&error, operation(), Some(&id));

        assert_eq!(value["evidence"]["session"]["provider"], "codex");
        assert_eq!(value["evidence"]["session"]["continuation"], "public-name");
        // Resumable and not replayable are different claims, and both are
        // present here without contradicting each other.
        assert_eq!(value["replaySafe"], false);
        assert!(!value.to_string().contains(PRIVATE), "{value}");
    }

    #[test]
    fn a_failure_without_evidence_reports_absence_rather_than_zero() {
        let value =
            Projection::new().failure(&AgentError::invalid_request("bad"), operation(), None);

        assert_eq!(value["evidence"]["session"], Value::Null);
        assert_eq!(value["evidence"]["usage"], Value::Null);
        assert_eq!(value["evidence"]["cost"], Value::Null);
        assert_eq!(value["cause"], Value::Null);
    }
}
