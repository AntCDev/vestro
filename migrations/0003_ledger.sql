-- =========================================================
-- 002_ledger.sql
--
-- Ledgering layer for the multichain payment processor.
-- Continues the numbering of the base schema (merchants … network_address_cursors).
--
-- Layers, in the one direction truth flows:
--
--   chain ──▶ chain_transactions ──▶ chain_movements ──▶ ledger_journals/entries
--            (what happened)        (what moved)        (what it means)
--
-- sweep_queue sits beside chain_movements as *operational* state: mutable,
-- rebuildable, and deliberately not part of the audit trail.
--
-- Requires PostgreSQL 15+ (UNIQUE NULLS NOT DISTINCT).
-- =========================================================


-- =========================================================
-- 10. Assets
--
-- Identity is a fact about a chain: (network_type, chain_ref, asset_kind, address).
-- Never a token_id. Routes are code and are deletable; assets are not.
-- =========================================================
CREATE TABLE IF NOT EXISTS assets (
    id             UUID PRIMARY KEY DEFAULT gen_random_uuid(),

    network_type   VARCHAR(20)  NOT NULL,          -- 'evm' | 'solana' | 'esplora'
    chain_ref      VARCHAR(50)  NOT NULL,          -- '8453' | 'mainnet-beta' | 'bitcoin' | …
    asset_kind     VARCHAR(20)  NOT NULL
        CHECK (asset_kind IN ('native', 'contract')),
    address        VARCHAR(255),                   -- canonical; NULL iff asset_kind='native'

    decimals       SMALLINT     NOT NULL CHECK (decimals >= 0 AND decimals <= 36),
    symbol         VARCHAR(32)  NOT NULL,

    registered     BOOLEAN      NOT NULL DEFAULT false,  -- some handler advertises it
    first_seen_at  TIMESTAMPTZ  NOT NULL DEFAULT now(),

    CHECK ((asset_kind = 'native') = (address IS NULL)),

    -- Canonicalization is enforced, not merely conventional. Solana is
    -- case-sensitive base58 and cannot be folded; it is validated at
    -- registration instead (charset + 32-byte decoded length).
    CHECK (network_type <> 'evm'     OR address = lower(address)),
    CHECK (network_type <> 'esplora' OR address = lower(address)),

    CONSTRAINT assets_identity
        UNIQUE NULLS NOT DISTINCT (network_type, chain_ref, asset_kind, address)
);

CREATE INDEX IF NOT EXISTS assets_chain_idx
    ON assets (network_type, chain_ref);

-- Withdrawal/route pickers only ever look at assets some handler claims.
CREATE INDEX IF NOT EXISTS assets_registered_idx
    ON assets (network_type, chain_ref)
    WHERE registered;


-- =========================================================
-- 11. Chain transactions
--
-- What we broadcast or observed, and what it cost.
-- status / block_number / block_hash / block_time are mutable;
-- everything else is fixed at insert.
-- =========================================================
CREATE TABLE IF NOT EXISTS chain_transactions (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),

    network_type  VARCHAR(20)  NOT NULL,
    chain_ref     VARCHAR(50)  NOT NULL,
    tx_hash       VARCHAR(255) NOT NULL,

    intent        VARCHAR(30)  NOT NULL
        CHECK (intent IN ('inbound', 'sweep', 'withdrawal', 'gas_refill',
                          'gas_advance', 'fee_settlement', 'conversion', 'external')),

    merchant_id   UUID REFERENCES merchants(id),   -- NULL only for operator-internal txs

    -- Which route broadcast this, when the system broadcast it at all.
    -- Deliberately NOT a foreign key: handlers live in code, not in the DB.
    -- A removed handler leaves a historical string behind, which is the point.
    token_id      VARCHAR(100),

    -- Gas / network fee. Its own asset, because on most chains it is not
    -- the asset that moved.
    fee_asset_id  UUID REFERENCES assets(id),
    fee_paid      NUMERIC(78,0) CHECK (fee_paid IS NULL OR fee_paid >= 0),
    fee_payer     VARCHAR(255),

    block_number  BIGINT,
    block_hash    VARCHAR(255),
    block_time    TIMESTAMPTZ,                     -- sources ledger_journals.occurred_at

    status        VARCHAR(20)  NOT NULL
        CHECK (status IN ('submitted', 'detected', 'confirmed',
                          'final', 'orphaned', 'failed')),

    created_at    TIMESTAMPTZ  NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ  NOT NULL DEFAULT now(),

    -- Nothing the system did not initiate has a route.
    CHECK (intent <> 'external' OR token_id IS NULL),

    -- Fee asset and fee amount travel together or not at all.
    CHECK ((fee_asset_id IS NULL) = (fee_paid IS NULL)),

    -- Idempotency latch for the chain layer. tx_hash alone is not unique:
    -- the same hash can legitimately exist on mainnet and on a testnet.
    UNIQUE (network_type, chain_ref, tx_hash)
);

