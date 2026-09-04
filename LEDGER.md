# Ledger, Movements & Fees

Companion to `README.md` and `NETWORKS.md`. Where `NETWORKS.md` describes how money is
*detected*, this document describes what happens to it afterwards: how on-chain activity is
recorded, how merchant balances are derived, and how operator fees accrue and settle.

> **Status: schema implemented, logic in progress.** The tables, constraints and views
> described here exist as `migrations/010_ledger.sql`, and that file is authoritative for
> column names and types — where this document and the migration disagree, the migration is
> right and this document is stale. What does *not* exist yet is most of the logic:
> sweeping, fee settlement, withdrawals and conversions are described here as designs, not as
> shipped code. Expect the settlement policy, sweep ordering and conversion handling to change
> more than the schema does.
>
> What is intended to be stable is the *shape*: three layers, double-entry, append-only,
> recognition gated on finality, and a ledger denominated in assets rather than in handlers.
>
> Reconciliation — the handling of value that moves for reasons the processor did not
> initiate — is specified separately in [`RECONCILIATION.md`](./RECONCILIATION.md). It shares
> this document's three layers and enters the pipeline at the movement level.

---

## 1. Design principle: three layers, one direction of truth

There are three distinct questions to answer, and conflating any two of them produces a
table that answers neither well:

| Layer                                | Question it answers                                     | Natural key                          | Mutability                                                                             |
|--------------------------------------|---------------------------------------------------------|--------------------------------------|----------------------------------------------------------------------------------------|
| `chain_transactions`                 | What did we broadcast or observe, and what did it cost? | `(network_type, chain_ref, tx_hash)` | `status`, `block_number`, `block_hash`, `block_time` mutable; the rest fixed at insert |
| `chain_movements`                    | Which value moved from where to where?                  | `(tx_id, event_index)`               | append-only, enforced by trigger                                                       |
| `ledger_journals` + `ledger_entries` | Who owns what, and who owes whom?                       | journal `dedupe_key`                 | strictly append-only; corrections are reversals, never edits                           |

Information flows in one direction only:

```
   chain  ──▶  chain_transactions  ──▶  chain_movements  ──▶  ledger
observed        (what happened)         (what moved)       (what it means)
                                              │
                                              └──▶  sweep_queue
                                                    (what to do about it)
```

The ledger never writes back down. Nothing in the chain layer is ever adjusted to make the
ledger balance.

`sweep_queue` hangs off the movement layer rather than sitting in the chain of truth at all. It
is a work queue, fully rebuildable from `chain_movements` plus the `custody_unswept` balance,
and it is the only mutable table in the ledger schema. It is deliberately not part of the audit
trail (§2.7).

Movements do not all originate from the processor. A movement may be observed at an address
the system controls without the system having initiated it — a late payment, a merchant
self-move using an exported key, a mistaken send, unsolicited dust. Such movements enter at
the `chain_movements` layer exactly like initiated ones and are subject to the same
double-entry treatment. Nothing about them is special-cased in balance derivation.

### Why transactions and movements are separate tables

One transaction can produce many value transfers. A batched sweep is the obvious case: a
single vault call sweeps twelve deposit addresses, pays gas exactly once, and emits twelve
`Transfer` events. If gas lived on the movement row it would have to be either duplicated
across all twelve — making any `SUM(gas_paid)` wrong — or split by an invented pro-rata rule.

Gas is a property of the transaction. Value transfers are properties of the movements. The
same split covers UTXO transactions with multiple outputs and Solana transactions with
multiple instructions, and it is what makes a failed-but-gas-burning transaction
representable at all (§9).

### Why the ledger is double-entry

A custodial processor is, in accounting terms, an entity that **holds assets and owes
liabilities against them**. The same 50 USDC is simultaneously:

- an asset the operator physically controls, sitting at some address, and
- a debt owed to a specific merchant.

Single-entry running balances cannot express that one unit of value is both of these at
once, which means they cannot answer the two questions this system exists to answer
correctly at the same time: *"is the operator solvent?"* and *"what can this merchant
withdraw?"* Double-entry answers both from one book, and makes the answer self-checking — if
debits and credits do not sum to zero, something is wrong and it is loud.

The familiar "running balance per account" statement is still available; it is a window
function over `ledger_entries`, not a different data model (§11.1).

---

## 1.1 Asset identity: the ledger does not know what a handler is

Everything in the ledger is denominated in an **asset**, identified by
`(network_type, chain_ref, asset_kind, address)`. Nothing in the ledger is denominated in a
`token_id`.

This is a deliberate split from the rest of the system, which speaks in `token_id` throughout:
the orchestrator resolves a `token_id` to a `TokenHandler`, invoices carry a `token_id`, the
checkout view is chosen per `token_id`. That is correct for those layers, because a `token_id`
is a **route** — a named, configured way of moving a particular asset, owned by a particular
piece of code.

Routes are code. They get renamed, forked, disabled, and replaced by a second implementation
of the same token — `USDC_BASE` built on raw RPC calls alongside `USDC_BASE_CRATES` built on a
library, both pointing at the same contract, both legitimate, either one deletable next month.
A ledger keyed on routes inherits every one of those instabilities: deleting a handler orphans
historical entries, and registering the same contract twice under two names splits one real
balance into two ledger balances that will never be reconciled with each other.

An asset is a fact about a chain. It survives the deletion of every line of code that ever
touched it.

### The two identities, and where each one lives

|           | `token_id` (route)                                                                                  | `asset_id`                                                                      |
|-----------|-----------------------------------------------------------------------------------------------------|---------------------------------------------------------------------------------|
| Answers   | "which configured path moves this?"                                                                 | "what is this?"                                                                 |
| Owned by  | the code — `TokenRegistry` at boot                                                                  | the chain                                                                       |
| Lifetime  | as long as the handler exists                                                                       | permanent                                                                       |
| Lives on  | `chain_transactions`, `chain_movements`, `invoices`, `fee_rates`, `sweep_queue`, journal `metadata` | `assets`, `chain_movements`, `sweep_queue`, `ledger_accounts`, `ledger_entries` |
| Deletable | yes, without data loss                                                                              | never                                                                           |

`chain_movements` carries **both**. That is the join point: at the moment value moves, the
system knows which handler moved it and which asset moved, and it records both. Afterwards the
ledger reads only the second. Full operational traceability — "which code broadcast this
withdrawal" is answerable for any historical transaction — with no coupling in the direction
that matters.

`sweep_queue.token_id` carries a route too, but as a *pin* rather than an identity: it names
the handler an operator chose to sweep with, and `NULL` means "resolve at sweep time from any
handler advertising this asset with `can_sweep`". That is what lets a payment received through
`USDC_BASE` be swept by `USDC_BASE_CRATES`.

### `asset_kind`, not a sentinel address

Native assets have no contract address. The EVM convention of using the zero address is an EVM
idiom; it means nothing on Solana and nothing on a UTXO chain, and encoding it into the ledger
buys one saved column in exchange for a special case in every network implementation, every
query and every canonicalization routine.

`asset_kind` is an explicit discriminator — `native` or `contract` — and `address` is `NULL`
exactly when the kind is `native`, enforced by a check constraint.

### Canonicalization is enforced, not assumed

Canonicalization has two halves. Both are load-bearing, and only one of them can be enforced
by a constraint.

**Address canonicalization.** Each network family provides at least one way for two
registrations of the same real asset to disagree textually:

| Network          | Hazard                                         | Canonical form                                         |
|------------------|------------------------------------------------|--------------------------------------------------------|
| EVM              | EIP-55 checksummed vs lowercase hex            | lowercase hex, `0x`-prefixed                           |
| Solana           | base58, case-**sensitive** — no safe case fold | stored as-is; validated for charset and decoded length |
| Esplora / bech32 | mixed case is valid in some encodings          | lowercase                                              |

Canonicalization happens once, at registration, and the stored form is constrained rather than
merely conventional. The EVM and bech32 cases are cheap to enforce directly:

``` sql
CHECK (network_type <> 'evm'     OR address = lower(address)),
CHECK (network_type <> 'esplora' OR address = lower(address))
```

Solana cannot be normalised this way, so its guard is validation at registration — reject
anything that is not a well-formed 32-byte base58 pubkey — rather than a transformation.

**`chain_ref` canonicalization.** This is the more dangerous half, because `chain_ref` is part
of `assets_identity` and *nothing in the schema can catch a disagreement*. Two handlers
registering the same USDC contract under `'base'` and `'8453'` produce two asset rows, two
`custody_treasury` balances for one real balance, and exactly the phantom shortfall this
section exists to prevent.

The rule: **`chain_ref` is whatever the network client's `chain_ref()` method returns, and
nothing else in the system may construct one.** Invoice creation already calls it rather than
hardcoding a cluster name, for the same reason and with the same failure mode if it didn't.

| Network family | Canonical `chain_ref`                                       | Source                                                          |
|----------------|-------------------------------------------------------------|-----------------------------------------------------------------|
| `evm`          | decimal chain id as text — `'1'`, `'8453'`, `'137'`         | what `eth_chainId` returns; there is no naming argument to have |
| `solana`       | cluster moniker — `'mainnet-beta'`, `'testnet'`, `'devnet'` | `SolanaNetwork::chain_ref()`                                    |
| `esplora`      | network name — `'bitcoin'`, `'testnet'`, `'signet'`         | network params                                                  |

Worked examples in §7 use `base` for readability. The stored value is `8453`.

The failure this prevents is quiet and slow: two ledger assets for one real asset, each holding
part of a merchant's balance, surfacing months later as a phantom shortfall that reconciliation
cannot source.

### Decimals and symbol belong to the asset, not the handler

Two handlers advertising the same address while disagreeing about decimals is a silent error of
up to twelve orders of magnitude, and it is unrecoverable once entries have been written
against both. Decimals and symbol are therefore columns on `assets`, written at first
registration, and any later registration that disagrees is a **hard failure at boot** (§2.4) —
not a warning, not last-write-wins.

This is the one place where the registry's usual "the DB is authoritative, operator edits win"
posture is inverted. An operator repointing a token to a different checkout view is a
preference. An operator changing an asset's decimals is a corruption. A trigger enforces the
same rule against everything that is not boot: `network_type`, `chain_ref`, `asset_kind`,
`address` and `decimals` are immutable once a row exists.

### Consequences worth stating

- Multiple routes may serve one asset without fragmenting the ledger. `USDC_BASE` and
  `USDC_BASE_CRATES` accrue into the same `custody_treasury` balance, because they are the
  same money.
- Fee rates remain per route (§6.2) while fee *receivables* are per asset. Those are not in
  conflict — see that section.
- An asset can exist with no handler at all. Airdrops and dust arrive as assets nobody
  registered, and the system records them in `custody_unsupported` (§3.1) rather than dropping
  them because no code claims them.
- Withdrawals resolve in the opposite direction from payments: given an asset, the registry is
  queried for handlers advertising it, filtered by capability (§2.5), and operator
  configuration decides whether one is chosen automatically or the choice is surfaced.

