import QRCode from 'qrcode';

/* ════════════════════════════════════════════════════════════════════════════
   CONFIG — fill these in
   ════════════════════════════════════════════════════════════════════════════ */

/** Reown (WalletConnect) project id — https://dashboard.reown.com */
const REOWN_PROJECT_ID = import.meta.env.VITE_REOWN_PROJECT_ID || '';

/** Shown inside the wallet's approval sheet. Must match the deployed origin. */
const APP_METADATA = {
  name: 'vestro',
  description: 'Invoice payment',
  url: window.location.origin, // must match the origin the page is served from
  icons: ['TODO_ABSOLUTE_URL_TO_ICON_PNG'],
};

/**
 * This page never talks to a node and never holds an RPC key. Unlike the
 * Solana view there is no pre-sign endpoint to call: EVM transactions carry no
 * server-supplied blockhash, and nonce and gas are the wallet's job. Every read
 * below (chain id, allowance, receipt) goes through the connected wallet's own
 * provider.
 */

/** Status strings from your invoices table. Adjust to match your enum. */
const SETTLED = new Set(['paid', 'confirmed', 'completed', 'settled']);
const DEAD = new Set(['expired', 'cancelled', 'canceled', 'failed', 'void']);

const POLL_MS = 4_000;
const POLL_MS_HIDDEN = 20_000;

/** How long we wait on a receipt before telling the payer to check themselves. */
const RECEIPT_TIMEOUT_MS = 300_000;
const RECEIPT_POLL_MS = 3_000;

const EXPLORERS: Record<number, string> = {
  1: 'https://etherscan.io',
  10: 'https://optimistic.etherscan.io',
  137: 'https://polygonscan.com',
  8453: 'https://basescan.org',
  42161: 'https://arbiscan.io',
  80002: 'https://amoy.polygonscan.com',
  84532: 'https://sepolia.basescan.org',
  11155111: 'https://sepolia.etherscan.io',
};

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
  /** the HD-derived deposit address for this invoice */
  wallet_address: string;
  /** 0x + 32 hex — the bytes16 identifier the vault emits */
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

interface CallArg {
  name: string;
  type: string;
  /** always a string on the wire — uint256 does not survive a JS number */
  value: string;
}

interface Approval {
  token_address: string;
  spender: string;
  amount_base_units: string;
  allowance_abi: string;
  approve_abi: string;
}

interface SmartPath {
  kind: string;
  vault_address: string;
  merchant_wallet: string;
  identifier: string;
  call: {
    /** one human-readable signature, e.g. "function pay(address token, ...)" */
    abi: string;
    function_name: string;
    args: CallArg[];
    value_base_units: string;
  };
  /** null for native — the pay step runs straight away */
  approval: Approval | null;
}

/** `data` as produced by evm_checkout_data. */
interface EvmCheckoutData {
  chain: {
    chain_id: number;
    name: string;
  };
  token: {
    symbol: string;
    address: string | null;
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
  /** null when no vault is configured for this chain */
  smart_path: SmartPath | null;
}

interface CheckoutResponse {
  invoice: CheckoutInvoice;
  view: CheckoutViewInfo;
  data: EvmCheckoutData;
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

let connectedAddress: string | null = null;
let chainOk = false;
/** null = not checked yet (ERC-20); false = allowance already covers it */
let needsApproval: boolean | null = null;
let closed = false;

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

function explorerTx(hash: string, chainId: number): string {
  const base = EXPLORERS[chainId];
  return base ? `${base}/tx/${hash}` : `https://blockscan.com/tx/${hash}`;
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

const toQuantity = (v: bigint) => `0x${v.toString(16)}`;
const sleep = (ms: number) => new Promise((r) => window.setTimeout(r, ms));

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
      ? `Native ${symbol}`
      : `${invoice.token_name || symbol} · ${truncate(data.token.address ?? '', 6, 4)}`,
  );

  setText('chain-name', data.chain.name);
  setText('chain-id', `chain id ${data.chain.chain_id}`);
  setText('warn-symbol', symbol);
  setText('warn-chain', data.chain.name);

