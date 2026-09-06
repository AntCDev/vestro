import './style.css';

// -----------------------------------------------------------------------------
// Config
// -----------------------------------------------------------------------------
const ENDPOINTS = {
  tokens: '/api/test/tokens',
  networks: '/api/test/networks',
  createInvoice: '/api/invoices',
} as const;

/**
 * How `amount_requested` goes over the wire.
 * true  -> "12.50"  (matches String / Decimal-with-serde_with backends)
 * false -> 12.5     (matches f64 / u64 backends)
 * Flip this one flag if the server rejects the field.
 */
const AMOUNT_AS_STRING = true;

// -----------------------------------------------------------------------------
// Types matching the Axum backend
// -----------------------------------------------------------------------------
interface TokenMetadata {
  id: string;
  name: string;
  detail: string;
  info: string;
}

interface NetworkSummaryResponse {
  evm_chain_ids: number[];
  solana_clusters: string[];
  bitcoin_networks: string[];
}

interface CreateInvoiceRequest {
  merchant_id: string;
  token_id: string;
  amount_requested: string | number;
  data?: string;
}

interface CreateInvoiceResponse {
  url: string;
  invoice_id: string;
}

// -----------------------------------------------------------------------------
// UI Element References
// -----------------------------------------------------------------------------
const form = document.querySelector<HTMLFormElement>('#invoice-form')!;
const merchantInput = document.querySelector<HTMLInputElement>('#merchant_id')!;
const amountInput = document.querySelector<HTMLInputElement>('#amount')!;
const amountUnit = document.querySelector<HTMLElement>('#amount-unit')!;
const dataInput = document.querySelector<HTMLInputElement>('#data')!;
const submitBtn = document.querySelector<HTMLButtonElement>('#submit-btn')!;
const errorBox = document.querySelector<HTMLDivElement>('#error-box')!;

const panel = document.querySelector<HTMLElement>('#panel')!;
const status = document.querySelector<HTMLElement>('#status')!;
const statusWord = document.querySelector<HTMLElement>('#status-word')!;

const tokenList = document.querySelector<HTMLDivElement>('#token-list')!;
const tokenCount = document.querySelector<HTMLElement>('#token-count')!;
const tokenInfo = document.querySelector<HTMLDivElement>('#token-info')!;

const envStatus = document.querySelector<HTMLElement>('#env-status')!;
const envWord = document.querySelector<HTMLElement>('#env-word')!;
const envEvm = document.querySelector<HTMLElement>('#env-evm')!;
const envSol = document.querySelector<HTMLElement>('#env-sol')!;
const envBtc = document.querySelector<HTMLElement>('#env-btc')!;
const envTokens = document.querySelector<HTMLElement>('#env-tokens')!;

const invoiceCard = document.querySelector<HTMLElement>('#invoice-card')!;
const resInvoiceId = document.querySelector<HTMLDivElement>('#res-invoice-id')!;
const resInvoiceUrl = document.querySelector<HTMLDivElement>('#res-invoice-url')!;
const copyUrlBtn = document.querySelector<HTMLButtonElement>('#copy-url-btn')!;
const openInvoiceLink = document.querySelector<HTMLAnchorElement>('#open-invoice-link')!;

// -----------------------------------------------------------------------------
// Status vocabulary — one word per state, coloured by tone.
// muted: open · accent: creating (live) · ok: created · stop: failed
// -----------------------------------------------------------------------------
type State = 'open' | 'creating' | 'created' | 'failed';

const TONE: Record<State, string> = {
  open: '',
  creating: 'status--accent',
  created: 'status--ok',
  failed: 'status--stop',
};

function setState(next: State) {
  status.className = `status ${TONE[next]}`.trim();
  statusWord.textContent = next.charAt(0).toUpperCase() + next.slice(1);
  // The live ring belongs to the element that is changing, and only while it changes.
  panel.classList.toggle('is-live', next === 'creating');
}

function setEnvState(word: string, tone: '' | 'status--accent' | 'status--ok' | 'status--stop') {
  envStatus.className = `status ${tone}`.trim();
  envWord.textContent = word;
}

function showError(message: string) {
  errorBox.textContent = message;
  errorBox.classList.remove('hidden');
}

function clearError() {
  errorBox.textContent = '';
  errorBox.classList.add('hidden');
}

// -----------------------------------------------------------------------------
// Tokens
// -----------------------------------------------------------------------------
let tokens: TokenMetadata[] = [];
let selectedTokenId: string | null = null;

function renderTokens() {
  tokenList.innerHTML = '';

  if (tokens.length === 0) {
    tokenList.innerHTML = '<p class="empty">No token handlers are registered. Configure at least one network and restart the server.</p>';
    tokenCount.textContent = '';
    return;
  }

  tokenCount.textContent = `${tokens.length} available`;

  for (const token of tokens) {
    const row = document.createElement('button');
    row.type = 'button';
    row.className = 'choice';
    row.dataset.tokenId = token.id;
    row.setAttribute('role', 'radio');
    row.setAttribute('aria-checked', 'false');

    row.innerHTML = `
      <span class="choice-main">
        <span class="choice-name"></span>
        <span class="choice-detail"></span>
      </span>
      <span class="choice-id"></span>
    `;
    row.querySelector('.choice-name')!.textContent = token.name;
    row.querySelector('.choice-detail')!.textContent = token.detail;
    row.querySelector('.choice-id')!.textContent = token.id;

    row.addEventListener('click', () => selectToken(token.id));
    tokenList.appendChild(row);
  }
}