-- The recognition worker only cares about transactions not yet at rest.
CREATE INDEX IF NOT EXISTS chain_transactions_open_idx
    ON chain_transactions (network_type, chain_ref, block_number)
    WHERE status IN ('submitted', 'detected', 'confirmed');

CREATE INDEX IF NOT EXISTS chain_transactions_merchant_idx
    ON chain_transactions (merchant_id, intent, created_at DESC);

-- Reorg handling: find everything anchored to a block that no longer exists.
CREATE INDEX IF NOT EXISTS chain_transactions_block_idx
    ON chain_transactions (network_type, chain_ref, block_number)
    WHERE block_number IS NOT NULL;


-- =========================================================
-- 12. Chain movements
--
-- Which value moved from where to where. Append-only.
-- One transaction, many movements: this is what keeps SUM(gas) honest.
-- =========================================================
CREATE TABLE IF NOT EXISTS chain_movements (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tx_id         UUID NOT NULL REFERENCES chain_transactions(id),
    event_index   INT  NOT NULL,   -- log_index / vout / instruction ordinal; 0 for native

    merchant_id   UUID REFERENCES merchants(id),
    invoice_id    UUID REFERENCES invoices(id),
    payment_id    UUID REFERENCES payments(id),   -- only invoice-matched inbound has one

    asset_id      UUID NOT NULL REFERENCES assets(id),   -- what moved
    token_id      VARCHAR(100),                          -- which route moved it, if any
    amount        NUMERIC(78,0) NOT NULL CHECK (amount > 0),

    from_address  VARCHAR(255),
    from_kind     VARCHAR(20)
        CHECK (from_kind IS NULL OR from_kind IN
              ('external', 'deposit_address', 'vault', 'merchant_main', 'gas', 'operator')),
    to_address    VARCHAR(255),
    to_kind       VARCHAR(20)
        CHECK (to_kind IS NULL OR to_kind IN
              ('external', 'deposit_address', 'vault', 'merchant_main', 'gas', 'operator')),

    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),

    -- A movement with neither end is not a movement.
    CHECK (from_address IS NOT NULL OR to_address IS NOT NULL),

    UNIQUE (tx_id, event_index)
);

-- "Where are my unswept funds" without knowing anything about chains.
CREATE INDEX IF NOT EXISTS chain_movements_custody_idx
    ON chain_movements (merchant_id, asset_id, to_kind)
    WHERE to_kind IN ('deposit_address', 'vault');

CREATE INDEX IF NOT EXISTS chain_movements_asset_idx
    ON chain_movements (asset_id, created_at DESC);

CREATE INDEX IF NOT EXISTS chain_movements_payment_idx
    ON chain_movements (payment_id) WHERE payment_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS chain_movements_tx_idx
    ON chain_movements (tx_id);

-- Reconciliation reaches for addresses, not ids.
CREATE INDEX IF NOT EXISTS chain_movements_to_addr_idx
    ON chain_movements (to_address) WHERE to_address IS NOT NULL;
CREATE INDEX IF NOT EXISTS chain_movements_from_addr_idx
    ON chain_movements (from_address) WHERE from_address IS NOT NULL;


