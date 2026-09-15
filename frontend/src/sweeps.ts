/* =============================================================
   Vestro — manual sweeps (test surface)

   GET  /api/test/sweeps          unclaimed groups, per address+asset
   POST /api/test/sweeps          one group -> one outbound_transfers row
   GET  /api/test/transfers/{id}  the transfer and the rows it claimed

   The outbound schema is still moving, so the transfer is rendered
   field by field off to_jsonb rather than against a fixed shape.
   ============================================================= */

import './style.css';
import './sweeps.css';

/* ---------------------------------------------------------------
   API contract — mirrors SweepGroupRef and the accept body
   --------------------------------------------------------------- */

export interface SweepGroup {
  merchant_id: string;
  network_type: string;
  chain_ref: string;
  custody_address: string;
  asset_id: string;
  asset_symbol: string;
  total: string | number;
  rows: number;
}

export interface SweepAccepted {
  transfer_id: string;
  custody_address: string;
  asset: string;
  queued_total: string | number;
  sweep_rows: number;
  poll: string;
}

export interface TransferView {
  transfer: Record<string, unknown>;
  sweep_queue: Array<Record<string, unknown>>;
}

/** The 409 body carries these inline so the caller can pick one. */
interface AssetChoice {
  asset_id: string;
  symbol: string;
  total: string | number;
}

interface HistoryItem {
  transfer_id: string;
  address: string;
  asset: string;
  requested_at: string;
  status: string;
}

/* ---------------------------------------------------------------
   Config
   --------------------------------------------------------------- */

const API_BASE = '';
const SWEEPS = `${API_BASE}/api/test/sweeps`;
const TRANSFER = (id: string) => `${API_BASE}/api/test/transfers/${id}`;

const QUEUE_REFRESH_MS = 8000;
const POLL_MS = 2000;
const POLL_LIMIT = 150; // five minutes, then stop asking

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

/* Status vocabulary. Unknown words fall through to muted rather than
   inventing a colour for a state the backend has not settled on. */
type Tone = 'muted' | 'accent' | 'ok' | 'warn' | 'stop';

const IN_MOTION = new Set([
  'building', 'built', 'signing', 'signed', 'broadcasting', 'broadcast',
  'submitting', 'submitted', 'sent', 'sweeping', 'retrying', 'confirming',
  'in_flight', 'inflight', 'detected', 'received', 'claimed',
]);
const DONE = new Set([
  'confirmed', 'finalized', 'finished', 'swept', 'complete', 'completed',
  'success', 'succeeded', 'settled', 'delivered',
]);
const INCOMPLETE = new Set([
  'partial', 'underpaid', 'overpaid', 'late', 'reorg', 'stuck',
  'underfunded', 'insufficient_gas', 'needs_attention', 'requires_attention',
]);
const FAILED = new Set([
  'failed', 'error', 'errored', 'cancelled', 'canceled', 'dropped',
  'orphaned', 'expired', 'reverted', 'refunded', 'abandoned', 'rejected',
]);

const TONE_TEXT: Record<Tone, string> = {
  muted: 'text-muted',
  accent: 'text-accent',
  ok: 'text-ok',
  warn: 'text-warn',
  stop: 'text-stop',
};

function toneOf(raw: unknown): { word: string; tone: Tone; terminal: boolean } {
  const word = String(raw ?? 'unknown').trim();
  const key = word.toLowerCase();
  if (DONE.has(key)) return { word, tone: 'ok', terminal: true };
  if (FAILED.has(key)) return { word, tone: 'stop', terminal: true };
  if (INCOMPLETE.has(key)) return { word, tone: 'warn', terminal: false };
  if (IN_MOTION.has(key)) return { word, tone: 'accent', terminal: false };
  return { word, tone: 'muted', terminal: false };
}

/* ---------------------------------------------------------------
   Small helpers
   --------------------------------------------------------------- */

const $ = <T extends HTMLElement>(id: string): T => {
  const el = document.getElementById(id);
  if (!el) throw new Error(`sweeps: missing #${id}`);
  return el as T;
};

const esc = (v: unknown): string =>
  String(v ?? '').replace(/[&<>"']/g, (c) =>
    ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]!));

const short = (s: string, head = 10, tail = 6): string =>
  s.length <= head + tail + 1 ? s : `${s.slice(0, head)}…${s.slice(-tail)}`;

const chainKey = (network: string, ref: string) => `${network}:${ref}`;

const clock = (iso: string | null | undefined): string => {
  if (!iso) return '—';
  const d = new Date(iso);
  return Number.isNaN(d.getTime())
    ? String(iso)
    : d.toLocaleTimeString([], { hour12: false });
};

