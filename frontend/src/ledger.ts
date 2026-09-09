/* =============================================================
   Vestro — ledger overview
   Reads GET /api/test/ledger and renders positions, journals,
   accounts, sweep backlog and reconciliation.
   ============================================================= */

import './style.css';
import './ledger.css';

/* ---------------------------------------------------------------
   API contract — mirrors LedgerOverviewResponse
   --------------------------------------------------------------- */

export interface PositionRow {
  merchant_id: string;
  asset_id: string;
  network_type: string;
  chain_ref: string;
  symbol: string | null;
  decimals: number;
  asset_registered: boolean;
  unswept: string;
  treasury: string;
  gas: string;
  unsupported: string;
  owed_to_merchant: string;
  fees_owed_by_merchant: string;
  gas_advanced: string;
  unexplained: string;
}

export interface AccountBalanceRow {
  account_id: string;
  merchant_id: string;
  kind: string;
  asset_id: string;
  network_type: string;
  chain_ref: string;
  asset_kind: string;
  asset_address: string | null;
  symbol: string | null;
  decimals: number;
  asset_registered: boolean;
  balance: string;
  entry_count: number;
  last_activity_at: string | null;
}

export interface EntryRow {
  entry_no: number;
  account_id: string;
  account_kind: string;
  asset_id: string;
  symbol: string | null;
  decimals: number;
  network_type: string;
  chain_ref: string;
  amount: string;
}

export interface JournalRow {
  id: string;
  kind: string;
  dedupe_key: string;
  merchant_id: string;
  tx_id: string | null;
  payment_id: string | null;
  reverses: string | null;
  metadata: Record<string, unknown> | null;
  occurred_at: string;
  created_at: string;
  tx_hash: string | null;
  network_type: string | null;
  chain_ref: string | null;
  entries: EntryRow[];
}

export interface SweepBacklogRow {
  merchant_id: string;
  network_type: string;
  chain_ref: string;
  asset_id: string;
  symbol: string | null;
  decimals: number;
  custody_address: string;
  custody_kind: string;
  authority_address: string | null;
  movement_count: number;
  total_amount: string;
  oldest_enqueued_at: string | null;
  next_available_at: string | null;
  max_attempts: number;
}

export interface ReconciliationRow {
  merchant_id: string;
  asset_id: string;
  symbol: string | null;
  decimals: number;
  network_type: string;
  chain_ref: string;
  ledger_unswept: string;
  queue_active: string;
  queue_abandoned: string;
  drift: string;
}

export interface LedgerOverview {
  generated_at: string;
  merchant_id: string | null;
  positions: PositionRow[];
  accounts: AccountBalanceRow[];
  journals: JournalRow[];
  sweep_backlog: SweepBacklogRow[];
  reconciliation: ReconciliationRow[];
}

/* ---------------------------------------------------------------
   Config
   --------------------------------------------------------------- */

const API_BASE = '';
const ENDPOINT = `${API_BASE}/api/test/ledger`;
const REFRESH_MS = 8000;
const GLOBAL = '__global__';
const CUSTOM = '__custom__';

const UUID_RE =
  /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

const EXPLORERS: Record<string, (hash: string) => string> = {
  'evm:1': (h) => `https://etherscan.io/tx/${h}`,
  'evm:11155111': (h) => `https://sepolia.etherscan.io/tx/${h}`,
  'evm:8453': (h) => `https://basescan.org/tx/${h}`,
  'evm:84532': (h) => `https://sepolia.basescan.org/tx/${h}`,
  'evm:137': (h) => `https://polygonscan.com/tx/${h}`,
  'evm:42161': (h) => `https://arbiscan.io/tx/${h}`,
  'solana:mainnet': (h) => `https://explorer.solana.com/tx/${h}`,
  'solana:devnet': (h) => `https://explorer.solana.com/tx/${h}?cluster=devnet`,
  'solana:testnet': (h) => `https://explorer.solana.com/tx/${h}?cluster=testnet`,
};

/* Account kinds, in the order they should read on a card. */
const CUSTODY_KINDS: Array<[keyof PositionRow, string]> = [
  ['unswept', 'unswept'],
  ['treasury', 'treasury'],
  ['gas', 'gas'],
  ['unsupported', 'unsupported'],
];

const OBLIGATION_KINDS: Array<[keyof PositionRow, string]> = [
  ['fees_owed_by_merchant', 'fees owed'],
  ['gas_advanced', 'gas advanced'],
];

