use kontur_core::{GatePolicy, OperatorId};

/// Session-wide inputs shared across every gate: the co-constructed prompt, the
/// agent identity, the operator roster, and the gate policy.
#[derive(Clone, Debug)]
pub struct SessionContext {
    pub prompt: String,
    pub prompt_author: OperatorId,
    pub agent_id: String,
    pub agent_model: String,
    pub agent_version: String,
    /// Public identities of the operators supervising this session.
    pub operators: Vec<OperatorId>,
    pub policy: GatePolicy,
}

impl SessionContext {
    pub fn new(
        prompt: impl Into<String>,
        prompt_author: OperatorId,
        agent_id: impl Into<String>,
        agent_model: impl Into<String>,
        agent_version: impl Into<String>,
        operators: Vec<OperatorId>,
    ) -> Self {
        SessionContext {
            prompt: prompt.into(),
            prompt_author,
            agent_id: agent_id.into(),
            agent_model: agent_model.into(),
            agent_version: agent_version.into(),
            operators,
            policy: GatePolicy::default(),
        }
    }

    pub fn with_policy(mut self, policy: GatePolicy) -> Self {
        self.policy = policy;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kontur_core::{Independence, OperatorId};

    fn op(n: u8) -> OperatorId {
        OperatorId([n; 32])
    }

    #[test]
    fn defaults_to_core_gate_policy() {
        let ctx = SessionContext::new("do the thing", op(1), "agent-01", "claude", "1", vec![op(1), op(2)]);
        assert_eq!(ctx.policy, GatePolicy::default());
        assert_eq!(ctx.operators.len(), 2);
    }

    #[test]
    fn with_policy_overrides() {
        let p = GatePolicy { independence: Independence::Pragmatic, ..GatePolicy::default() };
        let ctx = SessionContext::new("x", op(1), "a", "m", "v", vec![op(1)]).with_policy(p);
        assert_eq!(ctx.policy.independence, Independence::Pragmatic);
    }
}
