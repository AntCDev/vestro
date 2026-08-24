import QRCode from 'qrcode';
import { Buffer } from 'buffer';

// @solana/spl-token builds instruction data with Node's Buffer.
if (!(globalThis as unknown as { Buffer?: unknown }).Buffer) {
  (globalThis as unknown as { Buffer: unknown }).Buffer = Buffer;
}

/* ════════════════════════════════════════════════════════════════════════════
   CONFIG — fill these in
   ════════════════════════════════════════════════════════════════════════════ */

/** Reown (WalletConnect) project id — https://dashboard.reown.com */
const REOWN_PROJECT_ID = '7aba1a66fad0cbd0745cca56acc6ee6f';

/** Shown inside the wallet's approval sheet. Must match the deployed origin. */
const APP_METADATA = {
  name: 'rust-crypto',
  description: 'Invoice payment',
  url: window.location.origin, // must match the origin the page is served from
  icons: ['TODO_ABSOLUTE_URL_TO_ICON_PNG'],
};

/**
 * The backend calls the wallet path needs. This page never talks to a node
 * and never holds an RPC key.
 *
 * By default only `blockhash` is used: the wallet broadcasts through its own
 * RPC, so your server is never asked to relay bytes a stranger handed it.
 * `submit` is the fallback for wallets that can sign but not send — see
 * BROADCAST_VIA_BACKEND below.
 */
const ENDPOINTS = {
  blockhash: (id: string) => `/api/invoices/${encodeURIComponent(id)}/solana/blockhash`,
  submit: (id: string) => `/api/invoices/${encodeURIComponent(id)}/solana/submit`,
};

/**
 * Off: the wallet signs and broadcasts it itself, and the only server call is
 * for the blockhash. Nothing reaches your RPC that you didn't put there.
 *
 * On: the wallet only signs, and we POST the raw bytes for your server to
 * broadcast. Turn this on if you hit a wallet that implements
 * solana_signTransaction but not solana_signAndSendTransaction — mostly a
 * mobile-over-WalletConnect problem, injected wallets do both. If you do turn
 * it on, validate the transaction server-side before broadcasting; that
 * endpoint is otherwise an open relay pointed at your RPC quota.
 */
const BROADCAST_VIA_BACKEND = false;

/**
 * SPL only. The recipient's associated token account has to exist before a
 * transfer can land in it, and a freshly derived invoice address usually has
 * no ATA yet. With this on we prepend an idempotent create instruction: it is
 * a no-op if the account already exists, and costs the payer ~0.002 SOL of
 * rent if it does not. Turn it off only if you create the ATA yourself.
 */
const CREATE_RECIPIENT_ATA = true;

/** Status strings from your invoices table. Adjust to match your enum. */
const SETTLED = new Set(['paid', 'confirmed', 'completed', 'settled']);
const DEAD = new Set(['expired', 'cancelled', 'canceled', 'failed', 'void']);

const POLL_MS = 4_000;
const POLL_MS_HIDDEN = 20_000;

/* ════════════════════════════════════════════════════════════════════════════
   API types — mirror of the Rust serializers
   ════════════════════════════════════════════════════════════════════════════ */

interface CheckoutInvoice {
  id: string;
  merchant_id: string;
  token_id: string;
  token_name: string;
  token_detail: string;
  token_decimals: number | null;
  /** base units, as a string */
  amount_requested: string;
  amount_received: string;
  /** ATA for an SPL mint, owner address for native SOL */
  wallet_address: string;
  /** always the HD-derived owner address on Solana */
  payment_reference: string | null;
  status: string;
  required_confirmations: number | null;
  created_at: string;
  expires_at: string;
}

interface CheckoutViewInfo {
  id: string;
  path: string;
}

/** `data` as produced by sol_checkout_data. */
interface SolCheckoutData {
  cluster: string;
  token: {
    symbol: string;
    mint: string | null;
    decimals: number;
    is_native: boolean;
  };
  amount: {
    base_units: string;
    display: string;
  };
  naive_path: {
    deposit_address: string;
  };
  smart_path: {
    kind: string;
    recipient: string;
    reference: string;
    mint: string | null;
    token_program: string | null;
    solana_pay_url: string;
  };
}