  // ── Plain-transfer path: bare address, no EIP-681 URI.
  const deposit = data.naive_path.deposit_address;
  setText('deposit-address', deposit);
  setText('manual-chain-inline', data.chain.name);
  setText('manual-token-inline', symbol);
  setText(
      'manual-amount-inline',
      `${data.amount.display || toDisplay(invoice.amount_requested, decimals)} ${symbol}`,
  );
  if (!data.token.is_native && data.token.address) {
    setText('token-note-symbol', symbol);
    setText('token-address', data.token.address);
    show($('token-note'));
  }
  drawQR('address-qr', deposit);

  // ── Contract path.
  const smart = data.smart_path;
  if (!smart) {
    // Nothing to sign. Collapse the wallet path and open on the transfer.
    show($('no-smart-note'));
    ['btn-connect', 'btn-approve', 'btn-pay'].forEach((btnId) => {
      $<HTMLButtonElement>(btnId).disabled = true;
    });
    show($('toggle-call').parentElement!, false);
    show($('approval-note'), false);
    return;
  }

  if (smart.approval) {
    setText('approve-symbol', symbol);
  } else {
    // Native: there is no allowance, so the step should not exist at all
    // rather than sit there greyed out forever.
    show($('approve-step'), false);
    setText(
        'approval-note',
        `${symbol} only needs a single approval in your wallet to send.`,
    );
  }

  setText('call-vault', smart.vault_address);
  setText('call-fn', `${smart.call.function_name}()`);

