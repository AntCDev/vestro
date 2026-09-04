-- =========================================================================
-- 010_ledger.sql
--
-- Ledger, movements, sweeping and fees.
-- Companion to LEDGER.md. Requires PostgreSQL 15+ (UNIQUE NULLS NOT DISTINCT).
--
-- Layering, strictly one-directional:
--
--     chain ──▶ chain_transactions ──▶ chain_movements ──▶ ledger_*
--                                            │
--                                            └──▶ sweep_queue (work queue)
--
-- chain_movements, ledger_journals and ledger_entries are append-only and
-- enforced as such by trigger. sweep_queue is the only mutable table here and
-- is deliberately NOT part of the audit trail: it is fully rebuildable from
-- chain_movements plus the custody_unswept balance.
--
-- CANONICALIZATION NOTE (read before running):
--   chain_ref is part of assets_identity. It must be canonical per network
--   family and must never change for a given chain once rows exist. Pick one
--   form and enforce it in the handler constructor:
--       evm      -> numeric chain id as text: '8453', '1', '137'
--       solana   -> cluster name: 'mainnet-beta', 'devnet'
--       esplora  -> 'bitcoin', 'testnet', ...
--   Registering the same USDC contract under both 'base' and '8453' produces
--   two asset rows, two custody balances, and a phantom shortfall that
--   reconciliation cannot source. This is the single most expensive mistake
--   available in this file.
-- =========================================================================


-- =========================================================================
-- 1. Asset registry
--
-- An asset is a fact about a chain. It outlives every handler that ever
-- touched it, and it is what the entire ledger is denominated in. Nothing
-- below the presentation layer reads decimals or symbol.
-- =========================================================================
CREATE TABLE IF NOT EXISTS assets (
                                      id             UUID PRIMARY KEY DEFAULT gen_random_uuid(),

                                      network_type   VARCHAR(20)  NOT NULL,   -- 'evm' | 'solana' | 'esplora'
                                      chain_ref      VARCHAR(50)  NOT NULL,   -- canonical; see header note
                                      asset_kind     VARCHAR(20)  NOT NULL
                                          CHECK (asset_kind IN ('native', 'contract')),
                                      address        VARCHAR(255),            -- canonical; NULL iff kind = 'native'

                                      decimals       SMALLINT     NOT NULL CHECK (decimals >= 0 AND decimals <= 36),
                                      symbol         VARCHAR(32)  NOT NULL,

    -- Chain-specific facts that are properties of the asset itself, not of any
    -- route: Solana token_program / ATA program, Tron trc-kind, etc.
    -- Keeps this table from growing a column per network.
                                      asset_params   JSONB        NOT NULL DEFAULT '{}'::jsonb,

    -- true iff at least one handler currently advertises this asset.
    -- Cleared, never deleted: an asset that loses its last handler keeps every
    -- entry ever written against it and simply stops being withdrawable.
                                      registered     BOOLEAN      NOT NULL DEFAULT false,

                                      first_seen_at  TIMESTAMPTZ  NOT NULL DEFAULT now(),
                                      updated_at     TIMESTAMPTZ  NOT NULL DEFAULT now(),

                                      CONSTRAINT assets_native_has_no_address
                                          CHECK ((asset_kind = 'native') = (address IS NULL)),

    -- Cheap textual canonicalization where the encoding permits it.
    -- Solana is base58 and case-sensitive, so it gets validation at
    -- registration instead (reject anything that is not a well-formed
    -- 32-byte pubkey) rather than a CHECK here.
                                      CONSTRAINT assets_evm_lowercase
                                          CHECK (network_type <> 'evm'     OR address = lower(address)),
                                      CONSTRAINT assets_esplora_lowercase
                                          CHECK (network_type <> 'esplora' OR address = lower(address)),

                                      CONSTRAINT assets_identity
                                          UNIQUE NULLS NOT DISTINCT (network_type, chain_ref, asset_kind, address),

    -- Referenced by ledger_accounts so that "gas accounts hold native assets
    -- only" is a declarative FK + CHECK rather than a trigger.
                                      CONSTRAINT assets_id_kind_uq UNIQUE (id, asset_kind)
);

CREATE INDEX IF NOT EXISTS assets_chain_idx
    ON assets (network_type, chain_ref);

CREATE INDEX IF NOT EXISTS assets_registered_idx
    ON assets (network_type, chain_ref)
    WHERE registered;