const stamp = (iso: string | null | undefined): string => {
  if (!iso) return '—';
  const d = new Date(iso);
  return Number.isNaN(d.getTime()) ? String(iso) : d.toISOString().replace('T', ' ').slice(0, 19);
};

/** Amounts arrive as base units. Printed verbatim — no scaling here. */
const amount = (v: unknown): string => String(v ?? '0');

class ApiError extends Error {
  public status: number;

  constructor(status: number, message: string) {
    super(message);
    this.status = status;
    this.name = 'ApiError';
  }

  /** 409 reads "… holds 2 assets — re-send with asset_id: [ … ]". */
  get choices(): AssetChoice[] {
    const at = this.message.indexOf('[');
    if (at < 0) return [];
    try {
      const parsed: unknown = JSON.parse(this.message.slice(at));
      return Array.isArray(parsed) ? (parsed as AssetChoice[]) : [];
    } catch {
      return [];
    }
  }

  /** The part before the inline JSON, which is the readable half. */
  get headline(): string {
    const at = this.message.indexOf('[');
    return (at < 0 ? this.message : this.message.slice(0, at)).trim().replace(/[:—-]\s*$/, '');
  }
}

async function api<T>(url: string, init?: RequestInit): Promise<T> {
  const res = await fetch(url, init);
  const body = await res.text();
  if (!res.ok) throw new ApiError(res.status, body.trim() || res.statusText);
  return (body ? JSON.parse(body) : null) as T;
}

/* ---------------------------------------------------------------
   State
   --------------------------------------------------------------- */

let groups: SweepGroup[] = [];
let history: HistoryItem[] = [];
let currentTransfer: string | null = null;
let pollTimer: number | null = null;
let pollTries = 0;
let queueTimer: number | null = null;
let autoRefresh = false;
let inFlight = false;

/* ---------------------------------------------------------------
   Chrome — feed word, errors
   --------------------------------------------------------------- */

function feed(word: string, tone: Tone): void {
  const el = $('feed-status');
  el.className = `flex items-center gap-2 text-[9px] uppercase tracking-[0.22em] ${TONE_TEXT[tone]}`;
  $('feed-word').textContent = word;
  $('feed-dot').classList.toggle('v-breathe', tone === 'accent');
}

function fail(err: unknown): void {
  const box = $('error-box');
  const msg =
    err instanceof ApiError
      ? `${err.status} — ${err.headline}`
      : err instanceof Error
        ? err.message
        : String(err);
  box.textContent = msg;
  box.classList.remove('hidden');
  feed('Failed', 'stop');
}

function clearError(): void {
  $('error-box').classList.add('hidden');
}

/* ---------------------------------------------------------------
   Queue
   --------------------------------------------------------------- */

function renderQueue(): void {
  const host = $('queue');
  $('queue-count').textContent = groups.length ? `${groups.length} groups` : '—';

  if (!groups.length) {
    host.innerHTML = `
      <p class="v-lbl">Nothing queued</p>
      <p class="mt-3 max-w-[62ch] text-[13px] font-light leading-[1.6] text-muted">
        The ledger queues a row once a deposit is recognised. Send to a deposit address,
        wait for recognition, then refresh.
      </p>`;
    return;
  }

  const rows = groups
    .map((g, i) => {
      const key = chainKey(g.network_type, g.chain_ref);
      return `
      <tr class="v-pick v-reveal" data-i="${i}" style="animation-delay:${Math.min(i, 8) * 40}ms">
        <td class="v-mono text-[12px] text-ink" title="${esc(g.custody_address)}">${esc(short(g.custody_address, 12, 8))}</td>
        <td class="v-mono text-[12px] text-ink">${esc(g.asset_symbol)}</td>
        <td class="v-mono text-[11px] text-muted">${esc(key)}</td>
        <td class="v-mono v-num text-[12px] text-ink">${esc(amount(g.total))}</td>
        <td class="v-mono v-num text-[11px] text-muted">${g.rows}</td>
        <td class="text-right">
          <button type="button" class="v-btn v-btn--sm" data-sweep="${i}">Sweep</button>
        </td>
      </tr>`;
    })
    .join('');

  host.innerHTML = `
  <div class="overflow-x-auto">
    <table class="v-table">
      <thead>
        <tr>
          <th><span class="v-lbl">Address</span></th>
          <th><span class="v-lbl">Asset</span></th>
          <th><span class="v-lbl">Chain</span></th>
          <th class="v-num"><span class="v-lbl">Queued</span></th>
          <th class="v-num"><span class="v-lbl">Rows</span></th>
          <th></th>
        </tr>
      </thead>
      <tbody>${rows}</tbody>
    </table>
  </div>`;

  host.querySelectorAll<HTMLButtonElement>('[data-sweep]').forEach((btn) => {
    btn.addEventListener('click', (e) => {
      e.stopPropagation();
      const g = groups[Number(btn.dataset.sweep)];
      if (g) void sweep(g.custody_address, g.asset_id, null);
    });
  });

  host.querySelectorAll<HTMLTableRowElement>('tr[data-i]').forEach((tr) => {
    tr.addEventListener('click', () => {
      const g = groups[Number(tr.dataset.i)];
      if (!g) return;
      $<HTMLInputElement>('address').value = g.custody_address;
      $<HTMLInputElement>('asset-id').value = g.asset_id;
      hideChoices();
    });
  });
}

