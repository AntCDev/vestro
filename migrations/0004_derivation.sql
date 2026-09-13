-- 004_derivation_roles_and_gas.sql

-- ---------------------------------------------------------
-- 1. Derivation schemes: audit + boot assertion.
--    Never read at signing time. Code is authoritative; this table exists so
--    a scheme change that would re-point funded addresses fails loudly at
--    boot instead of silently deriving somewhere new.
-- ---------------------------------------------------------
CREATE TABLE derivation_schemes (
                                    network_type  VARCHAR(20) NOT NULL,
                                    version       SMALLINT    NOT NULL,
                                    coin_type     INTEGER     NOT NULL,
                                    template      TEXT        NOT NULL,   -- m/44'/{coin}'/{role}'/{index}'
                                    active        BOOLEAN     NOT NULL DEFAULT TRUE,
                                    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
                                    PRIMARY KEY (network_type, version)
);

CREATE UNIQUE INDEX derivation_schemes_one_active
    ON derivation_schemes (network_type) WHERE active;

-- ---------------------------------------------------------
-- 2. Merchant wallets: every named, long-lived wallet a merchant owns,
--    deposit-role and operational-role alike. There is no platform scope —
--    a merchant's gas is funded to the merchant's own feeder.
--    Per-invoice deposit addresses do NOT live here; they live on invoices.
-- ---------------------------------------------------------
DROP TABLE IF EXISTS merchant_wallets;

CREATE TABLE merchant_wallets (
                                  merchant_id     UUID        NOT NULL REFERENCES merchants(id) ON DELETE CASCADE,
                                  network_type    VARCHAR(20) NOT NULL,
                                  role            SMALLINT    NOT NULL,
                                  wallet_index    INTEGER     NOT NULL,
                                  purpose         VARCHAR(40) NOT NULL,   -- 'main' | 'gas_feeder' | 'fee_collector'
                                  address         VARCHAR(255) NOT NULL,
                                  derivation_path TEXT        NOT NULL,
                                  scheme_version  SMALLINT    NOT NULL,
                                  created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
                                  PRIMARY KEY (merchant_id, network_type, role, wallet_index)
);

-- One wallet per purpose per family. This is the lookup the sweeper and the
-- gas feeder actually use; role/index is the derivation fact, purpose is the
-- role in the system.
CREATE UNIQUE INDEX merchant_wallets_purpose_uq
    ON merchant_wallets (merchant_id, network_type, purpose);

-- Watcher classification: address -> (merchant, purpose). Address is
-- family-wide, so one row serves Base and Ethereum both.
CREATE UNIQUE INDEX merchant_wallets_address_uq
    ON merchant_wallets (network_type, address);

-- ---------------------------------------------------------
-- 3. Index allocator, per (merchant, family, role).
--    Deliberately NOT per chain_ref: coin_type is family-wide, so Base and
--    Ethereum share address space. A per-chain counter would hand the same
--    address to two invoices on two chains.
-- ---------------------------------------------------------
DROP TABLE IF EXISTS merchant_network_indices;

CREATE TABLE merchant_network_indices (
                                          merchant_id  UUID        NOT NULL REFERENCES merchants(id) ON DELETE CASCADE,
                                          network      VARCHAR(20) NOT NULL,
                                          role         SMALLINT    NOT NULL,
                                          next_index   INTEGER     NOT NULL,   -- highest index ALLOCATED, not next free
                                          updated_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
                                          PRIMARY KEY (merchant_id, network, role)
);

-- ---------------------------------------------------------
-- 4. Invoices carry the path they were derived at, so a deposit address can
--    be re-signed years later without re-deriving the scheme from code.
-- ---------------------------------------------------------
ALTER TABLE invoices
    ADD COLUMN wallet_role    SMALLINT NOT NULL DEFAULT 0,
    ADD COLUMN wallet_path    TEXT,
    ADD COLUMN scheme_version SMALLINT NOT NULL DEFAULT 1;

-- ---------------------------------------------------------
-- 5. Nonce allocation. Address is family-wide but nonces are per chain:
--    one feeder address has an independent nonce on Base and on Ethereum.
-- ---------------------------------------------------------
CREATE TABLE chain_nonces (
                              address      VARCHAR(255) NOT NULL,
                              network_type VARCHAR(20)  NOT NULL,
                              chain_ref    VARCHAR(64)  NOT NULL,
                              next_nonce   BIGINT       NOT NULL,
                              updated_at   TIMESTAMPTZ  NOT NULL DEFAULT now(),
                              PRIMARY KEY (address, network_type, chain_ref)
);

-- ---------------------------------------------------------
-- 6. Gas funding intents.
--    Keyed on the DESTINATION address, not the invoice: a deposit address can
--    need topping up more than once (failed sweep, fee spike), and the vault
--    path needs advances that have no single invoice. invoice_id is a
--    breadcrumb for attribution, not the identity.
-- ---------------------------------------------------------
CREATE TABLE gas_funding_intents (
                                     id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                                     merchant_id   UUID        NOT NULL REFERENCES merchants(id) ON DELETE CASCADE,
                                     invoice_id    UUID        NULL REFERENCES invoices(id) ON DELETE SET NULL,
                                     network_type  VARCHAR(20) NOT NULL,
                                     chain_ref     VARCHAR(64) NOT NULL,
                                     from_address  VARCHAR(255) NOT NULL,      -- merchant's gas_feeder
                                     to_address    VARCHAR(255) NOT NULL,      -- deposit address needing gas
                                     amount        NUMERIC(78, 0) NOT NULL,    -- base units (wei / lamports)
                                     nonce         BIGINT      NULL,           -- fixed once assigned; RBF reuses it
                                     tx_hash       VARCHAR(100) NULL UNIQUE,
                                     attempts      INTEGER     NOT NULL DEFAULT 0,
                                     status        VARCHAR(20) NOT NULL DEFAULT 'pending'
                                         CHECK (status IN ('pending','submitted','confirmed','failed','abandoned')),
                                     last_error    TEXT        NULL,
                                     created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
                                     updated_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- At most one live advance per destination per chain. This is what makes
-- "open an intent" idempotent under a retrying sweeper.
CREATE UNIQUE INDEX gas_funding_intents_live_uq
    ON gas_funding_intents (network_type, chain_ref, to_address)
    WHERE status IN ('pending', 'submitted');

CREATE INDEX gas_funding_intents_work_idx
    ON gas_funding_intents (network_type, chain_ref, status, created_at)
    WHERE status IN ('pending', 'submitted');

-- Once a feeder has a nonce assigned, claiming work must be serialized per
-- feeder address. This index supports SKIP LOCKED claiming in nonce order.
CREATE INDEX gas_funding_intents_feeder_idx
    ON gas_funding_intents (from_address, network_type, chain_ref, nonce);

INSERT INTO derivation_schemes (network_type, version, coin_type, template) VALUES
                                                                                ('evm',     1, 60,  'm/44''/{coin}''/{role}''/{index}'''),
                                                                                ('solana',  1, 501, 'm/44''/{coin}''/{role}''/{index}'''),
                                                                                ('esplora', 1, 0,   'm/44''/{coin}''/{role}''/{index}''');