---

## 2. Schema

Implemented as `migrations/010_ledger.sql`. Eight tables, six trigger groups, four views.
Requires PostgreSQL 15+ for `UNIQUE NULLS NOT DISTINCT`; on older versions the same effect
comes from a unique index over `COALESCE(address, '')`, which is uglier but equivalent.

### 2.1 Asset registry

``` sql
CREATE TABLE assets (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),

    network_type  VARCHAR(20)  NOT NULL,        -- 'evm' | 'solana' | 'esplora'
    chain_ref     VARCHAR(50)  NOT NULL,        -- canonical per §1.1
    asset_kind    VARCHAR(20)  NOT NULL,        -- 'native' | 'contract'
    address       VARCHAR(255),                 -- canonical; NULL iff asset_kind = 'native'

    decimals      SMALLINT     NOT NULL CHECK (decimals >= 0 AND decimals <= 36),
    symbol        VARCHAR(32)  NOT NULL,

    -- Chain-specific facts that are properties of the asset itself, not of any
    -- route: Solana token_program, Tron trc-kind, and whatever the next network
    -- needs. Keeps this table from growing a column per chain.
    asset_params  JSONB        NOT NULL DEFAULT '{}'::jsonb,

    registered    BOOLEAN      NOT NULL DEFAULT false,  -- some handler advertises it
    first_seen_at TIMESTAMPTZ  NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ  NOT NULL DEFAULT now(),

    CHECK ((asset_kind = 'native') = (address IS NULL)),
    CHECK (network_type <> 'evm'     OR address = lower(address)),
    CHECK (network_type <> 'esplora' OR address = lower(address)),

    CONSTRAINT assets_identity
        UNIQUE NULLS NOT DISTINCT (network_type, chain_ref, asset_kind, address),

    -- Referenced by ledger_accounts, so that "gas accounts hold native assets
    -- only" is a declarative FK plus CHECK rather than a trigger.
    CONSTRAINT assets_id_kind_uq UNIQUE (id, asset_kind)
);
```

`registered` separates "an asset the system can move" from "an asset the system has merely
seen". It is set when a handler advertises the asset at boot and cleared when none does. It is
not a deletion: an asset that once had a handler and no longer does keeps every entry ever
written against it, and simply stops being withdrawable.

`decimals` and `symbol` are presentation metadata. Nothing below the presentation layer reads
them, and no ledger arithmetic involves them.

`asset_params` is per-network by design and expected to grow. Solana stores `token_program`
there, because the program is what decides the ATA and is therefore a fact about the mint
rather than about any route that moves it.

### 2.2 Chain layer

``` sql
CREATE TABLE chain_transactions (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),

    network_type  VARCHAR(20)  NOT NULL,
    chain_ref     VARCHAR(50)  NOT NULL,
    tx_hash       VARCHAR(255) NOT NULL,

    intent        VARCHAR(30)  NOT NULL,
        -- inbound | sweep | withdrawal | gas_refill | gas_advance
        -- | fee_settlement | conversion | external
    merchant_id   UUID REFERENCES merchants(id),   -- NULL only for operator-internal txs

    -- Which route broadcast this, when the system broadcast it at all.
    -- NULL for anything observed rather than initiated (intent = 'external').
    -- Deliberately NOT a foreign key: handlers live in code, not in the DB.
    token_id      VARCHAR(100),

    -- Gas / network fee. Its own asset, because on most chains it is not the asset moved.
    fee_asset_id  UUID REFERENCES assets(id),
    fee_paid      NUMERIC(78,0),
    fee_payer     VARCHAR(255),

    block_number  BIGINT,
    block_hash    VARCHAR(255),
    block_time    TIMESTAMPTZ,   -- the economic event time; see below
    status        VARCHAR(20) NOT NULL,
        -- submitted | detected | confirmed | final | orphaned | failed

    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now(),

    UNIQUE (network_type, chain_ref, tx_hash),
    CHECK ((fee_asset_id IS NULL) = (fee_paid IS NULL)),
    CONSTRAINT chain_transactions_id_chain_uq UNIQUE (id, network_type, chain_ref)
);
```

`tx_hash` alone is not unique — the same hash can legitimately exist on mainnet and on a
testnet, and `chain_ref` is what separates them. The unique constraint is the idempotency latch
for the chain layer, the same role the `(invoice_id, tx_hash)` index plays for `payments`.

`token_id` is a plain string with no foreign key on purpose. A handler removed from the codebase
leaves a historical string behind — which is exactly the intended behaviour, and exactly why
the ledger does not reference it.

`block_time` exists because `ledger_journals.occurred_at` has to come from somewhere and it is
not `created_at`. It is nullable: Solana's `blockTime` can be absent on pruned transactions, and
recognition must not block on a missing timestamp. When it is absent, recognition falls back to
`now()` and records `{"occurred_at_exact": false}` in the journal metadata so reports can say so
rather than silently treating an approximation as a fact.

``` sql
CREATE TABLE chain_movements (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),

    tx_id         UUID        NOT NULL,
    network_type  VARCHAR(20) NOT NULL,
    chain_ref     VARCHAR(50) NOT NULL,

    event_index   INT         NOT NULL CHECK (event_index >= 0),   -- see §2.3
    event_ref     VARCHAR(100),                                    -- native identifier, verbatim

    merchant_id   UUID REFERENCES merchants(id),
    invoice_id    UUID REFERENCES invoices(id),
    payment_id    UUID REFERENCES payments(id),   -- nullable: only invoice-matched inbound has one

    asset_id      UUID NOT NULL REFERENCES assets(id),   -- what moved
    token_id      VARCHAR(100),                          -- which route moved it, if any
    amount        NUMERIC(78,0) NOT NULL CHECK (amount > 0),

    from_address  VARCHAR(255),
    from_kind     VARCHAR(20),
    to_address    VARCHAR(255),
    to_kind       VARCHAR(20),
        -- external | deposit_address | vault | merchant_main | gas | operator

    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),

    UNIQUE (tx_id, event_index),
    UNIQUE (tx_id, event_ref),

    -- A movement cannot be attached to a transaction on a different chain.
    FOREIGN KEY (tx_id, network_type, chain_ref)
        REFERENCES chain_transactions (id, network_type, chain_ref),

    -- Referenced by sweep_queue, so a queue row cannot drift from its movement.
    CONSTRAINT chain_movements_id_asset_uq UNIQUE (id, asset_id, merchant_id)
);
```

`amount` is always positive; direction is carried by `from_*` / `to_*`. The `*_kind` columns
are denormalised classifications of the addresses — derivable by joining against `invoices`,
`merchant_wallets` and config, but that join is expensive and appears in almost every dashboard
query, so they are stored. They are also what makes "where are my unswept funds" answerable
without knowing anything about chains (§11.2).

`to_kind` classifies the **address**, not the intent. That distinction matters once EVM sweeping
exists (§8.3): native token sent to a deposit address in order to pay for a sweep is still
arriving at a deposit address, and tagging it `gas` to keep it out of `custody_unswept` would
hide value that is genuinely stranded there.

`payment_id` is null for every movement reconciliation discovers, since those correspond to no
invoice. `token_id` is null for movements the system observed rather than initiated, since no
route was involved.

Amounts are base units, consistent with the rest of the system.

Movements are append-only, enforced by trigger. A reorg sets the parent transaction's `status`
to `orphaned` and, if a journal exists, produces a reversal; it never deletes a movement. What
was observed was observed.

### 2.3 Movement identity: `event_index` is a connector-assigned ordinal

`(tx_id, event_index)` is the identity of a movement, which means it must be **stable** —
re-processing the same RPC response must produce the same ordinal — and **unique within the
transaction**. It does *not* have to be meaningful, and on at least one network it cannot be.

Each connector defines its own derivation and documents it here. `event_ref` stores the native
identifier verbatim alongside it, so the ordinal is always re-derivable and always debuggable.

| Network   | `event_index`                  | `event_ref`                       | Note                                        |
|-----------|--------------------------------|-----------------------------------|---------------------------------------------|
| `evm`     | `log_index`                    | `"log:{n}"`                       | already unique and stable per receipt       |
| `esplora` | `vout`                         | `"vout:{n}"`                      | outputs only; inputs are the counterparty's |
| `solana`  | `account_index * 2 + is_token` | `"tokbal:{n}"` / `"lamports:{n}"` | see below                                   |

**Why Solana is not the instruction index.** Inner instructions are numbered independently of
top-level ones, so instruction index 3 can name two different events in one transaction. More
to the point, the Solana watcher does not detect credits by parsing instructions at all — it
detects them from `preTokenBalances` / `postTokenBalances` and lamport deltas. The thing it
actually observes is *an account whose balance changed*, and the index of that account in the
message's `accountKeys` array is unique within the transaction and stable across replays.

The `* 2 + is_token` encoding exists because one account index could in principle carry both a
lamport delta and a token-balance delta in the same transaction — ATA creation is the case where
it happens. Two slots per account costs nothing and removes the question.

An account index is not an instruction index and does not pretend to be. The column is an
identity, not a location; `event_ref` is where the location lives.

### 2.4 Registration at boot

The token registry already builds itself in code at startup and then syncs what it needs into
the database idempotently — that is the pattern `sync_checkout_views` establishes. Asset
registration is the same pattern with one inverted rule.

The handler trait grows one required method:

``` rust
/// The asset this route moves. Stable for the life of the handler:
/// changing it is changing what the route *is*, which makes it a new route.
fn asset(&self) -> AssetRef;

pub struct AssetRef {
    pub network_type: &'static str,
    pub chain_ref:    String,          // from NetworkClient::chain_ref(), never a literal
    pub kind:         AssetKind,       // Native | Contract
    pub address:      Option<String>,  // canonicalized by the constructor
    pub decimals:     u8,
    pub symbol:       String,
    pub params:       serde_json::Value,  // token_program, etc.
}
```

and the registry grows a sync that mirrors `sync_checkout_views`, except that a conflict is
fatal rather than logged:

``` rust
/// Seeds the asset registry from the registered handlers.
/// Idempotent on identity, STRICT on metadata: two handlers advertising the
/// same asset with different decimals is a startup failure, not a warning.
/// The alternative is a silent 10^n error that cannot be unwound once entries
/// have been written against it.
pub async fn sync_assets(&self, pool: &PgPool) -> Result<(), RegistryError> { … }
```

Boot output follows the existing shape, and the divergence pass at the end mirrors the one
`sync_checkout_views` already does for views:

```
🪙 Registering Token Handlers...
  ✅ USDC_BASE - USD Coin - Base - EvmErc20Handler
  ✅ USDC_BASE_CRATES - USD Coin - Base - AlloyErc20Handler

🏷️  Syncing assets...
  ✅ evm/8453/contract/0x833589…2913  USDC (6)  ← USDC_BASE, USDC_BASE_CRATES
  ✅ evm/8453/native                  ETH  (18) ← ETH_BASE
  ⚠️  evm/8453/contract/0xa0b869…eb48  USDT (6)  no handler registered — observed only
```

