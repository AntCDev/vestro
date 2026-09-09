/**
 * Vestro — merchant ledger (dummy)
 *
 * Shapes mirror the read models: v_merchant_positions, v_ledger_balances,
 * v_unswept_reconciliation. Amounts are base units and are held as bigint,
 * because an 18-decimal asset overflows Number well before it overflows the
 * numeric column it came from.
 *
 * Account kinds beyond `custody_unswept` and `payable_to_merchant` are inferred
 * from the columns of v_merchant_positions — rename them to match the enum.
 */

/* ── types ─────────────────────────────────────────────────────────── */

type NetworkType = 'solana' | 'evm' | 'bitcoin';
type AssetKind = 'native' | 'contract';

type AccountKind =
  | 'custody_unswept'
  | 'custody_treasury'
  | 'custody_gas'
  | 'custody_unsupported'
  | 'payable_to_merchant'
  | 'fees_receivable'
  | 'gas_advanced'
  | 'unexplained';

interface Asset {
  id: string;
  network_type: NetworkType;
  chain_ref: string;
  asset_kind: AssetKind;
  address: string | null;
  decimals: number;
  symbol: string | null;
  registered: boolean;
}

/** One row of v_ledger_balances. Signed: custody debits positive, credits negative. */
interface LedgerBalance {
  account_id: string;
  kind: AccountKind;
  asset_id: string;
  balance: bigint;
  entry_count: number;
  last_activity_at: string;
}

/** One row of v_merchant_positions. Presentation signs, as the view emits them. */
interface Position {
  asset_id: string;
  unswept: bigint;
  treasury: bigint;
  gas: bigint;
  unsupported: bigint;
  owed_to_merchant: bigint;
  fees_owed_by_merchant: bigint;
  gas_advanced: bigint;
  unexplained: bigint;
}

/** One row of v_unswept_reconciliation. */
interface Reconciliation {
  asset_id: string;
  ledger_unswept: bigint;
  queue_active: bigint;
  queue_abandoned: bigint;
  drift: bigint;
}

type StatusTone = 'muted' | 'accent' | 'ok' | 'warn' | 'stop';

interface Leg {
  kind: AccountKind;
  asset_id: string;
  amount: bigint;
}

interface Movement {
  id: string;
  event: string;
  status: string;
  tone: StatusTone;
  occurred_at: string;
  invoice_id?: string;
  tx_hash?: string;
  legs: [Leg, Leg];
}

const MERCHANT_ID = '14357362-c5b8-4dde-bbfe-f06aae09d769';

/* ── seed ──────────────────────────────────────────────────────────── */

const ASSETS: Asset[] = [
  {
    id: 'b2f8fd4c-af4a-4a33-a289-ea2d39a7bf0d',
    network_type: 'solana', chain_ref: 'devnet', asset_kind: 'contract',
    address: '4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU',
    decimals: 6, symbol: 'USDC', registered: true,
  },
  {
    id: '4f602629-b686-457a-b52b-41d900bc4f45',
    network_type: 'solana', chain_ref: 'devnet', asset_kind: 'native',
    address: null, decimals: 9, symbol: 'SOL', registered: true,
  },
  {
    id: 'c81d0a52-6d1e-4f77-9c0a-2a1f5b9c4e30',
    network_type: 'evm', chain_ref: '84532', asset_kind: 'contract',
    address: '0x036CbD53842c5426634e7929541eC2318f3dCF7e',
    decimals: 6, symbol: 'USDC', registered: true,
  },
  {
    id: 'd0a7c934-1b55-4d02-9f6d-7c3e2a91b884',
    network_type: 'evm', chain_ref: '84532', asset_kind: 'native',
    address: null, decimals: 18, symbol: 'ETH', registered: true,
  },
  {
    id: 'f5b3e1c7-9a24-4d18-8b60-6e0c4d7a2f19',
    network_type: 'bitcoin', chain_ref: 'signet', asset_kind: 'native',
    address: null, decimals: 8, symbol: 'BTC', registered: true,
  },
  {
    id: 'a9c2f460-3e7b-4a91-8c15-d24f8b0e6733',
    network_type: 'evm', chain_ref: '84532', asset_kind: 'contract',
    address: '0x9c3F7bE1a2D04e58c7B1fA6d09E4c2b8351aD7f0',
    decimals: 18, symbol: null, registered: false,
  },
];