-- =========================================================
-- 13. Sweep queue
--
-- Operational work state, NOT part of the audit trail. Every row here is
-- derivable from chain_movements + the ledger; if this table were dropped it
-- could be rebuilt. That is why it is the only table in this file that is
-- freely mutable.
--
-- One row per movement, not per address. The sweeper GROUPs rows into a batch
-- (a single vault call sweeping twelve deposit addresses), stamps all of them
-- with the same sweep_tx_id, and the twelve custody_unswept legs cancel.
-- Per-address rows with an accumulating amount would drift; per-movement rows
-- cannot.
--
-- Rows are enqueued in the same DB transaction as the payment_recognized
-- journal — recognized, not merely detected. Never sweep on detection.
-- =========================================================
CREATE TABLE IF NOT EXISTS sweep_queue (
    id                UUID PRIMARY KEY DEFAULT gen_random_uuid(),

    movement_id       UUID NOT NULL UNIQUE REFERENCES chain_movements(id),
    merchant_id       UUID NOT NULL REFERENCES merchants(id) ON DELETE CASCADE,
    asset_id          UUID NOT NULL REFERENCES assets(id),

    -- Denormalised so the claim query touches exactly one table.
    network_type      VARCHAR(20)  NOT NULL,
    chain_ref         VARCHAR(50)  NOT NULL,

    -- Where the value physically sits (== chain_movements.to_address).
    custody_address   VARCHAR(255) NOT NULL,
    -- Same vocabulary as chain_movements.to_kind, restricted to sweepable kinds.
    -- On EVM this is what distinguishes an HD deposit address from a contract vault.
    custody_kind      VARCHAR(20)  NOT NULL
        CHECK (custody_kind IN ('deposit_address', 'vault')),

    -- Whose key signs. Equal to custody_address on EVM HD wallets; the ATA
    -- owner on Solana SPL; the controlling address for a vault.
    -- NULL means "same as custody_address".
    authority_address VARCHAR(255),
    wallet_index      INT,          -- HD derivation index; NULL for vaults

    -- Anything the sweeper needs that is network-shaped: token program id,
    -- ATA, vault salt, memo, UTXO outpoints. Keeps this table from growing a
    -- column per chain.
    sweep_params      JSONB,

    amount            NUMERIC(78,0) NOT NULL CHECK (amount > 0),

    status            VARCHAR(20)  NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'claimed', 'broadcast',
                          'swept', 'failed', 'abandoned')),

    sweep_tx_id       UUID REFERENCES chain_transactions(id),

    attempts          INT          NOT NULL DEFAULT 0,
    last_error        TEXT,

    -- Backoff, and the gas-price threshold defer. The sweeper reads
    -- WHERE available_at <= now().
    available_at      TIMESTAMPTZ  NOT NULL DEFAULT now(),

    claimed_by        VARCHAR(64),   -- worker identity
    claimed_at        TIMESTAMPTZ,

    created_at        TIMESTAMPTZ  NOT NULL DEFAULT now(),
    updated_at        TIMESTAMPTZ  NOT NULL DEFAULT now(),

    -- A terminal-success row must point at the transaction that did it.
    CHECK (status <> 'swept' OR sweep_tx_id IS NOT NULL),
    CHECK (status NOT IN ('claimed', 'broadcast') OR claimed_by IS NOT NULL)
);

-- The claim query: FOR UPDATE SKIP LOCKED over one (chain, asset) at a time.
CREATE INDEX IF NOT EXISTS sweep_queue_claimable_idx
    ON sweep_queue (network_type, chain_ref, asset_id, available_at)
    WHERE status IN ('pending', 'failed');

-- Batching: group claimable rows by the address they will be swept from.
CREATE INDEX IF NOT EXISTS sweep_queue_batch_idx
    ON sweep_queue (merchant_id, asset_id, custody_address)
    WHERE status IN ('pending', 'failed');

-- Recovering rows a worker claimed and then died holding.
CREATE INDEX IF NOT EXISTS sweep_queue_stuck_idx
    ON sweep_queue (claimed_at)
    WHERE status IN ('claimed', 'broadcast');

CREATE INDEX IF NOT EXISTS sweep_queue_tx_idx
    ON sweep_queue (sweep_tx_id) WHERE sweep_tx_id IS NOT NULL;