-- =========================================================================
-- 2. Chain layer: transactions
--
-- What we broadcast or observed, and what it cost. Gas lives here and not on
-- movements, because one transaction can carry many transfers and gas is paid
-- exactly once.
-- =========================================================================
CREATE TABLE IF NOT EXISTS chain_transactions (
                                                  id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),

                                                  network_type  VARCHAR(20)  NOT NULL,
                                                  chain_ref     VARCHAR(50)  NOT NULL,
                                                  tx_hash       VARCHAR(255) NOT NULL,

                                                  intent        VARCHAR(30)  NOT NULL
                                                      CHECK (intent IN ('inbound', 'sweep', 'withdrawal', 'gas_refill',
                                                                        'gas_advance', 'fee_settlement', 'conversion',
                                                                        'external')),

                                                  merchant_id   UUID REFERENCES merchants(id),  -- NULL only for operator-internal

    -- Which ROUTE broadcast this, when we broadcast it at all.
    -- NULL for anything observed rather than initiated.
    -- Deliberately not a foreign key: handlers live in code, not in the DB,
    -- and a removed handler must leave its historical string behind.
                                                  token_id      VARCHAR(100),

    -- Network fee. Its own asset, because on most chains it is not the asset
    -- that moved.
                                                  fee_asset_id  UUID REFERENCES assets(id),
                                                  fee_paid      NUMERIC(78,0) CHECK (fee_paid IS NULL OR fee_paid >= 0),
                                                  fee_payer     VARCHAR(255),

                                                  block_number  BIGINT,
                                                  block_hash    VARCHAR(255),

    -- The economic event time, taken from the block. This is where
    -- ledger_journals.occurred_at comes from; it is NOT created_at.
                                                  block_time    TIMESTAMPTZ,

                                                  status        VARCHAR(20)  NOT NULL
                                                      CHECK (status IN ('submitted', 'detected', 'confirmed', 'final',
                                                                        'orphaned', 'failed')),

                                                  created_at    TIMESTAMPTZ  NOT NULL DEFAULT now(),
                                                  updated_at    TIMESTAMPTZ  NOT NULL DEFAULT now(),

    -- The idempotency latch for the chain layer. tx_hash alone is not unique:
    -- the same hash can legitimately exist on mainnet and on a testnet.
                                                  CONSTRAINT chain_transactions_identity
                                                      UNIQUE (network_type, chain_ref, tx_hash),

                                                  CONSTRAINT chain_transactions_fee_pair
                                                      CHECK ((fee_asset_id IS NULL) = (fee_paid IS NULL)),

    -- Referenced by chain_movements so a movement cannot be attached to a
    -- transaction on a different chain.
                                                  CONSTRAINT chain_transactions_id_chain_uq
                                                      UNIQUE (id, network_type, chain_ref)
);

CREATE INDEX IF NOT EXISTS chain_tx_merchant_idx
    ON chain_transactions (merchant_id, block_time DESC);

CREATE INDEX IF NOT EXISTS chain_tx_status_idx
    ON chain_transactions (network_type, chain_ref, status);

-- Recognition pass: transactions that have reached finality but whose journal
-- has not been written yet is a code-side question; this index serves the scan.
CREATE INDEX IF NOT EXISTS chain_tx_finality_idx
    ON chain_transactions (network_type, chain_ref, block_number)
    WHERE status IN ('detected', 'confirmed');


-- =========================================================================
-- 3. Chain layer: movements
--
-- Which value moved from where to where. Append-only. A reorg flips the
-- parent transaction to 'orphaned' and produces a reversal journal; it never
-- deletes a movement.
-- =========================================================================
CREATE TABLE IF NOT EXISTS chain_movements (
                                               id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),

                                               tx_id         UUID        NOT NULL,
                                               network_type  VARCHAR(20) NOT NULL,
                                               chain_ref     VARCHAR(50) NOT NULL,

    -- The connector's stable ordinal for value-moving events within this tx.
    -- EVM: log_index. UTXO: vout. Solana: a flattened ordinal assigned by the
    -- connector, NOT the bare instruction index -- inner instructions collide
    -- with top-level ones if you use that. The native identifier is kept
    -- verbatim in event_ref for debugging and for re-derivation.
                                               event_index   INT         NOT NULL CHECK (event_index >= 0),
                                               event_ref     VARCHAR(100),           -- e.g. '12', '2.3', 'vout:1'

                                               merchant_id   UUID REFERENCES merchants(id),
                                               invoice_id    UUID REFERENCES invoices(id),
                                               payment_id    UUID REFERENCES payments(id),  -- NULL unless invoice-matched

                                               asset_id      UUID          NOT NULL REFERENCES assets(id),  -- what moved
                                               token_id      VARCHAR(100),                                  -- which route, if any
                                               amount        NUMERIC(78,0) NOT NULL CHECK (amount > 0),

                                               from_address  VARCHAR(255),
                                               from_kind     VARCHAR(20)
                                                   CHECK (from_kind IS NULL OR from_kind IN
                                                                               ('external', 'deposit_address', 'vault', 'merchant_main',
                                                                                'gas', 'operator')),
                                               to_address    VARCHAR(255),
                                               to_kind       VARCHAR(20)
                                                   CHECK (to_kind IS NULL OR to_kind IN
                                                                             ('external', 'deposit_address', 'vault', 'merchant_main',
                                                                              'gas', 'operator')),

                                               created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),

                                               CONSTRAINT chain_movements_identity UNIQUE (tx_id, event_index),

                                               CONSTRAINT chain_movements_event_ref_uq UNIQUE (tx_id, event_ref),

                                               CONSTRAINT chain_movements_tx_fk
                                                   FOREIGN KEY (tx_id, network_type, chain_ref)
                                                       REFERENCES chain_transactions (id, network_type, chain_ref),

    -- Referenced by sweep_queue so a queue row cannot drift from its movement.
                                               CONSTRAINT chain_movements_id_asset_uq UNIQUE (id, asset_id, merchant_id)
);

