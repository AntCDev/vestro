-- 004_derivation_roles_and_operational_wallets.sql
BEGIN;

-- ---------------------------------------------------------
-- 1. Platform key scope. Owner-level separation from merchants.
-- ---------------------------------------------------------
CREATE TABLE platform_key_material (
                                       id                SMALLINT PRIMARY KEY DEFAULT 1 CHECK (id = 1),
                                       key_family        VARCHAR(20)  NOT NULL DEFAULT 'bip39',
                                       encrypted_secret  BYTEA        NOT NULL,
                                       encryption_nonce  BYTEA        NOT NULL,
                                       created_at        TIMESTAMPTZ  NOT NULL DEFAULT now()
);

-- ---------------------------------------------------------
-- 2. Derivation schemes: audit + boot assertion, never read at signing time.
-- ---------------------------------------------------------
CREATE TABLE derivation_schemes (
                                    network_type  VARCHAR(20) NOT NULL,
                                    version       SMALLINT    NOT NULL,
                                    coin_type     INTEGER     NOT NULL,
                                    template      TEXT        NOT NULL,   -- "m/44'/{coin}'/{role}'/{index}'"
                                    active        BOOLEAN     NOT NULL DEFAULT TRUE,
                                    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
                                    PRIMARY KEY (network_type, version)
);

CREATE UNIQUE INDEX derivation_schemes_one_active
    ON derivation_schemes (network_type) WHERE active;

-- ---------------------------------------------------------
-- 3. Merchant wallets, now role/index aware.
-- ---------------------------------------------------------
DROP TABLE IF EXISTS merchant_wallets;

CREATE TABLE merchant_wallets (
                                  merchant_id     UUID        NOT NULL REFERENCES merchants(id) ON DELETE CASCADE,
                                  network_type    VARCHAR(20) NOT NULL,
                                  role            SMALLINT    NOT NULL,
                                  wallet_index    INTEGER     NOT NULL,
                                  address         VARCHAR(255) NOT NULL,
                                  derivation_path TEXT        NOT NULL,
                                  scheme_version  SMALLINT    NOT NULL,
                                  created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
                                  PRIMARY KEY (merchant_id, network_type, role, wallet_index)
);

-- The canonical main wallet is (role 0, index 0). Deposit addresses live on
-- invoices; this table holds named, long-lived wallets only.
CREATE INDEX merchant_wallets_address_idx ON merchant_wallets (network_type, address);

-- ---------------------------------------------------------
-- 4. Index allocator, now per (merchant, network family, role).
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
-- 5. Operational wallets. Address is family-wide (no chain_ref):
--    one EVM feeder address serves every EVM chain; nonces are per chain.
-- ---------------------------------------------------------
CREATE TABLE operational_wallets (
                                     scope           VARCHAR(20) NOT NULL CHECK (scope IN ('platform', 'merchant')),
                                     merchant_id     UUID        NULL REFERENCES merchants(id) ON DELETE CASCADE,
                                     network_type    VARCHAR(20) NOT NULL,
                                     role            SMALLINT    NOT NULL,
                                     wallet_index    INTEGER     NOT NULL,
                                     purpose         VARCHAR(40) NOT NULL,
                                     address         VARCHAR(255) NOT NULL,
                                     derivation_path TEXT        NOT NULL,
                                     scheme_version  SMALLINT    NOT NULL,
                                     created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
                                     CHECK (
                                         (scope = 'platform' AND merchant_id IS NULL) OR
                                         (scope = 'merchant' AND merchant_id IS NOT NULL)
                                         )
);

CREATE UNIQUE INDEX operational_wallets_platform_key
    ON operational_wallets (network_type, role, wallet_index)
    WHERE scope = 'platform';

CREATE UNIQUE INDEX operational_wallets_merchant_key
    ON operational_wallets (merchant_id, network_type, role, wallet_index)
    WHERE scope = 'merchant';

-- ---------------------------------------------------------
-- 6. Invoices carry the path they were derived at.
-- ---------------------------------------------------------
ALTER TABLE invoices
    ADD COLUMN wallet_role    SMALLINT NOT NULL DEFAULT 0,
    ADD COLUMN wallet_path    TEXT,
    ADD COLUMN scheme_version SMALLINT NOT NULL DEFAULT 1;

-- ---------------------------------------------------------
-- 7. Gas feeding. One live advance per invoice; retries replace by nonce.
-- ---------------------------------------------------------
CREATE TABLE chain_nonces (
                              address      VARCHAR(255) NOT NULL,
                              network_type VARCHAR(20)  NOT NULL,
                              chain_ref    VARCHAR(64)  NOT NULL,
                              next_nonce   BIGINT       NOT NULL,
                              updated_at   TIMESTAMPTZ  NOT NULL DEFAULT now(),
                              PRIMARY KEY (address, network_type, chain_ref)
);

CREATE TABLE gas_funding_intents (
                                     id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                                     invoice_id    UUID        NOT NULL UNIQUE REFERENCES invoices(id) ON DELETE CASCADE,
                                     merchant_id   UUID        NOT NULL REFERENCES merchants(id) ON DELETE CASCADE,
                                     network_type  VARCHAR(20) NOT NULL,
                                     chain_ref     VARCHAR(64) NOT NULL,
                                     from_address  VARCHAR(255) NOT NULL,
                                     to_address    VARCHAR(255) NOT NULL,
                                     amount        NUMERIC(78, 0) NOT NULL,        -- base units (wei / lamports)
                                     nonce         BIGINT      NULL,               -- fixed once assigned; RBF reuses it
                                     tx_hash       VARCHAR(100) NULL UNIQUE,
                                     attempts      INTEGER     NOT NULL DEFAULT 0,
                                     status        VARCHAR(20) NOT NULL DEFAULT 'pending'
                                         CHECK (status IN ('pending','submitted','confirmed','failed','abandoned')),
                                     last_error    TEXT        NULL,
                                     created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
                                     updated_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX gas_funding_intents_open_idx
    ON gas_funding_intents (network_type, chain_ref, status)
    WHERE status IN ('pending', 'submitted');

INSERT INTO derivation_schemes (network_type, version, coin_type, template) VALUES
                                                                                ('evm',    1, 60,  'm/44''/{coin}''/{role}''/{index}'''),
                                                                                ('solana', 1, 501, 'm/44''/{coin}''/{role}''/{index}'''),
                                                                                ('esplora',1, 0,   'm/44''/{coin}''/{role}''/{index}''');

COMMIT;