interface CheckoutResponse {
  invoice: CheckoutInvoice;
  view: CheckoutViewInfo;
  data: SolCheckoutData;
}

interface PaymentSummary {
  tx_hash: string;
  amount: string;
  confirmations: number;
  status: string;
  payment_path: string | null;
}

interface StatusResponse {
  status: string;
  amount_requested: string;
  amount_received: string;
  required_confirmations: number | null;
  expires_at: string;
  updated_at: string;
  payments: PaymentSummary[];
  data: unknown;
}

/* ════════════════════════════════════════════════════════════════════════════
   DOM
   ════════════════════════════════════════════════════════════════════════════ */

const $ = <T extends HTMLElement = HTMLElement>(id: string): T => {
  const el = document.getElementById(id);
  if (!el) throw new Error(`missing element #${id}`);
  return el as T;
};

const show = (el: HTMLElement, on = true) => el.classList.toggle('hidden', !on);
const setText = (id: string, value: string) => { $(id).textContent = value; };

const truncate = (s: string, head = 6, tail = 6) =>
  s.length <= head + tail + 1 ? s : `${s.slice(0, head)}…${s.slice(-tail)}`;

function fail(message: string) {
  const box = $('error-box');
  box.textContent = message;
  show(box);
}

function notify(message: string) {
  const box = $('notice-box');
  box.textContent = message;
  show(box);
}

/* ════════════════════════════════════════════════════════════════════════════
   State
   ════════════════════════════════════════════════════════════════════════════ */

let invoiceId = '';
let checkout: CheckoutResponse | null = null;
let latest: StatusResponse | null = null;
let stopped = false;

/* ════════════════════════════════════════════════════════════════════════════
   Formatting
   ════════════════════════════════════════════════════════════════════════════ */

/** base units -> display units, without ever touching a float. */
function toDisplay(base: string, decimals: number): string {
  const negative = base.startsWith('-');
  const digits = (negative ? base.slice(1) : base).replace(/^0+(?=\d)/, '');
  if (decimals === 0) return (negative ? '-' : '') + digits;

  const padded = digits.padStart(decimals + 1, '0');
  const whole = padded.slice(0, padded.length - decimals);
  const frac = padded.slice(padded.length - decimals).replace(/0+$/, '');
  const grouped = whole.replace(/\B(?=(\d{3})+(?!\d))/g, ',');
  return `${negative ? '-' : ''}${grouped}${frac ? `.${frac}` : ''}`;
}

function explorerTx(hash: string, cluster: string): string {
  const suffix =
    cluster === 'mainnet-beta' || cluster === 'mainnet' ? '' : `?cluster=${encodeURIComponent(cluster)}`;
  return `https://solscan.io/tx/${hash}${suffix}`;
}

function formatCountdown(ms: number): string {
  if (ms <= 0) return 'expired';
  const total = Math.floor(ms / 1000);
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = total % 60;
  const pad = (n: number) => String(n).padStart(2, '0');
  return h > 0 ? `${h}:${pad(m)}:${pad(s)} left` : `${pad(m)}:${pad(s)} left`;
}

/* ════════════════════════════════════════════════════════════════════════════
   Boot
   ════════════════════════════════════════════════════════════════════════════ */

async function boot() {
  const id = new URLSearchParams(window.location.search).get('id');
  if (!id) {
    show($('loading-state'), false);
    fail('This link is missing an invoice id. Open the payment link you were given again.');
    return;
  }
  invoiceId = id;

  try {
    const res = await fetch(`/api/invoices/${encodeURIComponent(invoiceId)}/checkout`);
    if (!res.ok) {
      throw new Error(res.status === 404 ? 'Invoice not found.' : await res.text());
    }
    checkout = (await res.json()) as CheckoutResponse;
  } catch (e) {
    show($('loading-state'), false);
    fail(`Could not load this invoice. ${(e as Error).message}`);
    return;
  }

  show($('loading-state'), false);
  show($('checkout-view'));

  renderStatic(checkout);
  wireInteractions(checkout);

  tickExpiry();
  window.setInterval(tickExpiry, 1000);

  applyStatus({
    status: checkout.invoice.status,
    amount_requested: checkout.invoice.amount_requested,
    amount_received: checkout.invoice.amount_received,
    required_confirmations: checkout.invoice.required_confirmations,
    expires_at: checkout.invoice.expires_at,
    updated_at: checkout.invoice.created_at,
    payments: [],
    data: null,
  });

  poll();
}