-- "Where are my unswept funds", answerable without knowing anything about chains.
CREATE INDEX IF NOT EXISTS chain_movements_custody_idx
    ON chain_movements (merchant_id, asset_id, to_kind)
    WHERE to_kind IN ('deposit_address', 'vault');

CREATE INDEX IF NOT EXISTS chain_movements_payment_idx
    ON chain_movements (payment_id)
    WHERE payment_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS chain_movements_to_address_idx
    ON chain_movements (network_type, chain_ref, to_address);

CREATE INDEX IF NOT EXISTS chain_movements_from_address_idx
    ON chain_movements (network_type, chain_ref, from_address);

CREATE INDEX IF NOT EXISTS chain_movements_tx_idx
    ON chain_movements (tx_id);


-- =========================================================================
-- 4. Sweep queue
--
-- Per MOVEMENT, not per address. The sweeper GROUPs claimable rows by
-- custody_address, broadcasts one batched call, and stamps every row in the
-- batch with the same sweep_tx_id. Per-address rows carrying an accumulating
-- amount drift the moment anything fails mid-flight; per-movement rows cannot.
--
-- Rows are enqueued only once the source movement's transaction reaches the
-- finality the recognition pass requires -- enforce that in the enqueue query,
-- not here, since 'final' is per-network policy.
-- =========================================================================
CREATE TABLE IF NOT EXISTS sweep_queue (
                                           id               UUID PRIMARY KEY DEFAULT gen_random_uuid(),

                                           movement_id      UUID          NOT NULL,
                                           merchant_id      UUID          NOT NULL REFERENCES merchants(id),
                                           asset_id         UUID          NOT NULL REFERENCES assets(id),
                                           amount           NUMERIC(78,0) NOT NULL CHECK (amount > 0),

    -- Denormalized from assets purely so the claim query is one index scan.
                                           network_type     VARCHAR(20)   NOT NULL,
                                           chain_ref        VARCHAR(50)   NOT NULL,

    -- WHERE the value sits. custody_kind reuses the chain_movements.to_kind
    -- vocabulary on purpose: the correspondence between to_kind and the
    -- custody_* accounts is what makes the invariant check meaningful, and a
    -- parallel address-type vocabulary breaks it silently.
                                           custody_address  VARCHAR(255)  NOT NULL,
                                           custody_kind     VARCHAR(20)   NOT NULL
                                               CHECK (custody_kind IN ('deposit_address', 'vault')),

    -- WHO can move it. Identical to custody_address on EVM HD wallets; on
    -- Solana SPL the value sits in an ATA while the signing key belongs to the
    -- owner; in a vault the signer is the controlling contract's authority.
    -- Collapsing these two works until Solana, then does not.
                                           authority_address VARCHAR(255) NOT NULL,
                                           authority_ref     VARCHAR(100),  -- derivation index, seed, vault slot id

    -- The route to sweep WITH, if the operator pinned one. NULL means
    -- "resolve at sweep time from handlers advertising this asset, filtered by
    -- can_sweep". Never used as an identity -- a payment taken through
    -- USDC_BASE is swept perfectly well through USDC_BASE_CRATES.
                                           token_id         VARCHAR(100),

    -- Everything a specific chain needs and no other chain does: token program
    -- and ATA for Solana, memo, UTXO outpoints, vault call selector.
                                           sweep_params     JSONB         NOT NULL DEFAULT '{}'::jsonb,

                                           status           VARCHAR(20)   NOT NULL DEFAULT 'pending'
                                               CHECK (status IN ('pending', 'claimed', 'broadcast',
                                                                 'swept', 'failed', 'abandoned')),

    -- Deferral and backoff in one column: gas above threshold, amount below
    -- dust floor, or an nth retry all just push this forward.
                                           available_at     TIMESTAMPTZ   NOT NULL DEFAULT now(),

                                           claim_id         UUID,
                                           claim_expires_at TIMESTAMPTZ,
                                           attempts         INT           NOT NULL DEFAULT 0,
                                           last_error       TEXT,

                                           sweep_tx_id      UUID REFERENCES chain_transactions(id),

                                           created_at       TIMESTAMPTZ   NOT NULL DEFAULT now(),
                                           updated_at       TIMESTAMPTZ   NOT NULL DEFAULT now(),

    -- One queue row per movement, ever. This is the enqueue idempotency latch.
                                           CONSTRAINT sweep_queue_movement_uq UNIQUE (movement_id),

    -- The queue row cannot disagree with its movement about asset or owner.
                                           CONSTRAINT sweep_queue_movement_fk
                                               FOREIGN KEY (movement_id, asset_id, merchant_id)
                                                   REFERENCES chain_movements (id, asset_id, merchant_id),

                                           CONSTRAINT sweep_queue_claim_pair
                                               CHECK ((status = 'claimed') = (claim_id IS NOT NULL)),

                                           CONSTRAINT sweep_queue_broadcast_has_tx
                                               CHECK (status NOT IN ('broadcast', 'swept') OR sweep_tx_id IS NOT NULL)
);