The third line is the counterpart of `sync_checkout_views`' "mapped but no handler registered"
warning: an asset with entries and no route. It is the normal state after a handler is removed,
and the normal state for anything reconciliation discovered (§3.1).

### 2.5 Handler capabilities

A second trait method, defaulted so existing handlers compile unchanged:

``` rust
fn capabilities(&self) -> HandlerCapabilities { HandlerCapabilities::OBSERVE_ONLY }

pub struct HandlerCapabilities {
    pub can_observe:      bool,
    pub can_sweep:        bool,
    pub can_withdraw:     bool,
    pub can_estimate_fee: bool,
}
```

Capabilities are what make multiple routes per asset safe to expose. A read-only or test
handler never appears in a withdrawal picker; an operator sets a default route per asset; and
the merchant-facing choice between two routes for the same token becomes an opt-in advanced
feature rather than a merchant being asked to choose between "USDC" and "USDC_Crates" with no
basis for deciding. Defaulting to `OBSERVE_ONLY` means a new handler has to opt in to moving
money.

`can_sweep` is also what resolves a `sweep_queue` row whose `token_id` is `NULL`.

### 2.6 Ledger layer

``` sql
CREATE TABLE ledger_accounts (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    merchant_id UUID REFERENCES merchants(id),   -- NULL for operator / system accounts
    kind        VARCHAR(40) NOT NULL,            -- see §3

    asset_id    UUID        NOT NULL,
    -- Denormalized, but held true by the composite FK below, so it cannot drift.
    -- Present so that "gas accounts are native-only" is declarative.
    asset_kind  VARCHAR(20) NOT NULL,

    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),

    UNIQUE NULLS NOT DISTINCT (merchant_id, kind, asset_id),
    FOREIGN KEY (asset_id, asset_kind) REFERENCES assets (id, asset_kind),

    -- Merchant-scoped kinds must have a merchant; operator kinds must not.
    CONSTRAINT ledger_accounts_scope CHECK ( … ),

    -- Gas is denominated in the chain's native asset, always.
    CONSTRAINT ledger_accounts_gas_is_native
        CHECK (kind NOT IN ('custody_gas', 'gas_advance_receivable')
               OR asset_kind = 'native'),

    CONSTRAINT ledger_accounts_id_asset_uq UNIQUE (id, asset_id)
);
```

Accounts are `(merchant, kind, asset)`. One per asset, never a mixed-asset account — summing
across assets requires a price, and prices do not belong in the ledger.

``` sql
CREATE TABLE ledger_journals (
    id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    kind         VARCHAR(40)  NOT NULL,
    dedupe_key   VARCHAR(255) NOT NULL UNIQUE,

    merchant_id  UUID REFERENCES merchants(id),
    tx_id        UUID REFERENCES chain_transactions(id),
    payment_id   UUID REFERENCES payments(id),

    reverses     UUID REFERENCES ledger_journals(id),
    metadata     JSONB,        -- fee rate snapshot, route used, policy version, oracle quote, …

    occurred_at  TIMESTAMPTZ NOT NULL,                 -- when the underlying event happened
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()    -- when we recorded it
);

-- A journal can be reversed exactly once.
CREATE UNIQUE INDEX ledger_journals_reverses_uq
    ON ledger_journals (reverses) WHERE reverses IS NOT NULL;

CREATE TABLE ledger_entries (
    entry_no    BIGSERIAL PRIMARY KEY,
    journal_id  UUID          NOT NULL REFERENCES ledger_journals(id),
    account_id  UUID          NOT NULL,
    asset_id    UUID          NOT NULL,
    amount      NUMERIC(78,0) NOT NULL CHECK (amount <> 0),  -- signed: + debit, − credit

    FOREIGN KEY (account_id, asset_id) REFERENCES ledger_accounts (id, asset_id)
);
```

`ledger_entries.asset_id` duplicates `ledger_accounts.asset_id` deliberately: it is what the
balance constraint groups by, and having it on the entry means the constraint needs no join.
The composite foreign key — rather than a trigger — is what keeps the two in agreement.

`amount <> 0` because a zero leg passes the balance constraint while recording nothing, which
makes it a bug that is invisible by construction.

### 2.7 Sweep queue

``` sql
CREATE TABLE sweep_queue (
    id                UUID PRIMARY KEY DEFAULT gen_random_uuid(),

    movement_id       UUID          NOT NULL,
    merchant_id       UUID          NOT NULL REFERENCES merchants(id),
    asset_id          UUID          NOT NULL REFERENCES assets(id),
    amount            NUMERIC(78,0) NOT NULL CHECK (amount > 0),

    network_type      VARCHAR(20)   NOT NULL,   -- denormalized for the claim query
    chain_ref         VARCHAR(50)   NOT NULL,

    custody_address   VARCHAR(255)  NOT NULL,   -- where the value is
    custody_kind      VARCHAR(20)   NOT NULL,   -- 'deposit_address' | 'vault'
    authority_address VARCHAR(255)  NOT NULL,   -- what can sign for it
    authority_ref     VARCHAR(100),             -- derivation index, seed, vault slot

    token_id          VARCHAR(100),             -- a pin, not an identity
    sweep_params      JSONB         NOT NULL DEFAULT '{}'::jsonb,

    status            VARCHAR(20)   NOT NULL DEFAULT 'pending',
        -- pending | claimed | broadcast | swept | failed | abandoned
    available_at      TIMESTAMPTZ   NOT NULL DEFAULT now(),

    claim_id          UUID,
    claim_expires_at  TIMESTAMPTZ,
    attempts          INT           NOT NULL DEFAULT 0,
    last_error        TEXT,
    sweep_tx_id       UUID REFERENCES chain_transactions(id),

    created_at        TIMESTAMPTZ   NOT NULL DEFAULT now(),
    updated_at        TIMESTAMPTZ   NOT NULL DEFAULT now(),

    CONSTRAINT sweep_queue_movement_uq UNIQUE (movement_id),
    FOREIGN KEY (movement_id, asset_id, merchant_id)
        REFERENCES chain_movements (id, asset_id, merchant_id)
);

CREATE INDEX sweep_queue_claimable_idx
    ON sweep_queue (network_type, chain_ref, merchant_id, asset_id,
                    available_at, custody_address)
    WHERE status = 'pending';
```

**Per movement, not per address.** The sweeper groups claimable rows by `custody_address`,
broadcasts one batched call, and stamps every row in the batch with the same `sweep_tx_id`.
Per-address rows carrying an accumulating amount drift the moment anything fails mid-flight;
per-movement rows cannot, and `UNIQUE (movement_id)` is the enqueue idempotency latch.

**`merchant_id` leads the claim index** because a sweep batch cannot cross merchants: gas is one
wallet per merchant by design (§6.4), so the fee payer — and on EVM the sender — is
merchant-scoped, and a cross-merchant batch would have one merchant paying another's gas.

**`available_at` absorbs three things in one column**: gas above the configured threshold,
amount below the dust floor, and retry backoff. All three are "not yet, try after this time".

**The composite foreign key** to `(movement_id, asset_id, merchant_id)` means a queue row cannot
disagree with its movement about what moved or whose it is.

Rows are enqueued at recognition (§5.2), never at detection — a queue row for value that has not
finalized is work that might have to be undone. Enqueue is skipped entirely for the
reference/WalletConnect path, because that value never sat at a deposit address.

This is the only mutable table in the schema, and it is deliberately outside the audit trail.
Losing it entirely costs a rebuild from `chain_movements` plus `custody_unswept`, not a loss of
history.

### 2.8 Custody addressing: where value sits vs. who can move it

`custody_address` and `authority_address` are separate columns. On EVM HD they hold the same
string and the split looks like overhead. It is not.

| Network / path      | `custody_address`  | `authority_address`       | `authority_ref`   |
|---------------------|--------------------|---------------------------|-------------------|
| Solana, native      | owner pubkey       | same                      | derivation index  |
| Solana, SPL         | the **ATA**        | the ATA's **owner**       | derivation index  |
| EVM, HD deposit     | HD address         | same                      | derivation index  |
| EVM, contract vault | vault contract     | vault's configured signer | vault slot / salt |
| Esplora, HD deposit | HD address         | same                      | derivation index  |

Solana SPL is the row that breaks a collapsed design. The value sits in an account the system
never derived directly and holds no key for; the key that can move it belongs to the owner.

**Where each value lives on `invoices`, for Solana:**

| Column                       | Holds                                     | Native SOL                  | SPL / Token-2022    |
|------------------------------|-------------------------------------------|-----------------------------|---------------------|
| `invoices.wallet_address`    | the deposit address — where value lands   | the HD-derived owner pubkey | the **ATA**         |
| `invoices.payment_reference` | the HD-derived **owner** pubkey           | same as `wallet_address`    | the ATA's owner     |
| `invoices.wallet_index`      | derivation index                          | —                           | —                   |

`get_derive_address` returns `(deposit_address, index, Some(owner_address))`, and
`create_invoice_payment` writes the third element to `payment_reference`. For native SOL the
first and third are the same string, because `deposit_address` is `owner_address.clone()`.

**Why both exist, and why the payer-facing value is the owner rather than the deposit address.**
A wallet handed a destination address for an SPL transfer derives the ATA *from* that address.
Hand it the ATA and it derives the ATA of an ATA — a valid-looking address nobody controls and
no payment will ever be found at. So the frontend shows the owner pubkey; the ATA is what the
watcher matches against and what the sweeper drains. The Solana Pay reference key happens to
want the same value as the ATA owner, which is why one column serves both purposes.

The derivation is non-obvious enough that it should not be re-derived at each read site:

``` rust
/// The sweep authority for an invoice.
///
/// NOT `wallet_address` — for an SPL invoice that is the ATA, which cannot sign.
/// `payment_reference` holds the HD-derived owner, which is both the ATA owner
/// and the Solana Pay reference key. For native SOL the two columns hold the
/// same string; the fallback also covers networks that leave `payment_reference`
/// NULL, where an address is its own authority.
fn sweep_authority<'a>(wallet_address: &'a str, payment_reference: Option<&'a str>) -> &'a str {
    payment_reference.unwrap_or(wallet_address)
}
```

`sweep_params` carries the rest — `mint`, `token_program`, `owner`, `close_ata_after_sweep`,
`external_fee_payer`, UTXO outpoints, vault call selectors — so this table does not grow a
column per chain.

### 2.9 What the constraints actually enforce

Five checks carry most of the correctness weight. Four are declarative; two need deferred
constraint triggers because they are statements about a *set* of rows.

**`dedupe_key` uniqueness.** Same latch pattern as `webhook_events`. Keys look like
`payment_recognized:<payment_id>`, `sweep:<tx_id>`, `fee_settlement:<tx_id>`,
`external_credit:<tx_id>:<event_index>`, `reversal:<original_journal_id>`. A worker that runs
twice writes one journal. This is what makes the recognition pass safely retryable.