function selectToken(id: string) {
  selectedTokenId = id;
  const token = tokens.find((t) => t.id === id);

  tokenList.querySelectorAll<HTMLButtonElement>('.choice').forEach((row) => {
    const active = row.dataset.tokenId === id;
    row.classList.toggle('is-selected', active);
    row.setAttribute('aria-checked', String(active));
  });

  if (token?.info) {
    tokenInfo.textContent = token.info;
    tokenInfo.classList.remove('hidden');
  } else {
    tokenInfo.classList.add('hidden');
  }

  amountUnit.textContent = token ? token.name : '';
}

async function loadTokens() {
  try {
    const response = await fetch(ENDPOINTS.tokens);
    if (!response.ok) throw new Error(`Token handlers unavailable (${response.status})`);

    tokens = (await response.json()) as TokenMetadata[];
    renderTokens();
    envTokens.textContent = String(tokens.length);

    if (tokens.length === 1) selectToken(tokens[0].id);
  } catch (err) {
    tokenList.innerHTML = '<p class="empty">Token handlers could not be read. Is the server running?</p>';
    envTokens.textContent = '—';
    showError(err instanceof Error ? err.message : 'Token handlers could not be read.');
  }
}

// -----------------------------------------------------------------------------
// Networks
// -----------------------------------------------------------------------------
function joinOrDash(values: Array<string | number>): string {
  return values.length > 0 ? values.join(', ') : '—';
}

async function loadNetworks() {
  try {
    const response = await fetch(ENDPOINTS.networks);
    if (!response.ok) throw new Error(`Networks unavailable (${response.status})`);

    const data = (await response.json()) as NetworkSummaryResponse;
    envEvm.textContent = joinOrDash(data.evm_chain_ids);
    envSol.textContent = joinOrDash(data.solana_clusters);
    envBtc.textContent = joinOrDash(data.bitcoin_networks);

    const total = data.evm_chain_ids.length + data.solana_clusters.length + data.bitcoin_networks.length;
    setEnvState(total > 0 ? 'Connected' : 'Empty', total > 0 ? 'status--ok' : '');
  } catch {
    envEvm.textContent = '—';
    envSol.textContent = '—';
    envBtc.textContent = '—';
    setEnvState('Unreachable', 'status--stop');
  }
}

// -----------------------------------------------------------------------------
// Submission
// -----------------------------------------------------------------------------
const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
const INTEGER = /^[1-9]\d*$/;
form.addEventListener('submit', async (e) => {
  e.preventDefault();
  clearError();

  const merchantId = merchantInput.value.trim();
  if (!UUID.test(merchantId)) {
    showError('Merchant ID must be a UUID. Copy it from the sign-up response.');
    return;
  }

  if (!selectedTokenId) {
    showError('Select a token before creating the invoice.');
    return;
  }

  const rawAmount = amountInput.value.trim();

  // Validate integer format and ensure it's greater than 0
  if (!INTEGER.test(rawAmount)) {
    showError('Amount must be a positive whole integer in atomic units (e.g., 1000000 for 1 USDC).');
    return;
  }

  const payload: CreateInvoiceRequest = {
    merchant_id: merchantId,
    token_id: selectedTokenId,
    amount_requested: AMOUNT_AS_STRING ? rawAmount : Number(rawAmount),
    data: dataInput.value.trim() || undefined,
  };

  invoiceCard.classList.add('hidden');
  invoiceCard.classList.remove('reveal');
  submitBtn.disabled = true;
  submitBtn.textContent = 'Creating…';
  setState('creating');

  try {
    const response = await fetch(ENDPOINTS.createInvoice, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(payload),
    });

    if (!response.ok) {
      const errMessage = await response.text();
      throw new Error(errMessage || `Server error ${response.status}`);
    }

    const data: CreateInvoiceResponse = await response.json();

    resInvoiceId.textContent = data.invoice_id;
    resInvoiceUrl.textContent = data.url;
    openInvoiceLink.href = data.url;

    invoiceCard.classList.remove('hidden');
    invoiceCard.classList.add('reveal');
    setState('created');
  } catch (err) {
    showError(err instanceof Error ? err.message : 'The invoice could not be created.');
    setState('failed');
  } finally {
    submitBtn.disabled = false;
    submitBtn.textContent = 'Create invoice';
  }
});

// Copy the checkout URL
copyUrlBtn.addEventListener('click', async () => {
  const url = resInvoiceUrl.textContent ?? '';
  if (!url) return;

  try {
    await navigator.clipboard.writeText(url);
    copyUrlBtn.textContent = 'Copied';
    setTimeout(() => (copyUrlBtn.textContent = 'Copy URL'), 1500);
  } catch {
    copyUrlBtn.textContent = 'Copy failed';
    setTimeout(() => (copyUrlBtn.textContent = 'Copy URL'), 1500);
  }
});

// -----------------------------------------------------------------------------
// Boot
// -----------------------------------------------------------------------------
const merchantFromUrl = new URLSearchParams(location.search).get('merchant_id');
if (merchantFromUrl) merchantInput.value = merchantFromUrl;

setState('open');
setEnvState('Loading', 'status--accent');
loadTokens();
loadNetworks();
