# Network Implementations & Payment Detection Flow

> Companion to [`README.md`](./README.md). This document describes **how payments are
> detected**, per network. It is implementation-level: it names the actual functions,
> tables, constants and events involved, and calls out where behaviour is deliberate
> versus where it is a known gap.
>
>  **Status:** EVM and Solana are implemented and documented in full. Esplora is
> implemented, but remains untested and undocumented.
---

## 1. Design principle: agnostic orchestrator, optimized paths

The orchestrator that creates an invoice does not know what a chain is. It resolves a
token ID to a `TokenHandler`, the handler resolves to a `NetworkClient`, and the network
client is free to detect payment however that chain makes sense.

That freedom is the point. Every chain exposes a different "best" way to be told that
money arrived, and forcing them all through one abstraction (e.g. "poll balances")
throws away everything each chain is actually good at.

```
                        ┌──────────────────────┐
   POST /invoices  ──▶  │     Orchestrator     │   network-agnostic
                        │  (creates invoice)   │   knows: token_id, amount, merchant
                        └──────────┬───────────┘
                                   │ resolve token_id
                        ┌──────────▼───────────┐
                        │     TokenHandler     │   token-specific params
                        │  (USDC_ETH, SOL, …)  │   address, decimals, confirmations
                        └──────────┬───────────┘
                                   │
                        ┌──────────▼───────────┐
                        │    NetworkClient     │   chain-specific detection
                        │  EVM / SOL / Esplora │   watch_* services
                        └──────────┬───────────┘
                                   │ writes rows
                        ┌──────────▼───────────┐
                        │   payments/invoices  │  ──▶  webhook queue  ──▶  merchant
                        └──────────────────────┘
```

Everything below the orchestrator is allowed to be chain-shaped. Everything above it is
not allowed to care.

---

## 2. The two payment paths (and why both are always live)

Each invoice is presented to the payer as **one page with two options side by side**:

|                          | Naive path                                     | Smart path                                  |
|--------------------------|------------------------------------------------|---------------------------------------------|
| **UI**                   | QR code of a plain address                     | "Connect wallet" button                     |
| **Correlation**          | by *address* (one derived address per invoice) | by *identifier carried in the transaction*  |
| **User error surface**   | none — they scan and send                      | none — the app builds the tx                |
| **Wallet compatibility** | maximal (any wallet can send to an address)    | requires WalletConnect-capable wallet       |
| **Sweep required**       | yes (deposit address → treasury)               | no (funds land at destination directly)     |
| **Detection mechanism**  | scan blocks for transfers to watched addresses | scan for the contract event / reference key |

**Both are displayed at once and both must be watched at once.** The backend cannot know
which one the payer will use — they might connect a wallet, change their mind, and scan
the QR instead. So on any network that supports both, *two independent watcher services*
run concurrently against the same invoice set, and both write into the same `payments`
table. Idempotency (§6) is what makes that safe.

The naive path exists because embedding token/chain/memo metadata in a QR code is
unreliable in practice — a meaningful fraction of wallets misparse it or silently drop
the memo. A bare address QR is the one thing that works everywhere.

The smart path exists because it removes the two worst failure modes of the naive path:
the sweep step (gas cost + an extra on-chain hop + a window where funds sit at a
throwaway address) and manual identifier entry (a user typing a UUID into a memo field
is a support ticket generator).

### Per-network path availability

| Network             | Naive path                 | Smart path | Smart path mechanism                                                                                                                                  | Reuses deposit addresses           |
|---------------------|----------------------------|------------|-------------------------------------------------------------------------------------------------------------------------------------------------------|------------------------------------|
| **EVM**             | HD-derived deposit address | ✅          | `CustodialPaymentVault` contract call → `Payment` event carrying the invoice UUID                                                                     | ❌                                  |
| **Solana**          | HD-derived deposit address | ✅          | direct transfer to the merchant treasury, carrying the invoice's HD-derived pubkey as a Solana Pay `reference` — a read-only, non-signing account key | ❌                                  |
| **Esplora (UTXO)**  | HD-derived deposit address | ❌          | no equivalent primitive — naive only                                                                                                                  | ✅ (virgin only, ≥24h after expiry) |
| **TRON** *(future)* | HD-derived deposit address | TBD        | whatever TRON gives us for fee/energy optimization                                                                                                    | TBD                                |
> On Solana the correlation key is an **account key, not a memo**. The wallet-connect
> path attaches the invoice's HD-derived pubkey to the transfer as a Solana Pay
> `reference`: read-only, non-signing, carrying no data. Because validators index
> transactions under *every* account key they name, `getSignaturesForAddress` on that
> pubkey returns the payer's transaction even though the pubkey neither sent nor
> received anything. Nothing is ever typed by a human, and nothing depends on a wallet
> preserving a free-text field.

---

## 3. Shared vocabulary

### Payment lifecycle

```
                 ┌──────────────────────────────────────────┐
                 │                                          │
  (first sight)  ▼                                          │  re-mined after reorg
   ────────▶ detected ──▶ merchant_confirmed ──▶ system_confirmed
                 │                │
                 │ tx gone        │ tx gone
                 ▼                ▼
              orphaned ◀──────────┘
```

| Status               | Meaning                                                                    | Transition rule                     |
|----------------------|----------------------------------------------------------------------------|-------------------------------------|
| `detected`           | seen on chain, 0+ confirmations                                            | insert on first sighting            |
| `merchant_confirmed` | reached the invoice's `required_confirmations`                             | promoted in `refresh_confirmations` |
| `system_confirmed`   | reached `FINAL_CONFIRMATIONS`; considered irreversible, stops being polled | promoted in `refresh_confirmations` |
| `orphaned`           | the tx is no longer mined anywhere                                         | set in `handle_reorg`               |

### Invoice status

Derived, never incremented. `recompute_invoice_totals` recomputes from
`SUM(payments.amount) WHERE status <> 'orphaned'`:

| Received vs requested   | Status                                                   |
|-------------------------|----------------------------------------------------------|
| `= 0`                   | `pending`                                                |
| `> 0` and `< requested` | `underpaid`                                              |
| `= requested`           | `paid`                                                   |
| `> requested`           | `overpaid`                                               |
| —                       | `expired` (terminal; never overwritten by the recompute) |

### Amount representation

All amounts in the DB are **base units** (wei, lamports, satoshis, smallest token unit).
Decimals are applied at the presentation layer only. Nothing in the detection pipeline
ever handles a human-readable amount.

### Webhooks emitted by the EVM implementation

| Event               | Fired when                                                  | Once-only latch                                       |
|---------------------|-------------------------------------------------------------|-------------------------------------------------------|
| `payment.detected`  | first insert of a `(invoice_id, tx_hash)` row               | unique index on `payments`                            |
| `payment.confirmed` | payment crosses `required_confirmations`                    | guarded `UPDATE … WHERE status = 'detected'`          |
| `payment.finalized` | payment crosses `FINAL_CONFIRMATIONS`                       | guarded `UPDATE … WHERE status <> 'system_confirmed'` |
| `payment.orphaned`  | tx disappeared from the chain                               | status transition to `orphaned`                       |
| `payment.finished`  | invoice's received total first reaches the requested amount | guarded `UPDATE` on invoice status                    |