/* ---------------------------------------------------------------
   DOM helpers
   --------------------------------------------------------------- */

function need<T extends HTMLElement>(selector: string): T {
  const node = document.querySelector<T>(selector);
  if (!node) throw new Error(`Missing element: ${selector}`);
  return node;
}

function el<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  className?: string,
  text?: string,
): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

function clear(node: HTMLElement): void {
  while (node.firstChild) node.removeChild(node.firstChild);
}

type Tone = 'muted' | 'accent' | 'ok' | 'warn' | 'stop';

function status(word: string, tone: Tone): HTMLElement {
  const node = el('span', tone === 'muted' ? 'status' : `status status--${tone}`);
  node.appendChild(el('span', 'dot'));
  node.appendChild(document.createTextNode(word));
  return node;
}

function dataRow(key: string, value: string, tone?: 'zero' | 'neg'): HTMLElement {
  const row = el('div', 'data-row');
  row.appendChild(el('span', 'row-key', key));
  const cls = tone ? `row-value row-value--${tone}` : 'row-value';
  row.appendChild(el('span', cls, value));
  return row;
}

function kv(key: string, value: string, title?: string): HTMLElement {
  const row = el('div', 'kv');
  row.appendChild(el('span', 'row-key', key));
  const val = el('span', 'row-value', value);
  if (title) val.title = title;
  row.appendChild(val);
  return row;
}

function cell(text: string, className?: string, title?: string): HTMLTableCellElement {
  const td = el('td', className, text);
  if (title) td.title = title;
  return td;
}

function table(headers: Array<[string, boolean]>): {
  wrap: HTMLElement;
  body: HTMLTableSectionElement;
} {
  const wrap = el('div', 'tbl-wrap');
  const tbl = el('table', 'tbl');
  const head = el('thead');
  const tr = el('tr');
  for (const [label, numeric] of headers) {
    const th = el('th', numeric ? 'num' : undefined, label);
    tr.appendChild(th);
  }
  head.appendChild(tr);
  tbl.appendChild(head);
  const body = el('tbody');
  tbl.appendChild(body);
  wrap.appendChild(tbl);
  return { wrap, body };
}

function empty(message: string): HTMLElement {
  return el('p', 'empty', message);
}

/** A block in a narrow column: title, state word, then a run of facts. */
function unit(title: string, state: HTMLElement | null, titleHint?: string): HTMLElement {
  const box = el('div', 'unit');
  const head = el('div', 'unit-head');
  const name = el('span', 'unit-title', title);
  if (titleHint) name.title = titleHint;
  head.appendChild(name);
  if (state) head.appendChild(state);
  box.appendChild(head);
  return box;
}

function facts(pairs: Array<[string, string, boolean?]>): HTMLElement {
  const row = el('div', 'facts');
  for (const [key, value, dim] of pairs) {
    const fact = el('span', dim ? 'fact fact--dim' : 'fact');
    fact.appendChild(el('b', undefined, `${key} `));
    fact.appendChild(document.createTextNode(value));
    row.appendChild(fact);
  }
  return row;
}

/* ---------------------------------------------------------------
   Formatting
   --------------------------------------------------------------- */

function groupDigits(int: string): string {
  return int.replace(/\B(?=(\d{3})+(?!\d))/g, ',');
}

/**
 * Split a minor-unit value into sign and digits. Postgres NUMERIC can arrive
 * with a zero scale attached ("500000.000000"), so that is trimmed here.
 * Returns null when the value is not a whole number of minor units.
 */
function splitInt(raw: string): { sign: string; digits: string } | null {
  let s = raw.trim();
  let sign = '';
  if (s.startsWith('-')) {
    sign = '-';
    s = s.slice(1);
  } else if (s.startsWith('+')) {
    s = s.slice(1);
  }
  s = s.replace(/\.0+$/, '');
  if (!/^\d+$/.test(s)) return null;
  return { sign, digits: s };
}

/** Minor units to a decimal string. No floats — values can exceed 2^53. */
function units(raw: string | null | undefined, decimals: number): string {
  if (raw === null || raw === undefined || raw === '') return '—';
  const parts = splitInt(raw);
  if (!parts) return raw;
  const { sign } = parts;
  const s = parts.digits;

  const d = Math.max(0, decimals | 0);
  if (d === 0) return sign + groupDigits(s);

  const padded = s.padStart(d + 1, '0');
  const int = padded.slice(0, padded.length - d);
  let frac = padded.slice(padded.length - d).replace(/0+$/, '');
  const min = Math.min(2, d);
  while (frac.length < min) frac += '0';

  return sign + groupDigits(int) + (frac ? `.${frac}` : '');
}