-- The claim query. merchant_id leads because a sweep batch cannot cross
-- merchants: gas is one wallet per merchant by design, so the fee payer (and on
-- EVM, the sender) is merchant-scoped. A cross-merchant batch would have one
-- merchant paying another's gas, which is precisely the isolation the
-- per-merchant gas wallet exists to provide.
CREATE INDEX IF NOT EXISTS sweep_queue_claimable_idx
    ON sweep_queue (network_type, chain_ref, merchant_id, asset_id,
                    available_at, custody_address)
    WHERE status = 'pending';

-- Lease reaper: claims whose holder died.
CREATE INDEX IF NOT EXISTS sweep_queue_expired_claims_idx
    ON sweep_queue (claim_expires_at)
    WHERE status = 'claimed';

-- "What is this merchant waiting on", for the dashboard.
CREATE INDEX IF NOT EXISTS sweep_queue_merchant_idx
    ON sweep_queue (merchant_id, asset_id, status);

CREATE INDEX IF NOT EXISTS sweep_queue_tx_idx
    ON sweep_queue (sweep_tx_id)
    WHERE sweep_tx_id IS NOT NULL;


-- =========================================================================
-- 5. Ledger: accounts
--
-- (merchant, kind, asset). Never mixed-asset: summing across assets requires
-- a price, and prices do not belong in the ledger.
-- =========================================================================
CREATE TABLE IF NOT EXISTS ledger_accounts (
                                               id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),

                                               merchant_id UUID REFERENCES merchants(id),  -- NULL for operator/system
                                               kind        VARCHAR(40) NOT NULL
                                                   CHECK (kind IN ('custody_unswept', 'custody_treasury', 'custody_gas',
                                                                   'custody_unsupported', 'custody_operator',
                                                                   'payable_to_merchant', 'fees_receivable',
                                                                   'gas_advance_receivable', 'fee_revenue',
                                                                   'suspense_unexplained')),

                                               asset_id    UUID        NOT NULL,
    -- Denormalized, but held true by the composite FK below, so it cannot
    -- drift. Present so that "gas accounts are native-only" is declarative.
                                               asset_kind  VARCHAR(20) NOT NULL,

                                               created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),

                                               CONSTRAINT ledger_accounts_identity
                                                   UNIQUE NULLS NOT DISTINCT (merchant_id, kind, asset_id),

                                               CONSTRAINT ledger_accounts_asset_fk
                                                   FOREIGN KEY (asset_id, asset_kind) REFERENCES assets (id, asset_kind),

    -- Merchant-scoped kinds must have a merchant; operator kinds must not.
                                               CONSTRAINT ledger_accounts_scope
                                                   CHECK (
                                                       (merchant_id IS NOT NULL AND kind IN
                                                                                    ('custody_unswept', 'custody_treasury', 'custody_gas',
                                                                                     'custody_unsupported', 'payable_to_merchant',
                                                                                     'fees_receivable', 'gas_advance_receivable',
                                                                                     'suspense_unexplained'))
                                                           OR
                                                       (merchant_id IS NULL AND kind IN
                                                                                ('custody_operator', 'fee_revenue'))
                                                       ),

    -- Gas is denominated in the chain's native asset, always.
                                               CONSTRAINT ledger_accounts_gas_is_native
                                                   CHECK (kind NOT IN ('custody_gas', 'gas_advance_receivable')
                                                       OR asset_kind = 'native'),

    -- Referenced by ledger_entries: an entry cannot name an asset its account
    -- does not hold.
                                               CONSTRAINT ledger_accounts_id_asset_uq UNIQUE (id, asset_id)
);

CREATE INDEX IF NOT EXISTS ledger_accounts_merchant_idx
    ON ledger_accounts (merchant_id, asset_id);

CREATE INDEX IF NOT EXISTS ledger_accounts_kind_idx
    ON ledger_accounts (kind, asset_id);


