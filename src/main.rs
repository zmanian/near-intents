//! NEAR Intents: a distributed intent settlement coordination layer on Mosaik.
//!
//! Demonstrates:
//! - Streams for typed intent dissemination (User -> Solvers)
//! - Streams for solution submission (Solvers -> Auctioneers)
//! - A Raft group running an `AuctionStateMachine` (batch auction)
//! - Stream output of Settlement events from the auctioneer group
//!
//! Topology:
//!   User (Intent stream) -> Solvers (consume intents, produce solutions)
//!   -> Auctioneer Group (Raft RSM, consumes solutions) -> Settlement stream

#![allow(clippy::too_many_lines)]

mod auction;
mod types;

use {
	auction::{
		AuctionCommand, AuctionQuery, AuctionQueryResult, AuctionStateMachine,
	},
	futures::{SinkExt, StreamExt},
	mosaik::{discovery, primitives::Tag, *},
	types::{Action, Constraint, Intent, Settlement, Solution, Step},
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
	// Using Network::new for simplicity (same pattern as orderbook example).
	// Tags are added via discovery feed after creation.
	tracing::info!("creating network nodes...");

	// 3 auctioneer nodes (Raft group)
	let auctioneer0 = Network::new(network_id).await?;
	let auctioneer1 = Network::new(network_id).await?;
	let auctioneer2 = Network::new(network_id).await?;

	// 2 solver nodes
	let solver0 = Network::new(network_id).await?;
	let solver1 = Network::new(network_id).await?;

	// 1 user node
	let user_node = Network::new(network_id).await?;

	tracing::info!("all nodes created, cross-discovering...");

	// Cross-discover: auctioneers + one solver/user is enough to propagate
	// via gossip. We discover in a smaller group to reduce connection pressure.
	discover_all([&auctioneer0, &auctioneer1, &auctioneer2]).await?;
	discover_all([&solver0, &solver1, &user_node, &auctioneer0]).await?;

	// Add tags to each node via discovery feed
	add_tag(&auctioneer0, "auctioneer")?;
	add_tag(&auctioneer1, "auctioneer")?;
	add_tag(&auctioneer2, "auctioneer")?;
	add_tag(&solver0, "solver")?;
	add_tag(&solver1, "solver")?;
	add_tag(&user_node, "user")?;

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

	// --- 5. Solvers consume intents and produce solutions ---
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

	let mut solver0_solution_producer =
		solver0.streams().produce::<Solution>();
	let mut solver1_solution_producer =
		solver1.streams().produce::<Solution>();

	solver0_intent_consumer.when().subscribed().await;
	solver1_intent_consumer.when().subscribed().await;
	tracing::info!("solvers subscribed to user intent stream");

	// Auctioneer0 consumes solutions and intents
	let solver_tag = Tag::from("solver");
	let mut solution_consumer = auctioneer0
		.streams()
		.consumer::<Solution>()
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

	solution_consumer.when().subscribed().minimum_of(2).await;
	intent_consumer.when().subscribed().await;
	tracing::info!("auctioneer subscribed to solver and user streams");

	// --- 6. Spawn solver tasks ---
	let solver0_task = tokio::spawn(async move {
		let mut count = 0u32;
		while let Some(intent) = solver0_intent_consumer.next().await {
			tracing::info!("solver0 received intent id={}", intent.id);
			let solution = Solution {
				intent_id: intent.id,
				solver_id: "solver0".into(),
				execution_plan: vec![Step::Swap {
					dex: "ref-finance".into(),
					from: "NEAR".into(),
					to: "USDC".into(),
					amount: 1000,
				}],
				price: 980,
				score: 90 + u64::from(count),
			};
			if let Err(e) = solver0_solution_producer.send(solution).await {
				tracing::warn!("solver0 failed to send solution: {e}");
			}
			count += 1;
			if count >= 3 {
				break;
			}
		}
		tracing::info!("solver0 finished");
	});

	let solver1_task = tokio::spawn(async move {
		let mut count = 0u32;
		while let Some(intent) = solver1_intent_consumer.next().await {
			tracing::info!("solver1 received intent id={}", intent.id);
			let solution = Solution {
				intent_id: intent.id,
				solver_id: "solver1".into(),
				execution_plan: vec![
					Step::Swap {
						dex: "jumbo-exchange".into(),
						from: "NEAR".into(),
						to: "wETH".into(),
						amount: 500,
					},
					Step::Transfer {
						to: "user.near".into(),
						amount: 500,
					},
				],
				price: 970,
				score: 85 + u64::from(count),
			};
			if let Err(e) = solver1_solution_producer.send(solution).await {
				tracing::warn!("solver1 failed to send solution: {e}");
			}
			count += 1;
			if count >= 3 {
				break;
			}
		}
		tracing::info!("solver1 finished");
	});

	// --- 7. User sends intents ---
	tracing::info!("submitting intents...");

	intent_producer
		.send(Intent {
			id: 1,
			action: Action::Swap {
				from: "NEAR".into(),
				to: "USDC".into(),
				amount: 1000,
			},
			constraints: vec![
				Constraint::MinOutput(950),
				Constraint::MaxSlippage(50),
			],
			deadline: 100,
		})
		.await?;

	intent_producer
		.send(Intent {
			id: 2,
			action: Action::Bridge {
				from_chain: "NEAR".into(),
				to_chain: "Ethereum".into(),
				asset: "USDC".into(),
				amount: 500,
			},
			constraints: vec![Constraint::Deadline(200)],
			deadline: 200,
		})
		.await?;

	intent_producer
		.send(Intent {
			id: 3,
			action: Action::Stake {
				validator: "aurora.poolv1.near".into(),
				amount: 2000,
			},
			constraints: vec![Constraint::MinOutput(1900)],
			deadline: 150,
		})
		.await?;

	tracing::info!("all intents submitted");

	// --- 8. Auctioneer consumes intents and feeds to Raft group ---
	for _ in 0..3 {
		let intent = intent_consumer
			.next()
			.await
			.expect("expected intent from stream");
		tracing::info!("auctioneer received intent id={}", intent.id);
		g0.execute(AuctionCommand::SubmitIntent(intent)).await?;
	}
	tracing::info!("all intents submitted to auction");

	// Each solver sends 3 solutions (one per intent), 6 total
	for _ in 0..6 {
		let solution = solution_consumer
			.next()
			.await
			.expect("expected solution from stream");
		tracing::info!(
			"auctioneer received solution from {} for intent {}",
			solution.solver_id,
			solution.intent_id,
		);
		g0.execute(AuctionCommand::SubmitSolution(solution)).await?;
	}
	tracing::info!("all solutions submitted to auction");

	let _ = tokio::join!(solver0_task, solver1_task);
	tracing::info!("solvers finished processing");

	// --- 9. Execute ClearRound ---
	g0.execute(AuctionCommand::ClearRound).await?;
	tracing::info!("round cleared");

	// --- 10. Query round results ---
	let result = g0
		.query(AuctionQuery::RoundResult(0), Consistency::Strong)
		.await?;

	if let AuctionQueryResult::Round(Some(settlement)) = &result {
		tracing::info!(
			"round 0 settlement: matched_intents={:?}, winning_price={}",
			settlement.matched_intents,
			settlement.winning_solution_intent,
		);
	}

	let result = g0
		.query(AuctionQuery::PendingIntents, Consistency::Strong)
		.await?;

	if let AuctionQueryResult::Intents(intents) = &result {
		tracing::info!("{} intents still pending after round", intents.len());
	}

	let result = g0
		.query(AuctionQuery::CurrentRound, Consistency::Strong)
		.await?;

	if let AuctionQueryResult::RoundNumber(round) = &result {
		tracing::info!("current round: {round}");
	}

	// --- 11. Produce settlements on a Stream<Settlement> ---
	let mut settlement_producer =
		auctioneer0.streams().produce::<Settlement>();

	// Verify replication to followers
	g1.when().committed().reaches(g0.committed()).await;
	tracing::info!("follower caught up with leader");

	let result = g1
		.query(AuctionQuery::RoundResult(0), Consistency::Weak)
		.await?;
	if let AuctionQueryResult::Round(Some(settlement)) = result {
		tracing::info!(
			"follower confirms round 0: matched={:?}",
			settlement.matched_intents,
		);
		settlement_producer.send(settlement).await?;
	}

	// --- 12. Relayer: user node consumes settlements ---
	let mut relayer_consumer = user_node.streams().consume::<Settlement>();
	relayer_consumer.when().subscribed().await;

	if let Some(settlement) = relayer_consumer.next().await {
		tracing::info!(
			"relayer received settlement: round={}, matched={:?}",
			settlement.round,
			settlement.matched_intents,
		);
	}

	// --- 13. Done ---
	tracing::info!("near-intents example complete");
	Ok(())
}

/// Add a tag to a network node by updating its discovery entry.
fn add_tag(network: &Network, tag: &str) -> anyhow::Result<()> {
	let me = network.discovery().me();
	let entry = me.into_unsigned();
	let updated = entry.add_tags(Tag::from(tag));
	let signed = updated.sign(network.local().secret_key())?;
	network.discovery().feed(signed);
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
