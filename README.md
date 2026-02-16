# NEAR Intents on Mosaik

A distributed intent settlement coordination layer built on [Mosaik](https://github.com/zmanian/mosaik), demonstrating the [NEAR Intents / Defuse](https://docs.near-intents.org) protocol architecture with decentralized streams replacing the centralized Solver Relay.

## Architecture

```
User (Intent stream)
  |
  v
Solvers (consume intents, produce quotes via Mosaik Streams)
  |
  v
Auctioneer Group (3-node Raft RSM, consumes quotes, runs batch auction)
  |
  v
Settlement stream -> Relayer -> Verifier contract (on-chain)
```

### Nodes

| Role | Count | Purpose |
|------|-------|---------|
| **Auctioneer** | 3 | Raft consensus group running the `AuctionStateMachine`. Collects intents and solver quotes, runs batch settlement. |
| **Solver** | 2 | Market makers that consume user intents and compete with counter-quotes. Each solver represents a different DEX (ref-finance, jumbo-exchange). |
| **User** | 1 | Submits `token_diff` intents declaring desired balance changes. Receives settlement confirmations. |

### Protocol Flow

1. **Intent submission**: Users declare desired token balance changes using `token_diff` format (e.g., `-1000 USDC, +950 NEAR`).
2. **RFQ broadcast**: Intents are disseminated to all solvers via Mosaik Streams with `subscribe_if` tag-based filtering.
3. **Quote competition**: Solvers build counter `token_diff`s and compete on `amount_out`. In production, the Solver Relay waits 3000ms for quotes.
4. **Batch auction**: The auctioneer Raft group collects quotes and picks the best compatible quote per intent (highest `amount_out` with valid counter-diff).
5. **Settlement**: ClearRound verifies that combined token_diffs are zero-sum per asset (atomic settlement), then produces a `Settlement` event.
6. **On-chain relay**: Settlement events are streamed to a relayer node for submission to the NEAR Verifier contract.

## NEAR Intents Protocol Types

The implementation models the real [Defuse protocol](https://docs.near-intents.org/near-intents/market-makers/verifier/intent-types-and-execution):

- **`TokenDiff`**: `BTreeMap<AssetId, i128>` -- positive values = tokens to receive, negative = tokens to send
- **`AssetId`**: Defuse-style multi-token identifiers (e.g., `nep141:usdc.near`, `nep141:wrap.near`)
- **`Intent`**: Contains `signer_id`, `token_diff`, `deadline_ms`, `min_quote_deadline_ms`
- **`Quote`**: Solver response with `quote_hash`, `solver_token_diff`, `amount_out`, `expiration_ms`
- **`IntentStatus`**: Lifecycle tracking (`Pending` -> `Settled` | `NotFoundOrNotValid`)
- **`Settlement`**: Round result with `settled_intents`, `winning_quotes`, zero-sum `aggregate_flow`

## Mosaik vs. Centralized Solver Relay

The current NEAR Intents system uses a centralized [Solver Relay](https://docs.near-intents.org/near-intents/market-makers/bus/solver-relay) (`solver-relay-v2.chaindefuser.com`) as the WebSocket hub connecting users and solvers. This demo replaces it with Mosaik's decentralized primitives:

| Aspect | Centralized Solver Relay | Mosaik-based |
|--------|--------------------------|--------------|
| **Intent dissemination** | WebSocket to central server | Mosaik Streams with tag-based `subscribe_if` |
| **Quote collection** | JSON-RPC to central server | Typed Streams from solver nodes to auctioneer |
| **Settlement consensus** | Single operator | Raft group (3-node replicated state machine) |
| **Failure mode** | Single point of failure | Tolerates 1 node failure (Raft quorum) |
| **Discovery** | API key registration | Mosaik discovery with peer tags |
| **Ordering** | Server-dependent | Raft log provides total ordering |

## Running the Demo

```bash
RUST_LOG=info cargo run
```

Expected output:

```
creating network nodes...
all nodes created, cross-discovering...
all nodes discovered and tagged
waiting for auctioneer group to come online...
auctioneer group online, leader: <peer_id>
auctioneer re-synced with solvers and user
solvers subscribed to user intent stream
auctioneer subscribed to solver and user streams
submitting intents...
all intents submitted
solver0 received intent id=1 from alice.near
solver1 received intent id=1 from alice.near
...
auctioneer received intent id=1 from alice.near: {"nep141:usdc.near": -1000, "nep141:wrap.near": 950}
...
auctioneer received quote from solver0:ref-finance for intent 1: amount_out=950
auctioneer received quote from solver1:jumbo-exchange for intent 1: amount_out=940
...
round 0 settlement: settled=[1, 2, 3], winners=["ref-finance-1-0", "ref-finance-2-1", "ref-finance-3-2"]
  aggregate token flow: {"nep141:aurora.weth.near": 0, "nep141:meta-pool.near": 0, "nep141:usdc.near": 0, "nep141:wrap.near": 0}
  intent 1 status: Settled
  intent 2 status: Settled
  intent 3 status: Settled
follower confirms round 0: settled=[1, 2, 3]
relayer received settlement: round=0, settled=[1, 2, 3]
  ready for on-chain settlement via Verifier contract
near-intents example complete
```

## Project Structure

```
src/
  main.rs     -- Demo orchestration: node setup, streams, auction flow
  types.rs    -- NEAR Intents protocol types (token_diff, quotes, settlements)
  auction.rs  -- AuctionStateMachine (Raft RSM for batch settlement)
```

## Demo Intents

The demo submits three intents representing common DeFi operations:

| Intent | Signer | Operation | token_diff |
|--------|--------|-----------|------------|
| 1 | alice.near | USDC -> NEAR swap | `-1000 USDC, +950 NEAR` |
| 2 | bob.near | USDC -> wETH bridge | `-500 USDC, +15 wETH` |
| 3 | charlie.near | NEAR -> stNEAR staking | `-2000 NEAR, +1900 stNEAR` |

Two solvers compete: `ref-finance` (AMM pricing) and `jumbo-exchange` (multi-hop routing). The ref-finance solver consistently offers better `amount_out` and wins all three auctions.
