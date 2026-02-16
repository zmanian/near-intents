use serde::{Deserialize, Serialize};

/// The action an intent wants to execute.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Action {
	Swap {
		from: String,
		to: String,
		amount: u64,
	},
	Bridge {
		from_chain: String,
		to_chain: String,
		asset: String,
		amount: u64,
	},
	Stake {
		validator: String,
		amount: u64,
	},
}

/// Constraints that must be satisfied for an intent to be settled.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Constraint {
	MinOutput(u64),
	MaxSlippage(u16),
	Deadline(u64),
}

/// A user intent: a declarative description of a desired outcome.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Intent {
	pub id: u64,
	pub action: Action,
	pub constraints: Vec<Constraint>,
	pub deadline: u64,
}

/// A single execution step within a solution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Step {
	Swap {
		dex: String,
		from: String,
		to: String,
		amount: u64,
	},
	Transfer {
		to: String,
		amount: u64,
	},
}

/// A solver's proposed solution for a specific intent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Solution {
	pub intent_id: u64,
	pub solver_id: String,
	pub execution_plan: Vec<Step>,
	pub price: u64,
	pub score: u64,
}

/// The result of a settlement round: which intents were matched and the winning
/// solution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settlement {
	pub round: u64,
	pub matched_intents: Vec<u64>,
	pub winning_solution_intent: u64,
}