Every `enqueue_webhook` call shares the same DB transaction as the state change that
justifies it, so the state change and the notification commit or roll back together.

### Shared tables

| Table                  | Purpose                                                                                                                                                                             |
|------------------------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `invoices`             | one row per invoice; `network_type`, `chain_ref`, `wallet_address`, `token_address`, `created_block`, `amount_requested`, `amount_received`, `required_confirmations`, `expires_at` |
| `payments`             | one row per `(invoice_id, tx_hash)`; the unique index is the idempotency backbone                                                                                                   |
| `merchant_wallets`     | merchant destination wallet per `network_type`                                                                                                                                      |
| `network_scan_state`   | scan cursor per `(network_type, chain_ref, scope)` — `last_block` + `last_block_hash`                                                                                               |
| `network_seen_blocks`  | remembered block headers per scope, for reorg detection                                                                                                                             |
| `webhook queue`        | written via `enqueue_webhook`, drained by a separate dispatcher with at-least-once semantics                                                                                        |
| `reconciliation_watch` | the bounded set of addresses under reconciliation observation — main addresses, unswept deposit addresses with known funds, shared entities. Rebuilt from the DB every tick.        |
| `balance_probes`       | results of independent balance checks against the watch set, and the deltas they could not source                                                                                   |

`scope` is what lets independent scanners share one chain: the address scanner uses `SCAN_SCOPE_ADDRESSES`, the logs scanner uses `SCAN_SCOPE_LOGS`, reconciliation ingest uses `SCAN_SCOPE_RECONCILIATION`, and none can move another's cursor.
`SCAN_SCOPE_ADDRESSES`, the logs scanner uses `SCAN_SCOPE_LOGS`, and neither can move the
other's cursor.

The transition-rule column names EVM functions. Solana's
equivalents are `reconcile_statuses` (both promotion and orphaning; see §5.9) and
`recompute_invoice_totals`. `network_scan_state` and `network_seen_blocks` are **not
used by Solana** — it has no block scanner and no reorg table.

---

## 4. EVM — implemented

`EVMNetwork` is constructed with a `chain_id`, one or more RPC URLs, and an optional
vault contract address. `network_name` is `EVM_{chain_id}`, `chain_ref` is the chain ID
as a string. One instance per chain; Ethereum, Base and Polygon are three instances of
the same struct.

``` rust
EVMNetwork::new(chain_id, rpc_urls, contract_address)
```

`contract_address` being `None`/empty means the vault is not deployed on this chain
(yet). Native/ERC-20 address watching still runs; `watch_logs` logs a line and returns
instead of spinning.

### 4.1 Two services, one chain

| Service           | Path served                                     | Loop             | Correlates by                   |
|-------------------|-------------------------------------------------|------------------|---------------------------------|
| `watch_addresses` | naive QR (native + ERC-20 to a deposit address) | `tick_addresses` | `to` address                    |
| `watch_logs`      | WalletConnect (vault contract call)             | `tick_logs`      | invoice UUID in the event topic |

Both loops are structurally identical at the top level:

``` rust
loop {
    if let Err(e) = self.tick_*(pool).await {
        eprintln!(...);          // transient by assumption — cursor did not advance
    }
    sleep(POLL_INTERVAL_SECS).await;
}
```

The error posture is deliberate: **the cursor is only advanced on success**, so a failed
tick costs nothing but a poll interval. RPC hiccups, quorum splits mid-reorg and lagging
providers all resolve themselves by simply redoing the work next tick. Nothing in a tick
needs to be "rolled back" because nothing irreversible happens before the cursor moves.

### 4.2 RPC layer: quorum

Every RPC call goes through one of three quorum wrappers:

| Function        | Result shape        | Used by                                            |
|-----------------|---------------------|----------------------------------------------------|
| `call_rpc`      | plain string        | `eth_blockNumber`                                  |
| `call_rpc_json` | `serde_json::Value` | `eth_getBlockByNumber`, `eth_getTransactionByHash` |
| `get_logs`      | `Vec<Log>`          | `eth_getLogs`                                      |

Rules:

1. **One URL configured → skip quorum entirely.** Local dev and testnets where you only
   have one provider are not required to invent a second one.
2. **Multiple URLs → fan out to all of them concurrently** (`join_all`), then:
    - fewer than 2 successful responses → `Quorum failed` error (transient, retried);
    - otherwise return the first value that **at least 2 endpoints agree on**.
3. **All responded, none matched** → `Quorum disagreement`. This is *not* something a
   tiebreaker can fix — there is no majority to break the tie toward — so it is treated
   as transient and retried. In practice this is what a reorg looks like from the
   outside while providers are still converging.

Comparison is structural, not textual: `call_rpc_json` compares `serde_json::Value`s so
key ordering differences between providers don't cause false disagreements, and
`get_logs` sorts each provider's log set by `(block_number, log_index)` before comparing.

### 4.3 Who we watch

`load_watched_invoices` is the single source of truth for both services. It returns
everything with an **open interest** on this chain:

```sql
(i.status = 'pending' AND i.expires_at > now())
OR EXISTS (SELECT 1 FROM payments p
            WHERE p.invoice_id = i.id
              AND p.status IN ('detected','merchant_confirmed'))
```

The second clause is the restart-safety bit, and it's the non-obvious one: an invoice
that already went `paid` still needs its confirmation counter driven all the way to
`FINAL_CONFIRMATIONS`, and that has to survive a process restart with an empty in-memory
`pending` map. Rebuilding the watch set from the DB every tick means there is no
in-memory state whose loss can strand a payment.

The join against `merchant_wallets` pulls the expected destination wallet, used as a
defense-in-depth check in the logs path (§4.6).

### 4.4 `tick_addresses` — the naive path

```
1. load_watched_invoices          → Vec<WatchedInvoice>
2. eth_blockNumber                → tip
3. build by_address multimap      → HashMap<address_lc, Vec<WatchedInvoice>>
4. load cursor (or cold start)
5. detect_fork_point → handle_reorg → rewind cursor      [if reorg]
6. for n in cursor+1 ..= target:
     a. eth_getBlockByNumber(n, full = true)
     b. parent-hash continuity check
     c. apply_block                → native transfers
     d. get_erc20_transfers(n)     → eth_getLogs, Transfer topic
     e. apply_erc20_transfers
     f. remember_block + save_cursor
7. refresh_confirmations(tip)
8. prune_seen_blocks
```

**Address → invoices is a multimap.** The same deposit address *can* legitimately appear
on two invoices (address reuse within a merchant), and a matching tx credits every
invoice on it.
**TODO:** once addresses are strictly single-use, collapse to a `HashMap` and hard-error
on duplicates instead of silently double-crediting.

**Cold start.** With no cursor, the floor is `MIN(created_block)` across all open
invoices, falling back to the current tip if there are none. Then `start - 1`, so the
first block actually processed is `start`. This makes a restart after downtime backfill
automatically rather than silently skipping the gap.