/* ════════════════════════════════════════════════════════════════════════════
   Static render
   ════════════════════════════════════════════════════════════════════════════ */

function renderStatic(c: CheckoutResponse) {
  const { invoice, data } = c;
  const decimals = data.token.decimals;
  const symbol = data.token.symbol;

  setText('invoice-id', truncate(invoice.id, 8, 8));
  setText('amount-display', data.amount.display || toDisplay(invoice.amount_requested, decimals));
  setText('amount-symbol', symbol);
  setText('amount-base', `${invoice.amount_requested} base units · ${decimals} decimals`);
  setText(
    'token-line',
    data.token.is_native
      ? `Native SOL · ${data.cluster}`
      : `${invoice.token_name || symbol} · ${truncate(data.token.mint ?? '', 4, 4)} · ${data.cluster}`,
  );
  setText('warn-symbol', symbol);
  setText('warn-cluster', data.cluster);

  // ── Manual path: owner address only. Wallets derive the ATA themselves.
  const deposit = data.naive_path.deposit_address;
  setText('deposit-address', deposit);
  if (!data.token.is_native) {
    setText('ata-symbol', symbol);
    show($('ata-note'));
  }
  drawQR('address-qr', deposit);

  // ── Wallet path: Solana Pay transfer request, recipient + amount + reference.
  drawQR('pay-qr', data.smart_path.solana_pay_url);
}

function drawQR(canvasId: string, text: string) {
  const canvas = $<HTMLCanvasElement>(canvasId);
  QRCode.toCanvas(canvas, text, {
    width: 440,
    margin: 1,
    errorCorrectionLevel: 'M',
    color: { dark: '#07070b', light: '#ffffff' },
  })
    .then(() => {
      // qrcode writes a fixed pixel width AND height onto the element as inline
      // style. Inline style beats the utility classes, so the canvas ends up
      // clamped horizontally by its container while keeping the full 440px of
      // height — that's the vertical stretch. Drop what the library set and
      // size it ourselves; the 440px backing store stays for crispness.
      canvas.removeAttribute('style');
      canvas.style.width = '100%';
      canvas.style.height = 'auto';
    })
    .catch((e: unknown) => console.error('qr failed', e));
}

/* ════════════════════════════════════════════════════════════════════════════
   Interactions
   ════════════════════════════════════════════════════════════════════════════ */

const TAB_ON = ['bg-panel-2', 'text-chalk'];
const TAB_OFF = ['text-mist'];

function selectPath(path: 'wallet' | 'manual') {
  const wallet = path === 'wallet';
  const tabWallet = $('tab-wallet');
  const tabManual = $('tab-manual');

  tabWallet.classList.remove(...TAB_ON, ...TAB_OFF);
  tabManual.classList.remove(...TAB_ON, ...TAB_OFF);
  tabWallet.classList.add(...(wallet ? TAB_ON : TAB_OFF));
  tabManual.classList.add(...(wallet ? TAB_OFF : TAB_ON));
  tabWallet.setAttribute('aria-selected', String(wallet));
  tabManual.setAttribute('aria-selected', String(!wallet));

  show($('panel-wallet'), wallet);
  show($('panel-manual'), !wallet);
}