-- =========================================================
-- 14. Ledger accounts
--
-- (merchant, kind, asset). Never mixed-asset: summing across assets requires
-- a price, and prices do not belong in the ledger.
-- =========================================================
CREATE TABLE IF NOT EXISTS ledger_accounts (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    merchant_id UUID REFERENCES merchants(id),   -- NULL for operator / system accounts
    kind        VARCHAR(40) NOT NULL
        CHECK (kind IN (
            'custody_unswept',        -- asset:     at a deposit address or in the vault
            'custody_treasury',       -- asset:     merchant main wallet
            'custody_gas',            -- asset:     merchant gas account (native only)
            'custody_unsupported',    -- asset:     owned, real, no handler, unmovable
            'custody_operator',       -- asset:     where settled fees land
            'payable_to_merchant',    -- liability: what the merchant can withdraw
            'fees_receivable',        -- asset:     accrued, unsettled
            'gas_advance_receivable', -- asset:     operator-advanced native token
            'fee_revenue',            -- revenue:   recognized operator revenue
            'suspense_unexplained'    -- suspense:  expected to sit at zero, forever
        )),
    asset_id    UUID NOT NULL REFERENCES assets(id),
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),

    -- Operator-scoped kinds have no merchant; merchant-scoped kinds must have one.
    CHECK ((kind IN ('custody_operator', 'fee_revenue')) = (merchant_id IS NULL)),

    UNIQUE NULLS NOT DISTINCT (merchant_id, kind, asset_id)
);

CREATE INDEX IF NOT EXISTS ledger_accounts_merchant_asset_idx
    ON ledger_accounts (merchant_id, asset_id);


-- =========================================================
-- 15. Ledger journals + entries
--
-- Strictly append-only. Corrections are reversals, never edits.
-- =========================================================
CREATE TABLE IF NOT EXISTS ledger_journals (
    id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),

    kind         VARCHAR(40)  NOT NULL
        CHECK (kind IN ('payment_recognized', 'sweep', 'withdrawal',
                        'gas_refill', 'gas_advance', 'gas_burn_failed',
                        'conversion', 'fee_settlement',
                        'external_credit', 'external_debit',
                        'probe_adjustment', 'reclassification', 'reversal')),

    -- The latch. payment_recognized:<payment_id>, sweep:<tx_id>,
    -- external_credit:<tx_id>:<event_index>, reversal:<original_journal_id>, …
    -- A worker that runs twice writes one journal.
    dedupe_key   VARCHAR(255) NOT NULL UNIQUE,

    merchant_id  UUID REFERENCES merchants(id),
    tx_id        UUID REFERENCES chain_transactions(id),
    payment_id   UUID REFERENCES payments(id),

    reverses     UUID REFERENCES ledger_journals(id),
    metadata     JSONB,   -- fee rate snapshot, route used, policy version, oracle quote

    occurred_at  TIMESTAMPTZ NOT NULL,              -- when the block said it happened
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(), -- when the worker got to it

    CHECK ((kind = 'reversal') = (reverses IS NOT NULL))
);

-- A journal can be reversed exactly once.
CREATE UNIQUE INDEX IF NOT EXISTS ledger_journals_reverses_uq
    ON ledger_journals (reverses) WHERE reverses IS NOT NULL;

-- Reports read occurred_at; operational debugging reads created_at.
CREATE INDEX IF NOT EXISTS ledger_journals_merchant_time_idx
    ON ledger_journals (merchant_id, occurred_at DESC);

CREATE INDEX IF NOT EXISTS ledger_journals_tx_idx
    ON ledger_journals (tx_id) WHERE tx_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS ledger_journals_kind_time_idx
    ON ledger_journals (kind, occurred_at DESC);


CREATE TABLE IF NOT EXISTS ledger_entries (
    entry_no    BIGSERIAL PRIMARY KEY,
    journal_id  UUID NOT NULL REFERENCES ledger_journals(id),
    account_id  UUID NOT NULL REFERENCES ledger_accounts(id),

    -- Duplicated from ledger_accounts deliberately: the balance constraint
    -- groups by it, and having it here means the constraint needs no join.
    -- A trigger asserts the two agree.
    asset_id    UUID NOT NULL REFERENCES assets(id),

    amount      NUMERIC(78,0) NOT NULL CHECK (amount <> 0)  -- + debit, - credit
);