const BALANCES: LedgerBalance[] = [
  // Solana devnet · USDC — the observed invoice, received and not yet swept
  { account_id: 'ae99e22b-3b74-4aa7-9a0a-3db662799080', kind: 'custody_unswept',
    asset_id: 'b2f8fd4c-af4a-4a33-a289-ea2d39a7bf0d', balance: 500000n,
    entry_count: 1, last_activity_at: '2026-09-07T23:18:49-06:00' },
  { account_id: '3bc954b5-063a-4b7d-ac46-56fa0661f364', kind: 'payable_to_merchant',
    asset_id: 'b2f8fd4c-af4a-4a33-a289-ea2d39a7bf0d', balance: -500000n,
    entry_count: 1, last_activity_at: '2026-09-07T23:18:49-06:00' },

  // Solana devnet · SOL — gas float advanced by the operator
  { account_id: '6f10c8d3-2a4e-4b71-9d33-08c6b5e2a147', kind: 'custody_gas',
    asset_id: '4f602629-b686-457a-b52b-41d900bc4f45', balance: 250000000n,
    entry_count: 2, last_activity_at: '2026-09-07T22:41:03-06:00' },
  { account_id: '2b98d51a-77c3-4e60-b0f2-19a4c6e8d532', kind: 'gas_advanced',
    asset_id: '4f602629-b686-457a-b52b-41d900bc4f45', balance: -250000000n,
    entry_count: 2, last_activity_at: '2026-09-07T22:41:03-06:00' },

  // Base Sepolia · USDC — one sweep settled, one payment still in custody, fee taken
  { account_id: '84e0b7c2-15d9-42a8-9f37-6c1e0b5da904', kind: 'custody_unswept',
    asset_id: 'c81d0a52-6d1e-4f77-9c0a-2a1f5b9c4e30', balance: 2000000n,
    entry_count: 3, last_activity_at: '2026-09-07T23:11:20-06:00' },
  { account_id: '91f4a3d8-6b02-4c57-8e19-3d7a2c04f6b1', kind: 'custody_treasury',
    asset_id: 'c81d0a52-6d1e-4f77-9c0a-2a1f5b9c4e30', balance: 48500000n,
    entry_count: 4, last_activity_at: '2026-09-07T22:58:07-06:00' },
  { account_id: '7c3e9b15-4d80-4a26-b7f1-0e58c2a9d743', kind: 'payable_to_merchant',
    asset_id: 'c81d0a52-6d1e-4f77-9c0a-2a1f5b9c4e30', balance: -49250000n,
    entry_count: 7, last_activity_at: '2026-09-07T23:11:20-06:00' },
  { account_id: '5a2d7f60-9c31-4b48-a0e6-b18f3d5c2094', kind: 'fees_receivable',
    asset_id: 'c81d0a52-6d1e-4f77-9c0a-2a1f5b9c4e30', balance: -1250000n,
    entry_count: 3, last_activity_at: '2026-09-07T23:11:20-06:00' },

  // Base Sepolia · ETH — gas float
  { account_id: 'b6e1c07f-3a95-4d12-8b74-2f9d6e0a5c38', kind: 'custody_gas',
    asset_id: 'd0a7c934-1b55-4d02-9f6d-7c3e2a91b884', balance: 12000000000000000n,
    entry_count: 1, last_activity_at: '2026-09-07T20:44:55-06:00' },
  { account_id: 'c93f5a28-0d67-4e31-9a05-7b2e8c1d4f60', kind: 'gas_advanced',
    asset_id: 'd0a7c934-1b55-4d02-9f6d-7c3e2a91b884', balance: -12000000000000000n,
    entry_count: 1, last_activity_at: '2026-09-07T20:44:55-06:00' },

  // Bitcoin signet · BTC — an overpayment that has no invoice to sit against
  { account_id: 'd47b2e91-5c38-4f60-8a12-9e0d3b7c6a25', kind: 'custody_unswept',
    asset_id: 'f5b3e1c7-9a24-4d18-8b60-6e0c4d7a2f19', balance: 125000n,
    entry_count: 2, last_activity_at: '2026-09-07T21:59:31-06:00' },
  { account_id: 'e58c3f02-6a41-4d79-b3e8-0c9a1d5b4726', kind: 'payable_to_merchant',
    asset_id: 'f5b3e1c7-9a24-4d18-8b60-6e0c4d7a2f19', balance: -120000n,
    entry_count: 1, last_activity_at: '2026-09-07T21:57:12-06:00' },
  { account_id: 'f61d4a83-7b52-4e08-9c31-1a0b2e6d5847', kind: 'unexplained',
    asset_id: 'f5b3e1c7-9a24-4d18-8b60-6e0c4d7a2f19', balance: -5000n,
    entry_count: 1, last_activity_at: '2026-09-07T21:59:31-06:00' },

  // Base Sepolia · unregistered contract — dust sent to a deposit address
  { account_id: '0a5e9c76-2f14-4b83-8d60-5c7a1e9b3d42', kind: 'custody_unsupported',
    asset_id: 'a9c2f460-3e7b-4a91-8c15-d24f8b0e6733', balance: 1500000000000000000000n,
    entry_count: 1, last_activity_at: '2026-09-07T19:12:40-06:00' },
  { account_id: '1b6f0d87-3a25-4c94-9e71-6d8b2f0c4e53', kind: 'unexplained',
    asset_id: 'a9c2f460-3e7b-4a91-8c15-d24f8b0e6733', balance: -1500000000000000000000n,
    entry_count: 1, last_activity_at: '2026-09-07T19:12:40-06:00' },
];