function wireInteractions(c: CheckoutResponse) {
  selectPath('wallet');
  $('tab-wallet').addEventListener('click', () => selectPath('wallet'));
  $('tab-manual').addEventListener('click', () => selectPath('manual'));

  const copyBtn = $<HTMLButtonElement>('copy-address');
  copyBtn.addEventListener('click', async () => {
    await navigator.clipboard.writeText(c.data.naive_path.deposit_address);
    copyBtn.textContent = 'Copied';
    window.setTimeout(() => { copyBtn.textContent = 'Copy'; }, 1600);
  });

  const scanBlock = $('scan-block');
  $('toggle-scan').addEventListener('click', () => {
    const opening = scanBlock.classList.contains('hidden');
    show(scanBlock, opening);
    setText('toggle-scan-icon', opening ? '−' : '+');
  });

  $('btn-connect').addEventListener('click', () => void connectWallet());
  $('btn-send').addEventListener('click', () => void sendPayment());
}

/* ════════════════════════════════════════════════════════════════════════════
   Status + polling
   ════════════════════════════════════════════════════════════════════════════ */

async function poll() {
  while (!stopped) {
    try {
      const res = await fetch(`/api/invoices/${encodeURIComponent(invoiceId)}/status`);
      if (res.ok) {
        latest = (await res.json()) as StatusResponse;
        applyStatus(latest);
        if (SETTLED.has(latest.status) || DEAD.has(latest.status)) {
          stopped = true;
          setText('poll-indicator', 'Closed');
          break;
        }
      }
    } catch {
      // Network blip. Keep polling — the next tick is the retry.
    }
    await new Promise((r) => window.setTimeout(r, document.hidden ? POLL_MS_HIDDEN : POLL_MS));
  }
}

function applyStatus(s: StatusResponse) {
  if (!checkout) return;
  const decimals = checkout.data.token.decimals;
  const symbol = checkout.data.token.symbol;

  // ── pill
  const dot = $('status-dot');
  const pill = $('status-pill');
  dot.classList.remove('bg-mist', 'bg-sol-teal', 'bg-rose', 'bg-amber', 'breathe');
  pill.classList.remove('text-mist', 'text-sol-teal', 'text-rose', 'text-amber', 'border-line', 'border-sol-teal/40', 'border-rose/40');

  if (SETTLED.has(s.status)) {
    dot.classList.add('bg-sol-teal');
    pill.classList.add('text-sol-teal', 'border-sol-teal/40');
  } else if (DEAD.has(s.status)) {
    dot.classList.add('bg-rose');
    pill.classList.add('text-rose', 'border-rose/40');
  } else {
    dot.classList.add('bg-sol-teal', 'breathe');
    pill.classList.add('text-mist', 'border-line');
  }
  setText('status-text', s.status.replace(/_/g, ' '));

  // ── received
  const requested = BigInt(s.amount_requested || '0');
  const received = BigInt(s.amount_received || '0');
  if (received > 0n) {
    show($('received-row'));
    setText('amount-received', `${toDisplay(s.amount_received, decimals)} ${symbol}`);
    const pct = requested > 0n ? Number((received * 100n) / requested) : 0;
    $('received-bar').style.width = `${Math.min(100, pct)}%`;
  }

  // ── payments
  renderPayments(s.payments, s.required_confirmations);

  // ── terminal states take over the two action buttons
  if (SETTLED.has(s.status)) {
    lockActions('Paid');
    notify('Payment confirmed. You can close this page.');
  } else if (DEAD.has(s.status)) {
    lockActions('Closed');
    fail(`This invoice is ${s.status}. Ask the merchant for a new payment link.`);
  }
}

function lockActions(label: string) {
  const send = $<HTMLButtonElement>('btn-send');
  const connect = $<HTMLButtonElement>('btn-connect');
  send.disabled = true;
  connect.disabled = true;
  send.textContent = label;
}

