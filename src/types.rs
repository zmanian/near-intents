use {
	serde::{Deserialize, Serialize},
	std::collections::BTreeMap,
};

/// A Defuse-style asset identifier.
///
/// Format: `<standard>:<account>` e.g. `nep141:usdc.near`, `nep141:wrap.near`
///
/// In the real protocol, the multi-token standard on NEAR uses identifiers
/// like `nep141:usdc.near` for NEP-141 fungible tokens.
pub type AssetId = String;

/// A token_diff intent: declares desired balance changes per asset.
///
/// Positive values = tokens to receive.
/// Negative values = tokens to send.
///
/// Example: Alice wants to swap 1000 USDC for NEAR:
///   `{"nep141:usdc.near": -1000, "nep141:wrap.near": 950}`
///
/// This is the core primitive of the NEAR Intents / Defuse protocol.
/// Compatible token_diffs from different signers can be atomically settled
/// by the Verifier contract.
pub type TokenDiff = BTreeMap<AssetId, i128>;

/// A user intent following the NEAR Intents / Defuse protocol.
///
/// The signer declares a `token_diff` specifying desired balance changes.
/// Solvers compete to provide matching counter-diffs that satisfy the intent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Intent {
	/// Unique intent identifier (nonce in the real protocol is 256-bit).
	pub id: u64,

	/// The account that signed this intent.
	pub signer_id: String,

	/// Desired token balance changes.
	pub token_diff: TokenDiff,

	/// ISO 8601-style deadline (here simplified to unix millis).
	/// The intent is invalid after this time.
	pub deadline_ms: u64,

	/// Optional minimum deadline for solver quotes in milliseconds.
	/// Solvers must provide quotes valid for at least this long.
	/// Default in the real protocol is 60_000ms (1 minute).
	pub min_quote_deadline_ms: u64,
}

/// An RFQ (Request for Quote) broadcast to solvers.
///
/// In the real protocol, the Solver Relay broadcasts quote requests to all
/// connected solvers and waits up to 3000ms for responses.
///
/// Not used directly in the demo flow (solvers derive quotes from intents),
/// but included for protocol completeness.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct QuoteRequest {
	/// The intent this quote request is for.
	pub intent_id: u64,

	/// The asset the user is selling (negative in their token_diff).
	pub asset_in: AssetId,

	/// The asset the user is buying (positive in their token_diff).
	pub asset_out: AssetId,

	/// The exact amount of asset_in the user wants to sell.
	pub exact_amount_in: u128,

	/// Minimum deadline for the quote in milliseconds.
	pub min_deadline_ms: u64,
}

/// A solver's quote response to an RFQ.
///
/// Solvers compete by offering the best `amount_out` for the requested trade.
/// The quote includes a hash for on-chain verification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Quote {
	/// Links back to the original intent.
	pub intent_id: u64,

	/// Unique quote identifier (hash in the real protocol).
	pub quote_hash: String,

	/// The solver providing this quote.
	pub solver_id: String,

	/// Amount of asset_out the solver will provide.
	pub amount_out: u128,

	/// The solver's counter token_diff for atomic settlement.
	/// Must be compatible with the user's token_diff.
	pub solver_token_diff: TokenDiff,

	/// Expiration time for this quote (unix millis).
	pub expiration_ms: u64,
}

/// Intent lifecycle status, matching the real protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IntentStatus {
	/// Received but not yet settled.
	Pending,
	/// Settlement transaction has been broadcast.
	TxBroadcasted,
	/// Successfully settled on-chain.
	Settled,
	/// Intent was not valid or expired.
	NotFoundOrNotValid,
}

/// The result of a settlement round.
///
/// In a batch auction, multiple intents can be settled atomically by
/// combining compatible token_diffs from users and solvers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settlement {
	/// The auction round number.
	pub round: u64,

	/// Intent IDs that were settled in this round.
	pub settled_intents: Vec<u64>,

	/// The winning solver quote hashes.
	pub winning_quotes: Vec<String>,

	/// Aggregate token flow: the combined token_diffs for all participants.
	/// In a valid settlement, the sum across all diffs for each asset is zero.
	pub aggregate_flow: TokenDiff,
}
