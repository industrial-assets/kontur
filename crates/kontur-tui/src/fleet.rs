use crate::view::AgentCard;

/// Source of fleet (agent) status for the watch-floor. Mocked in this slice;
/// a real source arrives with the live-agent binding.
pub trait FleetSource {
    fn agents(&self) -> Vec<AgentCard>;
}

/// A scripted fleet for the demo console.
pub struct MockFleet {
    agents: Vec<AgentCard>,
}

impl MockFleet {
    pub fn new(agents: Vec<AgentCard>) -> Self {
        MockFleet { agents }
    }

    /// A small demo fleet: two calm agents and one needing sign-off.
    pub fn demo() -> Self {
        MockFleet::new(vec![
            AgentCard { id: "agent-01".into(), status: "analysing parser.rs".into(), tokens: 3100, needs_signoff: false },
            AgentCard { id: "agent-02".into(), status: "editing auth".into(), tokens: 1200, needs_signoff: false },
            AgentCard { id: "agent-03".into(), status: "needs sign-off".into(), tokens: 0, needs_signoff: true },
        ])
    }
}

impl FleetSource for MockFleet {
    fn agents(&self) -> Vec<AgentCard> {
        self.agents.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_fleet_has_one_needing_signoff() {
        let f = MockFleet::demo();
        let agents = f.agents();
        assert_eq!(agents.len(), 3);
        assert_eq!(agents.iter().filter(|a| a.needs_signoff).count(), 1);
    }

    #[test]
    fn new_round_trips_agents() {
        let a = AgentCard { id: "x".into(), status: "y".into(), tokens: 5, needs_signoff: false };
        let f = MockFleet::new(vec![a.clone()]);
        assert_eq!(f.agents(), vec![a]);
    }
}