-- =========================================================================
-- 6. Ledger: journals
--
-- Strictly append-only. Corrections are reversals, never edits.
-- =========================================================================
CREATE TABLE IF NOT EXISTS ledger_journals (
                                               id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),

                                               kind         VARCHAR(40) NOT NULL
                                                   CHECK (kind IN ('payment_recognized', 'sweep', 'withdrawal',
                                                                   'gas_refill', 'gas_advance', 'gas_burn_failed',
                                                                   'conversion', 'fee_settlement',
                                                                   'external_credit', 'external_debit',
                                                                   'probe_adjustment', 'reclassification', 'reversal')),

    -- The latch. payment_recognized:<payment_id>, sweep:<tx_id>,
    -- external_credit:<tx_id>:<event_index>, reversal:<journal_id>, ...
    -- A worker that runs twice writes one journal.
                                               dedupe_key   VARCHAR(255) NOT NULL UNIQUE,

                                               merchant_id  UUID REFERENCES merchants(id),
                                               tx_id        UUID REFERENCES chain_transactions(id),
                                               payment_id   UUID REFERENCES payments(id),

                                               reverses     UUID REFERENCES ledger_journals(id),

    -- Fee rate snapshot, route used, policy version, oracle quote.
    -- {"fee_bps": 100, "token_id": "USDC_BASE", "rate_source": "merchant_override"}
                                               metadata     JSONB,

                                               occurred_at  TIMESTAMPTZ NOT NULL,              -- block time
                                               created_at   TIMESTAMPTZ NOT NULL DEFAULT now() -- when we recorded it
);

-- A journal can be reversed exactly once.
CREATE UNIQUE INDEX IF NOT EXISTS ledger_journals_reverses_uq
    ON ledger_journals (reverses)
    WHERE reverses IS NOT NULL;

CREATE INDEX IF NOT EXISTS ledger_journals_merchant_idx
    ON ledger_journals (merchant_id, occurred_at DESC);

CREATE INDEX IF NOT EXISTS ledger_journals_tx_idx
    ON ledger_journals (tx_id)
    WHERE tx_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS ledger_journals_kind_idx
    ON ledger_journals (kind, occurred_at DESC);


-- =========================================================================
-- 7. Ledger: entries
--
-- Signed base units. Positive = debit, negative = credit.
-- =========================================================================
CREATE TABLE IF NOT EXISTS ledger_entries (
                                              entry_no    BIGSERIAL PRIMARY KEY,

                                              journal_id  UUID          NOT NULL REFERENCES ledger_journals(id),
                                              account_id  UUID          NOT NULL,

    -- Duplicated from ledger_accounts deliberately: it is what the balance
    -- constraint groups by, and having it on the entry means that constraint
    -- needs no join. The composite FK below is what keeps the two in agreement.
                                              asset_id    UUID          NOT NULL,

                                              amount      NUMERIC(78,0) NOT NULL CHECK (amount <> 0),

                                              CONSTRAINT ledger_entries_account_fk
                                                  FOREIGN KEY (account_id, asset_id)
                                                      REFERENCES ledger_accounts (id, asset_id)
);

CREATE INDEX IF NOT EXISTS ledger_entries_account_idx
    ON ledger_entries (account_id, entry_no);

CREATE INDEX IF NOT EXISTS ledger_entries_journal_idx
    ON ledger_entries (journal_id);

CREATE INDEX IF NOT EXISTS ledger_entries_asset_idx
    ON ledger_entries (asset_id);


-- =========================================================================
-- 8. Fee rates
--
-- Per ROUTE, not per asset: two routes for one asset may genuinely cost the
-- operator different amounts to run. The resulting receivables are per asset
-- (see the views) -- these do not conflict, because they happen at different
-- moments. Versioned rather than mutated, so recomputing a historical accrual
-- gives the historical answer.
-- =========================================================================
CREATE TABLE IF NOT EXISTS fee_rates (
                                         id             UUID PRIMARY KEY DEFAULT gen_random_uuid(),

                                         merchant_id    UUID REFERENCES merchants(id),  -- NULL = operator default
                                         token_id       VARCHAR(100) NOT NULL,          -- route, matches TokenRegistry
                                         basis_points   INT          NOT NULL CHECK (basis_points >= 0 AND basis_points <= 10000),

                                         effective_from TIMESTAMPTZ  NOT NULL DEFAULT now(),
                                         created_at     TIMESTAMPTZ  NOT NULL DEFAULT now(),

                                         CONSTRAINT fee_rates_identity
                                             UNIQUE NULLS NOT DISTINCT (merchant_id, token_id, effective_from)
);

-- Resolution: merchant override, else operator default, latest row with
-- effective_from <= payment.occurred_at.
CREATE INDEX IF NOT EXISTS fee_rates_lookup_idx
    ON fee_rates (token_id, merchant_id, effective_from DESC);


-- =========================================================================
-- 9. Integrity triggers
-- =========================================================================

