-- 005_outbound_transfers.sql

-- One row per *intent* to move value out of an address we control. This is
-- the request the orchestrator writes and the network worker fulfils.
-- The tx_hash is persisted BEFORE broadcast; that ordering is the whole design.
CREATE TABLE outbound_transfers (
                                    id               UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                                    merchant_id      UUID         NOT NULL REFERENCES merchants(id) ON DELETE RESTRICT,
                                    network_type     VARCHAR(20)  NOT NULL,
                                    chain_ref        VARCHAR(64)  NOT NULL,
                                    asset_id         UUID         NOT NULL REFERENCES assets(id),
                                    token_id         VARCHAR(64)  NULL,          -- handler that planned it; recorded, never keyed on
                                    intent           VARCHAR(20)  NOT NULL CHECK (intent IN ('sweep','gas_topup','withdrawal')),

    -- from side is a pair: where the value sits + who signs for it
                                    from_address     VARCHAR(255) NOT NULL,
                                    from_kind        VARCHAR(20)  NOT NULL,      -- deposit_address | vault | gas | merchant_main
                                    authority_role   SMALLINT     NOT NULL,
                                    authority_index  INTEGER      NOT NULL,
                                    fee_payer_role   SMALLINT     NULL,          -- NULL = authority pays
                                    fee_payer_index  INTEGER      NULL,

                                    to_address       VARCHAR(255) NOT NULL,
                                    amount_requested NUMERIC(78,0) NULL,         -- NULL = drain (TransferAmount::Max)
                                    amount_resolved  NUMERIC(78,0) NULL,         -- what build_and_sign actually put in the tx
                                    params           JSONB        NOT NULL DEFAULT '{}'::jsonb,

                                    status           VARCHAR(20)  NOT NULL DEFAULT 'pending'
                                        CHECK (status IN ('pending','signed','broadcast','confirmed','failed','expired','superseded')),
                                    tx_hash          VARCHAR(128) NULL,
                                    raw_tx           BYTEA        NULL,          -- rebroadcast verbatim, never re-sign
                                    nonce            BIGINT       NULL,
                                    valid_until      BIGINT       NULL,
                                    fee_estimate     NUMERIC(78,0) NULL,
                                    fee_paid         NUMERIC(78,0) NULL,
                                    block_number     BIGINT       NULL,
                                    supersedes       UUID         NULL REFERENCES outbound_transfers(id),

                                    attempts         INTEGER      NOT NULL DEFAULT 0,
                                    last_error       TEXT         NULL,
                                    claimed_at       TIMESTAMPTZ  NULL,          -- lease; a dead worker's claim expires
                                    created_at       TIMESTAMPTZ  NOT NULL DEFAULT now(),
                                    updated_at       TIMESTAMPTZ  NOT NULL DEFAULT now()
);

-- At most one live transfer per (chain, source address, asset). Two concurrent
-- sweeps of the same address would race on nonce / balance.
CREATE UNIQUE INDEX outbound_transfers_live_uq
    ON outbound_transfers (network_type, chain_ref, from_address, asset_id)
    WHERE status IN ('pending','signed','broadcast');

CREATE UNIQUE INDEX outbound_transfers_hash_uq
    ON outbound_transfers (network_type, chain_ref, tx_hash) WHERE tx_hash IS NOT NULL;

CREATE INDEX outbound_transfers_work_idx
    ON outbound_transfers (network_type, chain_ref, status, created_at)
    WHERE status IN ('pending','signed','broadcast');

-- Sweep rows point at the transfer that is moving them. Many rows -> one transfer.
ALTER TABLE sweep_queue ADD COLUMN transfer_id UUID NULL REFERENCES outbound_transfers(id);
CREATE INDEX sweep_queue_transfer_idx ON sweep_queue (transfer_id);
-- sweep_queue.status must allow: 'pending','claimed','broadcast','swept'. Adjust its CHECK if needed.

-- Settlement-side journal kinds / account kinds used by Ledgerer::record_outbound.
-- If ledger_journals.kind or ledger_accounts.kind have CHECKs, add:
--   journals: 'sweep_settled'      accounts: 'gas_expense'