**Scan ceiling is `tip - 1`, not `tip`.** The newest block is the one most likely to have
reached some providers and not others, so scanning right up to the tip invites avoidable
quorum failures. This costs nothing in confirmation accuracy because
`refresh_confirmations` counts against the *live* tip independently (§4.7) — it is purely
about not fighting propagation lag inside the scan loop.

**Per-tick budget.** `target = min(tip - 1, last_block + MAX_BLOCKS_PER_TICK)`. A long
outage is caught up over many ticks instead of melting the RPC provider in one burst,
and the tip is re-read each tick so catch-up converges.

**Parent-hash continuity.** Before applying a block we check `block.parent_hash ==
last_hash`. A mismatch means a reorg landed mid-scan; we `break` out of the loop and let
the next tick's reorg detection handle it properly rather than trying to patch it up
inline.

**`get_block(n, full = true)`** pulls the transaction bodies, so native-value matching is
one round trip instead of N `eth_getTransactionByHash` calls. `parse_block` filters as it
goes: contract creations (`to: null`) are skipped, and zero-value txs are skipped because
ERC-20 transfers carry no native value and are handled by the log filter instead.

#### `apply_block` — native transfers

For each `(tx_hash, to, value)` landing on a watched address:

- **skip if `inv.token_lc.is_some()`** — a token invoice at this address must never be
  credited by a plain ETH transfer;
- skip if `block.number < inv.created_block` (predates the invoice, not our money);
- `INSERT … ON CONFLICT (invoice_id, tx_hash) DO NOTHING`:
    - **inserted** → this is a first sighting → enqueue `payment.detected` in the *same*
      DB transaction;
    - **conflict** → already known (rescan, or re-mined post-reorg) → `UPDATE` the location
      only, and un-orphan it if it was orphaned. **The amount is never rewritten.**
- commit, then `recompute_invoice_totals`.

#### ERC-20 to a deposit address

`get_erc20_transfers(block, to_addresses)` builds one `eth_getLogs` filter per block:

``` json
{
  "fromBlock": "0x…", "toBlock": "0x…",
  "topics": [ERC20_TRANSFER_TOPIC0, null, [<to-topics>]]
}
```

`address_to_topic` left-pads the address to a 32-byte word (indexed `address` topics are
right-aligned). The third topic position is an OR-set, so all watched addresses are
covered in one call.

Defensive filtering on each returned log:

| Check               | Why                                                                                                                                             |
|---------------------|-------------------------------------------------------------------------------------------------------------------------------------------------|
| `!log.removed`      | reorg-removed entry from a lagging provider; the reorg path owns it                                                                             |
| `topics.len() == 3` | a standard `Transfer` has exactly 3; anything else either shares topic0 by coincidence or is a non-standard token, and is not safe to interpret |
| `data.len() >= 64`  | malformed / non-`uint256` payload                                                                                                               |
| `amount != 0`       | some tokens emit zero-value `Transfer`s                                                                                                         |

`apply_erc20_transfers` then credits **only** invoices whose `token_lc` matches the
emitting contract exactly. `token_lc == None` means a native invoice and is skipped
outright. Insert/conflict/webhook handling is identical to `apply_block`.

### 4.5 `tick_logs` — the WalletConnect path

```
1. load_watched_invoices
2. build by_id map                → HashMap<Uuid, WatchedInvoice>   (no multimap needed)
3. eth_blockNumber                → tip
4. load cursor (or cold start)
5. detect_fork_point_sparse → handle_reorg → rewind cursor          [if reorg]
6. chunked scan, MAX_LOG_BLOCK_RANGE blocks at a time:
     a. eth_getLogs(address = vault, topics = [payment_topic0()])
     b. fetch the chunk's LAST block header as an anchor  (after the logs)
     c. apply_payment_log for each log
     d. remember anchor + save_cursor
7. refresh_confirmations(tip)
8. prune_seen_blocks
```

Payment logs carry the invoice UUID natively, so this map is keyed by ID — there is no
address-collision problem to solve here.

`payment_topic0()` is a `OnceLock` reading `TOPIC_0` from the environment, falling back
to `DEFAULT_PAYMENT_TOPIC0`, lowercased. That makes redeploying the vault with a changed
event signature a config change.

**Anchors.** Unlike the address scanner, this path does not visit every block — it
queries ranges. So it can only remember **one header per chunk**: the header of the
chunk's last block. That header is fetched *after* the logs on purpose. If a reorg lands
between the two calls, the anchor won't match canonical on the next tick, and we rewind
and rescan — the ordering turns a race into a self-healing rescan rather than a missed
payment.

`MAX_LOG_BLOCK_RANGE` keeps each `eth_getLogs` inside provider limits; the outer
`MAX_BLOCKS_PER_TICK` budget still applies, so a large backfill is chunked twice.

### 4.6 Decoding the `Payment` event

```solidity
event Payment(
    address indexed merchant,
    address indexed token,
    bytes16 indexed identifier,   // UUIDv4, dashes stripped
    address payer,
    uint256 amountRequested,
    uint256 amountReceived,
    uint256 timestamp
);
```

| Slot        | Content           | Decoder                    |
|-------------|-------------------|----------------------------|
| `topics[0]` | event signature   | matched by the filter      |
| `topics[1]` | merchant          | `topic_to_address`         |
| `topics[2]` | token             | `topic_to_address`         |
| `topics[3]` | invoice UUID      | `topic_to_uuid`            |
| `data[0]`   | payer             | right-aligned address word |
| `data[1]`   | `amountRequested` | `hex_to_u128`              |
| `data[2]`   | `amountReceived`  | `hex_to_u128`              |
| `data[3]`   | `timestamp`       | unused                     |

**Alignment is the one thing that bites here.** An indexed `address` or `uint` is
**right**-aligned in its 32-byte word (`topic_to_address` takes `h[24..]`). An indexed
fixed-size `bytesN` is **left**-aligned / right-padded (`topic_to_uuid` takes
`h[..32]`, i.e. the first 16 bytes). They are opposites. Since the contract strips the
dashes from the UUIDv4, the first 16 bytes of the topic *are* the UUID.

A log with anything other than 4 topics, or short data, is rejected — someone else's
event that shares topic0, not ours.

#### `apply_payment_log` — validation before credit

| Check                                      | Behaviour on failure                                                       |
|--------------------------------------------|----------------------------------------------------------------------------|
| `log.removed`                              | return early; the reorg path owns removed entries                          |
| decodable                                  | log and skip — foreign event sharing topic0                                |
| `by_id` contains the UUID                  | silently skip — settled, expired, or another environment sharing the vault |
| `inv.merchant_wallet_lc == ev.merchant_lc` | log and ignore — wrong merchant credited                                   |
| expected token matches `ev.token_lc`       | log and ignore — paid in the wrong token                                   |
| `block_number >= inv.created_block`        | skip — predates the invoice                                                |

For a native invoice (`token_lc == None`) the expected token is `NATIVE_TOKEN_SENTINEL`
(`address(0)`) — the same value `payNative()` emits.

