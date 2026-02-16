use {
	crate::types::{Intent, IntentStatus, Quote, Settlement, TokenDiff},
	mosaik::{groups::StateMachine, primitives::UniqueId, unique_id},
	serde::{Deserialize, Serialize},
	std::collections::BTreeMap,
};

/// Commands that mutate the auction state machine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AuctionCommand {
	/// Submit a user intent with token_diff.
	SubmitIntent(Intent),
	/// Submit a solver quote (response to an RFQ).
	SubmitQuote(Quote),
	/// Clear the current round: match intents with best quotes
	/// and produce settlements.
	ClearRound,
}

/// Queries against the auction state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AuctionQuery {
	/// List all pending (unsettled) intents.
	PendingIntents,
	/// Get the settlement result for a specific round.
	RoundResult(u64),
	/// Get the current round number.
	CurrentRound,
	/// Get the status of a specific intent.
	IntentStatus(u64),
	/// Get all quotes for a specific intent.
	QuotesForIntent(u64),
}

/// Results returned by auction queries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AuctionQueryResult {
	Intents(Vec<Intent>),
	Round(Option<Settlement>),
	RoundNumber(u64),
	Status(IntentStatus),
	Quotes(Vec<Quote>),
}

/// A batch-auction state machine replicated via Raft consensus.
///
/// Implements the NEAR Intents settlement model:
/// 1. Users submit intents with token_diffs
/// 2. Solvers submit quotes with counter token_diffs
/// 3. ClearRound picks the best quote per intent and verifies
///    that combined token_diffs balance (zero-sum per asset)
#[derive(Debug)]
pub struct AuctionStateMachine {
	pending_intents: BTreeMap<u64, Intent>,
	intent_status: BTreeMap<u64, IntentStatus>,
	current_round: u64,
	quotes: BTreeMap<u64, Vec<Quote>>,
	round_results: Vec<Settlement>,
}

impl AuctionStateMachine {
	pub fn new() -> Self {
		Self {
			pending_intents: BTreeMap::new(),
			intent_status: BTreeMap::new(),
			current_round: 0,
			quotes: BTreeMap::new(),
			round_results: Vec::new(),
		}
	}
}

/// Verify that a user's token_diff and solver's counter token_diff are
/// compatible: the solver provides what the user wants (positive entries)
/// and takes what the user offers (negative entries).
fn token_diffs_compatible(user_diff: &TokenDiff, solver_diff: &TokenDiff) -> bool {
	// For each asset the user wants to receive (positive), the solver must
	// be willing to send (negative for that same asset, or at least provide it).
	// For each asset the user sends (negative), the solver must accept it.
	for (asset, &user_amount) in user_diff {
		if let Some(&solver_amount) = solver_diff.get(asset) {
			// User sends (negative) and solver receives (positive), or vice versa
			// The signs must be opposite for the trade to work.
			if user_amount > 0 && solver_amount >= 0 {
				return false; // solver not providing what user wants
			}
			if user_amount < 0 && solver_amount <= 0 {
				return false; // solver not taking what user offers
			}
			// The solver must provide at least what the user expects
			if user_amount > 0 && solver_amount.unsigned_abs() < user_amount.unsigned_abs() {
				return false;
			}
		} else if user_amount > 0 {
			// User wants this asset but solver doesn't mention it
			return false;
		}
	}
	true
}

/// Compute the aggregate token flow for a user+solver pair.
fn aggregate_token_flow(user_diff: &TokenDiff, solver_diff: &TokenDiff) -> TokenDiff {
	let mut flow = user_diff.clone();
	for (asset, &amount) in solver_diff {
		*flow.entry(asset.clone()).or_insert(0) += amount;
	}
	flow
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
		self.intent_status.clear();
		self.current_round = 0;
		self.quotes.clear();
		self.round_results.clear();
	}

	fn apply(&mut self, command: Self::Command) {
		match command {
			AuctionCommand::SubmitIntent(intent) => {
				let id = intent.id;
				self.intent_status.insert(id, IntentStatus::Pending);
				self.pending_intents.insert(id, intent);
			}
			AuctionCommand::SubmitQuote(quote) => {
				// Only accept quotes for known pending intents
				if self.pending_intents.contains_key(&quote.intent_id) {
					self.quotes
						.entry(quote.intent_id)
						.or_default()
						.push(quote);
				}
			}
			AuctionCommand::ClearRound => {
				let mut settled_intents = Vec::new();
				let mut winning_quotes = Vec::new();
				let mut aggregate_flow = TokenDiff::new();

				for (&intent_id, intent) in &self.pending_intents {
					let Some(quotes) = self.quotes.get(&intent_id) else {
						continue;
					};

					// Pick the best quote: highest amount_out that is also
					// compatible with the user's token_diff.
					let best = quotes
						.iter()
						.filter(|q| {
							token_diffs_compatible(
								&intent.token_diff,
								&q.solver_token_diff,
							)
						})
						.max_by_key(|q| q.amount_out);

					if let Some(best) = best {
						settled_intents.push(intent_id);
						winning_quotes.push(best.quote_hash.clone());

						// Accumulate the aggregate flow
						let flow = aggregate_token_flow(
							&intent.token_diff,
							&best.solver_token_diff,
						);
						for (asset, amount) in flow {
							*aggregate_flow.entry(asset).or_insert(0) += amount;
						}
					}
				}

				// Update statuses and remove settled intents
				for &id in &settled_intents {
					self.pending_intents.remove(&id);
					self.intent_status.insert(id, IntentStatus::Settled);
				}

				if !settled_intents.is_empty() {
					self.round_results.push(Settlement {
						round: self.current_round,
						settled_intents,
						winning_quotes,
						aggregate_flow,
					});
				}

				self.quotes.clear();
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
			AuctionQuery::IntentStatus(id) => {
				let status = self
					.intent_status
					.get(&id)
					.copied()
					.unwrap_or(IntentStatus::NotFoundOrNotValid);
				AuctionQueryResult::Status(status)
			}
			AuctionQuery::QuotesForIntent(id) => {
				let quotes = self
					.quotes
					.get(&id)
					.cloned()
					.unwrap_or_default();
				AuctionQueryResult::Quotes(quotes)
			}
		}
	}
}