  const args = $('call-args');
  const rows: CallArg[] = [
    ...smart.call.args,
    ...(smart.call.value_base_units !== '0'
      ? [{ name: 'value', type: 'wei', value: smart.call.value_base_units }]
      : []),
  ];
  args.replaceChildren(
    ...rows.map((a) => {
      const row = document.createElement('div');
      row.className = 'flex items-baseline justify-between gap-3';
      const label = document.createElement('span');
      label.className = 'font-mono text-[10px] text-mist/70 shrink-0';
      label.textContent = `${a.name} · ${a.type}`;
      const value = document.createElement('span');
      value.className = 'font-mono text-[11px] break-all text-right text-chalk';
      value.textContent = a.value.length > 24 ? truncate(a.value, 10, 8) : a.value;
      value.title = a.value;
      row.append(label, value);
      return row;
    }),
  );
}

function drawQR(canvasId: string, text: string) {
  const canvas = $<HTMLCanvasElement>(canvasId);
  QRCode.toCanvas(canvas, text, {
    width: 440,
    margin: 1,
    errorCorrectionLevel: 'M',
    color: { dark: '#070811', light: '#ffffff' },
  })
    .then(() => {
      // qrcode writes a fixed pixel width AND height onto the element as inline
      // style, which beats the utility classes and stretches the canvas
      // vertically. Drop what the library set and size it ourselves; the 440px
      // backing store stays for crispness.
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
  selectPath(c.data.smart_path ? 'wallet' : 'manual');
  $('tab-wallet').addEventListener('click', () => selectPath('wallet'));
  $('tab-manual').addEventListener('click', () => selectPath('manual'));

  const copyBtn = $<HTMLButtonElement>('copy-address');
  copyBtn.addEventListener('click', async () => {
    await navigator.clipboard.writeText(c.data.naive_path.deposit_address);
    copyBtn.textContent = 'Copied';
    window.setTimeout(() => { copyBtn.textContent = 'Copy'; }, 1600);
  });

  const callBlock = $('call-block');
  $('toggle-call').addEventListener('click', () => {
    const opening = callBlock.classList.contains('hidden');
    show(callBlock, opening);
    setText('toggle-call-icon', opening ? '−' : '+');
  });

  $('btn-connect').addEventListener('click', () => void connectWallet());
  $('btn-switch').addEventListener('click', () => void switchChain());
  $('btn-approve').addEventListener('click', () => void runApprove());
  $('btn-pay').addEventListener('click', () => void runPay());
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
    await sleep(document.hidden ? POLL_MS_HIDDEN : POLL_MS);
  }
}

function applyStatus(s: StatusResponse) {
  if (!checkout) return;
  const decimals = checkout.data.token.decimals;
  const symbol = checkout.data.token.symbol;

  // ── pill
  const dot = $('status-dot');
  const pill = $('status-pill');
  dot.classList.remove('bg-mist', 'bg-eth', 'bg-rose', 'bg-amber', 'breathe');
  pill.classList.remove('text-mist', 'text-eth', 'text-rose', 'text-amber', 'border-line', 'border-eth/40', 'border-rose/40');

  if (SETTLED.has(s.status)) {
    dot.classList.add('bg-eth');
    pill.classList.add('text-eth', 'border-eth/40');
  } else if (DEAD.has(s.status)) {
    dot.classList.add('bg-rose');
    pill.classList.add('text-rose', 'border-rose/40');
  } else {
    dot.classList.add('bg-eth', 'breathe');
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

  // ── terminal states take over the action buttons
  if (SETTLED.has(s.status)) {
    closed = true;
    lockActions('Paid');
    notify('Payment confirmed. You can close this page.');
  } else if (DEAD.has(s.status)) {
    closed = true;
    lockActions('Closed');
    fail(`This invoice is ${s.status}. Ask the merchant for a new payment link.`);
  }
}

function lockActions(label: string) {
  for (const id of ['btn-connect', 'btn-approve', 'btn-pay']) {
    $<HTMLButtonElement>(id).disabled = true;
  }
  $('btn-pay').textContent = label;
  show($('network-row'), false);
}

function renderPayments(payments: PaymentSummary[], required: number | null) {
  if (!checkout || payments.length === 0) return;
  const { chain, token } = checkout.data;

  show($('payments-section'));
  setText('payments-count', `${payments.length}`);

  const list = $('payments-list');
  list.replaceChildren(
    ...payments.map((p) => {
      const row = document.createElement('a');
      row.href = explorerTx(p.tx_hash, chain.chain_id);
      row.target = '_blank';
      row.rel = 'noreferrer';
      row.className =
        'block rounded-[10px] border border-line bg-panel-2 px-3 py-2.5 transition-colors hover:border-eth/40';

      const confirmed = required != null && p.confirmations >= required;
      const conf = required != null ? `${p.confirmations}/${required} conf` : `${p.confirmations} conf`;

      row.innerHTML = `
        <div class="flex items-baseline justify-between gap-3">
          <span class="font-mono text-[12px] tabular-nums text-chalk">
            ${toDisplay(p.amount, token.decimals)} ${token.symbol}
          </span>
          <span class="font-mono text-[10px] uppercase tracking-[0.14em] ${confirmed ? 'text-eth' : 'text-mist'}">
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
   Wallet path — Reown AppKit (ethers adapter) + viem for encoding
   Everything heavy is imported on first click so the page paints (and the
   plain-transfer path works) without waiting on the wallet bundle.
   ════════════════════════════════════════════════════════════════════════════ */

type Eip1193 = {
  request(args: { method: string; params?: unknown[] }): Promise<unknown>;
  on?(event: string, cb: (...args: unknown[]) => void): void;
};

type AppKitLike = {
  open(): Promise<void>;
  subscribeAccount(cb: (a: { isConnected?: boolean; address?: string }) => void): void;
  getWalletProvider(): unknown;
};

let appKit: AppKitLike | null = null;
let chainListenerBound = false;

async function resolveNetwork(chainId: number) {
  const nets = await import('@reown/appkit/networks');
  const byId: Record<number, unknown> = {
    1: nets.mainnet,
    137: nets.polygon,
    8453: nets.base,
    11155111: nets.sepolia,
    84532: nets.baseSepolia,
  };
  const known = byId[chainId];
  if (known) return known;

  // Anything else the package ships: match on id rather than name so a chain
  // added server-side keeps working without a frontend change.
  const found = Object.values(nets).find(
    (n) => n && typeof n === 'object' && 'id' in n && (n as { id: unknown }).id === chainId,
  );
  if (!found) {
    throw new Error(
      `chain ${chainId} is not available in this build — pay with the plain transfer instead`,
    );
  }
  return found;
}

async function getAppKit(): Promise<AppKitLike> {
  if (appKit) return appKit;

  const [{ createAppKit }, { EthersAdapter }] = await Promise.all([
    import('@reown/appkit'),
    import('@reown/appkit-adapter-ethers'),
  ]);

  const network = await resolveNetwork(checkout!.data.chain.chain_id);

  appKit = createAppKit({
    adapters: [new EthersAdapter()],
    networks: [network],
    defaultNetwork: network,
    projectId: REOWN_PROJECT_ID,
    metadata: APP_METADATA,
    features: { analytics: false, email: false, socials: false },
  } as never) as unknown as AppKitLike;

  appKit.subscribeAccount((account) => {
    if (account?.isConnected && account.address) {
      void onWalletConnected(account.address);
    } else {
      onWalletDisconnected();
    }
  });

  return appKit;
}

async function getProvider(): Promise<Eip1193> {
  const provider = (await getAppKit()).getWalletProvider() as Eip1193 | undefined;
  if (!provider?.request) throw new Error('no wallet provider is connected');
  return provider;
}

/** Step 1 — opens the AppKit modal, which is where the WalletConnect QR lives. */
async function connectWallet() {
  const detail = $('connect-detail');
  try {
    detail.textContent = 'Opening wallet…';
    const kit = await getAppKit();
    await kit.open();
    detail.textContent = connectedAddress ?? '';
  } catch (e) {
    detail.textContent = '';
    fail(`Could not open the wallet picker. ${(e as Error).message}`);
  }
}

async function onWalletConnected(address: string) {
  connectedAddress = address;
  setText('connect-detail', address);
  $<HTMLButtonElement>('btn-connect').textContent = 'Change wallet';

  try {
    const provider = await getProvider();
    // Wallets can switch chains under us; the banner has to follow. Once per
    // provider — reconnecting must not stack listeners.
    if (!chainListenerBound) {
      provider.on?.('chainChanged', () => void refreshChain());
      chainListenerBound = true;
    }
  } catch {
    // Provider not ready yet — refreshChain will surface the real error.
  }

  await refreshChain();
}

function onWalletDisconnected() {
  connectedAddress = null;
  chainOk = false;
  needsApproval = null;
  setText('connect-detail', '');
  setText('approve-detail', '');
  setText('pay-detail', '');
  $<HTMLButtonElement>('btn-connect').textContent = 'Connect wallet';
  show($('network-row'), false);
  refreshSteps();
}

/* ── chain ───────────────────────────────────────────────────────────────── */

async function currentChainId(): Promise<number> {
  const provider = await getProvider();
  const raw = (await provider.request({ method: 'eth_chainId' })) as string;
  return Number.parseInt(raw, 16);
}

async function refreshChain() {
  if (!checkout || !connectedAddress) return;
  const want = checkout.data.chain.chain_id;

  try {
    const have = await currentChainId();
    chainOk = have === want;

    show($('network-row'), !chainOk);
    if (!chainOk) {
      setText(
        'network-detail',
        `Your wallet is on chain ${have}. This invoice settles on ${checkout.data.chain.name} (${want}).`,
      );
      needsApproval = null;
      refreshSteps();
      return;
    }
  } catch (e) {
    chainOk = false;
    fail(`Could not read the wallet's network. ${(e as Error).message}`);
    return;
  }

  await refreshAllowance();
}

async function switchChain() {
  if (!checkout) return;
  const want = checkout.data.chain.chain_id;
  const btn = $<HTMLButtonElement>('btn-switch');
  btn.disabled = true;
  try {
    const provider = await getProvider();
    await provider.request({
      method: 'wallet_switchEthereumChain',
      params: [{ chainId: toQuantity(BigInt(want)) }],
    });
    await refreshChain();
  } catch (e) {
    const err = e as { code?: number; message?: string };
    if (err.code === 4902) {
      // We deliberately do not ship RPC URLs, so we cannot add the chain for
      // them — say so plainly instead of half-doing it.
      fail(
        `${checkout.data.chain.name} is not in your wallet yet. Add it there, then come back to this page.`,
      );
    } else if (!/reject|denied|cancel/i.test(err.message ?? '')) {
      fail(`Could not switch network. ${err.message ?? String(e)}`);
    }
  } finally {
    btn.disabled = false;
  }
}

/* ── allowance ───────────────────────────────────────────────────────────── */

async function refreshAllowance() {
  const smart = checkout?.data.smart_path;
  if (!smart) return;

  if (!smart.approval) {
    needsApproval = false;
    refreshSteps();
    return;
  }

  const detail = $('approve-detail');
  try {
    const { parseAbi, encodeFunctionData, decodeFunctionResult } = await import('viem');
    const abi = parseAbi([smart.approval.allowance_abi]) as any;
    const data = encodeFunctionData({
      abi,
      functionName: 'allowance',
      args: [connectedAddress as `0x${string}`, smart.approval.spender as `0x${string}`],
    });

    const provider = await getProvider();
    const raw = (await provider.request({
      method: 'eth_call',
      params: [{ to: smart.approval.token_address, data }, 'latest'],
    })) as `0x${string}`;

    const current = decodeFunctionResult({ abi, functionName: 'allowance', data: raw }) as bigint;
    const required = BigInt(smart.approval.amount_base_units);

    needsApproval = current < required;
    detail.textContent = needsApproval
      ? `Approves exactly ${checkout!.data.amount.display} ${checkout!.data.token.symbol}.`
      : 'Allowance already covers this invoice.';
  } catch (e) {
    // Assume an approval is needed: a redundant approve costs gas, a missing
    // one reverts the payment.
    needsApproval = true;
    detail.textContent = `Could not read your allowance (${(e as Error).message}). Approving anyway is safe.`;
  }
  refreshSteps();
}

/* ── step rail ───────────────────────────────────────────────────────────── */

const DOT_IDLE = 'bg-line';
const DOT_ACTIVE = 'bg-eth';
const DOT_DONE = 'bg-iris';

function setDot(id: string, state: 'idle' | 'active' | 'done') {
  const dot = $(id);
  dot.classList.remove(DOT_IDLE, DOT_ACTIVE, DOT_DONE, 'breathe');
  dot.classList.add(state === 'idle' ? DOT_IDLE : state === 'active' ? DOT_ACTIVE : DOT_DONE);
}

/** Single source of truth for what is lit and what is clickable. */
function refreshSteps() {
  if (!checkout?.data.smart_path || closed) return;

  const hasApproval = Boolean(checkout.data.smart_path.approval);
  const connected = Boolean(connectedAddress);
  const ready = connected && chainOk;

  setDot('step-1', connected ? 'done' : 'active');

  if (hasApproval) {
    const approveReady = ready && needsApproval === true;
    const approveDone = ready && needsApproval === false;
    setDot('step-2', approveDone ? 'done' : approveReady ? 'active' : 'idle');
    $<HTMLButtonElement>('btn-approve').disabled = !approveReady;
    $<HTMLButtonElement>('btn-approve').textContent = approveDone
        ? 'Allowed'
        : `Allow ${checkout.data.token.symbol}`;
  }

  const payReady = ready && (!hasApproval || needsApproval === false);
  setDot('step-3', payReady ? 'active' : 'idle');
  $<HTMLButtonElement>('btn-pay').disabled = !payReady;

  if (payReady && !$('pay-detail').textContent) {
    setText(
      'pay-detail',
      `Sends ${checkout.data.amount.display} ${checkout.data.token.symbol} to the payment contract.`,
    );
  }
}

/* ── transactions ────────────────────────────────────────────────────────── */

async function sendTx(tx: { to: string; data: string; value?: bigint }): Promise<string> {
  const provider = await getProvider();
  return (await provider.request({
    method: 'eth_sendTransaction',
    params: [
      {
        from: connectedAddress,
        to: tx.to,
        data: tx.data,
        ...(tx.value && tx.value > 0n ? { value: toQuantity(tx.value) } : {}),
      },
    ],
  })) as string;
}

/** Polls the wallet's own node. Resolves on success, throws on revert. */
async function waitForReceipt(hash: string): Promise<void> {
  const provider = await getProvider();
  const deadline = Date.now() + RECEIPT_TIMEOUT_MS;

  while (Date.now() < deadline) {
    const receipt = (await provider.request({
      method: 'eth_getTransactionReceipt',
      params: [hash],
    })) as { status?: string } | null;

    if (receipt) {
      if (receipt.status && BigInt(receipt.status) === 0n) {
        throw new Error('the transaction reverted on chain');
      }
      return;
    }
    await sleep(RECEIPT_POLL_MS);
  }
  throw new Error('still pending — check your wallet, then reload this page');
}

/** Coerce the self-describing args from the API into what viem wants. */
function coerceArg(arg: CallArg): unknown {
  if (/^u?int/.test(arg.type)) return BigInt(arg.value);
  if (arg.type === 'bool') return arg.value === 'true' || arg.value === '1';
  return arg.value; // address, bytes16, string — already hex or plain
}

/** Step 2 — allowance for exactly this amount. ERC-20 only. */
async function runApprove() {
  const smart = checkout?.data.smart_path;
  if (!smart?.approval || !connectedAddress) return;

  const btn = $<HTMLButtonElement>('btn-approve');
  const detail = $('approve-detail');
  btn.disabled = true;

  try {
    const { parseAbi, encodeFunctionData } = await import('viem');

    const data = encodeFunctionData({
      abi: parseAbi([smart.approval.approve_abi]) as any,
      functionName: 'approve',
      args: [
        smart.approval.spender as `0x${string}`,
        BigInt(smart.approval.amount_base_units),
      ],
    });

    detail.textContent = 'Waiting for your signature…';
    const hash = await sendTx({ to: smart.approval.token_address, data });

    detail.textContent = `Approving · ${truncate(hash, 8, 8)}`;
    await waitForReceipt(hash);

    needsApproval = false;
    detail.textContent = 'Approved. One more signature to pay.';
    refreshSteps();
  } catch (e) {
    const message = (e as Error).message ?? String(e);
    btn.disabled = false;
    if (/reject|denied|cancel/i.test(message)) {
      detail.textContent = 'Approval declined. Try again when you are ready.';
    } else {
      detail.textContent = '';
      fail(`The approval did not go through. ${message}`);
    }
  }
}

/** Step 3 — the vault call. This is the transaction the watcher matches. */
async function runPay() {
  const smart = checkout?.data.smart_path;
  if (!smart || !connectedAddress) return;

  const btn = $<HTMLButtonElement>('btn-pay');
  const detail = $('pay-detail');
  btn.disabled = true;

  try {
    const { parseAbi, encodeFunctionData } = await import('viem');
    const data = encodeFunctionData({
      abi: parseAbi([smart.call.abi]) as any,
      functionName: smart.call.function_name,
      args: smart.call.args.map(coerceArg),
    });
    detail.textContent = 'Waiting for your signature…';
    const hash = await sendTx({
      to: smart.vault_address,
      data,
      value: BigInt(smart.call.value_base_units),
    });

    detail.textContent = `Sent · ${truncate(hash, 8, 8)}`;
    btn.textContent = 'Sent';
    notify('Transaction sent. This page updates as soon as the payment is seen on chain.');

    await waitForReceipt(hash);
    detail.textContent = `Confirmed · ${truncate(hash, 8, 8)}`;
  } catch (e) {
    const message = (e as Error).message ?? String(e);
    if (/reject|denied|cancel/i.test(message)) {
      detail.textContent = 'Signature declined. Try again when you are ready.';
      btn.disabled = false;
    } else if (/pending/i.test(message)) {
      // Sent but not mined inside our window: not a failure, don't shout.
      detail.textContent = message;
    } else {
      detail.textContent = '';
      btn.disabled = false;
      fail(`The payment could not be sent. ${message}`);
    }
  }
}

boot();