**Balance per asset, not per journal.** A deferred constraint trigger asserting
`SUM(amount) = 0` **grouped by `(journal_id, asset_id)`**. Per-asset is essential: a sweep
journal carries USDC legs and ETH gas legs together, and those two are not commensurable.
Requiring the whole journal to sum to zero would force a conversion that never happened.

Note what per-asset grouping also buys, which per-`token_id` grouping did not: a payment
received through `USDC_BASE` and swept through `USDC_BASE_CRATES` produces legs that cancel,
because both resolve to the same asset. Under route-keyed accounts they would not have.

**No empty journals.** The balance trigger fires on entries, so a journal with zero entries
passes it vacuously — while holding a `dedupe_key`, which means the work it represents can never
be retried, and recording nothing. A second deferred trigger asserts at least one entry exists.

**Reversals reverse exactly.** `kind = 'reversal'` if and only if `reverses IS NOT NULL`, and the
legs of a reversal must be the exact negation of the original grouped by `(account_id, asset_id)`.
A "reversal" that does not cancel is a second, wrong journal wearing the word.

**Append-only, including `TRUNCATE`.** `ledger_journals`, `ledger_entries` and `chain_movements`
reject `UPDATE` and `DELETE` by row trigger and `TRUNCATE` by statement trigger. The second is
not paranoia: a row trigger does not fire on `TRUNCATE`, and `TRUNCATE` is exactly how an audit
trail gets lost.

`occurred_at` and `created_at` are deliberately distinct. Recognition happens when the worker
gets to it; the economic event happened at the block. Reports use `occurred_at`, operational
debugging `created_at`.

---

## 3. Chart of accounts

| Kind                     | Type      | Scope                   | Meaning                                                                      |
|--------------------------|-----------|-------------------------|------------------------------------------------------------------------------|
| `custody_unswept`        | asset     | merchant + asset        | Recognized funds still at a deposit address or in the vault                  |
| `custody_treasury`       | asset     | merchant + asset        | Funds in the merchant's main wallet                                          |
| `custody_gas`            | asset     | merchant + native asset | The merchant's gas account for that network/chain                            |
| `custody_unsupported`    | asset     | merchant + asset        | Value held in an asset with no registered handler — real, owned, not movable |
| `custody_operator`       | asset     | operator + asset        | Where settled fees land                                                      |
| `payable_to_merchant`    | liability | merchant + asset        | What the merchant can withdraw                                               |
| `fees_receivable`        | asset     | merchant + asset        | Fees accrued and not yet settled                                             |
| `gas_advance_receivable` | asset     | merchant + native asset | Native token the operator advanced to bootstrap a gas account (§6.5)         |
| `fee_revenue`            | revenue   | operator + asset        | Recognized operator revenue                                                  |
| `suspense_unexplained`   | suspense  | merchant + asset        | Balance-probe deltas with no movement to source them. Expected zero (§11.4)  |

Sign convention: **positive = debit, negative = credit.** Assets carry a positive balance,
liabilities and revenue a negative one. `payable_to_merchant` reading `-150` means the
merchant is owed 150.

The scope column is enforced, not documented: `ledger_accounts_scope` requires a `merchant_id`
for every merchant-scoped kind and forbids one for `custody_operator` and `fee_revenue`. There
is deliberately **no operator-scoped `custody_gas`** — adding one would create the shared gas
pool the isolation model rejects (§6.4).

The custody accounts mirror `chain_movements.to_kind` deliberately — `custody_unswept` tracks
value at `deposit_address`/`vault`, `custody_treasury` at `merchant_main`. That correspondence
is what makes the check in §11.3 meaningful. It is a correspondence, not an equality; the gap
between them is itself a useful number.

There is no `custody_segregated` account. Isolation is a property of the address, and therefore
lives in the chain layer, not in a parallel set of ledger accounts.

### 3.1 On `custody_unsupported`

Value can arrive in an asset the system has no route for: an airdrop, a spam token, a transfer
in a token the operator never registered. That value is real and it is the merchant's. It is
also unmovable — no handler means no sweep, no withdrawal, and no way to price it.

Recording it in `custody_unsupported` with the matching `payable_to_merchant` credit says
exactly that: owed, not withdrawable. The withdrawable query (§11.1) filters to assets with a
`can_withdraw` handler, so the balance appears without ever being offered.

**These assets are informational.** They exist so a merchant can see what arrived, and for no
other purpose. `ensure_observed_asset` writes them with `decimals = 0` and `symbol = 'UNKNOWN'`.
Zero decimals is not a guess to be corrected later; it is a refusal to guess. Base units are
base units, and the immutability trigger (§1.1) means a wrong non-zero value written from an RPC
metadata call could never be fixed. The frontend is free to resolve symbols and decimals for
display against whatever token list it likes — that is a presentation concern and explicitly
outside the core system, which needs to know only that the value exists and whose it is.

If a handler is registered later, a `reclassification` journal moves the balance to
`custody_treasury`, and nothing about the merchant's claim changed in the interim.

### 3.2 On `suspense_unexplained`

This is the only account in the book that exists in order to be empty.

An earlier draft of `RECONCILIATION.md` proposed a pair of external accounts,
`external:attributed` and `external:unexplained`. The first turns out to be unnecessary and is
dropped here: when an external event is attributed, ownership of the value is known, so the
ordinary accounts already balance — an inbound external credit debits `custody_treasury` and
credits `payable_to_merchant`, with nothing left over. What marks the event as external is the
*journal kind*, not a special account.

The unexplained case is different and does need a real account, because the point is precisely
**not** to grant the merchant a claim on value the system cannot source. A positive probe delta
debits the custody account and credits `suspense_unexplained`; a negative one does the reverse,
leaving `suspense_unexplained` with a positive balance that reads as "the book is short and
nobody can say why". Either direction is a monitoring alarm rather than an accounting entry to
shrug at, and the account is expected to sit at zero forever.

---

## 4. Journal kinds

| Kind                 | Trigger                                           | `dedupe_key`                               |
|----------------------|---------------------------------------------------|--------------------------------------------|
| `payment_recognized` | payment reaches `system_confirmed`                | `payment_recognized:<payment_id>`          |
| `sweep`              | sweep tx reaches `final`                          | `sweep:<tx_id>`                            |
| `withdrawal`         | withdrawal tx reaches `final`                     | `withdrawal:<tx_id>`                       |
| `gas_refill`         | merchant-funded refill reaches `final`            | `gas_refill:<tx_id>`                       |
| `gas_advance`        | operator-funded refill reaches `final` (§6.5)     | `gas_advance:<tx_id>`                      |
| `gas_burn_failed`    | tx reaches `failed` with non-zero `fee_paid`      | `gas_burn:<tx_id>`                         |
| `conversion`         | conversion tx reaches `final`                     | `conversion:<tx_id>`                       |
| `fee_settlement`     | settlement tx reaches `final`                     | `fee_settlement:<tx_id>`                   |
| `external_credit`    | observed inbound not matching a live invoice      | `external_credit:<tx_id>:<event_index>`    |
| `external_debit`     | observed outbound the system did not initiate     | `external_debit:<tx_id>:<event_index>`     |
| `probe_adjustment`   | balance-probe delta with no movement to source it | `probe_adjustment:<probe_id>`              |
| `reclassification`   | an unsupported asset gains a handler              | `reclassification:<account_id>:<asset_id>` |
| `reversal`           | a recognized tx is orphaned                       | `reversal:<original_journal_id>`           |

The middle four are the reconciliation surface. They are ordinary journals in every respect —
same latch, same balance constraint, same append-only discipline — and nothing downstream
distinguishes them when deriving a balance. `RECONCILIATION.md` describes when each fires.

`reversal` is reachable only on networks with probabilistic finality (§5.1). On Solana it is
unreachable for inbound payments by construction, and a Solana reversal attempt should raise
rather than proceed.

`gas_burn_failed` has no Solana *inbound* trigger either — a payer's failed transaction burns
the payer's fee, not the merchant's. On Solana it fires only for transactions the system itself
broadcast: failed sweeps, withdrawals and fee settlements.

Every one of these is written in the same DB transaction as the state change that justifies it,
behind a latch that can only succeed once — the same discipline `NETWORKS.md` requires of
webhooks. §5.3 says what that means concretely for the code that exists today.

---

## 5. Recognition: when value enters the ledger

**A payment enters the ledger when it reaches `system_confirmed`, not before.**

`required_confirmations` (the merchant's threshold) drives *webhooks*. The system's finality
threshold drives *the ledger*. They are different numbers answering different questions: "can I
ship the product?" versus "may I treat this as irreversible money?"

The same gate applies to reconciliation-discovered value. An `external_credit` is written when
its transaction reaches `final`, not when it is first observed, for exactly the reason that
governs payments: a reorg that unwinds an inbound transfer should never have produced a ledger
entry in the first place. Reconciliation changes *what* enters the ledger, never *when*.

### 5.1 The gate is per network, and its strength varies

What "finality" is worth differs by network, and the difference determines whether `reversal` is
a live code path or a dead one.

| Network   | Recognition fires at                                                            | Can a recognized journal be reversed?          |
|-----------|---------------------------------------------------------------------------------|------------------------------------------------|
| `solana`  | `system_confirmed`, which the watcher sets only on `ConfirmLevel::Finalized`    | **No.** A rooted slot cannot be undone.        |
| `evm`     | `system_confirmed` after N confirmations                                        | **Yes.** N confirmations is a probability.     |
| `esplora` | as EVM                                                                          | Yes.                                           |

On Solana this means `orphan_payment` can only fire against a payment that has not yet
finalized, and an unfinalized payment has no journal to reverse. That is a property worth
asserting rather than assuming: if a Solana orphan is ever observed against a payment that
already has a `payment_recognized` journal, either the confirmation ladder is wrong or an RPC
lied, and both deserve an alarm rather than a silent reversal.

The chain layer still moves on every network: `orphan_payment` sets
`chain_transactions.status = 'orphaned'` regardless. Movements are append-only and are not
deleted.

### 5.2 Write points

The detection pipeline already has the transaction boundaries the ledger needs. Nothing here
adds a new worker; each write goes into a transaction that already exists, behind a latch that
already exists.

| Detection event                | Chain layer                                                                                     | Sweep queue                                         | Ledger                        |
|--------------------------------|-------------------------------------------------------------------------------------------------|-----------------------------------------------------|-------------------------------|
| payment inserted (`detected`)  | upsert `chain_transactions` (`status='detected'`, `intent='inbound'`), insert `chain_movements` | —                                                   | —                             |
| payment → `merchant_confirmed` | `status='confirmed'`                                                                            | —                                                   | —                             |
| payment → `system_confirmed`   | `status='final'`, stamp `block_time`                                                            | enqueue one row per movement — **direct path only** | `payment_recognized`          |
| `orphan_payment`               | `status='orphaned'`                                                                             | delete `pending` rows for the movement              | reversal, if a journal exists |
| invoice totals recomputed      | —                                                                                               | —                                                   | —                             |
| invoice expired                | —                                                                                               | —                                                   | —                             |

The last two rows are deliberate. An invoice's status is a **commercial** fact — did the
customer pay what they said they would — and has no custody meaning. An expired invoice that
received 40 of 50 USDC still holds 40 USDC of the merchant's money, and that 40 was recognized
when it finalized regardless of what the invoice status says afterwards. Deriving custody from
invoice status would lose it.

Deleting `pending` queue rows on orphan is correct because `sweep_queue` is a work queue rather
than audit trail (§2.7) — but only while still `pending`. A row already `claimed` or `broadcast`
means a sweep is in flight against value that just vanished, which is not something to clean up
quietly.

### 5.3 The two payment paths book differently

Solana's `payment_path` is not a detail for the ledger to ignore, and the equivalent split
exists on every network that offers a WalletConnect-style direct-to-treasury option:

- **`direct`** — the payer sent to the invoice's deposit address (the ATA, or the owner pubkey
  for native SOL). Movement `to_kind = 'deposit_address'`. The journal debits `custody_unswept`,
  and a sweep row is enqueued.