-- Balance derivation, and the running-balance window function.
CREATE INDEX IF NOT EXISTS ledger_entries_account_idx
    ON ledger_entries (account_id, entry_no);

CREATE INDEX IF NOT EXISTS ledger_entries_journal_idx
    ON ledger_entries (journal_id);

CREATE INDEX IF NOT EXISTS ledger_entries_asset_idx
    ON ledger_entries (asset_id);


-- =========================================================
-- 16. Fee rates
--
-- Rates per route (token_id); receivables per asset. Not in conflict:
-- at accrual time the payment has a route, so that route's rate applies.
-- Once accrued the obligation is money, and money is denominated in an asset.
-- =========================================================
CREATE TABLE IF NOT EXISTS fee_rates (
    id             UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    merchant_id    UUID REFERENCES merchants(id) ON DELETE CASCADE,  -- NULL = operator default
    token_id       VARCHAR(100) NOT NULL,        -- route, matching TokenRegistry keys
    basis_points   INT NOT NULL CHECK (basis_points >= 0 AND basis_points <= 10000),
    effective_from TIMESTAMPTZ NOT NULL DEFAULT now(),

    UNIQUE NULLS NOT DISTINCT (merchant_id, token_id, effective_from)
);

-- Resolution: merchant override, else operator default; latest row with
-- effective_from <= payment.occurred_at. Versioned, never mutated, so
-- recomputing a historical accrual gives the historical answer.
CREATE INDEX IF NOT EXISTS fee_rates_resolution_idx
    ON fee_rates (token_id, merchant_id, effective_from DESC);


-- =========================================================
-- 17. Integrity
-- =========================================================

-- ---- 17.1 updated_at --------------------------------------------------
CREATE OR REPLACE FUNCTION touch_updated_at() RETURNS TRIGGER AS $$
BEGIN
    NEW.updated_at := now();
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS chain_transactions_touch ON chain_transactions;
CREATE TRIGGER chain_transactions_touch
    BEFORE UPDATE ON chain_transactions
    FOR EACH ROW EXECUTE FUNCTION touch_updated_at();

DROP TRIGGER IF EXISTS sweep_queue_touch ON sweep_queue;
CREATE TRIGGER sweep_queue_touch
    BEFORE UPDATE ON sweep_queue
    FOR EACH ROW EXECUTE FUNCTION touch_updated_at();


-- ---- 17.2 Append-only enforcement -------------------------------------
-- Statement-level: cheap, and the message is the same either way.
CREATE OR REPLACE FUNCTION forbid_mutation() RETURNS TRIGGER AS $$
BEGIN
    RAISE EXCEPTION
        '% is append-only; % is not permitted. Corrections are reversals.',
        TG_TABLE_NAME, TG_OP;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS ledger_journals_append_only ON ledger_journals;
CREATE TRIGGER ledger_journals_append_only
    BEFORE UPDATE OR DELETE ON ledger_journals
    FOR EACH STATEMENT EXECUTE FUNCTION forbid_mutation();

DROP TRIGGER IF EXISTS ledger_entries_append_only ON ledger_entries;
CREATE TRIGGER ledger_entries_append_only
    BEFORE UPDATE OR DELETE ON ledger_entries
    FOR EACH STATEMENT EXECUTE FUNCTION forbid_mutation();

DROP TRIGGER IF EXISTS chain_movements_append_only ON chain_movements;
CREATE TRIGGER chain_movements_append_only
    BEFORE UPDATE OR DELETE ON chain_movements
    FOR EACH STATEMENT EXECUTE FUNCTION forbid_mutation();


-- ---- 17.3 Entry asset must match its account's asset -------------------
CREATE OR REPLACE FUNCTION ledger_assert_entry_asset() RETURNS TRIGGER AS $$
DECLARE
    account_asset UUID;