**We credit `amountReceived`, not `amountRequested`.** This mirrors the vault's own
fee-on-transfer-safe accounting: for a fee-on-transfer token the vault records what
actually landed, and so do we. A delta between the two is logged as a note, and
`recompute_invoice_totals` lands the invoice on `paid` / `underpaid` / `overpaid`
accordingly.

**TODO:** `(invoice_id, tx_hash)` uniqueness collapses two `Payment` events for the
*same* invoice inside the *same* tx (e.g. a batching router) into a single credit.
Supporting that needs `log_index` in the unique key.

### 4.7 Confirmations

`refresh_confirmations` runs at the end of **both** ticks, against the current tip:

```
confirmations = GREATEST(0, tip - block_number + 1)
```

The including block itself counts as 1, so a payment in the tip block has 1 confirmation.

This is computed **off the DB**, not off the blocks just scanned. That is what lets an
invoice re-registered after a crash immediately pick up its real confirmation count
instead of restarting from zero.

Two thresholds:

| Threshold                | Source                                          | Promotes to          | Webhook             |
|--------------------------|-------------------------------------------------|----------------------|---------------------|
| `required_confirmations` | per invoice (defaults to `FINAL_CONFIRMATIONS`) | `merchant_confirmed` | `payment.confirmed` |
| `FINAL_CONFIRMATIONS`    | global constant                                 | `system_confirmed`   | `payment.finalized` |

Both promotions are guarded `UPDATE`s whose `WHERE` clause includes the prior status.
That guard *is* the exactly-once latch: two workers racing on the same payment cannot
both see `rows_affected() == 1`, so only one enqueues the webhook.

Once a payment is `system_confirmed` it is no longer polled, and once an invoice has no
non-terminal payments left it is dropped from the in-memory `pending` map — the DB query
at the top of the tick already excludes it, this just stops the map growing forever.

**TODO:** `FINAL_CONFIRMATIONS` is global today. It should be per-chain — 48 blocks is
~10 min on Ethereum and ~2 min on Polygon, and you probably want considerably more on the
latter.

### 4.8 Reorg handling

Two detectors, because the two scanners have different block-history density:

| Detector                   | Used by         | Assumption                                                                                           |
|----------------------------|-----------------|------------------------------------------------------------------------------------------------------|
| `detect_fork_point`        | address scanner | **dense** — we remembered every block, so we can demand a remembered hash at every height            |
| `detect_fork_point_sparse` | logs scanner    | **sparse** — only one anchor per getLogs chunk, so we walk back through the anchors we actually have |

Both start with the same fast path: re-fetch the cursor block; if its hash still matches
what we stored, there is no reorg and we return `None`.

Otherwise they walk backwards to `last_block - MAX_REORG_DEPTH` looking for the highest
height where our remembered hash still matches canonical. The dense detector treats a
missing remembered block as the fork point (pruned, or we cold-started above it) and
rescans forward from there. The sparse detector just skips to the next anchor.

If nothing survives inside the window, both clamp to the floor and rescan.
**TODO:** clamping is the wrong response here. A reorg deeper than `MAX_REORG_DEPTH`
means our assumptions about the chain are wrong — or the RPC set is serving a different
chain entirely. This should raise an operational alert, freeze the chain's payouts, and
surface on a health endpoint rather than silently rewinding.

#### `handle_reorg` — the two outcomes

Everything strictly above the fork point is untrustworthy. For each affected non-orphaned
payment, `locate_tx` asks the node where the tx is now:

| `locate_tx` result                | Meaning                              | Action                                                                | Webhook? |
|-----------------------------------|--------------------------------------|-----------------------------------------------------------------------|----------|
| `Some((Some(block), Some(hash)))` | still mined                          | update location, reset `confirmations = 0`, status back to `detected` | **no**   |
| `Some((Some(_), None))`           | mined-but-no-hash                    | orphan                                                                | **yes**  |
| `Some((None, _))`                 | back in the mempool                  | orphan                                                                | **yes**  |
| `None`                            | node has never heard of it — dropped | orphan                                                                | **yes**  |

**Re-mining is not a merchant-visible event.** The money never went away, it just moved
blocks; `amount_received` is unchanged and only the confirmation countdown restarts.
`refresh_confirmations` recomputes from the tip on that very same tick, so nothing is
lost. If the payment had already been reported as `merchant_confirmed`, crossing the
threshold again re-emits `payment.confirmed`.

**Orphaning is the only reorg case that notifies the merchant**, because it is the only
one where funds they were told about are actually gone — anything they shipped on the
back of `payment.detected` / `payment.confirmed` needs walking back on their side.
**TODO:** skip this call once merchant webhook settings exist and the merchant has opted
out of orphaned notifications.

After each payment, `recompute_invoice_totals` rebuilds the invoice from its surviving
payments. It is a **full recompute, never a decrement** — that is what makes reorg replay,
duplicate ticks and rescans all converge on the same number no matter how many times they
run.

If an invoice falls back below the requested amount after a reorg, we log it but do not
emit a second event; `payment.orphaned` already told the merchant why.
**TODO:** add `payment.reverted` if merchants ask for it.

### 4.9 Address derivation (naive path)

`derive_address(mnemonic, index)`:

```
BIP-39 mnemonic ──▶ seed ──▶ BIP-32 derive(get_derivation_path(index))
                                  ──▶ secp256k1 secret key
                                  ──▶ uncompressed public key (65 bytes)
                                  ──▶ keccak256(pubkey[1..])          // drop 0x04 prefix
                                  ──▶ last 20 bytes ──▶ 0x… address
```

This is the address that goes in the QR and into `invoices.wallet_address`, and the key
whose funds later get swept to treasury.

### 4.10 EVM known limitations

| Limitation                        | Impact                                                                                                                                                                         |
|-----------------------------------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `hex_to_u128` ceiling             | any value above `u128` errors out. Same ceiling as the rest of the money pipeline. **TODO:** move to `u256` (see the `wei_to_decimal` TODO).                                   |
| `FINAL_CONFIRMATIONS` is global   | wrong for fast chains; needs to be per-chain                                                                                                                                   |
| Deep reorg → clamp                | should alert and freeze instead                                                                                                                                                |
| Duplicate `Payment` events per tx | collapsed into one credit; needs `log_index` in the unique key                                                                                                                 |
| Address multimap                  | double-credits on address reuse by design; tighten once addresses are single-use                                                                                               |
| `REORG_WINDOW` const              | declared on the impl but currently unused; `MAX_REORG_DEPTH` is what actually bounds the walk-back. **TODO:** remove it or wire it up.                                         |
| Settlement trigger policy         | `payment.finished` fires on the *amount* threshold regardless of confirmations. **TODO:** make it a merchant setting: `on_detected` (current), `on_confirmed`, `on_finalized`. |
| Underpaid tolerance               | strictly `>=` today; no dust/rounding allowance. **TODO:** make configurable.                                                                                                  |

---

## 5. Solana — implemented

### 5.1 One service, not two

EVM runs two scanners over one chain because its two paths live in different places: ERC-20
`Transfer` logs for the naive path, `CustodialPaymentVault` events for the smart path. Solana
has no such split. Both paths are *account-indexed*: a transaction is discoverable from any
account key it names, whether that key received money, sent money, or was attached as a
reference and did neither.