async function loadQueue(): Promise<void> {
  if (inFlight) return;
  inFlight = true;
  feed('Syncing', 'accent');
  try {
    groups = await api<SweepGroup[]>(SWEEPS);
    $('generated-at').textContent = clock(new Date().toISOString());
    clearError();
    renderQueue();
    feed(autoRefresh ? 'Watching' : 'Idle', 'muted');
  } catch (err) {
    fail(err);
  } finally {
    inFlight = false;
  }
}

/* ---------------------------------------------------------------
   Sweep
   --------------------------------------------------------------- */

function hideChoices(): void {
  const box = $('choices');
  box.classList.add('hidden');
  box.innerHTML = '';
}

function renderChoices(address: string, choices: AssetChoice[]): void {
  const box = $('choices');
  box.innerHTML = `
    <p class="v-lbl">Two assets</p>
    <p class="mt-3 text-[13px] font-light leading-[1.6] text-muted">
      ${esc(short(address, 12, 8))} holds more than one. Pick the one to drain.
    </p>
    <div class="mt-4 flex flex-col gap-2">
      ${choices
        .map(
          (c) => `
        <button type="button" class="v-btn v-btn--ghost flex w-full items-center justify-between gap-4" data-asset="${esc(c.asset_id)}">
          <span>${esc(c.symbol)}</span>
          <span class="v-mono text-[11px] normal-case tracking-normal">${esc(amount(c.total))}</span>
        </button>`,
        )
        .join('')}
    </div>`;
  box.classList.remove('hidden');

  box.querySelectorAll<HTMLButtonElement>('[data-asset]').forEach((btn) => {
    btn.addEventListener('click', () => {
      const assetId = btn.dataset.asset!;
      $<HTMLInputElement>('asset-id').value = assetId;
      void sweep(address, assetId, null);
    });
  });
}

async function sweep(
  address: string,
  assetId: string | null,
  handlerId: string | null,
): Promise<void> {
  const btn = $<HTMLButtonElement>('submit');
  btn.disabled = true;
  btn.textContent = 'Sweeping';
  hideChoices();
  clearError();
  feed('Sweeping', 'accent');

  try {
    const accepted = await api<SweepAccepted>(SWEEPS, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({
        address,
        asset_id: assetId || null,
        handler_id: handlerId || null,
      }),
    });

    history = [
      {
        transfer_id: accepted.transfer_id,
        address: accepted.custody_address,
        asset: accepted.asset,
        requested_at: new Date().toISOString(),
        status: 'requested',
      },
      ...history.filter((h) => h.transfer_id !== accepted.transfer_id),
    ].slice(0, 12);

    renderHistory();
    watch(accepted.transfer_id);
    void loadQueue();
  } catch (err) {
    if (err instanceof ApiError && err.status === 409) {
      const choices = err.choices;
      if (choices.length) {
        renderChoices(address, choices);
        feed('Idle', 'muted');
      } else {
        fail(err);
      }
    } else {
      fail(err);
    }
  } finally {
    btn.disabled = false;
    btn.textContent = 'Sweep address';
  }
}

/* ---------------------------------------------------------------
   Transfer — the live region
   --------------------------------------------------------------- */

/* Promoted first, then whatever else to_jsonb hands back. */
const PROMOTED = [
  'status', 'state', 'phase',
  'network_type', 'chain_ref', 'asset_id', 'symbol',
  'amount', 'net_amount', 'fee', 'gas_used',
  'from_address', 'to_address', 'destination',
  'tx_hash', 'attempts', 'last_error', 'error',
  'created_at', 'updated_at', 'broadcast_at', 'confirmed_at',
];