- **`reference`** — the payer sent to the merchant's main wallet carrying the reference key.
  Movement `to_kind = 'merchant_main'`. The journal debits `custody_treasury` directly and **no
  sweep row is enqueued**, because the value is already where sweeping would put it.

This is the whole economic point of the WalletConnect path, and the ledger should show it: a
reference-path payment never appears in the sweep backlog. Everything else about the two
journals is identical, including fee accrual.

### 5.4 Atomicity is a requirement, not a preference

Every journal is written in the same database transaction as the state change that justifies it.
For `payment_recognized` that means the guarded `UPDATE payments SET status = 'system_confirmed'`
and the journal insert are **one transaction**, not two.

The detection code as originally written runs that `UPDATE` against the pool and then opens a
separate transaction for the webhook. For webhooks that is survivable — the enqueue is
idempotent and a missed one is a missed notification. For the ledger it is not: a crash between
the two leaves a payment permanently at `system_confirmed` with no journal, and the guarded
`UPDATE` that would have noticed can never fire again. The `UPDATE` has to move inside the
transaction.

### 5.5 What the merchant sees

Recognition produces three states, and they should be labelled distinctly in the dashboard:

| Dashboard label       | Source                                                         | Meaning                        |
|-----------------------|----------------------------------------------------------------|--------------------------------|
| In progress           | `payments` where `status IN ('detected','merchant_confirmed')` | Seen on chain, not yet final   |
| Confirmed / available | `payable_to_merchant` balance                                  | In the ledger, withdrawable    |
| Unswept               | `chain_movements` position at deposit addresses (§11.2)        | Physically not yet in treasury |

"Unswept" cuts across the other two: some unswept funds are recognized, some are still in
progress. That is correct, and worth showing as such rather than flattening.

---

## 6. Fees

### 6.1 Accrual, not deduction

Fees are **never** taken out of the swept amount. Sweeping 49.5 and leaving 0.5 behind as the
operator's cut strands dust at an address where the gas to collect it may exceed its value —
the fee is destroyed by the mechanism meant to collect it.

Instead, receiving 50 at a 1% fee produces a *receivable*: the merchant's balance is 50, and
separately they owe 0.5. Coins stay whole; the obligation is tracked in the ledger and settled
in bulk later.

This is not fully collateralised, and that is a deliberate, accepted trade-off. Merchants can
read their own keys (`RECONCILIATION.md` §10), so a determined merchant can drain their wallets
through an external wallet app and skip settlement. The mitigations are commercial rather than
cryptographic: the operator holds the record of what is owed, sets the settlement threshold per
merchant, and can suspend the account. The model is closer to an ad platform's billing
threshold than to escrow — thresholds start low, rise with trust, and non-payment stops
service.

Key export makes that exposure concrete rather than theoretical, and reconciliation is what
makes it *visible*: a merchant who empties their treasury produces an `external_debit`, their
`payable_to_merchant` drops accordingly, and `fees_receivable` stays exactly where it was. The
book then shows an unsecured receivable against a merchant with no balance, which is the honest
picture and the correct trigger for a commercial response.

### 6.2 Rate configuration: rates per route, receivables per asset

Rates are **per `token_id`** — per route, not per asset. `USDC_BASE` and `USDC_BASE_CRATES` may
point at the same contract and still carry different rates, because the route is the unit the
orchestrator, the handlers and the invoices all speak in, and because two routes for one asset
may genuinely cost the operator different amounts to run.

``` sql
CREATE TABLE fee_rates (
    id             UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    merchant_id    UUID REFERENCES merchants(id),   -- NULL = operator default
    token_id       VARCHAR(100) NOT NULL,           -- route, matching TokenRegistry keys
    basis_points   INT NOT NULL CHECK (basis_points >= 0 AND basis_points <= 10000),
    effective_from TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE NULLS NOT DISTINCT (merchant_id, token_id, effective_from)
);
```

Resolution: merchant-specific override, else operator default, taking the latest row with
`effective_from <= payment.occurred_at`. Rates are versioned rather than mutated, so recomputing
a historical accrual gives the historical answer.

**No configured rate resolves to zero, not to a failure.** A missing rate must never stop
recognition — the money arrived either way — so the resolver returns `basis_points = 0` with
`rate_source = "unconfigured"`, which is visible in the journal metadata and auditable later.

**Rates are configured per route; the resulting entries are denominated per asset.** These do
not conflict, because they happen at different moments. At accrual time the payment has a
route, so that route's rate applies. Once accrued, the obligation is money — the merchant owes
USDC, not "USDC via route A" — so it lands in a single `fees_receivable` account for that asset
and settles as one amount.

The consequence to be aware of: `fees_receivable` for an asset merges accruals made at
different rates, and the merged balance cannot be decomposed back into rates by reading it.
That is why the resolved rate and the route are **snapshotted into `ledger_journals.metadata` at
accrual time** — `{"fee_bps": 100, "token_id": "USDC_BASE", "rate_source": "merchant_override"}`.
The journal is the record of what was charged; the config table only says what would be charged
now; the balance says only what is owed.

Rounding is fixed at accrual time and stored, since base-unit percentages rarely divide evenly.
Round half-up on the fee and let the merchant balance absorb the remainder. A fee that rounds to
zero produces **no fee legs at all** rather than two zero legs, because `amount <> 0` rejects
them and a zero leg would record nothing while passing every constraint.

### 6.3 Settlement trigger

Fees accrue continuously into `fees_receivable` and settle in batches. The trigger is
operator-configurable, and the natural expression is a USD threshold rather than a per-asset
one:

> settle when a merchant's total outstanding fees exceed $20

Evaluating that means crossing asset boundaries, which the reaper does through the token
handler:

``` rust
token_handler.get_usd_value(amount, asset_id) -> Option<Decimal>
```

Real handlers consult whatever price source they are configured with; test handlers return
whatever the test demands. This keeps price lookup behind the same trait boundary as everything
else chain-shaped, and keeps prices out of the ledger — a USD figure is only ever used to
*decide whether to act*, never written as a ledger amount.

Pricing is a per-asset question routed through whichever handler serves that asset, which
introduces a wrinkle worth naming: two routes for one asset may return different quotes if they
are configured against different price sources. The operator's default route for an asset is the
one the reaper uses, so that the answer is deterministic. An asset with no registered handler
has no price at all, which is one more reason `custody_unsupported` value never participates in
settlement.

The USD threshold also solves fee-collection economics for free. Settling 400 units of a
memecoin whose total value is $0.30 costs more gas than it collects. The reaper therefore
evaluates in two stages:

1. **Trigger.** Sum `get_usd_value` across all of the merchant's `fees_receivable` balances.
   Below the operator's threshold, do nothing.
2. **Selection.** Once triggered, settle only those assets whose individual USD value clears a
   dust floor. Everything below it stays accrued and rides along to the next settlement.

Fee settlement is itself an on-chain transaction: it produces a `chain_transaction`, its own
`chain_movements`, and a `fee_settlement` journal — and it burns gas from the merchant's gas
account like every other transaction touching their funds.

Other trigger modes (fixed schedule, at-withdrawal, deduct-on-every-payment until cleared) are
the same query with a different predicate. Which combination is offered, and whether "whichever
fires first" is the default, is operator config.

### 6.4 Who pays gas: one wallet per merchant

The merchant does, in every transaction touching their funds: sweeps, conversions, withdrawals,
and fee settlements. Each merchant has **one gas wallet per network**, funded and spent only on
that merchant's behalf. The system is custodial, but funds and costs stay strictly
per-merchant — there is no shared gas pool whose costs would have to be allocated across
tenants. A merchant's accounts are only ever touched by that merchant's activity, plus
settlement of what they owe.

This is the accounting half of the isolation property described in `README.md`. Its value here
is that no journal ever needs an apportionment rule: every gas leg has exactly one merchant to
charge, and no reconciliation ever has to divide a shared balance between tenants.

Gas spent is recorded as a leg in the gas asset, reducing `custody_gas` and reducing
`payable_to_merchant` **in that same asset**. It is never converted into the payment asset.

Two consequences follow that are easy to miss:

**`custody_gas` is merchant-scoped, always.** `ledger_accounts_scope` enforces it, and there is
deliberately no operator-scoped variant. Adding one would create exactly the shared pool this
model rejects.

**A sweep batch cannot cross merchants.** The fee payer is merchant-scoped, so a batch spanning
two merchants would have one paying the other's gas. The sweeper's claim query groups by
`merchant_id` before `custody_address`, and `sweep_queue_claimable_idx` is ordered accordingly.
The batching example in §7.2 is twelve deposit addresses *belonging to one merchant*.

### 6.5 Bootstrapping a gas account

"The merchant pays gas" holds once a merchant has gas. It does not answer where the first native
balance comes from, and a brand-new merchant on a fresh chain has none — which means their first
sweep cannot pay for itself. There are two paths, and the ledger needs both.

**The merchant funds it.** Native token arrives from the merchant's own external wallet.
Reconciliation sees an inbound movement to a gas address matching no invoice and writes an
`external_credit`: debit `custody_gas`, credit `payable_to_merchant`. No obligation is created,
because the merchant funded their own account with their own money.

**The operator advances it.** The operator sends native token to the merchant's gas address to
get them started. This is a loan, not a gift, and the `gas_advance` journal says so in four
legs:

| account                     | asset    | amount              |
|-----------------------------|----------|---------------------|
| `custody_operator[op]`      | ETH@base | `-5000000000000000` |
| `custody_gas[m]`            | ETH@base | `+5000000000000000` |
| `payable_to_merchant[m]`    | ETH@base | `-5000000000000000` |
| `gas_advance_receivable[m]` | ETH@base | `+5000000000000000` |

The merchant gains a gas balance and a liability toward the operator simultaneously. Repayment
settles like a fee, and can ride along with the same reaper, since both are receivables against
the same merchant. Isolation is not broken by this: the advance is attributed to one merchant
from the first entry.

Recording this properly costs four lines and prevents the failure mode where operator subsidies
accumulate as an untracked hole in the operator's own position — invisible precisely because
nothing in the book was ever wrong, only incomplete.