So there is one service, `SolanaNetwork::watch_addresses`, one `tick`, and one cursor scope.
Both paths fall out of the same signature feed, and the only thing that distinguishes them is
which of our addresses the transaction credited. `scope` is unused; the Solana cursor table is
keyed by address instead.

```
        ┌─────────────────────────────────────────────────────────┐
        │  tick()  — every POLL_INTERVAL_SECS (2s)                 │
        └─────────────────────────────────────────────────────────┘
                  │
                  ├─ expire_invoices()          wall-clock only
                  ├─ load_watched_invoices()    → WatchIndex
                  ├─ discover_signatures()      per address, ADDRESS_CONCURRENCY=8
                  │        └─ getSignaturesForAddress, cursor-bounded
                  ├─ filter: already-booked tx_hashes
                  ├─ getTransaction (batched, MAX_TX_PER_BATCH=50, oldest first)
                  │        └─ parse_tx_view → classify → apply_transaction
                  ├─ recompute_invoice_totals() once per touched invoice
                  ├─ save_address_cursor()      capped at the finalized watermark
                  ├─ reconcile_statuses()       getSignatureStatuses, batched
                  └─ prune_address_cursors()
```

### 5.2 The two paths, concretely

Both are live for every invoice; the payer picks, and the backend finds out afterwards.

**Naive path.** The frontend renders a QR of the **HD-derived owner pubkey** — always the
owner, never an ATA. A native-SOL invoice is paid by sending lamports to it. A token invoice
is paid by sending the mint to it, and the payer's wallet derives the associated token account
on its own side, creating it if necessary. We store the address the money will actually land
on in `invoices.wallet_address`: the owner pubkey for native, the owner's ATA for the mint
otherwise.

**Smart path.** The wallet-connect flow builds a transfer straight to the merchant's own
treasury — `merchant_wallets.address` for native, that wallet's ATA for a token invoice — and
attaches the invoice's HD-derived owner pubkey as a Solana Pay `reference`. No sweep is ever
needed: the funds are already where they belong.

```
 native invoice                          token invoice
 ──────────────                          ─────────────
 wallet_address     = HD owner pubkey    wallet_address     = ATA(HD owner, mint, program)
 payment_reference  = HD owner pubkey    payment_reference  = HD owner pubkey
 merchant_target    = merchant wallet    merchant_target    = ATA(merchant wallet, mint, program)
 
 QR shows           HD owner pubkey      QR shows           HD owner pubkey
```

Note the collapse on the native side: `wallet_address == payment_reference`, byte for byte.
That is deliberate — the HD key *is* both the deposit account and the correlation key — and it
is the reason several guards in §5.8 exist at all.

`merchant_target` is not a column. It is recomputed each load from `merchant_wallets` plus the
invoice's mint and token program, and memoized per `(wallet, mint, program)` for the duration
of the load, because `find_program_address` is up to 255 SHA-256 rounds and most invoices in a
batch share the same triple. If the merchant has no `merchant_wallets` row for `'solana'`, the
target is empty and **the reference path is disabled for that invoice** — the direct path still
collects normally. Invoice creation logs this at creation time rather than leaving the watcher
to complain once per tick forever.

### 5.3 RPC layer: failover, not quorum

EVM runs a quorum across providers because a wrong `eth_getLogs` result is unfalsifiable from
the response alone. Solana's watcher instead uses **ordered failover**: `rpc()` tries each URL
in `rpc_urls` in turn and returns the first success. `rpc_batch()` does the same for JSON-RPC
batches, matching responses back by `id` rather than by position, and filling any id the
provider omitted with an error rather than silently shifting the results.

The reason quorum isn't needed here is that every number the watcher trusts is
self-validating. Balance deltas come from the transaction's own `meta`, and the transaction is
addressed by its signature — a value that cannot be forged into meaning something else. A
provider that lies produces a parse failure, not a wrong credit. The one place a provider *can*
hurt us is by not expanding address-lookup-table accounts into `message.accountKeys`, which
would misalign the balance arrays; `parse_tx_view` refuses the transaction outright in that
case rather than guessing an alignment (§5.7).

Two commitments are in play throughout:

| Constant               | Value       | Used for                                                                                                                     |
|------------------------|-------------|------------------------------------------------------------------------------------------------------------------------------|
| `DETECT_COMMITMENT`    | `confirmed` | listing signatures, fetching bodies — we want to see money early                                                             |
| `FINALIZED_COMMITMENT` | `finalized` | the watermark that gates cursor advancement and orphaning — we only make irreversible decisions against an irreversible root |

### 5.4 Address derivation and what invoice creation writes

`get_derive_address` is the only place an address is minted. It:

1. Reads `token_address` / `token_program` back off the invoice row. Creation commits those two
   columns **before** calling, precisely so this read sees them. A mint with no program is a
   hard error, never a legacy-SPL default: a wrong default yields a perfectly valid-looking
   address that nobody will ever pay into.
2. Bumps `merchant_network_indices.next_index` for `(merchant_id, network='solana', account_index=0)`
   with an atomic `INSERT … ON CONFLICT DO UPDATE … RETURNING`, so two concurrent invoices can
   never draw the same index.
3. Derives the owner pubkey from the merchant's decrypted BIP-39 mnemonic at that index.
4. Returns `(deposit_address, index, Some(owner_address))` — where `deposit_address` is the ATA
   for a token invoice and the owner itself for native, and the third element becomes
   `invoices.payment_reference`.
   Creation then writes `created_block` from the **finalized** slot, not the tip. `created_block`
   is used as a floor to discard transactions that predate the invoice, and a tip reading can sit
   ahead of where the payer's transaction lands. Finalized is always behind, so the error is
   always in the safe direction: a few extra signatures scanned rather than a real payment thrown
   away.

`wallet_address` is written **last**. Until it is non-empty the invoice is invisible to
`load_watched_invoices`, so a crash mid-creation leaves a row that is never polled and simply
expires, rather than one that is polled against a half-derived address.

### 5.5 Who we watch

`load_watched_invoices` selects invoices for this `(network_type='solana', chain_ref)` with a
non-empty `wallet_address` that are either still live (`pending`/`underpaid` and not expired)
**or** dead but still carrying a payment in `detected`/`merchant_confirmed`. The second clause
is what keeps a payment's confirmation ladder running after its invoice has been settled or
expired.

The set is capped at `MAX_WATCHED_INVOICES` (5 000), ordered by `created_at ASC`. Hitting the
cap logs loudly, because the ordering means the oldest invoices keep being polled while newer
ones starve — that is a sharding signal, not a steady state.

Each row becomes a `WatchedInvoice`, and the set is indexed once per tick into a `WatchIndex`:
maps from deposit address → invoices, reference key → invoices, and invoice id → invoice.
Attribution walks the ~30 account keys a transaction names and looks them up, rather than
scanning the whole watch set per transaction.