function isZero(raw: string | null | undefined): boolean {
  if (!raw) return true;
  const parts = splitInt(raw);
  if (!parts) return false;
  return /^0*$/.test(parts.digits);
}

function toBig(raw: string): bigint {
  const parts = splitInt(raw);
  if (!parts) return 0n;
  try {
    return BigInt(parts.sign + parts.digits);
  } catch {
    return 0n;
  }
}

function signed(raw: string, decimals: number): string {
  const value = units(raw, decimals);
  if (value === '—' || value.startsWith('-') || isZero(raw)) return value;
  return `+${value}`;
}

function pad(n: number): string {
  return String(n).padStart(2, '0');
}

function timestamp(iso: string | null | undefined): string {
  if (!iso) return '—';
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  return (
    `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ` +
    `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`
  );
}

function since(iso: string | null | undefined): string {
  if (!iso) return '';
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return '';
  const secs = Math.round((Date.now() - d.getTime()) / 1000);
  if (secs < 0) return `in ${humanSpan(-secs)}`;
  if (secs < 10) return 'just now';
  return `${humanSpan(secs)} ago`;
}

function humanSpan(secs: number): string {
  if (secs < 60) return `${secs}s`;
  if (secs < 3600) return `${Math.round(secs / 60)}m`;
  if (secs < 86400) return `${Math.round(secs / 3600)}h`;
  return `${Math.round(secs / 86400)}d`;
}

function short(value: string | null | undefined, head = 8, tail = 6): string {
  if (!value) return '—';
  if (value.length <= head + tail + 1) return value;
  return `${value.slice(0, head)}…${value.slice(-tail)}`;
}

function chain(network: string | null, ref: string | null): string {
  if (!network) return '—';
  return ref ? `${network} · ${ref}` : network;
}

function asset(symbol: string | null, assetId: string): string {
  return symbol ?? `asset ${short(assetId, 8, 4)}`;
}

function explorerUrl(
  network: string | null,
  ref: string | null,
  hash: string,
): string | null {
  if (!network || !ref) return null;
  const build = EXPLORERS[`${network}:${ref}`];
  return build ? build(hash) : null;
}

/* ---------------------------------------------------------------
   State
   --------------------------------------------------------------- */

let scope: string | null = null;
let limit = 60;
let auto = false;
let inFlight = false;
let timer: number | null = null;
let seenJournals = new Set<string>();
let suppressReveal = true;

const knownMerchants = new Set<string>();

/* ---------------------------------------------------------------
   Elements
   --------------------------------------------------------------- */

const ui = {
  generatedAt: need('#generated-at'),
  feedStatus: need('#feed-status'),
  feedWord: need('#feed-word'),
  scope: need<HTMLSelectElement>('#scope'),
  limit: need<HTMLSelectElement>('#limit'),
  auto: need<HTMLButtonElement>('#auto'),
  refresh: need<HTMLButtonElement>('#refresh'),
  customScope: need('#custom-scope'),
  customId: need<HTMLInputElement>('#custom-id'),
  customApply: need<HTMLButtonElement>('#custom-apply'),
  error: need('#error-box'),
  tags: need('#tags'),
  positions: need('#positions'),
  positionsCount: need('#positions-count'),
  journalsPanel: need('#journals-panel'),
  journals: need('#journals'),
  journalsCount: need('#journals-count'),
  accounts: need('#accounts'),
  accountsCount: need('#accounts-count'),
  backlog: need('#backlog'),
  backlogCount: need('#backlog-count'),
  reconciliation: need('#reconciliation'),
};

/* ---------------------------------------------------------------
   Feed status + errors
   --------------------------------------------------------------- */

function setFeed(word: string, tone: Tone): void {
  ui.feedStatus.className = tone === 'muted' ? 'status' : `status status--${tone}`;
  ui.feedWord.textContent = word;
}

function restFeed(): void {
  if (auto) setFeed('Live', 'accent');
  else setFeed('Idle', 'muted');
}

function showError(message: string): void {
  ui.error.textContent = message;
  ui.error.classList.remove('hidden');
}