function renderPayments(payments: PaymentSummary[], required: number | null) {
  if (!checkout || payments.length === 0) return;
  const { cluster, token } = checkout.data;

  show($('payments-section'));
  setText('payments-count', `${payments.length}`);

  const list = $('payments-list');
  list.replaceChildren(
    ...payments.map((p) => {
      const row = document.createElement('a');
      row.href = explorerTx(p.tx_hash, cluster);
      row.target = '_blank';
      row.rel = 'noreferrer';
      row.className =
        'block rounded-[10px] border border-line bg-panel-2 px-3 py-2.5 transition-colors hover:border-sol-teal/40';

      const confirmed = required != null && p.confirmations >= required;
      const conf = required != null ? `${p.confirmations}/${required} conf` : `${p.confirmations} conf`;

      row.innerHTML = `
        <div class="flex items-baseline justify-between gap-3">
          <span class="font-mono text-[12px] tabular-nums text-chalk">
            ${toDisplay(p.amount, token.decimals)} ${token.symbol}
          </span>
          <span class="font-mono text-[10px] uppercase tracking-[0.14em] ${confirmed ? 'text-sol-teal' : 'text-mist'}">
            ${conf}
          </span>
        </div>
        <div class="mt-1 flex items-baseline justify-between gap-3">
          <span class="font-mono text-[10px] text-mist/70">${truncate(p.tx_hash, 8, 8)}</span>
          <span class="font-mono text-[10px] text-mist/70">${p.payment_path ?? ''}</span>
        </div>`;
      return row;
    }),
  );
}

/* ── expiry fuse ─────────────────────────────────────────────────────────── */

function tickExpiry() {
  if (!checkout) return;
  const start = new Date(checkout.invoice.created_at).getTime();
  const end = new Date(checkout.invoice.expires_at).getTime();
  const now = Date.now();

  setText('expiry-line', formatCountdown(end - now));

  const span = Math.max(1, end - start);
  const left = Math.max(0, Math.min(1, (end - now) / span));
  $('fuse-bar').style.width = `${left * 100}%`;
}

/* ════════════════════════════════════════════════════════════════════════════
   Wallet path — Reown AppKit + @solana/web3.js
   Everything heavy is imported on first click so the page paints (and the
   manual path works) without waiting on the wallet bundle.
   ════════════════════════════════════════════════════════════════════════════ */

type AppKitLike = {
  open(): Promise<void>;
  subscribeAccount(cb: (a: { isConnected?: boolean; address?: string }) => void): void;
  getWalletProvider(): unknown;
};

let appKit: AppKitLike | null = null;
let connectedAddress: string | null = null;

async function getAppKit(): Promise<AppKitLike> {
  if (appKit) return appKit;

  const [{ createAppKit }, { SolanaAdapter }, networks] = await Promise.all([
    import('@reown/appkit'),
    import('@reown/appkit-adapter-solana'),
    import('@reown/appkit/networks'),
  ]);

  const cluster = checkout!.data.cluster;
  const network =
    cluster === 'devnet' ? networks.solanaDevnet
    : cluster === 'testnet' ? networks.solanaTestnet
    : networks.solana;

  appKit = createAppKit({
    // Pass wallet adapters here if you want Phantom/Solflare listed as
    // injected options alongside the WalletConnect QR:
    //   new SolanaAdapter({ wallets: [new PhantomWalletAdapter()] })
    adapters: [new SolanaAdapter({})],
    networks: [network],
    defaultNetwork: network,
    projectId: REOWN_PROJECT_ID,
    metadata: APP_METADATA,
    features: { analytics: false, email: false, socials: false },
  }) as unknown as AppKitLike;

  appKit.subscribeAccount((account) => {
    if (account?.isConnected && account.address) {
      onWalletConnected(account.address);
    } else {
      onWalletDisconnected();
    }
  });

  return appKit;
}

/** Step 1 — opens the AppKit modal, which is where the WalletConnect QR lives. */
async function connectWallet() {
  const detail = $('connect-detail');
  try {
    detail.textContent = 'Opening wallet…';
    const kit = await getAppKit();
    await kit.open();
  } catch (e) {
    detail.textContent = '';
    fail(`Could not open the wallet picker. ${(e as Error).message}`);
  }
}

function onWalletConnected(address: string) {
  connectedAddress = address;
  setText('connect-detail', address);
  $<HTMLButtonElement>('btn-connect').textContent = 'Change wallet';

  if (checkout && !SETTLED.has(checkout.invoice.status)) {
    $<HTMLButtonElement>('btn-send').disabled = false;
    $('step-2').classList.remove('bg-line');
    $('step-2').classList.add('bg-sol-teal');
    setText(
      'send-detail',
      `Sends ${checkout.data.amount.display} ${checkout.data.token.symbol} in one transaction.`,
    );
  }
}