const POSITIONS: Position[] = [
  { asset_id: 'b2f8fd4c-af4a-4a33-a289-ea2d39a7bf0d', unswept: 500000n, treasury: 0n, gas: 0n,
    unsupported: 0n, owed_to_merchant: 500000n, fees_owed_by_merchant: 0n, gas_advanced: 0n, unexplained: 0n },
  { asset_id: '4f602629-b686-457a-b52b-41d900bc4f45', unswept: 0n, treasury: 0n, gas: 250000000n,
    unsupported: 0n, owed_to_merchant: 0n, fees_owed_by_merchant: 0n, gas_advanced: 250000000n, unexplained: 0n },
  { asset_id: 'c81d0a52-6d1e-4f77-9c0a-2a1f5b9c4e30', unswept: 2000000n, treasury: 48500000n, gas: 0n,
    unsupported: 0n, owed_to_merchant: 49250000n, fees_owed_by_merchant: 1250000n, gas_advanced: 0n, unexplained: 0n },
  { asset_id: 'd0a7c934-1b55-4d02-9f6d-7c3e2a91b884', unswept: 0n, treasury: 0n, gas: 12000000000000000n,
    unsupported: 0n, owed_to_merchant: 0n, fees_owed_by_merchant: 0n, gas_advanced: 12000000000000000n, unexplained: 0n },
  { asset_id: 'f5b3e1c7-9a24-4d18-8b60-6e0c4d7a2f19', unswept: 125000n, treasury: 0n, gas: 0n,
    unsupported: 0n, owed_to_merchant: 120000n, fees_owed_by_merchant: 0n, gas_advanced: 0n, unexplained: 5000n },
  { asset_id: 'a9c2f460-3e7b-4a91-8c15-d24f8b0e6733', unswept: 0n, treasury: 0n, gas: 0n,
    unsupported: 1500000000000000000000n, owed_to_merchant: 0n, fees_owed_by_merchant: 0n,
    gas_advanced: 0n, unexplained: 1500000000000000000000n },
];

const RECONCILIATION: Reconciliation[] = [
  { asset_id: 'b2f8fd4c-af4a-4a33-a289-ea2d39a7bf0d', ledger_unswept: 500000n, queue_active: 500000n,
    queue_abandoned: 0n, drift: 0n },
  { asset_id: 'c81d0a52-6d1e-4f77-9c0a-2a1f5b9c4e30', ledger_unswept: 2000000n, queue_active: 1000000n,
    queue_abandoned: 1000000n, drift: 0n },
  { asset_id: 'f5b3e1c7-9a24-4d18-8b60-6e0c4d7a2f19', ledger_unswept: 125000n, queue_active: 120000n,
    queue_abandoned: 0n, drift: 5000n },
];