function hideError(): void {
  ui.error.classList.add('hidden');
  ui.error.textContent = '';
}

/* ---------------------------------------------------------------
   Fetch
   --------------------------------------------------------------- */

async function load(): Promise<void> {
  if (inFlight) return;
  inFlight = true;
  setFeed('Loading', 'accent');

  const url = new URL(ENDPOINT, window.location.origin);
  url.searchParams.set('limit', String(limit));
  if (scope) url.searchParams.set('merchant_id', scope);

  try {
    const res = await fetch(url.toString(), { headers: { accept: 'application/json' } });
    if (!res.ok) {
      const body = (await res.text()).trim();
      throw new Error(
        `Request failed with ${res.status}. ${body.slice(0, 240) || 'No detail returned.'}`,
      );
    }
    const data = (await res.json()) as LedgerOverview;
    hideError();
    harvestMerchants(data);
    render(data);
  } catch (err) {
    const detail = err instanceof Error ? err.message : String(err);
    showError(`The ledger could not be read. ${detail}`);
  } finally {
    inFlight = false;
    restFeed();
  }
}

function harvestMerchants(data: LedgerOverview): void {
  const before = knownMerchants.size;
  for (const row of data.positions) knownMerchants.add(row.merchant_id);
  for (const row of data.accounts) knownMerchants.add(row.merchant_id);
  for (const row of data.journals) knownMerchants.add(row.merchant_id);
  for (const row of data.sweep_backlog) knownMerchants.add(row.merchant_id);
  if (data.merchant_id) knownMerchants.add(data.merchant_id);
  if (knownMerchants.size !== before) syncScopeOptions();
}

function syncScopeOptions(): void {
  const current = scope ?? GLOBAL;
  clear(ui.scope);

  const all = el('option', undefined, 'All merchants');
  all.value = GLOBAL;
  ui.scope.appendChild(all);

  for (const id of [...knownMerchants].sort()) {
    const option = el('option', undefined, id);
    option.value = id;
    ui.scope.appendChild(option);
  }

  const custom = el('option', undefined, 'Enter a merchant ID…');
  custom.value = CUSTOM;
  ui.scope.appendChild(custom);

  const values = new Set([GLOBAL, CUSTOM, ...knownMerchants]);
  ui.scope.value = values.has(current) ? current : GLOBAL;
}

/* ---------------------------------------------------------------
   Render
   --------------------------------------------------------------- */

function render(data: LedgerOverview): void {
  ui.generatedAt.textContent = timestamp(data.generated_at);
  renderTags(data);
  renderPositions(data.positions);
  renderJournals(data.journals);
  renderAccounts(data.accounts);
  renderBacklog(data.sweep_backlog);
  renderReconciliation(data.reconciliation);
  suppressReveal = false;
}

function tag(label: string, value: string, tone?: Tone): HTMLElement {
  const box = el('div');
  box.appendChild(el('span', 'label', label));
  if (tone && tone !== 'muted') {
    const wrap = el('span', 'tag-value');
    wrap.appendChild(status(value, tone));
    box.appendChild(wrap);
  } else {
    box.appendChild(el('span', 'tag-value', value));
  }
  return box;
}

function renderTags(data: LedgerOverview): void {
  const drifting = data.reconciliation.filter((r) => !isZero(r.drift)).length;
  const unexplained = data.positions.filter((p) => !isZero(p.unexplained)).length;

  clear(ui.tags);
  ui.tags.appendChild(
    tag('Merchant', data.merchant_id ? short(data.merchant_id, 8, 4) : 'All'),
  );
  ui.tags.appendChild(tag('Positions', String(data.positions.length)));
  ui.tags.appendChild(tag('Journals', String(data.journals.length)));
  ui.tags.appendChild(tag('Awaiting sweep', String(data.sweep_backlog.length)));
  ui.tags.appendChild(
    drifting > 0
      ? tag('Drift', `${drifting} asset${drifting === 1 ? '' : 's'}`, 'stop')
      : tag('Drift', 'None', 'ok'),
  );
  if (unexplained > 0) {
    ui.tags.appendChild(
      tag('Unexplained', `${unexplained} asset${unexplained === 1 ? '' : 's'}`, 'warn'),
    );
  }
}

/* --- positions -------------------------------------------------------- */