**Addresses actually polled** are `deposit_address` and `payment_reference`, deduped — so a
native invoice costs one feed, a token invoice two. `merchant_target` is deliberately **never
polled**. It is the merchant's own treasury; polling it would drag every unrelated movement on
that wallet through this loop for no gain, since the reference key already surfaces exactly the
transactions we care about.

### 5.6 Discovery: per-address signature cursors

`network_address_cursors` holds one row per `(network_type, chain_ref, address)` with
`last_signature` and `last_slot`.

`getSignaturesForAddress` returns newest-first and stops at `until`. `discover_signatures`
pages backwards until it reaches the cursor, then reverses so credits are applied oldest-first.
It has three stop conditions, and each exists because the others can fail:

| Stop                                            | Why it is needed                                                                                                                                                                 |
|-------------------------------------------------|----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `until` matches the cursor signature            | the normal case                                                                                                                                                                  |
| slot drops **below** `last_slot` (strictly `<`) | the cursor signature can become unreachable — pruned history on a non-archival node, or a fork that dropped it — and `until` would then never match and we'd page toward genesis |
| page cap `MAX_SIG_PAGES_PER_ADDRESS`            | last resort against an address being spammed                                                                                                                                     |

The floor comparison is strictly `<`, never `<=`. Solana packs many transactions into one slot,
and `<=` silently discarded every signature that shared a slot with the cursor but sorted after
it. Re-listing the cursor's own slot costs nothing — the already-booked filter in §5.9 drops
the duplicates — while losing one costs a payment.

On a **cold start** (no cursor row) the floor is `MIN(created_block) - CREATED_SLOT_MARGIN` over
every invoice that has ever used this address. An invoice cannot be paid before it existed, so
anything older belongs to somebody else's history.

**Cursor advancement is the subtle part.** After the tick's transactions are applied, the cursor
walks the address's signature list in order and stops at the first signature that is neither
applied nor a known-failed transaction — it never steps over a hole. It also stops at
`finalized_slot`, so the cursor is only ever parked on an irreversible signature. Everything
above the finalized watermark is re-listed on every tick by design: that is how a transaction
that was dropped and re-landed is picked up, with no reorg machinery anywhere in the file.

If paging stopped on the page cap, the scan is marked incomplete and **the cursor is not saved
at all**, with a loud error. Advancing it would step past older signatures that were never
listed, and those would be unreachable forever.

### 5.7 Reading a transaction: balance deltas, not instructions

`parse_tx_view` reduces a `jsonParsed` transaction to three things: the account key set, the
signer set, and balance deltas.

It deliberately **does not parse instructions**. A payer can move money with `transfer`,
`transferChecked`, a CPI from a router or aggregator, several transfers in one transaction, or
create-ATA-and-transfer in one shot. Pre/post balances cover all of them and cannot be spoofed
by instruction shape; maintaining a list of every way somebody can send a token could not.

- **Native deltas** are `postBalances[i] - preBalances[i]`, keyed by account.
- **Token deltas** are folded from `preTokenBalances` (−1) and `postTokenBalances` (+1), keyed
  by **`(token account address, mint)`**. The mint in the key is what makes a wrong-token
  transfer structurally uncreditable: an invoice for mint X reads a key that a transfer of mint
  Y never touches. We never look at ATAs that don't correspond to the invoice's mint, because
  we never look them up.
- **Signers** come from the per-key `signer` flag, falling back to the first
  `numRequiredSignatures` keys when the encoding doesn't carry it. This is the one thing beyond
  balances that attribution needs (§5.8).
  Two refusals rather than guesses: a `meta` that is absent *or explicitly null*, and a balance
  array longer than the account key list — the signature of a provider that didn't expand address
  lookup tables. Guessing the alignment in the second case credits the wrong address. The parser
  also cross-checks that the returned transaction's first signature is the one that was asked
  for, so a mis-zipped batch response fails loudly instead of attributing one payer's money to
  another invoice.

**Token-2022** needs no special handling here, and that is a consequence of using deltas. Its
ATA derivation is identical apart from the program id, which is already a seed. Transfer-fee
and hook extensions change how much arrives, and the post balance *is* how much arrived — so
detection is correct today with no extension-aware code. What it is not is *fee-aware*: see
§5.13.

### 5.8 Attribution — `classify`

For each transaction, `classify` returns a list of `(invoice, amount, path)`.

**Direct path.** For every account key the transaction names that is some invoice's
`deposit_address`: credit the invoice's own delta in the invoice's own asset, if positive, and
if the slot is plausible. One extra condition — **the deposit address must not have signed**.
Closing an ATA refunds its rent to the owner, which appears as a positive lamport delta on the
HD key inside a transaction that key authorized; on a native invoice that is a false credit. An
ATA can never sign, so the guard costs nothing on the token side.

**Reference path**, for every key that is some invoice's `payment_reference`, under four
conditions:

1. **The reference key did not sign.** A Solana Pay reference is read-only and non-signing by
   construction. This is the guard that stops the merchant's own sweep — HD address → treasury —
   from being re-credited as a payment, which on a native invoice looks *exactly* like a
   smart-path payment: our key is present and the treasury balance went up. It also stops a
   third party from steering attribution by naming our HD key in a transaction of their own.
2. **The reference account's own lamports did not go down.** Belt and braces against the same
   class of outbound flow.
3. **The invoice was not already credited on the direct path in this transaction.** Guaranteed
   to trigger on every native naive-path payment, since there the two addresses are one key.
   If money *also* moved into the treasury in that same transaction, only the direct leg is
   credited and the split is logged for manual review.
4. **No other invoice is claiming the same credit.** Claimants are grouped by
   `(merchant_target, mint)`; a group with more than one claimant credits nothing and logs,
   because one treasury credit with two claimants cannot be split. Two references pointing at
   *different* merchants, or at different mints on the same merchant, are separate credits and
   both settle normally — one transaction batching payment for two of a merchant's invoices is
   legitimate and is not blocked.
   A reference that is present but produces no treasury credit in the expected asset — wrong mint,
   wrong destination, or an attempt to correlate without paying — is logged and credited nothing.
   **The reference alone is never evidence of payment.** Money moving is.

Both paths also require `slot_is_plausible`: the transaction's slot must be at least
`created_block - CREATED_SLOT_MARGIN`. The margin absorbs skew between the node that stamped
`created_block` and the node the payer's wallet submitted through.

The path taken is persisted in `payments.payment_path` (`'direct'` | `'reference'`), which EVM
has no equivalent of.

### 5.9 Recording credits — `apply_transaction`

Every credit a single signature produced is written in **one database transaction**, along with
its `payment.detected` webhook. That is not cosmetic: `tick` skips re-fetching signatures that
are already on record, so a half-committed transaction would leave one invoice credited and
another permanently skipped.

Idempotency rests on the unique index `payments (invoice_id, tx_hash)`. The insert is
`ON CONFLICT DO NOTHING`, so the every-tick rescan of the unfinalized window is free. The only
UPDATE that can fire is the **resurrection** case — a payment we orphaned whose transaction has
since re-landed — and it rewrites the slot and status but **never the amount**. The same
signature always moved the same money; letting the amount change would let a replay inflate a
total.