const MOVEMENTS: Movement[] = [
  {
    id: '9f2a4c81', event: 'payment.confirmed', status: 'Confirmed', tone: 'ok',
    occurred_at: '2026-09-07T23:18:49-06:00',
    invoice_id: 'a14378db-7b22-4e8d-b5f6-e3a02cd35e43',
    tx_hash: '2aWumRMU5ifChfJnRfYBCzhznF6kUnCXjiAR1CazucwMo5AzPJz2QESw45e6za6UujCNTLAMp58NeRfzDYezAtgv',
    legs: [
      { kind: 'custody_unswept', asset_id: 'b2f8fd4c-af4a-4a33-a289-ea2d39a7bf0d', amount: 500000n },
      { kind: 'payable_to_merchant', asset_id: 'b2f8fd4c-af4a-4a33-a289-ea2d39a7bf0d', amount: -500000n },
    ],
  },
  {
    id: '7b3d1e05', event: 'payment.detected', status: 'Detected', tone: 'accent',
    occurred_at: '2026-09-07T23:11:20-06:00',
    invoice_id: 'c2904f7e-1a63-4b09-8d52-f70ab8c31d64',
    tx_hash: '0x8f41c7b0d2e95a63f18c04b7ae62d3915c0847fb2e19d6a3c58f07b41e2d9a06',
    legs: [
      { kind: 'custody_unswept', asset_id: 'c81d0a52-6d1e-4f77-9c0a-2a1f5b9c4e30', amount: 2000000n },
      { kind: 'payable_to_merchant', asset_id: 'c81d0a52-6d1e-4f77-9c0a-2a1f5b9c4e30', amount: -2000000n },
    ],
  },
  {
    id: '4e8c2a60', event: 'fee.accrued', status: 'Confirmed', tone: 'ok',
    occurred_at: '2026-09-07T23:11:20-06:00',
    invoice_id: 'c2904f7e-1a63-4b09-8d52-f70ab8c31d64',
    legs: [
      { kind: 'payable_to_merchant', asset_id: 'c81d0a52-6d1e-4f77-9c0a-2a1f5b9c4e30', amount: 50000n },
      { kind: 'fees_receivable', asset_id: 'c81d0a52-6d1e-4f77-9c0a-2a1f5b9c4e30', amount: -50000n },
    ],
  },
  {
    id: '1d09f4b7', event: 'sweep.settled', status: 'Swept', tone: 'ok',
    occurred_at: '2026-09-07T22:58:07-06:00',
    tx_hash: '0x53a0e94b7c216fd80a3e5b1972c4d086fa71e3b95d2c604817af35e0b9d1c742',
    legs: [
      { kind: 'custody_unswept', asset_id: 'c81d0a52-6d1e-4f77-9c0a-2a1f5b9c4e30', amount: -48500000n },
      { kind: 'custody_treasury', asset_id: 'c81d0a52-6d1e-4f77-9c0a-2a1f5b9c4e30', amount: 48500000n },
    ],
  },
  {
    id: '6c5b3a92', event: 'deposit.unattributed', status: 'Overpaid', tone: 'warn',
    occurred_at: '2026-09-07T21:59:31-06:00',
    tx_hash: 'e7d1c0938ab2f4560e19d7c3ab850f26d41b9e07c3a2f8b5610d94ce27a3b0f8',
    legs: [
      { kind: 'custody_unswept', asset_id: 'f5b3e1c7-9a24-4d18-8b60-6e0c4d7a2f19', amount: 5000n },
      { kind: 'unexplained', asset_id: 'f5b3e1c7-9a24-4d18-8b60-6e0c4d7a2f19', amount: -5000n },
    ],
  },
  {
    id: '3a7e0d14', event: 'gas.funded', status: 'Confirmed', tone: 'ok',
    occurred_at: '2026-09-07T20:44:55-06:00',
    tx_hash: '0x1c7b09f4a3e6d258b0c14f97e3a025d68b7c40f1932ae5d80c614b7f2a09e35d',
    legs: [
      { kind: 'custody_gas', asset_id: 'd0a7c934-1b55-4d02-9f6d-7c3e2a91b884', amount: 12000000000000000n },
      { kind: 'gas_advanced', asset_id: 'd0a7c934-1b55-4d02-9f6d-7c3e2a91b884', amount: -12000000000000000n },
    ],
  },
];

