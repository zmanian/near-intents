//! NEAR Intents: a distributed intent settlement coordination layer on Mosaik.
//!
//! Demonstrates the NEAR Intents / Defuse protocol architecture:
//! - token_diff intents (declarative balance changes)
//! - RFQ-based solver competition with quote responses
//! - Atomic settlement via Raft-replicated auction state machine
//! - Settlement stream output for on-chain relay
//!
//! Topology:
//!   User (Intent stream) -> Solvers (consume intents, produce quotes)
//!   -> Auctioneer Group (Raft RSM, consumes quotes) -> Settlement stream

#![allow(clippy::too_many_lines)]

mod auction;
mod types;

use {
	auction::{
		AuctionCommand, AuctionQuery, AuctionQueryResult, AuctionStateMachine,
	},
	futures::{SinkExt, StreamExt},
	mosaik::{discovery, primitives::Tag, *},
	std::collections::BTreeMap,
	types::{Intent, Quote, Settlement},
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
	tracing_subscriber::fmt()
		.with_env_filter(
			tracing_subscriber::EnvFilter::try_from_default_env()
				.unwrap_or_else(|_| "info,mosaik=info".parse().unwrap()),
		)
		.init();

	let network_id = NetworkId::random();
	let group_key = GroupKey::random();

	// --- 1. Create network nodes ---
	tracing::info!("creating network nodes...");

	// 3 auctioneer nodes (Raft group for settlement consensus)
	let auctioneer0 = Network::new(network_id).await?;
	let auctioneer1 = Network::new(network_id).await?;
	let auctioneer2 = Network::new(network_id).await?;

	// 2 solver (market maker) nodes
	let solver0 = Network::new(network_id).await?;
	let solver1 = Network::new(network_id).await?;

	// 1 user node
	let user_node = Network::new(network_id).await?;

	tracing::info!("all nodes created, cross-discovering...");

	discover_all([&auctioneer0, &auctioneer1, &auctioneer2]).await?;
	discover_all([&solver0, &solver1, &user_node, &auctioneer0]).await?;

	// Broadcast tags to all nodes so subscribe_if predicates work immediately
	let all_nodes: Vec<&Network> = vec![
		&auctioneer0,
		&auctioneer1,
		&auctioneer2,
		&solver0,
		&solver1,
		&user_node,
	];
	broadcast_tag(&auctioneer0, "auctioneer", &all_nodes)?;
	broadcast_tag(&auctioneer1, "auctioneer", &all_nodes)?;
	broadcast_tag(&auctioneer2, "auctioneer", &all_nodes)?;
	broadcast_tag(&solver0, "solver", &all_nodes)?;
	broadcast_tag(&solver1, "solver", &all_nodes)?;
	broadcast_tag(&user_node, "user", &all_nodes)?;

	tracing::info!("all nodes discovered and tagged");

	// --- 2. Auctioneers join a Raft group with AuctionStateMachine ---
	let g0 = auctioneer0
		.groups()
		.with_key(group_key)
		.with_state_machine(AuctionStateMachine::new())
		.join();

	let g1 = auctioneer1
		.groups()
		.with_key(group_key)
		.with_state_machine(AuctionStateMachine::new())
		.join();

	let g2 = auctioneer2
		.groups()
		.with_key(group_key)
		.with_state_machine(AuctionStateMachine::new())
		.join();

	// --- 3. Wait for group online ---
	tracing::info!("waiting for auctioneer group to come online...");
	g0.when().online().await;
	g1.when().online().await;
	g2.when().online().await;

	let leader = g0
		.leader()
		.expect("leader should be elected after online");
	tracing::info!("auctioneer group online, leader: {leader}");

	// --- 4. User produces Stream<Intent> ---
	let mut intent_producer = user_node.streams().produce::<Intent>();

	// --- 5. Solvers consume intents and produce quotes ---
	let user_tag = Tag::from("user");
	let mut solver0_intent_consumer = solver0
		.streams()
		.consumer::<Intent>()
		.subscribe_if(move |peer: &discovery::PeerEntry| {
			peer.tags().contains(&user_tag)
		})
		.build();

	let user_tag = Tag::from("user");
	let mut solver1_intent_consumer = solver1
		.streams()
		.consumer::<Intent>()
		.subscribe_if(move |peer: &discovery::PeerEntry| {
			peer.tags().contains(&user_tag)
		})
		.build();

	let mut solver0_quote_producer = solver0.streams().produce::<Quote>();
	let mut solver1_quote_producer = solver1.streams().produce::<Quote>();

	// Re-sync auctioneer with solvers and user after producers are created.
	auctioneer0
		.discovery()
		.sync_with(solver0.local().addr())
		.await?;
	auctioneer0
		.discovery()
		.sync_with(solver1.local().addr())
		.await?;
	auctioneer0
		.discovery()
		.sync_with(user_node.local().addr())
		.await?;
	tracing::info!("auctioneer re-synced with solvers and user");

	solver0_intent_consumer.when().subscribed().await;
	solver1_intent_consumer.when().subscribed().await;
	tracing::info!("solvers subscribed to user intent stream");

	// Auctioneer0 consumes quotes and intents
	let solver_tag = Tag::from("solver");
	let mut quote_consumer = auctioneer0
		.streams()
		.consumer::<Quote>()
		.subscribe_if(move |peer: &discovery::PeerEntry| {
			peer.tags().contains(&solver_tag)
		})
		.build();

	let user_tag = Tag::from("user");
	let mut intent_consumer = auctioneer0
		.streams()
		.consumer::<Intent>()
		.subscribe_if(move |peer: &discovery::PeerEntry| {
			peer.tags().contains(&user_tag)
		})
		.build();

	quote_consumer.when().subscribed().minimum_of(2).await;
	intent_consumer.when().subscribed().await;
	tracing::info!("auctioneer subscribed to solver and user streams");

	// --- 6. Spawn solver tasks ---
	// Solver0: "ref-finance" AMM solver - provides NEAR/USDC liquidity
	let solver0_task = tokio::spawn(async move {
		let mut count = 0u32;
		while let Some(intent) = solver0_intent_consumer.next().await {
			tracing::info!(
				"solver0 received intent id={} from {}",
				intent.id,
				intent.signer_id
			);

			// Build a counter token_diff: opposite signs from the user's diff
			let mut solver_diff = BTreeMap::new();
			for (asset, &amount) in &intent.token_diff {
				solver_diff.insert(asset.clone(), -amount);
			}

			// Compute the amount_out (what user receives)
			let amount_out: u128 = intent
				.token_diff
				.values()
				.filter(|&&v| v > 0)
				.map(|&v| v as u128)
				.sum();

			let quote = Quote {
				intent_id: intent.id,
				quote_hash: format!("ref-finance-{}-{count}", intent.id),
				solver_id: "solver0:ref-finance".into(),
				amount_out: amount_out + u128::from(count) * 5,
				solver_token_diff: solver_diff,
				expiration_ms: intent.deadline_ms,
			};

			if let Err(e) = solver0_quote_producer.send(quote).await {
				tracing::warn!("solver0 failed to send quote: {e}");
			}
			count += 1;
			if count >= 3 {
				break;
			}
		}
		tracing::info!("solver0 finished");
	});

	// Solver1: "jumbo-exchange" solver - provides multi-hop routing
	let solver1_task = tokio::spawn(async move {
		let mut count = 0u32;
		while let Some(intent) = solver1_intent_consumer.next().await {
			tracing::info!(
				"solver1 received intent id={} from {}",
				intent.id,
				intent.signer_id
			);

			let mut solver_diff = BTreeMap::new();
			for (asset, &amount) in &intent.token_diff {
				solver_diff.insert(asset.clone(), -amount);
			}

			let amount_out: u128 = intent
				.token_diff
				.values()
				.filter(|&&v| v > 0)
				.map(|&v| v as u128)
				.sum();

			let quote = Quote {
				intent_id: intent.id,
				quote_hash: format!("jumbo-{}-{count}", intent.id),
				solver_id: "solver1:jumbo-exchange".into(),
				// Slightly worse pricing than solver0
				amount_out: amount_out.saturating_sub(10) + u128::from(count) * 3,
				solver_token_diff: solver_diff,
				expiration_ms: intent.deadline_ms,
			};

			if let Err(e) = solver1_quote_producer.send(quote).await {
				tracing::warn!("solver1 failed to send quote: {e}");
			}
			count += 1;
			if count >= 3 {
				break;
			}
		}
		tracing::info!("solver1 finished");
	});

	// --- 7. User submits token_diff intents ---
	tracing::info!("submitting intents...");

	let now_ms = std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.unwrap()
		.as_millis() as u64;

	// Intent 1: Swap 1000 USDC for NEAR (min 950 NEAR)
	intent_producer
		.send(Intent {
			id: 1,
			signer_id: "alice.near".into(),
			token_diff: BTreeMap::from([
				("nep141:usdc.near".into(), -1000),
				("nep141:wrap.near".into(), 950),
			]),
			deadline_ms: now_ms + 120_000,
			min_quote_deadline_ms: 60_000,
		})
		.await?;

	// Intent 2: Swap 500 USDC for wETH (cross-chain bridge intent)
	intent_producer
		.send(Intent {
			id: 2,
			signer_id: "bob.near".into(),
			token_diff: BTreeMap::from([
				("nep141:usdc.near".into(), -500),
				("nep141:aurora.weth.near".into(), 15),
			]),
			deadline_ms: now_ms + 180_000,
			min_quote_deadline_ms: 60_000,
		})
		.await?;

	// Intent 3: Swap 2000 NEAR for stNEAR (liquid staking)
	intent_producer
		.send(Intent {
			id: 3,
			signer_id: "charlie.near".into(),
			token_diff: BTreeMap::from([
				("nep141:wrap.near".into(), -2000),
				("nep141:meta-pool.near".into(), 1900),
			]),
			deadline_ms: now_ms + 150_000,
			min_quote_deadline_ms: 60_000,
		})
		.await?;

	tracing::info!("all intents submitted");

	// --- 8. Auctioneer consumes intents and feeds to Raft group ---
	for _ in 0..3 {
		let intent = intent_consumer
			.next()
			.await
			.expect("expected intent from stream");
		tracing::info!(
			"auctioneer received intent id={} from {}: {:?}",
			intent.id,
			intent.signer_id,
			intent.token_diff,
		);
		g0.execute(AuctionCommand::SubmitIntent(intent)).await?;
	}
	tracing::info!("all intents submitted to auction");

	// Auctioneer collects quotes (2 solvers x 3 intents = 6 quotes)
	for _ in 0..6 {
		let quote = quote_consumer
			.next()
			.await
			.expect("expected quote from stream");
		tracing::info!(
			"auctioneer received quote from {} for intent {}: amount_out={}",
			quote.solver_id,
			quote.intent_id,
			quote.amount_out,
		);
		g0.execute(AuctionCommand::SubmitQuote(quote)).await?;
	}
	tracing::info!("all quotes submitted to auction");

	let _ = tokio::join!(solver0_task, solver1_task);
	tracing::info!("solvers finished processing");

	// --- 9. Execute ClearRound (batch settlement) ---
	let clear_index = g0.execute(AuctionCommand::ClearRound).await?;
	tracing::info!("round cleared at index {clear_index}");

	g0.when().committed().reaches(clear_index).await;

	// --- 10. Query round results ---
	let result = g0
		.query(AuctionQuery::RoundResult(0), Consistency::Weak)
		.await?;

	if let AuctionQueryResult::Round(Some(settlement)) = &result {
		tracing::info!(
			"round 0 settlement: settled={:?}, winners={:?}",
			settlement.settled_intents,
			settlement.winning_quotes,
		);
		tracing::info!(
			"  aggregate token flow: {:?}",
			settlement.aggregate_flow,
		);
	}

	let result = g0
		.query(AuctionQuery::PendingIntents, Consistency::Weak)
		.await?;

	if let AuctionQueryResult::Intents(intents) = &result {
		tracing::info!("{} intents still pending after round", intents.len());
	}

	// Query individual intent statuses
	for id in 1..=3u64 {
		let result = g0
			.query(AuctionQuery::IntentStatus(id), Consistency::Weak)
			.await?;
		if let AuctionQueryResult::Status(status) = &result {
			tracing::info!("  intent {id} status: {status:?}");
		}
	}

	let result = g0
		.query(AuctionQuery::CurrentRound, Consistency::Weak)
		.await?;

	if let AuctionQueryResult::RoundNumber(round) = &result {
		tracing::info!("current round: {round}");
	}

	// --- 11. Produce settlements on a Stream<Settlement> ---
	let mut settlement_producer =
		auctioneer0.streams().produce::<Settlement>();

	// Sync user_node with auctioneer0 so it discovers the Settlement stream
	user_node
		.discovery()
		.sync_with(auctioneer0.local().addr())
		.await?;

	// Create consumer before sending, so it's subscribed when data arrives
	let mut relayer_consumer = user_node.streams().consume::<Settlement>();
	relayer_consumer.when().subscribed().await;
	tracing::info!("relayer subscribed to settlement stream");

	// Verify replication to followers
	g1.when().committed().reaches(g0.committed()).await;
	tracing::info!("follower caught up with leader");

	let result = g1
		.query(AuctionQuery::RoundResult(0), Consistency::Weak)
		.await?;
	if let AuctionQueryResult::Round(Some(settlement)) = result {
		tracing::info!(
			"follower confirms round 0: settled={:?}",
			settlement.settled_intents,
		);
		settlement_producer.send(settlement).await?;
	}

	// --- 12. Relayer: user node receives settlements for on-chain relay ---
	if let Some(settlement) = relayer_consumer.next().await {
		tracing::info!(
			"relayer received settlement: round={}, settled={:?}",
			settlement.round,
			settlement.settled_intents,
		);
		tracing::info!(
			"  ready for on-chain settlement via Verifier contract"
		);
	}

	// --- 13. Done ---
	tracing::info!("near-intents example complete");
	Ok(())
}

/// Tag a network node and broadcast the signed entry to all other nodes.
fn broadcast_tag(
	network: &Network,
	tag: &str,
	all_nodes: &[&Network],
) -> anyhow::Result<()> {
	let me = network.discovery().me();
	let entry = me.into_unsigned();
	let updated = entry.add_tags(Tag::from(tag));
	let signed = updated.sign(network.local().secret_key())?;
	for node in all_nodes {
		node.discovery().feed(signed.clone());
	}
	Ok(())
}

/// Utility: cross-discover all networks with each other.
async fn discover_all(
	networks: impl IntoIterator<Item = &Network>,
) -> anyhow::Result<()> {
	let networks = networks.into_iter().collect::<Vec<_>>();
	for (i, net_i) in networks.iter().enumerate() {
		for (j, net_j) in networks.iter().enumerate() {
			if i != j {
				net_i.discovery().sync_with(net_j.local().addr()).await?;
			}
		}
	}
	Ok(())
}