function statusOf(t: Record<string, unknown>): unknown {
  return t.status ?? t.state ?? t.phase ?? null;
}

function fieldRow(k: string, v: unknown, t: Record<string, unknown>): string {
  let value: string;

  if (v === null || v === undefined || v === '') {
    value = '<span class="text-faint">—</span>';
  } else if (k === 'tx_hash') {
    const key = chainKey(String(t.network_type ?? ''), String(t.chain_ref ?? ''));
    const url = EXPLORERS[key]?.(String(v));
    value = url
      ? `<a href="${esc(url)}" target="_blank" rel="noreferrer" class="text-ink transition-colors duration-150 hover:text-accent">${esc(short(String(v), 12, 10))}</a>`
      : esc(short(String(v), 12, 10));
  } else if (typeof v === 'object') {
    value = esc(JSON.stringify(v));
  } else if (/_at$/.test(k)) {
    value = esc(stamp(String(v)));
  } else {
    const s = String(v);
    value = esc(s.length > 46 ? short(s, 24, 12) : s);
  }

  return `
    <div class="flex items-baseline justify-between gap-6 border-t border-sunken py-2 first:border-t-0">
      <span class="v-mono text-[11px] text-label">${esc(k)}</span>
      <span class="v-mono text-right text-[12px] text-ink">${value}</span>
    </div>`;
}

function renderTransfer(view: TransferView): void {
  const t = view.transfer ?? {};
  const { word, tone, terminal } = toneOf(statusOf(t));

  const statusEl = $('transfer-status');
  statusEl.className = `text-[9px] uppercase tracking-[0.22em] ${TONE_TEXT[tone]}`;
  statusEl.textContent = word;

  const live = !terminal;
  $('transfer-panel').classList.toggle('v-live', live);
  const dot = $('transfer-dot');
  dot.classList.toggle('hidden', !live);
  dot.classList.toggle('v-breathe', live);

  const keys = Object.keys(t);
  const ordered = [
    ...PROMOTED.filter((k) => keys.includes(k)),
    ...keys.filter((k) => !PROMOTED.includes(k) && k !== 'id').sort(),
  ];

  const claimed = view.sweep_queue ?? [];
  const qKeys = claimed.length
    ? ['id', 'asset_id', 'amount', 'status', 'attempts', 'enqueued_at', 'tx_id'].filter((k) =>
        Object.prototype.hasOwnProperty.call(claimed[0], k),
      )
    : [];
  const queueCols = qKeys.length ? qKeys : claimed.length ? Object.keys(claimed[0]).slice(0, 6) : [];

  $('transfer').innerHTML = `
    <div class="flex flex-wrap items-baseline justify-between gap-4">
      <span class="v-mono text-[12px] text-ink" title="${esc(t.id)}">${esc(short(String(t.id ?? currentTransfer ?? ''), 14, 10))}</span>
      <span class="v-lbl v-lbl--faint">${claimed.length} claimed ${claimed.length === 1 ? 'row' : 'rows'}</span>
    </div>

    <div class="mt-5 bg-sunken/60 p-4">
      ${ordered.map((k) => fieldRow(k, t[k], t)).join('')}
    </div>

    ${
      claimed.length
        ? `<div class="mt-8">
             <span class="v-lbl">Claimed</span>
             <div class="mt-4 overflow-x-auto">
               <table class="v-table">
                 <thead><tr>${queueCols
                   .map((k) => `<th class="v-lbl${/amount/.test(k) ? ' v-num' : ''}">${esc(k)}</th>`)
                   .join('')}</tr></thead>
                 <tbody>
                   ${claimed
                     .map(
                       (r) => `<tr>${queueCols
                         .map((k) => {
                           if (k === 'status') {
                             const s = toneOf(r[k]);
                             return `<td class="text-[9px] uppercase tracking-[0.22em] ${TONE_TEXT[s.tone]}">${esc(s.word)}</td>`;
                           }
                           const raw = r[k];
                           const cell =
                             raw === null || raw === undefined
                               ? '<span class="text-faint">—</span>'
                               : /_at$/.test(k)
                                 ? esc(stamp(String(raw)))
                                 : esc(short(String(raw), 12, 8));
                           return `<td class="v-mono text-[11px] text-ink${/amount/.test(k) ? ' v-num' : ''}">${cell}</td>`;
                         })
                         .join('')}</tr>`,
                     )
                     .join('')}
                 </tbody>
               </table>
             </div>
           </div>`
        : ''
    }`;

  const item = history.find((h) => h.transfer_id === currentTransfer);
  if (item) {
    item.status = word;
    renderHistory();
  }

  if (terminal) stopPoll();
}