/** Movements the fake feed appends, oldest first. */
const INCOMING: Movement[] = [
  {
    id: '0b46e9a3', event: 'payment.detected', status: 'Detected', tone: 'accent',
    occurred_at: new Date().toISOString(),
    invoice_id: '5d81b3c0-4f92-4a17-b86e-20c7d9a41f35',
    tx_hash: '4kQpX2mRt9vLb7Nc3sHy6UzF1wA8dEgJ5PmT0nYrKqSbW3ZxVfC7hLuD2eRa9tGn',
    legs: [
      { kind: 'custody_unswept', asset_id: 'b2f8fd4c-af4a-4a33-a289-ea2d39a7bf0d', amount: 1250000n },
      { kind: 'payable_to_merchant', asset_id: 'b2f8fd4c-af4a-4a33-a289-ea2d39a7bf0d', amount: -1250000n },
    ],
  },
  {
    id: '8e2c7f15', event: 'fee.accrued', status: 'Confirmed', tone: 'ok',
    occurred_at: new Date().toISOString(),
    invoice_id: '5d81b3c0-4f92-4a17-b86e-20c7d9a41f35',
    legs: [
      { kind: 'payable_to_merchant', asset_id: 'b2f8fd4c-af4a-4a33-a289-ea2d39a7bf0d', amount: 31250n },
      { kind: 'fees_receivable', asset_id: 'b2f8fd4c-af4a-4a33-a289-ea2d39a7bf0d', amount: -31250n },
    ],
  },
];

/* ── formatting ────────────────────────────────────────────────────── */

const DISPLAY_FRAC = 8;

const asset = (id: string): Asset =>
  ASSETS.find((a) => a.id === id) ?? {
    id, network_type: 'evm', chain_ref: '0', asset_kind: 'contract',
    address: null, decimals: 0, symbol: null, registered: false,
  };

const CHAINS: Record<string, string> = {
  '1': 'Ethereum', '10': 'Optimism', '137': 'Polygon', '8453': 'Base',
  '42161': 'Arbitrum', '84532': 'Base Sepolia', '11155111': 'Sepolia',
};

function chainLabel(a: Asset): string {
  if (a.network_type === 'evm') return CHAINS[a.chain_ref] ?? `chain ${a.chain_ref}`;
  if (a.network_type === 'solana') return `Solana ${a.chain_ref}`;
  return `Bitcoin ${a.chain_ref}`;
}

function symbolOf(a: Asset): string {
  if (a.symbol) return a.symbol;
  return a.address ? shorten(a.address) : 'UNKNOWN';
}

function group(digits: string): string {
  return digits.replace(/\B(?=(\d{3})+(?!\d))/g, ',');
}

/** Exact base-units → decimal string. No rounding: this is money. */
function exactAmount(v: bigint, decimals: number): string {
  const neg = v < 0n;
  const abs = neg ? -v : v;
  const base = 10n ** BigInt(decimals);
  const whole = group((abs / base).toString());
  const frac = decimals > 0 ? (abs % base).toString().padStart(decimals, '0') : '';
  return `${neg ? '-' : ''}${whole}${frac ? '.' + frac : ''}`;
}

/** Trimmed for a column; the exact value goes in the title attribute. */
function displayAmount(v: bigint, decimals: number): string {
  const exact = exactAmount(v, decimals);
  if (decimals <= DISPLAY_FRAC) return exact;
  const [w, f = ''] = exact.split('.');
  const cut = f.slice(0, DISPLAY_FRAC);
  return `${w}.${cut}${f.slice(DISPLAY_FRAC).replace(/0+$/, '') ? '…' : ''}`;
}

function shorten(s: string, head = 6, tail = 4): string {
  return s.length <= head + tail + 1 ? s : `${s.slice(0, head)}·${s.slice(-tail)}`;
}

function clock(iso: string): string {
  const d = new Date(iso);
  return d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit', hour12: false });
}

function stamp(iso: string): string {
  const d = new Date(iso);
  return `${d.toISOString().slice(0, 10)} ${clock(iso)}`;
}