-- ---- 9.1 Append-only enforcement -------------------------------------------
-- The FOR EACH STATEMENT / TRUNCATE variant matters: a row trigger does not
-- fire on TRUNCATE, and TRUNCATE is exactly how an audit trail gets lost.
CREATE OR REPLACE FUNCTION ledger_reject_mutation() RETURNS trigger
    LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION
        '% is append-only; attempted %. Corrections are reversal journals, never edits.',
        TG_TABLE_NAME, TG_OP
        USING ERRCODE = 'restrict_violation';
END;
$$;

DROP TRIGGER IF EXISTS ledger_journals_append_only ON ledger_journals;
CREATE TRIGGER ledger_journals_append_only
    BEFORE UPDATE OR DELETE ON ledger_journals
    FOR EACH ROW EXECUTE FUNCTION ledger_reject_mutation();

DROP TRIGGER IF EXISTS ledger_journals_no_truncate ON ledger_journals;
CREATE TRIGGER ledger_journals_no_truncate
    BEFORE TRUNCATE ON ledger_journals
    FOR EACH STATEMENT EXECUTE FUNCTION ledger_reject_mutation();

DROP TRIGGER IF EXISTS ledger_entries_append_only ON ledger_entries;
CREATE TRIGGER ledger_entries_append_only
    BEFORE UPDATE OR DELETE ON ledger_entries
    FOR EACH ROW EXECUTE FUNCTION ledger_reject_mutation();

DROP TRIGGER IF EXISTS ledger_entries_no_truncate ON ledger_entries;
CREATE TRIGGER ledger_entries_no_truncate
    BEFORE TRUNCATE ON ledger_entries
    FOR EACH STATEMENT EXECUTE FUNCTION ledger_reject_mutation();

-- Movements are append-only too. A reorg sets chain_transactions.status to
-- 'orphaned' and writes a reversal journal; it does not delete the movement
-- that was observed.
DROP TRIGGER IF EXISTS chain_movements_append_only ON chain_movements;
CREATE TRIGGER chain_movements_append_only
    BEFORE UPDATE OR DELETE ON chain_movements
    FOR EACH ROW EXECUTE FUNCTION ledger_reject_mutation();


-- ---- 9.2 Every journal balances, per asset ---------------------------------
-- Per-asset, not per-journal: a sweep journal carries USDC legs and ETH gas
-- legs together and those are not commensurable. Requiring the whole journal
-- to sum to zero would force a conversion that never happened.
--
-- DEFERRABLE INITIALLY DEFERRED so the legs can be inserted one at a time
-- within a transaction and are only checked at COMMIT.
CREATE OR REPLACE FUNCTION ledger_assert_journal_balanced() RETURNS trigger
    LANGUAGE plpgsql AS $$
DECLARE
    offending RECORD;
BEGIN
    SELECT e.asset_id, SUM(e.amount) AS total
    INTO offending
    FROM ledger_entries e
    WHERE e.journal_id = NEW.journal_id
    GROUP BY e.asset_id
    HAVING SUM(e.amount) <> 0
    LIMIT 1;

    IF FOUND THEN
        RAISE EXCEPTION
            'journal % does not balance for asset %: debits/credits sum to %',
            NEW.journal_id, offending.asset_id, offending.total
            USING ERRCODE = 'check_violation';
    END IF;

    RETURN NULL;
END;
$$;

DROP TRIGGER IF EXISTS ledger_entries_balanced ON ledger_entries;
CREATE CONSTRAINT TRIGGER ledger_entries_balanced
    AFTER INSERT ON ledger_entries
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION ledger_assert_journal_balanced();


-- ---- 9.3 No empty journals -------------------------------------------------
-- The balance trigger above fires on entries, so a journal with zero entries
-- passes it vacuously. An empty journal holds a dedupe_key -- meaning the work
-- it represents can never be retried -- while recording nothing.
CREATE OR REPLACE FUNCTION ledger_assert_journal_nonempty() RETURNS trigger
    LANGUAGE plpgsql AS $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM ledger_entries WHERE journal_id = NEW.id) THEN
        RAISE EXCEPTION 'journal % (%) was committed with no entries',
            NEW.id, NEW.kind
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NULL;
END;
$$;

DROP TRIGGER IF EXISTS ledger_journals_nonempty ON ledger_journals;
CREATE CONSTRAINT TRIGGER ledger_journals_nonempty
    AFTER INSERT ON ledger_journals
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION ledger_assert_journal_nonempty();


-- ---- 9.4 A reversal reverses exactly, and only reverses -------------------
-- kind = 'reversal' iff reverses IS NOT NULL, and the legs of a reversal must
-- be the exact negation of the original per asset.
CREATE OR REPLACE FUNCTION ledger_assert_reversal_exact() RETURNS trigger
    LANGUAGE plpgsql AS $$
DECLARE
    offending RECORD;