Before any of this, `tick` filters candidates against payments already on record, bounded by
both the watched invoice ids and the candidate signature list, so the query is served by
`payments_txhash_idx` and returns at most one row per candidate.

### 5.10 Confirmations and "reorgs" — `reconcile_statuses`

Solana has no reorg in the EVM sense. A signature is the transaction's identity, not a
`(block, index)` pair, so a transaction that re-lands is simply found again at a different
slot. There is no `handle_reorg`, no `network_seen_blocks`, no parent-hash chain.

What replaces both `refresh_confirmations` and `handle_reorg` is one pass over every payment in
`detected` or `merchant_confirmed`, asking `getSignatureStatuses` what the cluster currently
thinks:

| Response                                   | Meaning                           | Action       |
|--------------------------------------------|-----------------------------------|--------------|
| `null`, slot ≤ finalized, history searched | dropped for good                  | → `orphaned` |
| `null`, slot > finalized                   | may still be propagating          | leave alone  |
| `err != null`                              | landed but the transaction failed | → `orphaned` |
| `confirmationStatus`                       | the level it reached              | promote      |

A `null` is only trusted on the history-searching pass. From the status cache alone it is not
evidence of anything.

`searchTransactionHistory` makes the node fall back to the ledger or BigTable, which several
providers bill separately and rate-limit hard. So the batch is split by age:
payments newer than `RECENT_STATUS_WINDOW_SLOTS` (300) are still in the status cache and are
queried without it; only older ones pay for the lookup.

Levels map through `ConfirmLevel`:

| RPC `confirmationStatus` | `ConfirmLevel` | written to `payments.confirmations` |
|--------------------------|----------------|-------------------------------------|
| `processed`              | `Detected`     | `CONF_DETECTED` = 1                 |
| `confirmed`              | `Confirmed`    | `CONF_CONFIRMED` = 16               |
| `finalized`              | `Finalized`    | `CONF_FINALIZED` = 32               |

The invoice's `required_confirmations` is bucketed the same way (`≥32` → finalized, `≥2` →
confirmed, else detected), so a merchant asking for "12 confirmations" on a chain that has no
such notion gets the nearest meaningful commitment rather than a number that means nothing.
`finalized` is terminal: the payment moves to `system_confirmed` and drops out of this pass
forever, which is what keeps it bounded. Every promotion is a guarded UPDATE, so two workers
racing cannot both emit a webhook.

Only orphaning triggers a recompute of invoice totals. Promotion moves a payment between
`detected`, `merchant_confirmed` and `system_confirmed`, all of which count toward
`amount_received` identically — recomputing after one is a full re-aggregation that cannot
change a number.

### 5.11 Invoice totals and expiry

`recompute_invoice_totals` rebuilds `amount_received`, `status` and `tx_hash` from
`SUM(amount) WHERE status <> 'orphaned'` — always a full recompute, never a delta, so rescans,
duplicate ticks and dropped transactions all converge on the same number. `tx_hash` holds the
earliest non-orphaned payment.

This is also where multi-transfer underpayment resolves itself with no special casing: three
partial sends to the deposit address are three payment rows, and the invoice flips `underpaid`
→ `paid` on the third. The reference path is expected to be a single wallet transaction, but it
goes through identical arithmetic, so a partial there behaves the same way rather than being a
code path nobody tested.

`expired` is terminal and never overwritten by the recompute. Expiry itself is pure wall-clock —
nothing chain-derived — and covers `underpaid` as well as `pending`, so a partially-paid invoice
actually leaves the watch set instead of being polled every two seconds forever. Payments
already recorded keep climbing the confirmation ladder regardless, via the second clause in
§5.5.

`prune_address_cursors` drops cursor rows no live invoice references and older than seven days.
It **bails on an empty whitelist**: `address <> ALL('{}')` is true for every row, so pruning
during a quiet minute would wipe the table and force a cold-start rescan of every address. For
the same reason `tick` returns early when nothing is watched, before reaching the prune.

### 5.12 Webhooks

Same set as EVM, plus `invoice.expired`, and each shares the DB transaction of the state change
that justifies it.

| Event               | Fired when                                         | Dedupe key                                  |
|---------------------|----------------------------------------------------|---------------------------------------------|
| `payment.detected`  | first insert of an `(invoice_id, tx_hash)` row     | `payment.detected:{invoice_id}:{signature}` |
| `payment.confirmed` | payment reaches the invoice's required level       | `payment.confirmed:{payment_id}`            |
| `payment.finalized` | payment reaches `finalized`                        | `payment.finalized:{payment_id}`            |
| `payment.orphaned`  | transaction dropped or failed on chain             | `payment.orphaned:{payment_id}:{slot}`      |
| `payment.finished`  | invoice's total first reaches the requested amount | `payment.finished:{invoice_id}:{status}`    |
| `invoice.expired`   | wall-clock expiry                                  | `invoice.expired:{invoice_id}`              |

`webhook_events` is unique on `(merchant_id, dedupe_key)`, which means the key must be unique
across **event types**, not just within one. Every key above is therefore prefixed with its
event type and scoped to its subject. A bare `payment_id` would make `payment.confirmed` and
`payment.finalized` collide; a bare signature would swallow the second event when one
transaction pays two of the same merchant's invoices. The slot in the orphan key lets a payment
that was dropped, re-landed and dropped again notify twice.

`payment.finished` fires on **detection**, not confirmation. Making that a merchant setting
(`on_detected` / `on_confirmed` / `on_finalized`) is a known TODO.

### 5.13 Tunables

| Constant                     | Value      | Meaning                                                                                                                                |
|------------------------------|------------|----------------------------------------------------------------------------------------------------------------------------------------|
| `NETWORK_TYPE`               | `"solana"` | the one network string — `invoices`, `merchant_wallets`, `merchant_network_indices`, `network_address_cursors` all use it. Never `sol` |
| `POLL_INTERVAL_SECS`         | 2          | tick period                                                                                                                            |
| `SIG_PAGE_LIMIT`             | 1000       | RPC maximum per signature page                                                                                                         |
| `MAX_SIG_PAGES_PER_ADDRESS`  | 20         | ceiling on paging; exceeding it freezes that address's cursor and logs                                                                 |
| `MAX_TX_PER_BATCH`           | 50         | `getTransaction` batch size                                                                                                            |
| `MAX_STATUS_PER_BATCH`       | 256        | `getSignatureStatuses` batch size                                                                                                      |
| `ADDRESS_CONCURRENCY`        | 8          | parallel address scans                                                                                                                 |
| `MAX_WATCHED_INVOICES`       | 5 000      | watch-set cap; ~2 addresses each is the real RPC bound                                                                                 |
| `CREATED_SLOT_MARGIN`        | 64         | slack on the `created_block` floor, absorbing node skew                                                                                |
| `RECENT_STATUS_WINDOW_SLOTS` | 300        | below this age, skip `searchTransactionHistory`                                                                                        |

### 5.14 Known limitations