const KIND_LABEL: Record<AccountKind, string> = {
  custody_unswept: 'custody · unswept',
  custody_treasury: 'custody · treasury',
  custody_gas: 'custody · gas',
  custody_unsupported: 'custody · unsupported',
  payable_to_merchant: 'payable to merchant',
  fees_receivable: 'fees receivable',
  gas_advanced: 'gas advanced',
  unexplained: 'unexplained',
};

const esc = (s: string): string =>
  s.replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c] as string));

function cell(v: bigint, decimals: number): string {
  const zero = v === 0n;
  return `<td class="right${zero ? ' zero' : ''}" title="${esc(exactAmount(v, decimals))}">${
    zero ? '0' : esc(displayAmount(v, decimals))
  }</td>`;
}

function statusMark(word: string, tone: StatusTone): string {
  const cls = tone === 'muted' ? '' : ` status--${tone}`;
  return `<span class="status${cls}"><span class="dot"></span>${esc(word)}</span>`;
}

/* ── scope ─────────────────────────────────────────────────────────── */

interface Scope { key: string; text: string; match: (a: Asset) => boolean; }

const SCOPES: Scope[] = [
  { key: 'all', text: 'All networks', match: () => true },
  ...Array.from(
    new Map(ASSETS.map((a) => [`${a.network_type}:${a.chain_ref}`, a])).values(),
  ).map((a) => ({
    key: `${a.network_type}:${a.chain_ref}`,
    text: chainLabel(a),
    match: (x: Asset) => x.network_type === a.network_type && x.chain_ref === a.chain_ref,
  })),
];

let scopeKey = 'all';
const inScope = (assetId: string): boolean =>
  (SCOPES.find((s) => s.key === scopeKey) ?? SCOPES[0]).match(asset(assetId));

/* ── render ────────────────────────────────────────────────────────── */

const $ = <T extends HTMLElement>(id: string): T => document.getElementById(id) as T;

function renderIdent(): void {
  $('ident').innerHTML = [
    ['merchant', MERCHANT_ID],
    ['accounts', `${BALANCES.length} across ${new Set(ASSETS.map((a) => a.network_type)).size} networks`],
    ['book', 'double-entry · base units'],
  ]
    .map(
      ([k, v]) =>
        `<div class="data-row"><span class="row-key">${k}</span><span class="row-value">${esc(v)}</span></div>`,
    )
    .join('');
}

function renderScope(): void {
  $('scope').innerHTML = SCOPES.map(
    (s) =>
      `<button type="button" class="choice" role="radio" data-scope="${s.key}" aria-checked="${
        s.key === scopeKey
      }">${esc(s.text)}</button>`,
  ).join('');
  $('as-of').textContent = `As of ${stamp(new Date().toISOString())}`;
}

function renderCounts(): void {
  const assets = ASSETS.filter((a) => inScope(a.id));
  const ids = new Set(assets.map((a) => a.id));
  const accounts = BALANCES.filter((b) => ids.has(b.asset_id));
  const entries = accounts.reduce((n, b) => n + b.entry_count, 0);
  const unregistered = assets.filter((a) => !a.registered).length;

  $('counts').innerHTML = [
    ['assets held', String(assets.length)],
    ['accounts open', String(accounts.length)],
    ['postings', String(entries)],
    ['unregistered assets', String(unregistered)],
  ]
    .map(
      ([k, v]) =>
        `<div class="data-row"><span class="row-key">${k}</span><span class="row-value">${v}</span></div>`,
    )
    .join('');
}

function legsFor(assetId: string): LedgerBalance[] {
  return BALANCES.filter((b) => b.asset_id === assetId);
}