function stopPoll(): void {
  if (pollTimer !== null) {
    window.clearInterval(pollTimer);
    pollTimer = null;
  }
  feed(autoRefresh ? 'Watching' : 'Idle', 'muted');
}

async function readTransfer(id: string): Promise<void> {
  try {
    const view = await api<TransferView>(TRANSFER(id));
    if (currentTransfer !== id) return; // another sweep took over
    renderTransfer(view);
    clearError();
  } catch (err) {
    stopPoll();
    fail(err);
  }
}

function watch(id: string): void {
  stopPoll();
  currentTransfer = id;
  pollTries = 0;
  feed('Sweeping', 'accent');
  void readTransfer(id);

  pollTimer = window.setInterval(() => {
    pollTries += 1;
    if (pollTries > POLL_LIMIT) {
      stopPoll();
      return;
    }
    void readTransfer(id);
  }, POLL_MS);
}

/* ---------------------------------------------------------------
   Session history
   --------------------------------------------------------------- */

function renderHistory(): void {
  const host = $('history');
  $('history-count').textContent = history.length ? String(history.length) : '—';

  if (!history.length) {
    host.innerHTML = `
      <p class="v-lbl">Nothing yet</p>
      <p class="mt-3 text-[13px] font-light leading-[1.6] text-muted">
        Transfers opened in this session are listed here.
      </p>`;
    return;
  }

  host.innerHTML = history
    .map((h) => {
      const s = toneOf(h.status);
      const open = h.transfer_id === currentTransfer;
      return `
      <button type="button" data-open="${esc(h.transfer_id)}"
              class="flex w-full items-baseline justify-between gap-4 border-t border-sunken py-3 text-left transition-colors duration-150 first:border-t-0 hover:bg-sunken/40 ${open ? 'bg-sunken/40' : ''}">
        <span class="min-w-0">
          <span class="v-mono block truncate text-[12px] text-ink">${esc(short(h.address, 10, 6))}</span>
          <span class="v-mono block text-[11px] text-muted">${esc(h.asset)} · ${esc(clock(h.requested_at))}</span>
        </span>
        <span class="shrink-0 text-[9px] uppercase tracking-[0.22em] ${TONE_TEXT[s.tone]}">${esc(s.word)}</span>
      </button>`;
    })
    .join('');

  host.querySelectorAll<HTMLButtonElement>('[data-open]').forEach((btn) => {
    btn.addEventListener('click', () => watch(btn.dataset.open!));
  });
}

/* ---------------------------------------------------------------
   Wiring
   --------------------------------------------------------------- */

function setAuto(on: boolean): void {
  autoRefresh = on;
  const btn = $<HTMLButtonElement>('auto');
  btn.textContent = on ? 'Auto refresh on' : 'Auto refresh off';
  btn.classList.toggle('v-btn--on', on);

  if (queueTimer !== null) {
    window.clearInterval(queueTimer);
    queueTimer = null;
  }
  if (on) queueTimer = window.setInterval(() => void loadQueue(), QUEUE_REFRESH_MS);
  if (pollTimer === null) feed(on ? 'Watching' : 'Idle', 'muted');
}

function boot(): void {
  renderQueue();
  renderHistory();

  $('transfer').innerHTML = `
    <p class="v-lbl">No transfer</p>
    <p class="mt-3 max-w-[62ch] text-[13px] font-light leading-[1.6] text-muted">
      Sweep an address and the transfer opens here, refreshing until it settles.
    </p>`;

  $('refresh').addEventListener('click', () => void loadQueue());
  $('auto').addEventListener('click', () => setAuto(!autoRefresh));
  $('clear').addEventListener('click', () => {
    (['address', 'asset-id', 'handler-id'] as const).forEach((id) => {
      $<HTMLInputElement>(id).value = '';
    });
    hideChoices();
    clearError();
  });

  $<HTMLFormElement>('sweep-form').addEventListener('submit', (e) => {
    e.preventDefault();
    const address = $<HTMLInputElement>('address').value.trim();
    const assetId = $<HTMLInputElement>('asset-id').value.trim();
    const handlerId = $<HTMLInputElement>('handler-id').value.trim();

    if (!address) {
      fail(new Error('Enter the deposit address to drain.'));
      return;
    }
    if (assetId && !UUID_RE.test(assetId)) {
      fail(new Error('Asset must be a UUID, or empty when the address holds one asset.'));
      return;
    }
    void sweep(address, assetId || null, handlerId || null);
  });

  void loadQueue();
}

boot();
