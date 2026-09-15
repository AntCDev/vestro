BEGIN;

-- 1. sweep_queue: the lease is transfer_id, not claim_id.
ALTER TABLE sweep_queue DROP CONSTRAINT sweep_queue_claim_pair;
ALTER TABLE sweep_queue DROP CONSTRAINT sweep_queue_broadcast_has_tx;

ALTER TABLE sweep_queue
    ADD CONSTRAINT sweep_queue_transfer_pair
        CHECK ((status IN ('claimed', 'broadcast', 'swept')) = (transfer_id IS NOT NULL));

ALTER TABLE sweep_queue DROP COLUMN claim_id;          -- drops sweep_queue_expired_claims_idx with it
ALTER TABLE sweep_queue DROP COLUMN claim_expires_at;

-- 2. Settlement journal kind.
ALTER TABLE ledger_journals DROP CONSTRAINT ledger_journals_kind_check;
ALTER TABLE ledger_journals
    ADD CONSTRAINT ledger_journals_kind_check
        CHECK (kind IN ('payment_recognized', 'sweep', 'sweep_settled', 'withdrawal',
                        'gas_refill', 'gas_advance', 'gas_burn_failed',
                        'conversion', 'fee_settlement',
                        'external_credit', 'external_debit',
                        'probe_adjustment', 'reclassification', 'reversal'));

-- 3. Gas expense account.
ALTER TABLE ledger_accounts DROP CONSTRAINT ledger_accounts_kind_check;
ALTER TABLE ledger_accounts
    ADD CONSTRAINT ledger_accounts_kind_check
        CHECK (kind IN ('custody_unswept', 'custody_treasury', 'custody_gas',
                        'custody_unsupported', 'custody_operator',
                        'payable_to_merchant', 'fees_receivable',
                        'gas_advance_receivable', 'gas_expense', 'fee_revenue',
                        'suspense_unexplained'));

ALTER TABLE ledger_accounts DROP CONSTRAINT ledger_accounts_scope;
ALTER TABLE ledger_accounts
    ADD CONSTRAINT ledger_accounts_scope CHECK (
        (merchant_id IS NOT NULL AND kind IN
                                     ('custody_unswept', 'custody_treasury', 'custody_gas',
                                      'custody_unsupported', 'payable_to_merchant',
                                      'fees_receivable', 'gas_advance_receivable', 'gas_expense',
                                      'suspense_unexplained'))
            OR
        (merchant_id IS NULL AND kind IN ('custody_operator', 'fee_revenue')));

ALTER TABLE ledger_accounts DROP CONSTRAINT ledger_accounts_gas_is_native;
ALTER TABLE ledger_accounts
    ADD CONSTRAINT ledger_accounts_gas_is_native
        CHECK (kind NOT IN ('custody_gas', 'gas_advance_receivable', 'gas_expense')
            OR asset_kind = 'native');

-- 4. Outbound intents that chain_transactions cannot currently spell.
ALTER TABLE chain_transactions DROP CONSTRAINT chain_transactions_intent_check;
ALTER TABLE chain_transactions
    ADD CONSTRAINT chain_transactions_intent_check
        CHECK (intent IN ('inbound', 'sweep', 'withdrawal', 'gas_topup', 'gas_refill',
                          'gas_advance', 'fee_settlement', 'conversion', 'external'));

COMMIT;