function onWalletDisconnected() {
  connectedAddress = null;
  setText('connect-detail', '');
  setText('send-detail', '');
  $<HTMLButtonElement>('btn-connect').textContent = 'Connect wallet';
  $<HTMLButtonElement>('btn-send').disabled = true;
  $('step-2').classList.add('bg-line');
  $('step-2').classList.remove('bg-sol-teal');
}

/* ── The backend calls ───────────────────────────────────────────────────────
   Everything that would otherwise need an RPC key lives behind these. Both are
   scoped to the invoice id so the server can reject anything that isn't a live
   invoice, and neither takes a URL or a cluster from the client — the server
   already knows which network this invoice is on.

   Only the first is used unless BROADCAST_VIA_BACKEND is on.
   ──────────────────────────────────────────────────────────────────────────── */

interface BlockhashResponse {
  /** base58 blockhash, goes straight into tx.recentBlockhash */
  blockhash: string;
  /** optional; the server keeps it to know when the tx can no longer land */
  last_valid_block_height?: number;
}

async function fetchRecentBlockhash(): Promise<BlockhashResponse> {
  const res = await fetch(ENDPOINTS.blockhash(invoiceId), {
    method: 'GET',
    headers: { accept: 'application/json' },
  });
  if (!res.ok) throw new Error((await res.text()) || 'could not get a recent blockhash');
  return (await res.json()) as BlockhashResponse;
}

interface SubmitResponse {
  /** base58 transaction signature, echoed back for the explorer link */
  signature: string;
}

/**
 * TODO(backend) — POST /api/invoices/:id/solana/submit
 * OPTIONAL. Only called when BROADCAST_VIA_BACKEND is on.
 *
 * sends:    { "transaction": "<base64 of the fully signed, serialized tx>" }
 * expects:  200 { "signature": "5Kd3…" }
 *           4xx with a plain-text or { "error": "…" } body — whatever you
 *           return here is shown to the payer verbatim, so keep it readable
 *
 * Server side: base64-decode, sendRawTransaction against your keyed RPC, and
 * return the signature. Deserialize and check it first — right destination,
 * right mint, amount >= amount_requested, reference present, no instructions
 * you didn't expect. A stranger's bytes can't spend anything of yours, but
 * without the check this endpoint will happily burn your RPC quota
 * broadcasting whatever it's handed.
 */
async function submitSignedTransaction(transactionBase64: string): Promise<string> {
  const res = await fetch(ENDPOINTS.submit(invoiceId), {
    method: 'POST',
    headers: { 'content-type': 'application/json', accept: 'application/json' },
    body: JSON.stringify({ transaction: transactionBase64 }),
  });
  if (!res.ok) throw new Error((await res.text()) || 'the network rejected this transaction');
  const body = (await res.json()) as SubmitResponse;
  return body.signature;
}

/** Uint8Array -> base64, chunked so a large tx doesn't blow the call stack. */
function toBase64(bytes: Uint8Array): string {
  let binary = '';
  const CHUNK = 0x8000;
  for (let i = 0; i < bytes.length; i += CHUNK) {
    binary += String.fromCharCode(...bytes.subarray(i, i + CHUNK));
  }
  return btoa(binary);
}