---

## 7. Worked examples

Following one sequence: three payments of 50 USDC on Base at a 1% fee, then a sweep, then a fee
settlement, then a withdrawal, then a merchant self-move; then a Solana SPL sweep, which has a
shape EVM does not. `m` is the merchant, `op` the operator. Assets are written `SYMBOL@chain`;
routes are written as the `token_id` string. `chain_ref` is shown as `base` for readability; the
stored value is `8453` (§1.1).

### 7.1 Inbound payment reaching `system_confirmed`

**`chain_transactions`**

| id     | network | chain_ref | tx_hash  | intent  | merchant | token_id    | fee_asset | fee_paid | status |
|--------|---------|-----------|----------|---------|----------|-------------|-----------|----------|--------|
| `tx-1` | evm     | base      | `0xaaa…` | inbound | m        | `USDC_BASE` | —         | —        | final  |

Gas is null: the payer paid it, not the merchant.

**`chain_movements`**

| tx_id  | idx | payment_id | asset       | token_id    | amount   | from_kind | to_kind         | to_address |
|--------|-----|------------|-------------|-------------|----------|-----------|-----------------|------------|
| `tx-1` | 12  | `pay-1`    | `USDC@base` | `USDC_BASE` | 50000000 | external  | deposit_address | `0xdep1…`  |

Both identities on one row: `USDC@base` is what moved, `USDC_BASE` is what moved it.

**`ledger_journals`** — kind `payment_recognized`, dedupe `payment_recognized:pay-1`, metadata
`{"fee_bps": 100, "token_id": "USDC_BASE", "rate_source": "merchant_override", "payment_path": "direct", "occurred_at_exact": true}`

**`ledger_entries`**

| account                  | asset     | amount      |
|--------------------------|-----------|-------------|
| `custody_unswept[m]`     | USDC@base | `+50000000` |
| `payable_to_merchant[m]` | USDC@base | `-50000000` |
| `fees_receivable[m]`     | USDC@base | `+500000`   |
| `fee_revenue[op]`        | USDC@base | `-500000`   |

Sums to zero within `USDC@base`. After three such payments the merchant is owed 150 USDC, owes
1.5 USDC in fees, and 150 USDC sits unswept on chain. Nothing has been shaved off anything.

The same transaction also enqueues one `sweep_queue` row against the movement, because
`to_kind = 'deposit_address'`. Had this been a reference-path payment, `to_kind` would be
`merchant_main`, the first leg would debit `custody_treasury`, and no queue row would exist.

Note that the route appears only in `metadata`. Had one of the three payments arrived through
`USDC_BASE_CRATES` at a different rate, the three accruals would carry different `fee_bps`
snapshots and still land in one `fees_receivable[m, USDC@base]` balance.

### 7.2 Sweep, batching all three deposit addresses

Gas price crossed the merchant's configured threshold, so the sweeper fired. All three addresses
belong to `m`; a batch never spans merchants (§6.4).

**`chain_transactions`**

| id     | intent | merchant | token_id    | fee_asset | fee_paid           | status |
|--------|--------|----------|-------------|-----------|--------------------|--------|
| `tx-4` | sweep  | m        | `USDC_BASE` | ETH@base  | `3000000000000000` | final  |

**`chain_movements`** — one row per address swept, one transaction

| tx_id  | idx | payment_id | asset     | amount   | from_kind       | to_kind       |
|--------|-----|------------|-----------|----------|-----------------|---------------|
| `tx-4` | 0   | —          | USDC@base | 50000000 | deposit_address | merchant_main |
| `tx-4` | 1   | —          | USDC@base | 50000000 | deposit_address | merchant_main |
| `tx-4` | 2   | —          | USDC@base | 50000000 | deposit_address | merchant_main |

**`ledger_entries`** — kind `sweep`, dedupe `sweep:tx-4`

| account                  | asset     | amount              |
|--------------------------|-----------|---------------------|
| `custody_unswept[m]`     | USDC@base | `-150000000`        |
| `custody_treasury[m]`    | USDC@base | `+150000000`        |
| `custody_gas[m]`         | ETH@base  | `-3000000000000000` |
| `payable_to_merchant[m]` | ETH@base  | `+3000000000000000` |

Two assets, one journal, each summing to zero independently. The USDC legs relocate value
without changing what is owed; the ETH legs reduce both the gas asset and the ETH liability,
because gas is spent from the merchant's own funds.

**The value legs come from the queue rows in the batch, not from the movement amounts.** Those
agree here, but they do not always: if the sweeper drained an address holding one recognized and
one not-yet-recognized payment, the movement covers both and the queue covered only the first.
See §10.1.

Note what did *not* happen: the merchant's USDC balance is still exactly 150. Gas did not come
out of it. On a chain where the payment asset and the gas asset are the same (native BTC, native
ETH) the legs collapse into one asset and the balance does drop — but that is a property of that
chain, not a general rule, and it must not be generalised into one.

The `custody_unswept` legs also cancel cleanly even if the payments were recognized through one
route and swept through another, because both sides resolve to `USDC@base` (§2.9).

### 7.3 Fee settlement

The reaper runs: `fees_receivable` totals 1.5 USDC ≈ $1.50, below the $20 threshold. Nothing
happens. Some weeks and many payments later the total reaches $21.40 — 20.80 in `USDC@base` and
0.60 in a low-value asset below the dust floor. Only USDC settles.

**`chain_transactions`**

| id     | intent         | merchant | fee_asset | fee_paid          | status |
|--------|----------------|----------|-----------|-------------------|--------|
| `tx-9` | fee_settlement | m        | ETH@base  | `900000000000000` | final  |

**`chain_movements`**

| tx_id  | idx | asset     | amount   | from_kind     | to_kind  |
|--------|-----|-----------|----------|---------------|----------|
| `tx-9` | 0   | USDC@base | 20800000 | merchant_main | operator |

**`ledger_entries`** — kind `fee_settlement`, metadata carries the USD quotes used to decide

| account                  | asset     | amount             |
|--------------------------|-----------|--------------------|
| `custody_treasury[m]`    | USDC@base | `-20800000`        |
| `custody_operator[op]`   | USDC@base | `+20800000`        |
| `payable_to_merchant[m]` | USDC@base | `+20800000`        |
| `fees_receivable[m]`     | USDC@base | `-20800000`        |
| `custody_gas[m]`         | ETH@base  | `-900000000000000` |
| `payable_to_merchant[m]` | ETH@base  | `+900000000000000` |

The receivable is extinguished, the liability drops by the same amount, and the value has
physically moved to the operator. The sub-dust-floor asset keeps its accrued balance for next
time.

### 7.4 Withdrawal

The merchant withdraws 100 USDC to an address they control outside the system.

| account                  | asset     | amount              |
|--------------------------|-----------|---------------------|
| `custody_treasury[m]`    | USDC@base | `-100000000`        |
| `payable_to_merchant[m]` | USDC@base | `+100000000`        |
| `custody_gas[m]`         | ETH@base  | `-1200000000000000` |
| `payable_to_merchant[m]` | ETH@base  | `+1200000000000000` |

The corresponding movement has `to_kind = 'external'`. This is the point at which funds leave
custody, and the ledger says so: the asset and the liability shrink together.

### 7.5 A merchant self-move using an exported key

The merchant moves 40 USDC out of their treasury with their own key. The processor did not build
this transaction, does not know the route, and finds out by observing it.

**`chain_transactions`**

| id      | intent   | merchant | token_id | fee_asset | fee_paid           | status |
|---------|----------|----------|----------|-----------|--------------------|--------|
| `tx-14` | external | m        | —        | ETH@base  | `800000000000000`  | final  |

`token_id` is null because no route was involved. `fee_paid` is populated because the gas came
out of the merchant's own address regardless of who signed.

**`ledger_entries`** — kind `external_debit`, dedupe `external_debit:tx-14:0`

| account                  | asset     | amount             |
|--------------------------|-----------|--------------------|
| `custody_treasury[m]`    | USDC@base | `-40000000`        |
| `payable_to_merchant[m]` | USDC@base | `+40000000`        |
| `custody_gas[m]`         | ETH@base  | `-800000000000000` |
| `payable_to_merchant[m]` | ETH@base  | `+800000000000000` |

Structurally identical to a withdrawal (§7.4), which is the point: the same value left custody
by the same door, and only the journal kind records that the system did not initiate it. Any
fees already accrued are untouched (§6.1).

### 7.6 A Solana SPL sweep, with rent reclaim

Sweeping an SPL deposit produces **two movements in two assets**, because closing the ATA
returns its rent lamports. The merchant's gas wallet signs as fee payer; the ATA owner signs as
transfer authority (§8.1).

**`chain_movements`**

| tx_id  | idx     | event_ref     | asset               | amount   | from_kind       | to_kind       |
|--------|---------|---------------|---------------------|----------|-----------------|---------------|
| `tx-4` | `8*2+1` | `tokbal:8`    | USDC@mainnet-beta   | 50000000 | deposit_address | merchant_main |
| `tx-4` | `8*2+0` | `lamports:8`  | SOL@mainnet-beta    | 2039280  | deposit_address | gas           |

**`ledger_entries`** — kind `sweep`

| account                  | asset             | amount       |
|--------------------------|-------------------|--------------|
| `custody_unswept[m]`     | USDC@mainnet-beta | `-50000000`  |
| `custody_treasury[m]`    | USDC@mainnet-beta | `+50000000`  |
| `custody_gas[m]`         | SOL@mainnet-beta  | `+2039280`   |
| `payable_to_merchant[m]` | SOL@mainnet-beta  | `-2039280`   |
| `custody_gas[m]`         | SOL@mainnet-beta  | `-5000`      |
| `payable_to_merchant[m]` | SOL@mainnet-beta  | `+5000`      |

The rent legs **credit** the merchant, unlike every other gas leg in this document: rent was
posted from somewhere when the ATA was created and is now coming back, so it increases both the
gas asset and what is owed. The last two legs are the ordinary transaction fee, going the other
way. Netting them by hand into one leg would be arithmetically identical and would destroy the
ability to answer "how much rent have we recovered", so they stay separate.

If the ATA is not closed — a policy choice, since keeping it avoids paying rent again on the next
payment to the same address — the rent legs simply do not exist and nothing else changes.
`sweep_params.close_ata_after_sweep` records which policy applied to a given queue row.

---

## 8. Sweeping: who pays the fee, and what each chain permits

This is the axis the three network families differ on most. It decides how many transactions a
sweep takes, whether value is left stranded, and — through that — whether the ledger's
reconciliation checks can ever settle at zero.

| Network / path      | Who pays the sweep fee                                                          | Can custody be fully drained?                         | Txs per sweep        |
|---------------------|---------------------------------------------------------------------------------|-------------------------------------------------------|----------------------|
| Solana, native      | separate fee payer — gas wallet signs first, deposit account signs as authority | Yes, to zero; the account is then deallocated         | 1                    |
| Solana, SPL         | same                                                                            | Yes; `CloseAccount` also reclaims the ATA rent        | 1                    |
| EVM, HD deposit     | **the sending address itself** — EVM has no native fee delegation               | Only after funding it with native token first         | 2 (fund, then sweep) |
| EVM, contract vault | the merchant gas wallet, calling the vault                                      | Yes                                                   | 1                    |
| Esplora / UTXO      | deducted from the inputs                                                        | Inputs consumed entirely; remainder returns as change | 1                    |

