use {
	crate::types::{Intent, Settlement, Solution},
	mosaik::{groups::StateMachine, primitives::UniqueId, unique_id},
	serde::{Deserialize, Serialize},
	std::collections::BTreeMap,
};

/// Commands that mutate the auction state machine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AuctionCommand {
	SubmitIntent(Intent),
	SubmitSolution(Solution),
	ClearRound,
}

/// Queries against the auction state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AuctionQuery {
	PendingIntents,
	RoundResult(u64),
	CurrentRound,
}

/// Results returned by auction queries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AuctionQueryResult {
	Intents(Vec<Intent>),
	Round(Option<Settlement>),
	RoundNumber(u64),
}

/// A batch-auction state machine replicated via Raft consensus.
///
/// Collects intents and solver solutions, then clears each round by picking the
/// highest-scoring solution per intent.
#[derive(Debug)]
pub struct AuctionStateMachine {
	pending_intents: BTreeMap<u64, Intent>,
	current_round: u64,
	solutions: BTreeMap<u64, Vec<Solution>>,
	round_results: Vec<Settlement>,
}

impl AuctionStateMachine {
	pub fn new() -> Self {
		Self {
			pending_intents: BTreeMap::new(),
			current_round: 0,
			solutions: BTreeMap::new(),
			round_results: Vec::new(),
		}
	}
}

impl StateMachine for AuctionStateMachine {
	const ID: UniqueId = unique_id!(
		"6e656172696e74656e74732d61756374696f6e2d763100000000000000000001"
	);

	type Command = AuctionCommand;
	type Query = AuctionQuery;
	type QueryResult = AuctionQueryResult;

	fn reset(&mut self) {
		self.pending_intents.clear();
		self.current_round = 0;
		self.solutions.clear();
		self.round_results.clear();
	}

	fn apply(&mut self, command: Self::Command) {
		match command {
			AuctionCommand::SubmitIntent(intent) => {
				let id = intent.id;
				self.pending_intents.insert(id, intent);
			}
			AuctionCommand::SubmitSolution(solution) => {
				self.solutions
					.entry(solution.intent_id)
					.or_default()
					.push(solution);
			}
			AuctionCommand::ClearRound => {
				let mut matched = Vec::new();

				for (&intent_id, solutions) in &self.solutions {
					if !self.pending_intents.contains_key(&intent_id) {
						continue;
					}
					// Pick the highest-scoring solution for this intent.
					if let Some(best) =
						solutions.iter().max_by_key(|s| s.score)
					{
						matched.push(Settlement {
							round: self.current_round,
							matched_intents: vec![intent_id],
							winning_solution_intent: best.price,
						});
					}
				}

				// Remove matched intents from pending.
				for settlement in &matched {
					for id in &settlement.matched_intents {
						self.pending_intents.remove(id);
					}
				}

				self.round_results.extend(matched);
				self.solutions.clear();
				self.current_round += 1;
			}
		}
	}

	fn query(&self, query: Self::Query) -> Self::QueryResult {
		match query {
			AuctionQuery::PendingIntents => AuctionQueryResult::Intents(
				self.pending_intents.values().cloned().collect(),
			),
			AuctionQuery::RoundResult(round) => {
				let result = self
					.round_results
					.iter()
					.find(|s| s.round == round)
					.cloned();
				AuctionQueryResult::Round(result)
			}
			AuctionQuery::CurrentRound => {
				AuctionQueryResult::RoundNumber(self.current_round)
			}
		}
	}
}