/** Step 2 — build, sign, send. One signature, no approval step. */
async function sendPayment() {
  if (!checkout || !connectedAddress) return;
  const sendBtn = $<HTMLButtonElement>('btn-send');
  const detail = $('send-detail');
  const { data } = checkout;

  sendBtn.disabled = true;
  detail.textContent = 'Building transaction…';

  try {
    const web3 = await import('@solana/web3.js');
    const { PublicKey, Transaction, SystemProgram } = web3;

    const payer = new PublicKey(connectedAddress);
    const owner = new PublicKey(data.smart_path.reference); // HD-derived owner
    const reference = owner; // Solana Pay reference == owner address
    const amount = BigInt(data.amount.base_units);

    const tx = new Transaction();

    if (data.token.is_native) {
      // Native SOL: recipient == owner == wallet_address, all the same key.
      const ix = SystemProgram.transfer({
        fromPubkey: payer,
        toPubkey: new PublicKey(data.smart_path.recipient),
        lamports: amount,
      });
      // Solana Pay: reference rides along as a read-only, non-signer key so
      // your watcher can find this transfer by address.
      ix.keys.push({ pubkey: reference, isSigner: false, isWritable: false });
      tx.add(ix);
    } else {
      const spl = await import('@solana/spl-token');
      const mint = new PublicKey(data.token.mint!);
      const programId = data.smart_path.token_program
        ? new PublicKey(data.smart_path.token_program)
        : spl.TOKEN_PROGRAM_ID;

      // WalletConnect does not resolve token accounts for us — the transaction
      // has to name them. The payer's ATA is derived here; the recipient's is
      // whatever the backend already committed to invoices.wallet_address.
      const source = spl.getAssociatedTokenAddressSync(mint, payer, false, programId);
      const destination = new PublicKey(data.smart_path.recipient);

      // Sanity check: recipient should be the ATA of (owner, mint).
      const derived = spl.getAssociatedTokenAddressSync(mint, owner, true, programId);
      if (!derived.equals(destination)) {
        console.warn('recipient is not the ATA derived from the reference owner', {
          recipient: destination.toBase58(),
          derived: derived.toBase58(),
        });
      }

      if (CREATE_RECIPIENT_ATA) {
        tx.add(
          spl.createAssociatedTokenAccountIdempotentInstruction(
            payer,
            destination,
            owner,
            mint,
            programId,
          ),
        );
      }

      const ix = spl.createTransferCheckedInstruction(
        source,
        mint,
        destination,
        payer,
        amount,
        data.token.decimals,
        [],
        programId,
      );
      ix.keys.push({ pubkey: reference, isSigner: false, isWritable: false });
      tx.add(ix);
    }

    tx.feePayer = payer;

    // The one server call this path makes. Blockhash only — no key, no relay.
    const { blockhash } = await fetchRecentBlockhash();
    tx.recentBlockhash = blockhash;

    detail.textContent = 'Waiting for your signature…';

    const provider = (await getAppKit()).getWalletProvider() as {
      signAndSendTransaction?: <T>(t: T) => Promise<{ signature?: string } | string>;
      signTransaction?: <T>(t: T) => Promise<T>;
    };

    let signature: string;

    if (!BROADCAST_VIA_BACKEND) {
      // Default: the wallet signs and pushes it through its own RPC. Your
      // server sees this payment the same way it sees a scanned Solana Pay
      // transfer — when the watcher picks it up on chain.
      if (!provider?.signAndSendTransaction) {
        throw new Error(
          'this wallet can sign but not broadcast — turn on BROADCAST_VIA_BACKEND to support it',
        );
      }
      const result = await provider.signAndSendTransaction(tx);
      signature = typeof result === 'string' ? result : (result.signature ?? '');
    } else {
      // Fallback: sign here, broadcast on the server.
      if (!provider?.signTransaction) {
        throw new Error('this wallet cannot sign Solana transactions in a browser');
      }
      const signed = await provider.signTransaction(tx);
      detail.textContent = 'Broadcasting…';
      signature = await submitSignedTransaction(toBase64(signed.serialize()));
    }

    detail.textContent = signature ? `Sent · ${truncate(signature, 8, 8)}` : 'Sent';
    sendBtn.textContent = 'Sent';
    notify('Transaction sent. This page updates as soon as your API sees it on chain.');
  } catch (e) {
    const message = (e as Error).message ?? String(e);
    detail.textContent = '';
    sendBtn.disabled = false;
    // A user closing the wallet sheet is not an error worth shouting about.
    if (/reject|denied|cancel/i.test(message)) {
      detail.textContent = 'Signature declined. Try again when you are ready.';
    } else {
      fail(`The payment could not be sent. ${message}`);
    }
  }
}

boot();