BEGIN
    IF (NEW.kind = 'reversal') <> (NEW.reverses IS NOT NULL) THEN
        RAISE EXCEPTION
            'journal %: kind=''reversal'' and a non-null reverses must be set together',
            NEW.id
            USING ERRCODE = 'check_violation';
    END IF;

    IF NEW.reverses IS NULL THEN
        RETURN NULL;
    END IF;

    SELECT account_id, asset_id, SUM(amount) AS residual
    INTO offending
    FROM ledger_entries
    WHERE journal_id IN (NEW.id, NEW.reverses)
    GROUP BY account_id, asset_id
    HAVING SUM(amount) <> 0
    LIMIT 1;

    IF FOUND THEN
        RAISE EXCEPTION
            'reversal % does not cancel journal % on account %/asset %: residual %',
            NEW.id, NEW.reverses, offending.account_id, offending.asset_id,
            offending.residual
            USING ERRCODE = 'check_violation';
    END IF;

    RETURN NULL;
END;
$$;

DROP TRIGGER IF EXISTS ledger_journals_reversal_exact ON ledger_journals;
CREATE CONSTRAINT TRIGGER ledger_journals_reversal_exact
    AFTER INSERT ON ledger_journals
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION ledger_assert_reversal_exact();


-- ---- 9.5 updated_at -------------------------------------------------------
CREATE OR REPLACE FUNCTION set_updated_at() RETURNS trigger
    LANGUAGE plpgsql AS $$
BEGIN
    NEW.updated_at := now();
    RETURN NEW;
END;
$$;

DROP TRIGGER IF EXISTS assets_updated_at ON assets;
CREATE TRIGGER assets_updated_at
    BEFORE UPDATE ON assets
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();

DROP TRIGGER IF EXISTS chain_transactions_updated_at ON chain_transactions;
CREATE TRIGGER chain_transactions_updated_at
    BEFORE UPDATE ON chain_transactions
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();

DROP TRIGGER IF EXISTS sweep_queue_updated_at ON sweep_queue;
CREATE TRIGGER sweep_queue_updated_at
    BEFORE UPDATE ON sweep_queue
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();


-- ---- 9.6 assets: identity and decimals are immutable ----------------------
-- Two handlers advertising the same address while disagreeing about decimals
-- is a silent error of up to twelve orders of magnitude, and it is
-- unrecoverable once entries exist. sync_assets treats it as a boot failure;
-- this trigger is the backstop against anything else.
CREATE OR REPLACE FUNCTION assets_reject_identity_change() RETURNS trigger
    LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.network_type IS DISTINCT FROM OLD.network_type
        OR NEW.chain_ref  IS DISTINCT FROM OLD.chain_ref
        OR NEW.asset_kind IS DISTINCT FROM OLD.asset_kind
        OR NEW.address    IS DISTINCT FROM OLD.address
        OR NEW.decimals   IS DISTINCT FROM OLD.decimals
    THEN
        RAISE EXCEPTION
            'asset % identity/decimals are immutable; changing them corrupts every entry written against it',
            OLD.id
            USING ERRCODE = 'restrict_violation';
    END IF;
    RETURN NEW;
END;
$$;

DROP TRIGGER IF EXISTS assets_immutable_identity ON assets;
CREATE TRIGGER assets_immutable_identity
    BEFORE UPDATE ON assets
    FOR EACH ROW EXECUTE FUNCTION assets_reject_identity_change();


-- =========================================================================
-- 10. Derived views
--
-- Balances are always derived, never stored and never incremented.
-- =========================================================================

-- 10.1 Every account balance, with its asset resolved.
CREATE OR REPLACE VIEW v_ledger_balances AS
SELECT
    la.id           AS account_id,
    la.merchant_id,
    la.kind,
    la.asset_id,
    a.network_type,
    a.chain_ref,
    a.asset_kind,
    a.address       AS asset_address,
    a.symbol,
    a.decimals,
    a.registered    AS asset_registered,
    COALESCE(SUM(le.amount), 0) AS balance,
    COUNT(le.entry_no)          AS entry_count,
    MAX(lj.occurred_at)         AS last_activity_at
FROM ledger_accounts la
         JOIN assets a           ON a.id = la.asset_id
         LEFT JOIN ledger_entries le  ON le.account_id = la.id
         LEFT JOIN ledger_journals lj ON lj.id = le.journal_id
GROUP BY la.id, a.id;

COMMENT ON VIEW v_ledger_balances IS
    'Signed balances: positive = debit. Assets carry a positive balance; liabilities and revenue a negative one. payable_to_merchant of -150 means the merchant is owed 150.';