### 8.1 Solana

The fee payer is the first signer and need not be a party to the transfer, so the merchant gas
wallet pays while the deposit account signs only as the transfer authority. This is why Solana
deposit addresses never need pre-funding, and it is why `sweep_params.external_fee_payer` is set
for every Solana queue row.

One runtime constraint governs the amount: a system account may not be left holding a nonzero
balance below the rent-exempt minimum (roughly 890,880 lamports for a zero-data account). A
native SOL sweep is therefore **drain-to-zero or leave-at-least-rent-exempt**. There is no free
choice of partial amount, and the sweeper must not offer one.

If the deposit account were to pay its own fee, the swept amount would be `balance − fee` and the
residue would remain at the address forever. That is the drift case in §11.5, and the separate
fee payer is what avoids it.

### 8.2 UTXO

The fee comes out of the inputs, so the swept asset and the fee asset are the same and the legs
collapse into one asset, as §7.2 anticipated for native chains. Nothing remains at the address
unless the sweeper builds a change output back to it, and a change output is an ordinary inbound
movement that credits `custody_unswept` again and enqueues a new sweep row. Both cases are
self-consistent with no special handling.

### 8.3 EVM HD deposits: the case that strands value

This is the reason the contract vault exists. Sweeping an ERC-20 from an HD deposit address
requires that address to hold native token for gas, which means a `gas_refill` transaction to the
deposit address before the sweep can be broadcast. Whatever is left over after the sweep stays
there.

That leftover is booked honestly rather than special-cased. Native token arriving at a deposit
address is a movement with `to_kind = 'deposit_address'` like any other, so it credits
`custody_unswept` in the native asset and produces a queue row. The sweep then burns part of it,
and the remainder sits in `custody_unswept` as dust — which is exactly what it is. Its queue row
moves to `abandoned` once it falls below the economic floor, and it stays visible.

**Do not route sweep gas to `custody_gas` by tagging it `to_kind = 'gas'`.** `to_kind` classifies
the address, not the intent (§2.2). Overriding it here would hide the stranded dust, and hiding
it is how the EVM HD path comes to look as cheap as the vault path when it is not. The
`queue_abandoned` figure in §11.5 is the number that says whether the vault is worth building.

---

## 9. Failed transactions that still burn gas

A reverted EVM transaction still consumes gas. The chain layer records it as a normal
transaction with `status = 'failed'` and a populated `fee_paid`; movements are simply absent,
because no value moved.

The ledger records a `gas_burn_failed` journal with **gas legs only, no value legs**:

| account                  | asset    | amount             |
|--------------------------|----------|--------------------|
| `custody_gas[m]`         | ETH@base | `-400000000000000` |
| `payable_to_merchant[m]` | ETH@base | `+400000000000000` |

The merchant is genuinely poorer by the gas, and no value legs are warranted. This is a second
reason the balance constraint is grouped per asset: a journal touching only the gas asset is
perfectly valid and must not be forced to reference the asset it failed to move.

This case becomes more common once keys are exported (§10.3), since a merchant's own transaction
is one of the more likely reasons a sweep reverts.

On Solana, a failed transaction still debits the fee payer — which is the merchant's gas wallet,
not the deposit account — so the legs are identical in shape.

---

## 10. Ordering hazards and reversals

### 10.1 Sweeping funds that are not yet recognized

The realistic failure: a deposit address holds one finalized payment and one at three
confirmations, the gas-price trigger fires, and the sweeper moves the *address balance* — both
of them. `sweep_queue` covers only the recognized one, because rows are enqueued at recognition
(§5.2). So the sweep produces a movement for the full balance while the queue accounted for part
of it, and a naive journal built from movement amounts would debit `custody_unswept` for value
that was never credited there, driving it negative.

Two rules resolve it, and both are already implied by the schema:

**The sweep journal's value legs are summed from the queue rows stamped with that
`sweep_tx_id`, never from the movement amounts.** The movement records what the chain did; the
journal records what was recognized. They are allowed to differ and this is when they do.

**Recognition branches on whether the value has already moved.** A `swept_by_tx_id UUID NULL`
column on `payments`, set when a sweep includes that payment's funds, gives:

- funds still at the deposit address → debit `custody_unswept`, enqueue a sweep row, as normal
- already moved by a sweep → debit `custody_treasury` directly, write no sweep leg, enqueue
  nothing

The sweep journal only ever covers payments already recognized at sweep time; late-recognized
payments arrive straight into treasury. This keeps both invariants without making the sweeper
wait for finality — which matters, because the entire point of gas-price-triggered sweeping is
to move opportunistically when gas is cheap, not when confirmations happen to be ready.

### 10.2 Reorg after recognition

Rare at the system's finality threshold, and structurally impossible on Solana (§5.1) — but
"rare" combined with "append-only" means the path has to exist before it is needed on the chains
where it is reachable.

A `reversal` journal is written with `reverses` pointing at the original and every leg negated;
the deferred trigger asserts the negation is exact, per `(account, asset)`. The original is never
edited or deleted, and `ledger_journals_reverses_uq` means a journal can be reversed exactly
once. If the merchant already withdrew against the reversed funds, `payable_to_merchant` goes
negative — they are overdrawn. That is the honest representation, and it converts an accounting
problem into a collections problem, which is the correct place for it to live.

### 10.3 A merchant moving funds the sweeper is already spending

Key export makes two parties able to sign for one address, and the ledger has to survive the
race even though it cannot prevent it.

The chain resolves the conflict first: on EVM one of the two transactions wins the nonce and the
other fails or is replaced; on a UTXO chain one spends the output and the other becomes invalid;
on Solana one wins the ordering within the slot. In every case the ledger sees a
`gas_burn_failed` for the loser (§9) and a normal journal for the winner, and no leg is ever
written for value that did not move — sweep journals are written on `final`, not on broadcast,
precisely so that a lost race produces no value legs at all.

What the ledger cannot do is make the merchant's sweep policy sensible afterwards. A merchant who
empties an address mid-batch may leave the sweeper repeatedly attempting an uneconomic sweep of
dust; `sweep_queue.attempts` and `available_at` are what bound that, and `abandoned` is where it
ends. That is undesired behaviour rather than incorrect accounting, and it is called out as an
accepted consequence of key export in `RECONCILIATION.md` §10 and §11.

---

## 11. Deriving balances

Balances are always derived, never stored and never incremented. Four views ship with the
migration; the queries below are what they contain.

### 11.1 Ledger balances (ownership) — `v_ledger_balances`, `v_merchant_positions`

``` sql
SELECT a.kind, a.asset_id, s.symbol, SUM(e.amount) AS balance
FROM ledger_entries e
JOIN ledger_accounts a ON a.id = e.account_id
JOIN assets s          ON s.id = a.asset_id
WHERE a.merchant_id = $1
GROUP BY a.kind, a.asset_id, s.symbol;
```

Withdrawable in asset *A* is `-balance(payable_to_merchant[A])`, optionally net of
`fees_receivable[A]` and `gas_advance_receivable[A]` depending on operator policy, and further
filtered to assets that have a registered handler with `can_withdraw`. Those framings all read
the same few numbers, so switching between "net obligations off the displayed balance" and
"show owed separately and block withdrawal above a threshold" is a policy change, not a
migration. Keeping it that way is deliberate.

The capability filter is what keeps `custody_unsupported` value visible without being offered:
the balance is real and shown, the withdraw button is absent because no code can move it.
`assets.registered` is the database-side half of that filter; `can_withdraw` lives in code and
cannot be queried.

The per-account running statement, if wanted:

``` sql
SELECT e.entry_no, j.kind, e.amount,
       SUM(e.amount) OVER (ORDER BY e.entry_no) AS running_balance
FROM ledger_entries e
JOIN ledger_journals j ON j.id = e.journal_id
WHERE e.account_id = $1
ORDER BY e.entry_no;
```

### 11.2 Physical position (location)

"How much USDC is unswept, and across how many addresses" comes from `chain_movements`, not the
ledger. This is forced rather than preferred: the ledger only knows about finalized value, so a
payment at 2/12 confirmations is physically sitting in an HD address and entirely invisible to
the ledger. A dashboard figure that excluded it would not match what a block explorer shows for
that address.

``` sql
WITH deltas AS (
    SELECT m.to_address AS address, m.to_kind AS kind, m.asset_id, m.merchant_id,
           m.amount AS delta
    FROM chain_movements m JOIN chain_transactions t ON t.id = m.tx_id
    WHERE t.status NOT IN ('orphaned','failed')
    UNION ALL
    SELECT m.from_address, m.from_kind, m.asset_id, m.merchant_id, -m.amount
    FROM chain_movements m JOIN chain_transactions t ON t.id = m.tx_id
    WHERE t.status NOT IN ('orphaned','failed')
)
SELECT asset_id, kind,
       COUNT(*) FILTER (WHERE bal > 0) AS addresses,
       SUM(bal) AS total
FROM (
    SELECT address, kind, asset_id, SUM(delta) AS bal
    FROM deltas WHERE merchant_id = $1
    GROUP BY address, kind, asset_id
) x
WHERE bal <> 0
GROUP BY asset_id, kind;
```

Filtering `kind IN ('deposit_address','vault')` gives the unswept figure directly, per asset,
with an address count — which also feeds the sweep policy, since "spread thin across 40
addresses" and "concentrated in 2" are very different gas propositions for the same total.
`v_sweep_backlog` answers the same question from the queue side, grouped the way the sweeper
actually batches.

This query is only as complete as movement ingest. It sees every movement the system has
recorded, which after reconciliation ships includes movements the system did not initiate, and
which does **not** include anything sent to an address after it left the reconciliation watch
set (`RECONCILIATION.md` §3.2).

### 11.3 Ledger against chain position

`custody_unswept` (ledger) and the deposit-address position (chain) are expected to *differ*,
and the difference is meaningful — but the comparison has to be made across **all custody kinds
at once**, not per kind:

```
(chain position at deposit_address + vault + merchant_main)
  −  (custody_unswept + custody_treasury)
    =  value detected but not yet recognized
```

The per-kind version breaks the moment a sweep moves unrecognized value (§10.1): the deposit
position drops by more than `custody_unswept` does, and the difference reappears at
`merchant_main` where `custody_treasury` has not yet caught up. Summed across kinds it cancels
correctly.

That number should always be ≥ 0, and should always be explainable by the set of payments
currently in `detected` / `merchant_confirmed`. If it is negative, or non-zero with no in-flight
payments, something is wrong. This is the cheapest and most valuable monitoring check in the
system, and it is worth having before the first sweep runs in anger.