function renderPositions(rows: PositionRow[]): void {
  clear(ui.positions);
  ui.positionsCount.textContent = rows.length
    ? `${rows.length} asset${rows.length === 1 ? '' : 's'}`
    : '';

  if (rows.length === 0) {
    ui.positions.appendChild(
      empty('No balances yet. Pay a testnet invoice and the first position appears here.'),
    );
    return;
  }

  for (const row of rows) {
    const card = el('div', 'card');

    const head = el('div', 'card-head');
    head.appendChild(el('span', 'card-sym', asset(row.symbol, row.asset_id)));
    if (row.asset_registered) {
      head.appendChild(el('span', 'card-net', chain(row.network_type, row.chain_ref)));
    } else {
      head.appendChild(status('Unregistered', 'warn'));
    }
    card.appendChild(head);

    const headline = el('div', 'headline');
    headline.appendChild(el('span', 'label', 'Owed to merchant'));
    headline.appendChild(
      el('span', 'headline-value', units(row.owed_to_merchant, row.decimals)),
    );
    card.appendChild(headline);

    const rowsBox = el('div', 'rows');
    for (const [key, label] of [...CUSTODY_KINDS, ...OBLIGATION_KINDS]) {
      const raw = row[key] as string;
      rowsBox.appendChild(
        dataRow(label, units(raw, row.decimals), isZero(raw) ? 'zero' : undefined),
      );
    }
    card.appendChild(rowsBox);

    if (!row.asset_registered) {
      card.appendChild(
        el(
          'div',
          'warning',
          'This asset is not in the registry. Balances are tracked but not swept.',
        ),
      );
    }

    if (!isZero(row.unexplained)) {
      card.appendChild(
        el(
          'div',
          'warning warning--stop',
          `Unexplained balance of ${units(row.unexplained, row.decimals)}. ` +
            'Custody and obligations do not agree for this asset.',
        ),
      );
    }

    ui.positions.appendChild(card);
  }
}

/* --- journals --------------------------------------------------------- */

function journalBalance(entries: EntryRow[]): boolean {
  const sums = new Map<string, bigint>();
  for (const entry of entries) {
    sums.set(entry.asset_id, (sums.get(entry.asset_id) ?? 0n) + toBig(entry.amount));
  }
  for (const total of sums.values()) if (total !== 0n) return false;
  return true;
}

function renderJournals(rows: JournalRow[]): void {
  clear(ui.journals);
  ui.journalsCount.textContent = rows.length
    ? `${rows.length} shown, newest first`
    : '';

  if (rows.length === 0) {
    ui.journals.appendChild(
      empty('Nothing posted yet. Journals are written when a payment is recognised.'),
    );
    return;
  }

  const next = new Set<string>();

  for (const row of rows) {
    next.add(row.id);
    const fresh = !suppressReveal && !seenJournals.has(row.id);
    const article = el('article', fresh ? 'journal reveal' : 'journal');

    /* head */
    const head = el('div', 'journal-head');
    head.appendChild(el('span', 'journal-kind', row.kind));

    const marks = el('div', 'journal-marks');
    if (row.reverses) marks.appendChild(status('Reversal', 'stop'));
    marks.appendChild(
      journalBalance(row.entries) ? status('Balanced', 'ok') : status('Unbalanced', 'stop'),
    );
    marks.appendChild(el('span', 'journal-time', timestamp(row.occurred_at)));
    head.appendChild(marks);
    article.appendChild(head);

    /* meta */
    const grid = el('div', 'kv-grid');
    grid.appendChild(kv('journal', short(row.id, 8, 6), row.id));

    if (row.tx_hash) {
      const line = el('div', 'kv');
      line.appendChild(el('span', 'row-key', 'transaction'));
      const url = explorerUrl(row.network_type, row.chain_ref, row.tx_hash);
      if (url) {
        const link = el('a', 'row-value link link--mono', short(row.tx_hash, 10, 8));
        link.href = url;
        link.target = '_blank';
        link.rel = 'noreferrer noopener';
        link.title = row.tx_hash;
        line.appendChild(link);
      } else {
        const val = el('span', 'row-value', short(row.tx_hash, 10, 8));
        val.title = row.tx_hash;
        line.appendChild(val);
      }
      grid.appendChild(line);
    }

    if (row.network_type) grid.appendChild(kv('network', chain(row.network_type, row.chain_ref)));
    if (row.payment_id) grid.appendChild(kv('payment', short(row.payment_id, 8, 6), row.payment_id));
    if (!scope) grid.appendChild(kv('merchant', short(row.merchant_id, 8, 4), row.merchant_id));
    if (row.reverses) grid.appendChild(kv('reverses', short(row.reverses, 8, 6), row.reverses));
    grid.appendChild(kv('posted', timestamp(row.created_at), `${since(row.created_at)}`));
    article.appendChild(grid);

    /* entries */
    if (row.entries.length > 0) {
      const box = el('div', 'entries');
      for (const entry of row.entries) {
        const line = el('div', 'entry');
        line.appendChild(el('span', 'entry-no', String(entry.entry_no)));
        line.appendChild(el('span', 'entry-account', entry.account_kind));
        line.appendChild(
          el(
            'span',
            'entry-asset',
            `${asset(entry.symbol, entry.asset_id)} · ${chain(entry.network_type, entry.chain_ref)}`,
          ),
        );
        line.appendChild(el('span', 'entry-amount', signed(entry.amount, entry.decimals)));
        box.appendChild(line);
      }
      article.appendChild(box);
    }

    /* metadata */
    const meta = metadataTags(row.metadata);
    if (meta) article.appendChild(meta);

    ui.journals.appendChild(article);
  }

  seenJournals = next;
}