-- 10.2 What a merchant holds and what they owe, per asset.
-- Withdrawability also depends on a handler advertising can_sweep/can_withdraw,
-- which lives in code -- asset_registered is the DB-side half of that filter.
CREATE OR REPLACE VIEW v_merchant_positions AS
SELECT
    la.merchant_id,
    la.asset_id,
    a.network_type,
    a.chain_ref,
    a.symbol,
    a.decimals,
    a.registered AS asset_registered,
    COALESCE(SUM(le.amount) FILTER (WHERE la.kind = 'custody_unswept'),     0) AS unswept,
    COALESCE(SUM(le.amount) FILTER (WHERE la.kind = 'custody_treasury'),    0) AS treasury,
    COALESCE(SUM(le.amount) FILTER (WHERE la.kind = 'custody_gas'),         0) AS gas,
    COALESCE(SUM(le.amount) FILTER (WHERE la.kind = 'custody_unsupported'), 0) AS unsupported,
    -- Negated: the liability is stored as a credit.
    -COALESCE(SUM(le.amount) FILTER (WHERE la.kind = 'payable_to_merchant'), 0) AS owed_to_merchant,
    COALESCE(SUM(le.amount) FILTER (WHERE la.kind = 'fees_receivable'),        0) AS fees_owed_by_merchant,
    COALESCE(SUM(le.amount) FILTER (WHERE la.kind = 'gas_advance_receivable'), 0) AS gas_advanced,
    COALESCE(SUM(le.amount) FILTER (WHERE la.kind = 'suspense_unexplained'),   0) AS unexplained
FROM ledger_accounts la
         JOIN assets a               ON a.id = la.asset_id
         LEFT JOIN ledger_entries le ON le.account_id = la.id
WHERE la.merchant_id IS NOT NULL
GROUP BY la.merchant_id, la.asset_id, a.id;


-- 10.3 Sweep backlog, grouped the way the sweeper batches it.
CREATE OR REPLACE VIEW v_sweep_backlog AS
SELECT
    sq.merchant_id,
    sq.network_type,
    sq.chain_ref,
    sq.asset_id,
    a.symbol,
    a.decimals,
    sq.custody_address,
    sq.custody_kind,
    sq.authority_address,
    COUNT(*)               AS movement_count,
    SUM(sq.amount)         AS total_amount,
    MIN(sq.created_at)     AS oldest_enqueued_at,
    MIN(sq.available_at)   AS next_available_at,
    MAX(sq.attempts)       AS max_attempts
FROM sweep_queue sq
         JOIN assets a ON a.id = sq.asset_id
WHERE sq.status = 'pending'
GROUP BY sq.merchant_id, sq.network_type, sq.chain_ref, sq.asset_id, a.id,
         sq.custody_address, sq.custody_kind, sq.authority_address;


-- 10.4 The invariant that matters: the queue and the ledger should agree
-- about how much value is sitting unswept. A non-zero drift means either a
-- journal was written without enqueuing, or a queue row was marked swept
-- without a corresponding sweep journal.
CREATE OR REPLACE VIEW v_unswept_reconciliation AS
WITH queued AS (
    SELECT
        merchant_id,
        asset_id,
        COALESCE(SUM(amount) FILTER (
            WHERE status IN ('pending', 'claimed', 'broadcast')), 0) AS active_amount,
        -- Abandoned value is still unswept. It is just never going to be swept:
        -- gas dust below the economic floor, typically. Dropping it from this
        -- side of the comparison would report it as drift forever, which trains
        -- everyone to ignore the alarm.
        COALESCE(SUM(amount) FILTER (WHERE status = 'abandoned'), 0) AS abandoned_amount
    FROM sweep_queue
    WHERE status IN ('pending', 'claimed', 'broadcast', 'abandoned')
    GROUP BY merchant_id, asset_id
),
     booked AS (
         SELECT la.merchant_id, la.asset_id, COALESCE(SUM(le.amount), 0) AS booked_amount
         FROM ledger_accounts la
                  LEFT JOIN ledger_entries le ON le.account_id = la.id
         WHERE la.kind = 'custody_unswept'
         GROUP BY la.merchant_id, la.asset_id
     )
SELECT
    COALESCE(q.merchant_id, b.merchant_id) AS merchant_id,
    COALESCE(q.asset_id,    b.asset_id)    AS asset_id,
    a.symbol,
    a.network_type,
    a.chain_ref,
    COALESCE(b.booked_amount, 0)    AS ledger_unswept,
    COALESCE(q.active_amount, 0)    AS queue_active,
    COALESCE(q.abandoned_amount, 0) AS queue_abandoned,
    COALESCE(b.booked_amount, 0)
        - COALESCE(q.active_amount, 0)
        - COALESCE(q.abandoned_amount, 0) AS drift
FROM queued q
         FULL OUTER JOIN booked b
                         ON b.merchant_id IS NOT DISTINCT FROM q.merchant_id
                             AND b.asset_id = q.asset_id
         JOIN assets a ON a.id = COALESCE(q.asset_id, b.asset_id);

COMMENT ON VIEW v_unswept_reconciliation IS
    'drift should be zero. Non-zero is a monitoring alarm, not a number to reconcile away by hand.';