Once reconciliation ships the check gains a second explainable term: value observed through an
`external_credit` whose transaction has not yet reached `final`. The check does not change shape,
but "explainable by in-flight payments" becomes "explainable by in-flight payments and
in-flight external credits".

Balances are computed on demand. Materialised or snapshot balances are a performance answer to
be applied when the query is measurably slow, not before.

### 11.4 Ledger against actual chain balance

§11.3 compares two things the system already believes. It cannot detect anything the system never
saw — rebasing assets, fee-on-transfer discrepancies, or value that arrived before an address was
watched.

Balance probing is the independent check, specified in `RECONCILIATION.md` §8. Its output reaches
the ledger only through `probe_adjustment` journals against `suspense_unexplained`, and only when
a delta cannot be sourced to any movement.

The intended steady state is that `suspense_unexplained` is empty across every merchant and
asset. A non-zero balance is a signal that movement ingest is missing a case, and the response is
to fix ingest rather than to widen a tolerance.

### 11.5 Queue against ledger — `v_unswept_reconciliation`

The queue and the ledger should agree about how much value is sitting unswept:

```
custody_unswept balance  −  (queue_active + queue_abandoned)  =  0
```

where `queue_active` sums rows in `pending`/`claimed`/`broadcast` and `queue_abandoned` sums
`abandoned` ones. Non-zero drift means either a journal was written without enqueuing, or a queue
row was marked `swept` without a corresponding sweep journal.

**Abandoned rows count.** Abandoned value is still unswept — it is merely never going to be
swept, being gas dust below the economic floor (§8.3). Dropping it from the queue side would
report it as drift forever, which trains everyone to ignore the alarm. The view exposes the two
figures separately so the difference between "waiting" and "written off" stays legible, while
`drift` counts both.

**What drift actually measures.** Drift arises from value that *physically remains at a custody
address* after a sweep, not from value consumed by fees:

- **UTXO**: the fee is consumed. The full queued amount leaves `custody_unswept`; less arrives at
  `custody_treasury`; the difference is a fee leg. Sums to zero, drift is zero.
- **Native SOL with a self-paying deposit account**: the residue remains at the address. The
  sweep journal removes less than the queue row claimed. Permanent drift — hence §8.1's separate
  fee payer.
- **EVM HD leftover gas**: remains at the address, correctly still in `custody_unswept`, queue
  row `abandoned` rather than deleted. Counted, not drift.

---

## 12. Invariants

Alongside those in `NETWORKS.md` §8. If a change breaks one of these it is a bug regardless of
what else it fixes.

1. `ledger_entries` are **never updated or deleted**. Corrections are reversal journals.
2. Every journal sums to zero **per `(journal_id, asset_id)`**, and no journal has zero entries.
3. Every journal has a `dedupe_key` backed by a DB uniqueness constraint, not application logic.
4. Value enters the ledger exactly once, at finality — `system_confirmed` for payments, `final`
   for everything else — never before.
5. Amounts are base units everywhere below the presentation layer. No mixed-asset amounts, and
   no fiat amounts in the ledger at all — USD is used to *decide*, never to *record*.
6. Gas is recorded in the gas asset. It is never netted against a payment asset without an
   explicit `conversion` journal recording the rate.
7. `chain_movements.amount > 0` always; direction is carried by `from_*` / `to_*`.
8. The ledger never writes back into the chain layer.
9. Fees accrue as receivables and are settled by explicit transactions. Nothing is ever silently
   deducted from a swept or withdrawn amount.
10. Ledger history is never destroyed by any deletion path (§13).
11. **No ledger table references a `token_id`.** Accounts and entries are denominated in
    `asset_id`; routes appear only in the chain layer, in `sweep_queue` as a pin, and in journal
    metadata. Removing a handler from the codebase must never orphan or invalidate a ledger row.
12. **One real asset is one `assets` row.** Canonical form — including `chain_ref` — is enforced
    at registration, and two handlers advertising the same asset with different decimals or
    symbol is a startup failure. Asset identity and decimals are immutable once written.
13. Observed value is never silently discarded. An asset with no handler is recorded in
    `custody_unsupported`, not dropped.
14. `suspense_unexplained` is expected to be zero. Any non-zero balance is an alarm, not a
    tolerance.
15. **A sweep batch never crosses merchants.** Gas is per-merchant, so a cross-merchant batch
    would have one merchant paying another's costs.
16. `sweep_queue` is rebuildable and disposable. Nothing depends on it that cannot be recomputed
    from `chain_movements` and `custody_unswept`, and `v_unswept_reconciliation` is what proves
    it.

---

## 13. Deletion and retention

Merchant deletion is **TBD in detail**, but one part is settled: it is a flag plus removal of
personally identifiable fields — name, contact details, credentials, webhook URLs and secrets —
and explicitly **not** a row deletion, and explicitly **not** any removal of ledger, journal,
movement or transaction history.

Financial records of value that actually moved through custody are exactly the records a
custodian is obliged to retain, and a deleted ledger cannot be reconstructed from the chain
alone: fee accruals, receivables, reversals and the recognition timeline have no on-chain
representation. The existing `ON DELETE CASCADE` foreign keys on `merchants` therefore need
revisiting before any deletion flow ships — appropriate for the current early-stage tables,
actively wrong for the ledger. Note that the ledger tables added in `010_ledger.sql` deliberately
reference `merchants(id)` **without** `ON DELETE CASCADE`, so a deletion flow written today would
fail loudly rather than silently erase a book.

**Handler removal is a separate and much cheaper path.** Deleting a route from the codebase
clears `assets.registered` on the next boot if nothing else advertises that asset, and does
nothing at all to the ledger, because no ledger row references the route. Historical
`chain_transactions.token_id` values become strings naming code that no longer exists, which is
correct: they record what happened, and what happened involved that code. The asset, its balances
and its history are untouched. This is the practical payoff of §1.1 and is worth an explicit
test — delete a handler, assert every balance is unchanged.

Which fields count as PII, retention periods, and how a deleted merchant's accounts appear in
operator reporting are open. `COMPLIANCE.md` is the place for the jurisdictional side.

---

## 14. TBD

Known-open, roughly ordered by how much each would change the shape above.

**The EVM vault model.** Two shapes are under consideration: a default contract acting only as a
relay, and an optional per-merchant contract that a merchant funds in exchange for batched sweeps
and lower per-payment gas. The choice determines whether `to_kind = 'vault'` is a per-merchant
position or a shared one, and a shared one is the single place where the "no merchant's funds
touch another's" property of §6.4 is not trivially true. It is also why the isolation audit is a
distinct roadmap item. Until it is decided, no journal should assume a vault position is
attributable to one merchant without a lookup.

**Stranded gas on EVM HD deposits.** §8.3 books it honestly and §11.5 counts it, but nothing
recovers it. If EVM volume lands mostly on HD deposits rather than the vault, `queue_abandoned`
grows monotonically with no mechanism to ever reclaim it. That is an acceptable cost, but it
should be a decided one rather than discovered at month three — and the number is worth watching
from the first EVM sweep, because it is the input to the vault decision above.

**Conversions and pricing.** Non-native-asset sweeps may need converting some value to the native
asset first to pay gas, when the gas account is empty. That is a `conversion` journal with legs in
two assets and a rate snapshot in `metadata` — never an implicit haircut on a balance. Slippage,
failure mid-conversion, and which venue the conversion routes through are unspecified. Related:
once conversions exist, "how much is this merchant up in USD" becomes a reporting question needing
an oracle and a chosen valuation basis. That belongs in a reporting layer, firmly outside the
ledger.

**Automated gas refills.** Currently manual. The intended shape is a per-network/chain policy that
converts from allowed assets when the gas balance drops below a threshold, but the policy
structure, its interaction with sweep triggers, and whether it may trigger a `gas_advance` (§6.5)
rather than a conversion are not designed. Solana makes this less urgent than EVM does, since the
gas wallet pays for everything and deposit addresses never need funding.

**Refunds.** Not modelled. The leaning is non-refundable at the platform level, leaving refunds to
the merchant's own process. If modelled later it needs a refund journal kind plus a
withdrawal-shaped outbound transaction, and a decision on what happens to fees already accrued
against the refunded amount — reverse them, keep them, or make it configurable. Returning a
mistaken send (`RECONCILIATION.md` §12) is the same shape and should reuse whatever this becomes.

**Fee settlement modes.** The USD-threshold reaper is specified above; fixed schedule,
at-withdrawal, and deduct-on-every-payment-until-cleared are named but not designed, nor is
"whichever fires first". Whether `gas_advance_receivable` settles through the same reaper or its
own is also open.

**Price source semantics.** `get_usd_value` is specified as a handler method but its staleness
tolerance, failure behaviour (does settlement halt if the price source is down?), whether quotes
are cached per reaper run, and how two routes for one asset are reconciled beyond "use the
operator default" are all open.

**Withdrawal request lifecycle.** §7.4 covers a completed withdrawal. A requested,
pending-approval, or failed withdrawal has no representation yet — probably a
`withdrawal_requests` table upstream of the chain layer, with the journal written only on
finality, mirroring recognition. Route selection (§2.5) belongs to this flow.

**Reclassification mechanics.** §3.1 describes moving `custody_unsupported` value into
`custody_treasury` when a handler is registered for an asset. The trigger, whether it is automatic
at boot or an operator action, and what happens if the handler is later removed again, are
unspecified.

**`authority_address` on `invoices`.** §2.8 reads the Solana sweep authority out of
`payment_reference`, which works because the ATA owner and the Solana Pay reference key want the
same value. That is a coincidence of two roles, not a design, and it survives only as long as
nothing else starts writing to that column. An explicit `authority_address` column would cost one
migration and remove the coupling; it has not been done because doing it before the sweeper exists
means guessing at what the sweeper needs.

**Multi-asset invoices.** Out of scope. One invoice, one asset, one amount. If that ever changes,
the per-asset fee accrual model survives but invoice-level totals do not.

**Operator-level accounts.** `custody_operator` and `fee_revenue` are sketched as merchant-null
accounts in the same book. Whether operator P&L should be a separate book, and how the operator's
own withdrawals of settled revenue are recorded, is not worked out.

**Partial sweeps.** Most examples assume a sweep moves the full address balance. Partial sweeps
are representable in the movement layer and interact awkwardly with §10.1's recognition
branching — and on native Solana they are constrained by the rent-exempt floor (§8.1), which means
"sweep half" is not always an available option regardless of what the policy wants.

**Ordering across chains.** `entry_no` gives a total order within the ledger, but journals from
different chains arrive in wall-clock order, not block order. `occurred_at` exists for this and is
probably sufficient; it has not been stress-tested against a reorg on one chain concurrent with
normal activity on another. The nullable `block_time` fallback (§2.2) is a related loose end: a
journal with `occurred_at_exact: false` orders by when we noticed, not when it happened.

**Table growth.** `chain_movements` grows without bound and is append-only, and reconciliation
ingest makes it grow faster than the payment path alone would. Partitioning by `created_at` or by
chain is the obvious answer; not needed yet, and cheaper to add before the table is large.