function metadataTags(metadata: Record<string, unknown> | null): HTMLElement | null {
  if (!metadata) return null;
  const keys = Object.keys(metadata);
  if (keys.length === 0) return null;

  const wrap = el('div', 'meta-tags');
  for (const key of keys.sort()) {
    const value = metadata[key];
    const printed =
      value === null || value === undefined
        ? 'null'
        : typeof value === 'object'
          ? JSON.stringify(value)
          : String(value);
    const node = el('span', 'meta-tag');
    node.appendChild(el('b', undefined, `${key} `));
    node.appendChild(document.createTextNode(printed));
    wrap.appendChild(node);
  }
  return wrap;
}

/* --- accounts --------------------------------------------------------- */

function renderAccounts(rows: AccountBalanceRow[]): void {
  clear(ui.accounts);
  ui.accountsCount.textContent = rows.length ? `${rows.length} open` : '';

  if (rows.length === 0) {
    ui.accounts.appendChild(empty('Accounts open the first time an asset is posted to.'));
    return;
  }

  const { wrap, body } = table([
    ['Account', false],
    ['Asset', false],
    ['Balance', true],
    ['Last', false],
  ]);

  for (const row of rows) {
    const tr = el('tr');
    tr.appendChild(
      cell(
        row.kind,
        undefined,
        `account ${row.account_id}\nmerchant ${row.merchant_id}\n${row.entry_count} entries`,
      ),
    );
    tr.appendChild(
      cell(
        `${asset(row.symbol, row.asset_id)} · ${row.network_type}`,
        'dim',
        `${chain(row.network_type, row.chain_ref)}\n${row.asset_address ?? row.asset_kind}`,
      ),
    );
    tr.appendChild(
      cell(units(row.balance, row.decimals), isZero(row.balance) ? 'num faint' : 'num'),
    );
    tr.appendChild(
      cell(
        row.last_activity_at ? since(row.last_activity_at).replace(' ago', '') : '—',
        'dim',
        timestamp(row.last_activity_at),
      ),
    );
    body.appendChild(tr);
  }

  ui.accounts.appendChild(wrap);
}

/* --- sweep backlog ---------------------------------------------------- */

function backlogStatus(row: SweepBacklogRow): HTMLElement {
  if (row.max_attempts > 0) return status('Retrying', 'accent');
  if (!row.next_available_at) return status('Queued', 'muted');
  const due = new Date(row.next_available_at).getTime();
  if (!Number.isNaN(due) && due <= Date.now()) return status('Ready', 'accent');
  return status('Queued', 'muted');
}

