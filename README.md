# Vestro (Early Alpha)

> ⚠️ **Status: Early Alpha.** The core architecture is in place and the first major phase (invoice creation, payment observation across both payment paths, and webhook delivery) is complete — this project has graduated from proof-of-concept to early alpha. That said, large parts of the system (sweeping, ledgering) are still in progress or unimplemented, and nothing here has been audited. **Do not use this in production or with real funds.** See the [Roadmap & timeline](#roadmap--timeline) for exactly where things stand.

> ⚠️ **This project is custodial by design.** The operator's server generates and holds the private keys / signing authority for every merchant wallet it creates. Merchants are able to export their own keys as a glass-break measure, which means custody is shared in practice — but the operator remains the custodian in the sense that matters legally, and remains responsible for any funds the system receives. See [`COMPLIANCE.md`](./COMPLIANCE.md) before deploying anywhere beyond your own local testing, and [`RECONCILIATION.md`](./RECONCILIATION.md) §10 for what shared custody means operationally.

## Documentation

Three companion documents go deeper than this README:

- 📡 **[`NETWORKS.md`](./NETWORKS.md)** — implementation-level documentation of **how payments are detected**, per network: the dual payment paths, the payment lifecycle, scan cursors, reorg handling, idempotency guarantees, and the contract any new network implementation must satisfy. If you want to understand or extend the detection layer, start here.
- 📒 **[`LEDGER.md`](./LEDGER.md)** — the design of the **ledgering system**: how on-chain activity is recorded, how merchant balances are derived, and how operator fees accrue and settle. A three-layer, double-entry, append-only design. Currently a design sketch ahead of the sweeping implementation.
- 🔁 **[`RECONCILIATION.md`](./RECONCILIATION.md)**  — what the system does about value that arrives at, or leaves from, a controlled address without the processor having initiated it: late payments, merchant self-moves, mistaken sends, dust. Movement-driven rather than balance-driven. Currently a design sketch ahead of the sweeping and isolation work.
- ⚖️ **[`COMPLIANCE.md`](./COMPLIANCE.md)** — a plain-language explanation of **why this software is custodial, what that tends to mean legally**, and what changes the moment you enable multi-tenant mode. Not legal advice, but required reading before deploying this anywhere real.

## Index

- [What this is](#what-this-is)
- [Why](#why)
- [Roadmap & timeline](#roadmap--timeline)
- [Supported networks](#supported-networks-in-progress)
- [Payment flows](#payment-flows)
- [Architecture](#architecture)
- [Fund isolation](#fund-isolation)
- [Webhooks](#webhooks)
- [Data / correctness principles](#data--correctness-principles)
- [Deployment modes](#deployment-modes)
- [Explicit non-goals](#explicit-non-goals-no-kyc-no-onoff-ramp)
- [Compared to existing tools](#compared-to-existing-tools-as-of-2026-08-04)
- [Tech stack](#tech-stack)
- [License](#license)
- [Before you deploy this anywhere real](#before-you-deploy-this-anywhere-real)

## What this is

A self-hostable, backend-first payment processor for accepting cryptocurrency payments across multiple networks, without depending on a hosted third-party gateway. You run it, you hold the keys, you own the data — and, as above, you also hold *other people's* keys if you enable multi-tenant mode.

The goal is a single, coherent invoicing/payment-detection layer that can sit behind any merchant backend and tell it, reliably, "this invoice was paid" — regardless of which chain or token the customer used.

## Why

Most existing self-hosted options are either narrowly scoped to a single chain, or only partially open source. This project exists to explore what a genuinely chain-agnostic, fully open implementation looks like, and to have a concrete, inspectable piece of infrastructure rather than a black box.

It is also a learning project — a way to get hands-on with HD wallet derivation, chain-specific watching/confirmation logic, and the operational edge cases (reorgs, underpayment, idempotent webhook delivery) that any real payment system eventually has to deal with.

## Roadmap & timeline

### ✅ Phase 1 — Observing *(complete)*

The full path from "create an invoice" to "tell the merchant it was paid": an invoice can be created, its payment observed through either of the dual payment paths (naive QR or WalletConnect), the resulting events ingested, and the webhook delivered to the merchant's URL.

* [x] Overall architecture: the network-agnostic orchestrator and the `NetworkClient` / `TokenHandler` traits
* [x] Network and token registration system
* [x] Network implementation — **EVM**, fully tested as the development network
* [x] Webhook delivery (at-least-once, retried, transactional with state changes)

### 🚧 Phase 2 — Network expansion: Solana & Esplora *(in progress)*

Before continuing into sweeping and ledgering, the observing layer is being extended to the two remaining planned networks. This confirms the `NetworkClient` / `TokenHandler` trait surface actually holds up across a non-EVM account model (Solana) and a UTXO model (Esplora) before more is built on top of it.

* [x] **Solana** network implementation — finished and fully tested
* [ ] **Esplora** (Bitcoin-style UTXO) network implementation — implemented, currently untested and undocumented

### 🔜 Phase 3 — Sweeping & ledgering

Moving funds from per-invoice deposit addresses to merchant main accounts, and accounting for every unit of value while doing it. The design sketch of the ledgering system is done — see [`LEDGER.md`](./LEDGER.md).

- [x] Ledgering system design (three-layer, double-entry, append-only — see `LEDGER.md`)
- [ ] Sweeping network code: deposit addresses → merchant main accounts
- [ ] Gas refilling mechanics for deposit addresses that need native token to move ERC-20s
- [ ] Full ledgering system implementation (`chain_transactions`, `chain_movements`, journals/entries)
- [ ] Orchestrator endpoints to trigger sweeps manually and to configure the conditions under which they happen automatically
### 🔜 Phase 4 — Containerization

- [x] Dockerfiles for the core services — written and tested working, see [`DOCKER.md`](./DOCKER.md)
- [ ] Package and document the rest of the deployment story so others can stand up their own instance

### 🔜 Phase 5 — Fund isolation & reconciliation

Two related problems that are best solved together. *Isolation* is the property that no merchant's funds, gas, or signing authority ever touch another merchant's — no shared pools, no shared gas accounts. *Reconciliation* is what keeps the books correct once value starts moving for reasons the payment path did not initiate. See [`RECONCILIATION.md`](./RECONCILIATION.md).

- [ ] Decide the EVM vault model — relay-only default vs optional per-merchant funded contract for batching
- [ ] Isolation audit: confirm every network implementation actually keeps merchant funds and gas separate, and close the gaps the vault currently leaves open
- [ ] Movement-level ingest for watched addresses — both directions, invoice-independent
- [ ] Reconciliation journals, `external:attributed` / `external:unexplained` accounts
- [ ] Balance probing and the unexplained-delta alarm
- [ ] Merchant key export: used-index manifest, standalone recovery CLI, documented derivation paths per network

### 🔜 Phase 6 — Hardening

- [ ] Proper audit pass over the whole codebase

## Supported networks (in progress)

- **EVM** — Ethereum mainnet + L2s (Base, etc.) — *complete, current reference network*
- **Solana** — Mainnet, devnet. *complete*
- **Esplora-compatible** (Bitcoin and similar UTXO chains) — *planned (Phase 2, after Solana)*
 
Token support is designed to be cheap to extend: most EVM tokens reuse the same handler logic with different addresses/decimals, so adding a new ERC-20/BEP-20-style token is close to a config change rather than new code.
 
## Payment flows

Two ways to pay an invoice, by design:

- **Naive QR** — a plain address QR code. Maximally compatible (many wallets fail to parse QR codes that embed token/network/memo metadata reliably), at the cost of requiring a sweep step from the deposit address to treasury.
- **WalletConnect** — a direct connection to the user's wallet, allowing a single atomic transaction (e.g. a smart contract call on EVM, or a treasury transfer carrying a reference key on Solana) with no separate sweep required and no risk of user error on manual identifier entry.

The system is built to soft-prefer WalletConnect where available, while keeping the naive QR path fully functional as a fallback. See [`NETWORKS.md`](./NETWORKS.md) for the full detection model behind each path.

## Architecture

The system is split into three layers:

1. **`NetworkClient` (trait)** — one implementation per network (`EVMNetwork`, `SolanaNetwork`, `EsploraNetwork`). Each network exposes a common set of required capabilities (e.g. `watch_payments`), but is free to implement them however makes sense for that chain — an EVM network might run `watch_blocks` + `watch_logs`, Esplora might only need `watch_blocks`, Solana might watch for memos.

2. **`TokenHandler` (trait)** — tokens are registered against a handler (e.g. `USDC_ETH` → `EVMHandler`, `USDC_BASE` → `BaseHandler`). Handlers contain the token-specific logic and call down into their network client with the right addresses/parameters.

3. **Orchestrator** — the entry point for creating an invoice. It is deliberately network-agnostic: it doesn't know or care whether a token ID resolves to EVM, Solana, or Esplora. It generates the invoice record, hands off to the relevant token handler to start watching for payment, and separately runs a service that scans for completed payments and dispatches webhooks (with retry and at-least-once delivery semantics).

This separation means adding a new chain means implementing one trait, and adding a new token on an existing chain means (in most cases) registering a handler with different parameters — not writing new payment-detection logic from scratch. The full contract a new network must satisfy is documented in [`NETWORKS.md`](./NETWORKS.md).

## Fund isolation

Each merchant's crypto only ever touches that merchant's crypto. There are no shared deposit pools and no shared gas accounts — a merchant's gas is funded to their own addresses, and a sweep moves their funds to their own main address. This is an accounting property first: it means every unit of value on the ledger has exactly one owner, and no reconciliation ever has to apportion a shared balance between tenants.

The property is close to trivially true on networks where every address is derived from the merchant's own seed. The exception is the EVM batching vault, which is shared infrastructure by construction, and which is the reason the isolation audit is a distinct roadmap item rather than something to be assumed.

Two things are called "containers" in adjacent contexts and are unrelated: this section is about **fund isolation**, while **containerization** in the roadmap means Docker.

Merchants are able to export the keys to their own wallets. See [`RECONCILIATION.md`](./RECONCILIATION.md) §10.

## Webhooks

Payments can be underpaid, overpaid, or corrected across multiple transactions, so the webhook model reports state rather than a single "paid" boolean. Most events are per-transaction; `payment.finished` is the exception, reported at the invoice level.

- `payment.detected` — fired on detected funds, reporting received/total/expected amounts (so partial payments are visible)
- `payment.confirmed` — fired once the configured confirmation depth is reached
- `payment.finalized` — fired once a deeper, final confirmation depth is reached, this is the point the server stops polling and assumes a payment can't or won't be statistically reverted, for merchants who want stronger reorg guarantees than `payment.confirmed` alone provides
- `payment.orphaned` — fired if a previously confirmed transaction is reorganized out; the merchant decides how to react
- `payment.finished` — fired once an invoice's received total first reaches the requested amount, aggregating across all transactions toward that invoice

Both confirmation depths (`payment.confirmed` and `payment.finalized`) are configurable per token/network. Monitoring continues after initial confirmation, at a reduced frequency, to allow reorg detection through to finalization.

## Data / correctness principles

- PostgreSQL, designed around ACID guarantees and idempotent operations — invoice creation, sweeps, and webhook dispatch are all built to be safely retryable without double-processing.
- The ledger (in design — see [`LEDGER.md`](./LEDGER.md)) is double-entry and append-only: corrections are reversals, never edits, and merchant balances are always derived, never incremented.
- No NFT, trading, or speculative-market functionality. This is infrastructure for accepting payment, not a wallet, exchange, or trading tool.
- Reconciliation is movement-driven, not balance-driven: external events are ingested as attributed movements with a transaction hash and a counterparty, never as unsourced balancing entries. Value that genuinely cannot be sourced lands in a dedicated account that is expected to sit at zero and raises an alarm when it does not.

## Deployment modes

The software runs in one of two modes, set at deployment/config time. This is not just a UI toggle — it changes what obligations fall on whoever operates the instance. See [`COMPLIANCE.md`](./COMPLIANCE.md) for why this distinction matters.

### 1. Solo mode

The operator is the only merchant. There is no merchant signup flow, no per-merchant onboarding, and no third party ever has funds passing through the instance other than the operator's own. This is the mode intended for "I'm running this for my own site(s)."

- No KYB flow is required or presented.
- Still fully custodial (see above) — the operator holds the keys for their own funds, same as any self-custody setup, just mediated by this software instead of a personal wallet.

### 2. Multi-tenant mode

Signups are open, and unrelated third parties can register as merchants and receive funds through the operator's instance. This is a materially different situation: the operator becomes the custodian of value on behalf of people they don't control, and in most jurisdictions is considered a service provider to them.

- **KYB (Know Your Business) on the merchant is a hard requirement to enable this mode.** The software will not allow multi-tenant signups to go live with KYB disabled. The operator selects and configures their own KYB provider/keys; this project does not ship a bundled KYB vendor.
- Multi-tenant mode is the mode that triggers most real-world compliance obligations (AML/CTF registration and reporting, recordkeeping, beneficial-owner checks, etc., depending on jurisdiction). Read `COMPLIANCE.md` in full before flipping this switch.

## Explicit non-goals: no KYC, no on/off-ramp

- **No end-customer KYC.** This project does not identify, verify, or collect data on the *end customer* paying an invoice — the person sending funds from their own wallet. Payments are accepted directly from customer-controlled wallets (naive QR or WalletConnect); the software has no visibility into who that customer is beyond an on-chain address, and it is not designed to gain that visibility. If a merchant's own regulatory situation requires end-customer KYC, that is out of scope for this project and is the merchant's (or the merchant's own tooling's) responsibility, not something this processor performs.
- **No fiat on-ramp or off-ramp, at all.** This project never converts crypto to fiat or fiat to crypto, never touches a bank account or card rail, and never integrates a fiat payment processor. Funds go in as crypto (from the customer's wallet) and stay as crypto for as long as they are within this system's custody. What a merchant does with funds *after* withdrawing them from the platform (e.g. sending them to their own exchange account) is entirely outside this project.

## Compared to existing tools (as of 2026-08-04)

The self-hosted / open-source crypto payment processor space is real but still sparse, and it clusters strongly along one axis:

- **Open-source + self-hosted** projects almost always choose the **non-custodial** path (merchant or end-user holds keys; the server only watches, generates addresses from xpubs, or coordinates via smart contracts). This keeps the compliance surface small and is elegant from a pure “don’t touch the money” engineering perspective.
- Projects that are willing to take **full custody** (generate and hold the private keys / signing authority for deposit addresses and any batching contracts) almost always become **SaaS / commercial gateways**. The operational, legal, and security burden of custody is high enough that the natural business model is to charge for the service rather than publish the full stack.

This project deliberately sits in the remaining empty square:

**Fully custodial + fully open source + self-hosted + multi-network (EVM + L2s, Solana, Esplora-style UTXO) + multi-tenant capable (with hard KYB gate) + deliberately easy to extend or replace pieces.**

That combination is uncommon. Most comparable software lands in one of the other three cells of the matrix.

| Project                                                                                                             | Custody model                                                         | Open source / self-hosted                                  | Network coverage (high level)                                                                                                                             | Notes / differentiation vs this project                                                                                                                                                                                                                                                                           |
|---------------------------------------------------------------------------------------------------------------------|-----------------------------------------------------------------------|------------------------------------------------------------|-----------------------------------------------------------------------------------------------------------------------------------------------------------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| **BTCPay Server**                                                                                                   | Non-custodial                                                         | Fully open, mature, self-hosted                            | Bitcoin-first (on-chain + Lightning). Community plugins for a few others (LTC, XMR, limited ETH/USDT). Cross-chain plugins exist but are not first-class. | The gold standard for Bitcoin. Excellent multi-tenant support, plugins, accounting, POS apps. Not designed as a unified multi-chain (especially EVM + Solana) invoicing layer with shared orchestration, dual payment paths, and network-optimized detection.                                                     |
| **SHKeeper**                                                                                                        | Non-custodial                                                         | Fully open (GPL-3.0), self-hosted (k3s/Helm)               | Broad: BTC (+ LN), ETH + many EVM L2s, SOL, TRX, XMR, XRP, TON, AVAX, etc. + multi-chain USDT/USDC/DAI/PYUSD. Actively expanded through 2025–2026.        | Solid multi-coin self-hosted gateway with CMS plugins and unique-address invoices. Non-custodial by design. Does not expose the same network-agnostic orchestrator + dual-path (naive HD QR + smart/WalletConnect with contract or memo) model, nor the same emphasis on operator-replaceable detection backends. |
| **PayRam**                                                                                                          | Non-custodial (smart-contract coordinated; no deposit keys on server) | Self-hosted, open scripts + components, stablecoin-focused | EVM (ETH, Base, Polygon, …), Tron, Bitcoin, TON; Solana in progress. Strong USDT/USDC/PYUSD focus.                                                        | Modern stablecoin-era alternative to BTCPay. Easy one-command deploy, white-label/operator mode, “no keys on server” architecture. Non-custodial by design. Different trade-offs from a fully custodial system that can own sweeping policy, batching contracts, and internal ledger semantics.                   |
| **Bitcart**                                                                                                         | Non-custodial                                                         | Fully open (MIT), self-hosted                              | BTC, LTC, BCH, XMR, ETH, TRX, USDT and more (~50 coins claimed)                                                                                           | Mature Python stack with good developer tooling and Lightning support. Again non-custodial.                                                                                                                                                                                                                       |
| Other notable self-hosted / OSS efforts (XPayLabs-style, neverpay, DV.net Merchant, various lighter gateways, etc.) | Almost all non-custodial                                              | Varying degrees of openness                                | Usually EVM + TRON or a smaller set                                                                                                                       | Most are either single-purpose, lighter-weight, or still non-custodial. Few combine full multi-tenant custody + unified orchestrator across EVM/Solana/UTXO with the dual-path UX described below.                                                                                                                |
| Commercial / SaaS processors (NOWPayments, Inqud, CoinGate, BitPay, Coinbase Commerce, etc.)                        | Custodial (or hybrid)                                                 | Closed                                                     | Broad                                                                                                                                                     | Exactly the model this project is an alternative to: you get the convenience of multi-chain invoices and webhooks, but you do not own the keys, the data, or the code, and you accept their fee schedule, ToS, and deplatforming risk.                                                                            |

**Why the custodial + open-source combination is rare (and why this project chooses it)**

Engineers building open-source payment infrastructure usually prefer non-custodial designs for the same reasons most crypto software does: lower regulatory surface, no private-key custody risk on the server, and a cleaner “we only watch the chain” story. The moment you decide the operator *will* generate and hold keys (for HD deposit addresses, for a batching vault’s admin keys, for automated sweeping under configurable policies, etc.), you inherit real compliance and operational obligations. Most people who accept that burden then productize it as a hosted service.

This project accepts the custodial model (see the top-level warnings and `COMPLIANCE.md`) because the merchant-facing product I actually want to run and offer looks like a normal payment processor: reliable “invoice paid” signals, dual payment options that work for both technical and non-technical payers, configurable confirmation/finality, under/over-payment handling, sweeps under operator policy, and the ability to run either solo or multi-tenant (with KYB required for the latter). Non-custodial designs are elegant; they are not always the best product shape when you are the one who has to support real merchants day-to-day.

The architecture is built so that the custodial choice does not force every operator into the same implementation details:

- The **orchestrator is network-agnostic**. It only knows token IDs, amounts, merchants, and invoices. Everything chain-shaped lives behind `NetworkClient` + `TokenHandler` traits.
- EVM ships with a **CustodialPaymentVault** contract that enables the smart (WalletConnect) path and batching/sweep policies. Operators are free to deploy a different contract, change the event signature (config), or disable the vault entirely and run pure address watching.
- Detection currently uses direct JSON-RPC calls (for learning and full control over quorum, reorg handling, cursor semantics, and idempotency). Because the network boundary is a trait, it is straightforward to:
  - Swap an implementation for one built on popular crates,
  - Run both side-by-side (e.g. a “reference” RPC path and a crate-based path),
  - Or add entirely different backends.
- Adding Solana Pay, Request Network, x402 (or any other protocol-level payment primitive) is the same shape of work: implement a `NetworkClient` (or a thin adapter) that understands that protocol’s detection/confirmation model, register the relevant token handlers, and the orchestrator and webhook layer continue to work unchanged. The same applies to future chains (TRON energy optimizations, etc.).

In short: the project is opinionated about the *product* (custodial multi-network processor with dual paths and multi-tenant option) while remaining deliberately unopinionated about many of the *implementation* choices underneath. That is the gap it is trying to occupy relative to both the non-custodial OSS tools and the closed custodial SaaS products.

## Tech stack

- **Backend:** Rust
- **Database:** PostgreSQL
- **Frontend (planned):** TypeScript + Tailwind

## License

**MIT.** This project is free software: free to use, modify, distribute, and deploy in any way, by anyone.

To be clear about what that means here: this is **not a commercial product**. There is no company behind it, no paid tier, no support contract, and no warranty of any kind. Anyone may deploy it wherever and however they wish — but doing so is entirely **under the responsibility of the operator**, including all custodial, security, and compliance obligations that come with running it (see [`COMPLIANCE.md`](./COMPLIANCE.md)).

## Before you deploy this anywhere real

Read [`COMPLIANCE.md`](./COMPLIANCE.md). It is not legal advice, but it explains why this software is custodial, what that tends to mean legally in different places, and what changes the moment you flip on multi-tenant mode.

---

This is a solo, spare-time project in active development. Feedback, issues, and PRs are welcome, but expect breaking changes for now.