- **Fee-on-transfer Token-2022 mints read as underpaid.** Detection is correct — we credit what
  arrived — but a mint that skims on transfer means what arrived is less than what was
  requested, and the invoice sits at `underpaid`. Handling this needs the mint's transfer-fee
  config read at invoice creation and folded into the requested amount. Explicitly out of scope
  for now.
- **Split-path transactions need a human.** One transaction that both pays the deposit address
  and moves money into the treasury credits only the direct leg and logs.
- **Two claimants on one treasury credit credit nothing.** Correct but unhelpful; resolving it
  needs per-transaction amount matching, which is guesswork when amounts collide.
- **The reference path credits the whole treasury delta** in the invoice's asset for that
  transaction. Fine for a wallet-built transfer; a hand-built transaction that pays the
  merchant twice for different reasons in one signature would over-credit.
- **Single worker assumed.** Guarded UPDATEs and the monotonic cursor make a second instance
  safe rather than correct — it would double the RPC bill and could park a cursor on an earlier
  signature within the same slot. There is no leader election.
- **A permanently unparseable transaction freezes its address's cursor**, by design (never step
  over a hole), but nothing escalates it beyond a log line.
- **Page-cap truncation leaves an unreachable gap.** The cursor is frozen and the error is
  loud, but signatures older than the truncation point are never listed. Raising the cap or
  sharding is the only remedy.
- **Late payments to an expired invoice are invisible** unless that invoice still has an open
  payment. Deliberate — expiry means expiry — but it means an out-of-band refund process is
  needed for a payer who was slow.
- **No dust tolerance.** `received >= requested` is strict; a rounding shortfall of one base
  unit leaves the invoice `underpaid`.
- **Watch-set starvation above the cap.** Ordering by `created_at ASC` means the oldest
  invoices win, and newer ones are simply not polled.
- **Invoices predating `created_block`** have no slot floor and fall back to scanning the whole
  feed for their address, capped by the page limit.

---

## 6. Esplora (Bitcoin-style UTXO) — TODO

> **Not implemented.**

### 6.1 Paths

- **Naive only.** There is no smart path. Bitcoin has no equivalent primitive that a
  connected wallet can use to attach an invoice identifier to a payment in a way that is
  both universally supported and reliably retrievable. `OP_RETURN` is not it — support is
  inconsistent across wallets and it changes the fee profile.

So Esplora is the network that justifies the whole "network-optimized paths" framing in
reverse: the orchestrator must be able to produce an invoice with **one** payment option
without any of the shared machinery assuming a second one exists.

### 6.2 To specify

- [ ] **Detection mechanism.** Esplora HTTP API: `/address/:addr/txs` polling vs
  block-scanning via `/block/:hash/txs`. Address-endpoint polling is far cheaper and
  is the obvious first implementation.
- [ ] **UTXO accounting.** A payment is a set of outputs, not a single value. Decide how
  `payments.amount` is populated — per-output rows, or one row per tx summing all
  outputs to the watched address. This is where the EVM-shaped `(invoice_id,
      tx_hash)` unique key needs the most scrutiny.
- [ ] **Mempool / 0-conf.** Esplora exposes unconfirmed txs. Decide whether a mempool
  sighting creates a `detected` row (consistent with EVM, where `detected` can mean
  0 confirmations) or whether we wait for a block.
- [ ] **RBF and double-spends.** The UTXO equivalent of an orphan, and more common than a
  reorg. `handle_reorg`'s "is this tx still there" logic maps reasonably well, but
  the trigger is different — needs its own path.
- [ ] **Reorg detection.** Block hashes and heights map cleanly onto
  `network_seen_blocks`, so `detect_fork_point` is largely portable. Confirm the
  Esplora API gives enough header history to do the walk-back.
- [ ] **Change addresses & the gap limit.** HD derivation is BIP-44/49/84 with an
  account/change/index structure. Decide the derivation scheme and the gap-limit
  policy for rescans.
- [ ] **Sweep + fee estimation.** UTXO consolidation cost is materially different from
  EVM's flat transfer cost. The sweep is not a solved problem copied from EVM.
- [ ] **Confirmation counting.** This one is genuinely the same as EVM: height-based,
  `tip - block_height + 1`.

---

## 7. Adding a new network

The contract a new `NetworkClient` has to satisfy:

1. **Derive an address from a provided seed + an index** (naive path). The seed source is a parameter, not a constant: each merchant has their own seed, and index `0` is reserved for that merchant's main address on the network. The network implementation does not know or care where the seed came from.
2. **Run at least one watcher service** that, given the DB pool, writes `payments` rows
   for money arriving against watched invoices.
3. **Be idempotent.** Re-running any tick over the same range must produce no new
   effects. In practice this means: `INSERT … ON CONFLICT DO NOTHING` on
   `(invoice_id, tx_hash)`, never rewriting an amount on conflict, and recomputing
   invoice totals rather than incrementing them.
4. **Enqueue webhooks in the same DB transaction as the state change** that justifies
   them, behind a guard that can only succeed once.
5. **Advance its cursor only on success**, so a failed tick is a no-op.
6. **Scope its scan state** so multiple services on one chain don't fight over one cursor.
7. **Expose a reconciliation surface**: given a watch set of addresses, ingest **every** movement touching them in either direction, whether or not it maps to an invoice. This is a strictly larger surface than payment detection and is a separate scan scope with its own cursor. See [`RECONCILIATION.md`](./RECONCILIATION.md) §2.
8. **Declare an address-reuse policy.** Networks that reuse deposit addresses may only reuse addresses that have never received value, and only after the invoice's expiry plus a quarantine of at least twenty-four hours.
9. **Pin and document a derivation path**, including which mainstream wallets agree with it. A merchant importing an exported mnemonic into a third-party wallet and seeing an empty account is a recovery failure, and the divergence is worst on Solana.


What it does *not* have to do: use the same number of services, use blocks, use a
confirmation count, or support two payment paths. TRON, when it lands, is expected to
look nothing like EVM's fee model even though it will reuse the general shape — the plan
is to lean on whatever TRON gives us for energy/bandwidth optimization rather than
pretending it's Ethereum with different constants.

---

## 8. Correctness invariants

These hold across every network implementation, present and future. If a change breaks
one of them, it is a bug regardless of what else it fixes.

1. **`payments` is append-mostly.** An amount is written once, at insert. Location
   (`block_number`, `block_hash`) and status may be updated; the amount may not.
2. **Invoice totals are always a full recompute** from non-orphaned payments. Never a
   delta.
3. **Every webhook has a once-only latch** that is a DB constraint or a guarded `UPDATE`,
   not application logic.
4. **Cursors advance only after the work is committed.**
5. **An expired invoice is never resurrected** by a late payment recompute.
6. **Nothing durable lives only in memory.** The watch set is rebuilt from the DB every
   tick; the in-memory `pending` map is a hint, not state.
7. **Amounts are base units everywhere below the presentation layer.**
8. **An expired invoice is never resurrected by a late payment recompute** — and the value of that late payment is still recorded, through reconciliation rather than through the invoice. Invoice status is a claim about a commercial agreement; ledger balance is a claim about custody. See [`RECONCILIATION.md`](./RECONCILIATION.md) §9.