function renderBacklog(rows: SweepBacklogRow[]): void {
  clear(ui.backlog);
  ui.backlogCount.textContent = rows.length
    ? `${rows.length} group${rows.length === 1 ? '' : 's'}`
    : '';

  if (rows.length === 0) {
    ui.backlog.appendChild(empty('Nothing waiting. Everything received has been swept.'));
    return;
  }

  for (const row of rows) {
    const hint = row.authority_address
      ? `${row.custody_address}\nauthority ${row.authority_address}`
      : row.custody_address;
    const box = unit(short(row.custody_address, 10, 6), backlogStatus(row), hint);

    box.appendChild(
      el(
        'div',
        'unit-sub',
        `${asset(row.symbol, row.asset_id)} · ${chain(row.network_type, row.chain_ref)} · ${row.custody_kind}`,
      ),
    );

    const pairs: Array<[string, string, boolean?]> = [
      ['amount', units(row.total_amount, row.decimals)],
      ['movements', String(row.movement_count)],
    ];
    if (row.oldest_enqueued_at) pairs.push(['waiting', since(row.oldest_enqueued_at)]);
    if (row.max_attempts > 0) pairs.push(['attempts', String(row.max_attempts)]);
    box.appendChild(facts(pairs));

    ui.backlog.appendChild(box);
  }
}

/* --- reconciliation --------------------------------------------------- */

function renderReconciliation(rows: ReconciliationRow[]): void {
  clear(ui.reconciliation);

  if (rows.length === 0) {
    ui.reconciliation.appendChild(
      empty('Nothing to reconcile until custody holds an unswept balance.'),
    );
    return;
  }

  for (const row of rows) {
    const clean = isZero(row.drift);
    const box = unit(
      `${asset(row.symbol, row.asset_id)} · ${chain(row.network_type, row.chain_ref)}`,
      clean ? status('Matched', 'ok') : status('Drift', 'stop'),
    );

    const pairs: Array<[string, string, boolean?]> = [
      ['ledger', units(row.ledger_unswept, row.decimals)],
      ['queue', units(row.queue_active, row.decimals)],
      ['drift', signed(row.drift, row.decimals), clean],
    ];
    if (!isZero(row.queue_abandoned)) {
      pairs.splice(2, 0, ['abandoned', units(row.queue_abandoned, row.decimals)]);
    }
    box.appendChild(facts(pairs));

    ui.reconciliation.appendChild(box);
  }
}

/* ---------------------------------------------------------------
   Controls
   --------------------------------------------------------------- */

function setScope(next: string | null): void {
  scope = next;
  seenJournals = new Set();
  suppressReveal = true;
  void load();
}

function showCustom(on: boolean): void {
  ui.customScope.classList.toggle('hidden', !on);
  ui.customApply.classList.toggle('hidden', !on);
}

ui.scope.addEventListener('change', () => {
  const value = ui.scope.value;
  if (value === CUSTOM) {
    showCustom(true);
    ui.customId.focus();
    return;
  }
  showCustom(false);
  setScope(value === GLOBAL ? null : value);
});

function applyCustom(): void {
  const value = ui.customId.value.trim();
  if (!UUID_RE.test(value)) {
    showError('That is not a merchant ID. Paste the UUID shown when the account was created.');
    ui.customId.focus();
    return;
  }
  hideError();
  knownMerchants.add(value);
  syncScopeOptions();
  ui.scope.value = value;
  showCustom(false);
  setScope(value);
}

ui.customApply.addEventListener('click', applyCustom);
ui.customId.addEventListener('keydown', (event) => {
  if (event.key === 'Enter') {
    event.preventDefault();
    applyCustom();
  }
});

ui.limit.addEventListener('change', () => {
  limit = Number(ui.limit.value) || 60;
  void load();
});

ui.refresh.addEventListener('click', () => void load());

function setAuto(on: boolean): void {
  auto = on;
  ui.auto.textContent = on ? 'Auto refresh on' : 'Auto refresh off';
  ui.auto.classList.toggle('is-on', on);
  ui.auto.setAttribute('aria-pressed', String(on));

  /* One live region per view — the journals panel, while it is changing. */
  ui.journalsPanel.classList.toggle('is-live', on);

  if (timer !== null) {
    window.clearInterval(timer);
    timer = null;
  }
  if (on) timer = window.setInterval(() => void load(), REFRESH_MS);

  restFeed();
}

ui.auto.addEventListener('click', () => setAuto(!auto));

document.addEventListener('visibilitychange', () => {
  if (document.hidden || !auto) return;
  void load();
});

/* ---------------------------------------------------------------
   Boot
   --------------------------------------------------------------- */

const initial = new URLSearchParams(window.location.search).get('merchant_id');
if (initial && UUID_RE.test(initial)) {
  scope = initial;
  knownMerchants.add(initial);
  syncScopeOptions();
}

ui.auto.setAttribute('aria-pressed', 'false');
void load();