BEGIN
    SELECT asset_id INTO account_asset
      FROM ledger_accounts WHERE id = NEW.account_id;

    IF account_asset IS DISTINCT FROM NEW.asset_id THEN
        RAISE EXCEPTION
            'entry asset % does not match account % asset %',
            NEW.asset_id, NEW.account_id, account_asset;
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS ledger_entries_asset_matches ON ledger_entries;
CREATE TRIGGER ledger_entries_asset_matches
    BEFORE INSERT ON ledger_entries
    FOR EACH ROW EXECUTE FUNCTION ledger_assert_entry_asset();


-- ---- 17.4 Gas accounts hold native assets only ------------------------
CREATE OR REPLACE FUNCTION ledger_assert_account_asset_kind() RETURNS TRIGGER AS $$
DECLARE
    k VARCHAR(20);
BEGIN
    IF NEW.kind IN ('custody_gas', 'gas_advance_receivable') THEN
        SELECT asset_kind INTO k FROM assets WHERE id = NEW.asset_id;
        IF k IS DISTINCT FROM 'native' THEN
            RAISE EXCEPTION
                'account kind % requires a native asset; asset % is %',
                NEW.kind, NEW.asset_id, k;
        END IF;
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS ledger_accounts_asset_kind ON ledger_accounts;
CREATE TRIGGER ledger_accounts_asset_kind
    BEFORE INSERT ON ledger_accounts
    FOR EACH ROW EXECUTE FUNCTION ledger_assert_account_asset_kind();


-- ---- 17.5 Journals balance, per asset ---------------------------------
-- Per (journal_id, asset_id), not per journal: a sweep journal carries USDC
-- legs and ETH gas legs together and those are not commensurable. Requiring
-- the whole journal to sum to zero would force a conversion that never
-- happened.
--
-- Deferred, so a journal can be written leg by leg inside one transaction.
CREATE OR REPLACE FUNCTION ledger_assert_journal_balanced() RETURNS TRIGGER AS $$
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
            'journal % does not balance for asset %: sum = %',
            NEW.journal_id, offending.asset_id, offending.total;
    END IF;
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS ledger_entries_balanced ON ledger_entries;
CREATE CONSTRAINT TRIGGER ledger_entries_balanced
    AFTER INSERT ON ledger_entries
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION ledger_assert_journal_balanced();


-- =========================================================
-- 18. Derived views
--
-- Balances are never stored. These are the two reads the rest of the system
-- makes constantly; everything else is a window function away.
-- =========================================================

-- Balance per account. Debits positive, credits negative:
-- payable_to_merchant reading -150000000 means the merchant is owed 150 USDC.
CREATE OR REPLACE VIEW ledger_account_balances AS
SELECT
    a.id           AS account_id,
    a.merchant_id,
    a.kind,
    a.asset_id,
    ast.network_type,
    ast.chain_ref,
    ast.symbol,
    ast.decimals,
    COALESCE(SUM(e.amount), 0) AS balance
FROM ledger_accounts a
JOIN assets ast              ON ast.id = a.asset_id
LEFT JOIN ledger_entries e   ON e.account_id = a.id
GROUP BY a.id, a.merchant_id, a.kind, a.asset_id,
         ast.network_type, ast.chain_ref, ast.symbol, ast.decimals;


-- What the ledger says is unswept, per merchant and asset, against what the
-- sweep queue believes is still outstanding. The two are a correspondence,
-- not an equality — the gap is itself a useful number (LEDGER.md §10.3).
CREATE OR REPLACE VIEW unswept_position AS
SELECT
    b.merchant_id,
    b.asset_id,
    b.symbol,
    b.balance                        AS ledger_unswept,
    COALESCE(q.queued_amount, 0)     AS queued_unswept,
    COALESCE(q.queued_rows, 0)       AS queued_rows,
    b.balance - COALESCE(q.queued_amount, 0) AS drift
FROM ledger_account_balances b
LEFT JOIN (
    SELECT merchant_id, asset_id,
           SUM(amount) AS queued_amount,
           COUNT(*)    AS queued_rows
      FROM sweep_queue
     WHERE status IN ('pending', 'claimed', 'broadcast', 'failed')
     GROUP BY merchant_id, asset_id
) q ON q.merchant_id = b.merchant_id AND q.asset_id = b.asset_id
WHERE b.kind = 'custody_unswept';