function detailBlock(p: Position, a: Asset): string {
  const legs = legsFor(a.id);
  const net = legs.reduce((s, b) => s + b.balance, 0n);

  const legRows = legs
    .map(
      (b) => `
      <div class="leg">
        <span class="leg-key">${esc(KIND_LABEL[b.kind])}</span>
        <span class="leg-val" title="${esc(exactAmount(b.balance, a.decimals))}">${esc(
        displayAmount(b.balance, a.decimals),
      )} · ${b.entry_count} entr${b.entry_count === 1 ? 'y' : 'ies'}</span>
      </div>`,
    )
    .join('');

  const meta = [
    ['asset id', shorten(a.id, 8, 4)],
    ['address', a.address ? shorten(a.address, 10, 6) : 'native'],
    ['decimals', String(a.decimals)],
    ['registered', a.registered ? 'yes' : 'no'],
    ['unsupported', displayAmount(p.unsupported, a.decimals)],
    ['gas advanced', displayAmount(p.gas_advanced, a.decimals)],
    ['last activity', legs.length ? stamp(legs.map((b) => b.last_activity_at).sort().reverse()[0]) : '—'],
  ]
    .map(
      ([k, v]) => `<div class="leg"><span class="leg-key">${k}</span><span class="leg-val">${esc(v)}</span></div>`,
    )
    .join('');

  const balanced = net === 0n;

  return `
    <div class="legs">${legRows}
      <div class="leg leg--net">
        <span class="leg-key">net</span>
        <span class="leg-val">${esc(displayAmount(net, a.decimals))} ${statusMark(
    balanced ? 'Balanced' : 'Drift',
    balanced ? 'ok' : 'warn',
  )}</span>
      </div>
    </div>
    <div class="legs legs--meta">${meta}</div>
    ${
      a.registered
        ? ''
        : '<div class="warning warning--inset">This asset is not registered as a token handler. It is held in custody, excluded from payable, and cannot be swept until a handler exists for it.</div>'
    }`;
}

function renderPositions(): void {
  const rows = POSITIONS.filter((p) => inScope(p.asset_id));
  const body = $('positions');

  if (!rows.length) {
    body.innerHTML = `<tr><td colspan="8"><div class="empty"><span class="label">No positions</span>
      <p>Nothing has posted on this network yet. Create an invoice on it and the first confirmed payment opens the accounts.</p></div></td></tr>`;
    return;
  }

  body.innerHTML = rows
    .map((p, i) => {
      const a = asset(p.asset_id);
      const id = `pos-${i}`;
      return `
      <tr>
        <td>
          <button type="button" class="row-toggle" data-detail="${id}" aria-expanded="false" aria-controls="${id}">
            <span class="caret">+</span><span>${esc(symbolOf(a))}</span>
          </button>
        </td>
        <td class="chain">${esc(chainLabel(a))}</td>
        ${cell(p.unswept, a.decimals)}
        ${cell(p.treasury, a.decimals)}
        ${cell(p.gas, a.decimals)}
        ${cell(p.owed_to_merchant, a.decimals)}
        ${cell(p.fees_owed_by_merchant, a.decimals)}
        ${cell(p.unexplained, a.decimals)}
      </tr>
      <tr class="detail hidden" id="${id}"><td colspan="8">${detailBlock(p, a)}</td></tr>`;
    })
    .join('');

  const drifted = BALANCES.reduce((s, b) => s + (inScope(b.asset_id) ? b.balance : 0n), 0n) !== 0n;
  const el = $('integrity');
  el.className = `status status--${drifted ? 'warn' : 'ok'}`;
  (el.lastElementChild as HTMLElement).textContent = drifted ? 'Drift' : 'Balanced';
}

function renderRecon(): void {
  const rows = RECONCILIATION.filter((r) => inScope(r.asset_id));
  const body = $('recon');

  if (!rows.length) {
    body.innerHTML = `<tr><td colspan="7"><div class="empty"><span class="label">Nothing unswept</span>
      <p>No custody is waiting on this network, so there is nothing to reconcile against the sweep queue.</p></div></td></tr>`;
  } else {
    body.innerHTML = rows
      .map((r) => {
        const a = asset(r.asset_id);
        const drift = r.drift !== 0n;
        return `
        <tr>
          <td>${esc(symbolOf(a))}</td>
          <td class="chain">${esc(chainLabel(a))}</td>
          ${cell(r.ledger_unswept, a.decimals)}
          ${cell(r.queue_active, a.decimals)}
          ${cell(r.queue_abandoned, a.decimals)}
          ${cell(r.drift, a.decimals)}
          <td class="right">${statusMark(drift ? 'Drift' : 'Reconciled', drift ? 'warn' : 'ok')}</td>
        </tr>`;
      })
      .join('');
  }

  const anyDrift = rows.some((r) => r.drift !== 0n);
  const el = $('recon-state');
  el.className = `status status--${anyDrift ? 'warn' : 'ok'}`;
  (el.lastElementChild as HTMLElement).textContent = anyDrift ? 'Drift' : 'Reconciled';

  $('recon-note').innerHTML = anyDrift
    ? '<div class="warning">Ledger custody exceeds what the sweep queue is holding. The difference is an unattributed deposit — it stays out of payable until it is matched to an invoice or written to the merchant.</div>'
    : '<p class="label label--faint">Ledger custody matches the sweep queue</p>';
}

function movementNode(m: Movement): HTMLElement {
  const a = asset(m.legs[0].asset_id);
  const el = document.createElement('article');
  el.className = 'movement reveal';

  const legs = m.legs
    .map(
      (l) => `
      <div class="leg">
        <span class="leg-key">${esc(KIND_LABEL[l.kind])}</span>
        <span class="leg-val" title="${esc(exactAmount(l.amount, asset(l.asset_id).decimals))}">${
        l.amount > 0n ? '+' : ''
      }${esc(displayAmount(l.amount, asset(l.asset_id).decimals))} ${esc(symbolOf(asset(l.asset_id)))}</span>
      </div>`,
    )
    .join('');

  const refs: string[] = [`<span class="ref"><b>${clock(m.occurred_at)}</b> · ${esc(chainLabel(a))}</span>`];
  if (m.invoice_id) refs.push(`<span class="ref">invoice ${esc(shorten(m.invoice_id, 8, 4))}</span>`);
  if (m.tx_hash) refs.push(`<span class="ref">tx ${esc(shorten(m.tx_hash, 8, 6))}</span>`);

  el.innerHTML = `
    <div class="movement-head">
      <span class="movement-event">${esc(m.event)}</span>
      ${statusMark(m.status, m.tone)}
    </div>
    <div class="legs">${legs}</div>
    <div class="movement-refs">${refs.join('')}</div>`;
  return el;
}

function renderMovements(): void {
  const box = $('postings');
  const rows = MOVEMENTS.filter((m) => inScope(m.legs[0].asset_id));
  box.innerHTML = '';

  if (!rows.length) {
    box.innerHTML = `<div class="empty"><span class="label">No postings</span>
      <p>Nothing has posted on this network in the last 24 hours. Widen the scope to see the rest of the book.</p></div>`;
    return;
  }
  rows.forEach((m) => box.appendChild(movementNode(m)));
}

function renderAll(): void {
  renderCounts();
  renderPositions();
  renderRecon();
  renderMovements();
}

/* ── behaviour ─────────────────────────────────────────────────────── */

document.addEventListener('click', (e) => {
  const t = e.target as HTMLElement;

  const scopeBtn = t.closest<HTMLButtonElement>('[data-scope]');
  if (scopeBtn) {
    scopeKey = scopeBtn.dataset.scope as string;
    document
      .querySelectorAll<HTMLElement>('[data-scope]')
      .forEach((b) => b.setAttribute('aria-checked', String(b.dataset.scope === scopeKey)));
    renderAll();
    return;
  }

  const toggle = t.closest<HTMLButtonElement>('[data-detail]');
  if (toggle) {
    const row = document.getElementById(toggle.dataset.detail as string);
    if (!row) return;
    const open = row.classList.toggle('hidden') === false;
    toggle.setAttribute('aria-expanded', String(open));
    const caret = toggle.querySelector('.caret');
    if (caret) caret.textContent = open ? '−' : '+';
  }
});

/* Fake feed. Replace with the SSE/WebSocket stream that already drives observation. */
let pending = 0;
function stream(): void {
  const next = INCOMING[pending];
  if (!next) return;
  pending += 1;

  const m: Movement = { ...next, occurred_at: new Date().toISOString() };
  MOVEMENTS.unshift(m);

  if (inScope(m.legs[0].asset_id)) {
    const box = $('postings');
    box.querySelector('.empty')?.remove();
    box.prepend(movementNode(m));
  }
  window.setTimeout(stream, 7000);
}

renderIdent();
renderScope();
renderAll();
window.setTimeout(stream, 5000);
