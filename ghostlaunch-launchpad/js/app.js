/**
 * GhostLaunch Launchpad — single-page token launcher.
 *
 * Flow: hero (name + symbol) → CTA reveals the launch form on the same page
 * → Step 1 Token Identity → Step 2 Cloak Stealth → total + Confirm in Wallet.
 *
 * Scaffold status: wallet connection is real (Solflare-first). Logo IPFS
 * upload, launch submission (backend proxy), and the "just launched" feed
 * are stubbed with TODOs. Devnet only — no mainnet execution.
 */
import { CONFIG, LAMPORTS_PER_SOL } from "./config.js";
import { wallet } from "./wallet.js";
import { fetchCurveProgress, getConnection } from "./pump.js";
import {
  PublicKey,
  Keypair,
  SystemProgram,
  Transaction,
  VersionedTransaction,
} from "@solana/web3.js";
import bs58 from "bs58";

/* ---------------------------------- utils --------------------------------- */

export function esc(s) {
  return String(s ?? "")
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

export function toast(msg, type = "info") {
  const el = document.createElement("div");
  el.className = `toast toast-${type}`;
  el.textContent = msg;
  document.getElementById("toasts").appendChild(el);
  setTimeout(() => el.classList.add("show"), 10);
  setTimeout(() => {
    el.classList.remove("show");
    setTimeout(() => el.remove(), 400);
  }, 4200);
}

export function shortAddr(addr) {
  const s = String(addr);
  return s.length > 12 ? `${s.slice(0, 4)}…${s.slice(-4)}` : s;
}

let ghostLogoUid = 0;
/* Spectral Ghost Mark — matches the Framer marketing site logo exactly:
   40x48 ghost, 145deg gradient #00e5ff -> #7c3aed, dark eyes + diagonal veil. */
function ghostLogo(size = 28) {
  const h = Math.round((size * 48) / 40);
  const u = ++ghostLogoUid;
  return `<svg class="logo-ghost" width="${size}" height="${h}" viewBox="0 0 40 48" fill="none" aria-hidden="true">
    <defs>
      <linearGradient id="glg${u}" x1="6" y1="7" x2="34" y2="41" gradientUnits="userSpaceOnUse">
        <stop offset="0.15" stop-color="#00e5ff"/><stop offset="1" stop-color="#7c3aed"/>
      </linearGradient>
      <clipPath id="glc${u}">
        <path d="M 0 20 Q 0 0 20 0 L 20 0 Q 40 0 40 20 L 40 43 Q 40 48 35 48 L 5 48 Q 0 48 0 43 Z"/>
      </clipPath>
    </defs>
    <path d="M 0 20 Q 0 0 20 0 L 20 0 Q 40 0 40 20 L 40 43 Q 40 48 35 48 L 5 48 Q 0 48 0 43 Z" fill="url(#glg${u})"/>
    <g clip-path="url(#glc${u})">
      <rect x="10" y="17" width="5" height="9" rx="3" fill="#050508"/>
      <rect x="25" y="17" width="5" height="9" rx="3" fill="#050508"/>
      <rect x="-4" y="36" width="50" height="4" fill="#050508" transform="rotate(-18 21 38)"/>
    </g>
  </svg>`;
}

/* ------------------------------ wallet header ----------------------------- */

function renderWalletButtons() {
  const pk = wallet.publicKey?.toBase58?.() ?? wallet.publicKey?.toString?.();
  const html = wallet.connected
    ? `<button class="btn btn-wallet connected" id="btn-disconnect" title="Disconnect">
         <span class="dot"></span>${esc(shortAddr(pk))}</button>`
    : `<button class="btn btn-primary" id="btn-connect">Connect wallet</button>`;
  document.getElementById("nav-wallet").innerHTML = html;
  document.getElementById("trust-wallet").innerHTML = wallet.connected ? "" : html.replace('id="btn-connect"', 'id="btn-connect-2"');

  for (const id of ["btn-connect", "btn-connect-2"]) {
    const b = document.getElementById(id);
    if (b) b.onclick = openWalletModal;
  }
  const dc = document.getElementById("btn-disconnect");
  if (dc) dc.onclick = () => wallet.disconnect().catch((e) => toast(e.message, "error"));
}

wallet.on("connect", () => {
  renderWalletButtons();
  toast("Connected — welcome behind the veil.", "success");
  closeModal();
});
wallet.on("disconnect", renderWalletButtons);

/* ------------------------------- wallet modal ------------------------------ */

function openWalletModal() {
  const defs = wallet.defs();
  const rows = defs
    .map(
      (d) => `<button class="wallet-row" data-wallet="${esc(d.name)}">
        <span class="wallet-name">${esc(d.name)}
          ${d.recommended ? '<span class="pill">Recommended</span>' : ""}
        </span>
        <span class="wallet-state">${d.detected ? "Detected" : "Not detected"}</span>
      </button>`
    )
    .join("");
  document.getElementById("modal-root").innerHTML = `
    <div class="modal-backdrop" id="modal-backdrop">
      <div class="modal">
        <div class="modal-head">
          <h3>Connect a wallet</h3>
          <button class="icon-btn" id="modal-close" aria-label="Close">✕</button>
        </div>
        <p class="muted">Solflare is our pick. Phantom works too.</p>
        <div class="wallet-list">${rows}</div>
        <p class="tiny muted">New to Solana? <a href="https://solflare.com" target="_blank" rel="noopener">Get Solflare →</a></p>
      </div>
    </div>`;
  document.getElementById("modal-close").onclick = closeModal;
  document.getElementById("modal-backdrop").onclick = (e) => {
    if (e.target.id === "modal-backdrop") closeModal();
  };
  document.querySelectorAll(".wallet-row").forEach((b) => {
    b.onclick = async () => {
      b.disabled = true;
      try {
        await wallet.connect(b.dataset.wallet);
      } catch (e) {
        toast(e.message, "error");
        b.disabled = false;
      }
    };
  });
}

function closeModal() {
  document.getElementById("modal-root").innerHTML = "";
}

/* --------------------------------- form ----------------------------------- */

function wireForm() {
  // Logo upload preview.
  // TODO: replace the local preview with a real IPFS pin (image + metadata
  // JSON) and keep the returned URI (≤200 chars) for the create call.
  const imgInput = document.getElementById("f-image");
  const uploadBox = document.getElementById("img-upload");
  const showPicker = () => imgInput.click();
  uploadBox.onclick = showPicker;
  uploadBox.onkeydown = (e) => {
    if (e.key === "Enter" || e.key === " ") {
      e.preventDefault();
      showPicker();
    }
  };
  imgInput.onchange = () => {
    const f = imgInput.files[0];
    if (!f) return;
    const url = URL.createObjectURL(f);
    document.getElementById("img-preview").innerHTML =
      `<img src="${url}" alt="token logo preview">`;
  };

  // Stealth sliders.
  const w = document.getElementById("f-wallets");
  w.oninput = () => { syncStealthControls(); };
  const j = document.getElementById("f-jitter");
  j.oninput = () => (document.getElementById("jitter-val").textContent = `±${j.value}%`);
  const d = document.getElementById("f-devbuy");
  d.oninput = () => (document.getElementById("devbuy-val").textContent = Number(d.value).toFixed(1));

  // Optional add-ons → running total.
  wireAddons();

  document.getElementById("btn-confirm").onclick = onConfirm;
}

/* ------------------------------ add-on total ------------------------------ */

const ADDON_PRICES = { deepVeil: 0.2, extraWallet: 0.1, radarWatch: 0.1 };
const MAX_EXTRA_WALLETS = 5;

function getStealthTier() {
  const el = document.querySelector('input[name="stealth-tier"]:checked');
  return el ? el.value : "standard";
}

function getAddons() {
  return {
    deepVeil: getStealthTier() === "deepveil",
    extraWallets: parseInt(document.getElementById("wallets-qty").textContent, 10) || 0,
    radarWatch: document.getElementById("addon-radar").checked,
  };
}

function addonsTotal(a) {
  return (
    (a.deepVeil ? ADDON_PRICES.deepVeil : 0) +
    a.extraWallets * ADDON_PRICES.extraWallet +
    (a.radarWatch ? ADDON_PRICES.radarWatch : 0)
  );
}

function updateTotal() {
  const a = getAddons();
  const extra = addonsTotal(a);
  const total = CONFIG.launchFeeSol + extra;
  document.getElementById("total-amount").textContent = `${total.toFixed(2)} SOL`;

  const summary = document.getElementById("addons-summary");
  const lines = [];
  if (a.deepVeil) lines.push(["Deep Veil", `+${ADDON_PRICES.deepVeil.toFixed(2)} SOL`]);
  if (a.extraWallets > 0)
    lines.push([
      `Extra stealth wallets × ${a.extraWallets}`,
      `+${(a.extraWallets * ADDON_PRICES.extraWallet).toFixed(2)} SOL`,
    ]);
  if (a.radarWatch) lines.push(["Radar sniper-watch", `+${ADDON_PRICES.radarWatch.toFixed(2)} SOL`]);
  if (lines.length) {
    summary.classList.remove("hidden");
    summary.innerHTML =
      `<div><span>Base launch (stealth included)</span><span>${CONFIG.launchFeeSol.toFixed(2)} SOL</span></div>` +
      lines.map(([k, v]) => `<div><span>${esc(k)}</span><span>${esc(v)}</span></div>`).join("");
  } else {
    summary.classList.add("hidden");
    summary.innerHTML = "";
  }
  return total;
}

function wireAddons() {
  document.querySelectorAll('input[name="stealth-tier"]').forEach((r) => {
    r.onchange = () => { updateTotal(); syncStealthControls(); };
  });
  document.getElementById("addon-radar").onchange = updateTotal;

  const qtyEl = document.getElementById("wallets-qty");
  const minus = document.getElementById("wallets-minus");
  const plus = document.getElementById("wallets-plus");
  const refresh = () => {
    const q = parseInt(qtyEl.textContent, 10) || 0;
    minus.disabled = q <= 0;
    plus.disabled = q >= MAX_EXTRA_WALLETS;
    document.getElementById("addon-wallets-card").style.borderColor =
      q > 0 ? "var(--cyan)" : "";
    updateTotal();
    syncStealthControls();
  };
  minus.onclick = () => {
    qtyEl.textContent = Math.max(0, (parseInt(qtyEl.textContent, 10) || 0) - 1);
    refresh();
  };
  plus.onclick = () => {
    qtyEl.textContent = Math.min(MAX_EXTRA_WALLETS, (parseInt(qtyEl.textContent, 10) || 0) + 1);
    refresh();
  };
  refresh();
}

/* Keeps the stealth UI honest:
   - "Deep Veil" tier = backend maxes everything; no sliders to confuse.
   - "Custom" tier reveals the advanced sliders.
   - "Standard" tier hides them (sane defaults).
   - Extra wallets ADD to the effective wallet count shown. */
function syncStealthControls() {
  const tier = getStealthTier();
  const extra = parseInt(document.getElementById("wallets-qty").textContent, 10) || 0;

  document.getElementById("custom-stealth").classList.toggle("hidden", tier !== "custom");

  const w = document.getElementById("f-wallets");
  const base = tier === "custom" ? (parseInt(w.value, 10) || 0) : 3;
  const label = document.getElementById("wallets-val");
  if (label) label.textContent = extra > 0 ? `${base} + ${extra} extra = ${base + extra}` : `${base}`;
}

async function onConfirm() {
  const name = document.getElementById("f-name").value.trim();
  const symbol = document.getElementById("f-symbol").value.trim().toUpperCase();
  const desc = document.getElementById("f-desc").value.trim();
  const website = document.getElementById("f-website").value.trim();
  const twitter = document.getElementById("f-twitter").value.trim();

  if (!name) {
    toast("Give your token a name.", "error");
    document.getElementById("f-name").focus();
    return;
  }
  if (name.length > CONFIG.maxNameLen) {
    toast(`Name too long (max ${CONFIG.maxNameLen} chars).`, "error");
    return;
  }
  if (!symbol) {
    toast("Give your token a symbol.", "error");
    document.getElementById("f-symbol").focus();
    return;
  }
  if (symbol.length > CONFIG.maxSymbolLen) {
    toast(`Symbol too long (max ${CONFIG.maxSymbolLen} chars).`, "error");
    return;
  }
  if (!wallet.connected) {
    toast("Connect a wallet first.", "error");
    openWalletModal();
    return;
  }

  // Token metadata URI: pasted image URL wins; otherwise the placeholder
  // until IPFS pinning ships (TODO).
  const imgUrl = document.getElementById("f-imgurl").value.trim();
  const tokenUri = imgUrl || CONFIG.placeholderTokenUri;
  if (tokenUri.length > CONFIG.maxUriLen) {
    toast(`Image URL too long (max ${CONFIG.maxUriLen} chars).`, "error");
    return;
  }

  const addons = getAddons();
  const total = updateTotal();
  const tier = getStealthTier();
  const custom = tier === "custom";
  const stealthOn = custom ? document.getElementById("f-stealth").checked : true;
  const devBuyRaw = parseFloat(document.getElementById("f-devbuy").value) || 0;
  // Honest mapping: the backend stealths every dev buy, so "stealth off"
  // means a naked launch with no dev buy at all.
  const devBuySol = stealthOn ? devBuyRaw : 0;

  const draft = {
    name,
    symbol,
    desc,
    website,
    twitter,
    tokenUri,
    stealth: stealthOn,
    wallets: custom ? parseInt(document.getElementById("f-wallets").value, 10) : 3,
    jitterPct: custom ? parseInt(document.getElementById("f-jitter").value, 10) : 5,
    spread: custom ? document.getElementById("f-spread").checked : true,
    devBuySol,
    addons,
    totalSol: total,
  };
  console.log("[launchpad] launch draft", draft);

  // Double-check the total with the user before any signature.
  const btn = document.getElementById("btn-confirm");
  btn.disabled = true;
  openLaunchModal(draft);
  runLaunch(draft)
    .catch((e) => lpFail(e))
    .finally(() => {
      btn.disabled = false;
    });
}

/* ------------------------- launch flow (live backend) ------------------------ */
/*
 * Real launch flow against the GhostLaunch backend (v0.5.0):
 *   1. fee   — creator signs a SOL transfer (0.2 + add-ons) to the payment wallet
 *   2. build — POST /v1/launchpad/build (public; the on-chain payment is the auth)
 *   3. fund  — creator signs funding txs; hop transfers auto-signed with one-time keys
 *   4. create— creator + mint sign the create tx
 *   5. buys  — buyer keypairs sign per the slot schedule
 * Devnet only. The server never sees any private key.
 */

function sleep(ms) {
  return new Promise((r) => setTimeout(r, ms));
}

/** Poll getSignatureStatus until confirmed/finalized (or timeout). */
async function confirmSignature(connection, sig, timeoutMs = 90000) {
  const start = Date.now();
  for (;;) {
    const st = await connection.getSignatureStatus(sig).catch(() => null);
    const s = st?.value;
    if (s?.confirmationStatus === "confirmed" || s?.confirmationStatus === "finalized") return;
    if (s?.err) throw new Error("Transaction failed on-chain.");
    if (Date.now() - start > timeoutMs) {
      throw new Error(
        `Confirmation timed out — check the explorer: https://explorer.solana.com/tx/${sig}?cluster=devnet`
      );
    }
    await sleep(1200);
  }
}

function decodeUnsignedTx(base64) {
  const bytes = Uint8Array.from(atob(base64), (c) => c.charCodeAt(0));
  return VersionedTransaction.deserialize(bytes);
}

/**
 * Give a backend-built unsigned tx a fresh blockhash just before signing.
 * The backend stamps build-time blockhashes (150-slot validity); our
 * sequential sign-and-confirm flow can outlast them on a slow devnet.
 * Safe: the blockhash isn't part of any instruction, only the message.
 */
async function refreshBlockhash(connection, vtx) {
  const { blockhash } = await connection.getLatestBlockhash("confirmed");
  vtx.message.recentBlockhash = blockhash;
  return vtx;
}

function keypairFromB58(s) {
  return Keypair.fromSecretKey(bs58.decode(s));
}

async function sendSignedTx(connection, vtx) {
  const sig = await connection.sendRawTransaction(vtx.serialize(), {
    skipPreflight: false,
    preflightCommitment: "confirmed",
  });
  await confirmSignature(connection, sig);
  return sig;
}

/* --------------------------- progress modal --------------------------- */

const LP_STEPS = [
  { id: "fee", title: "Fee payment", desc: "Sign the launch fee in your wallet" },
  { id: "build", title: "Building bundle", desc: "Verifying payment, building stealth txs" },
  { id: "fund", title: "Funding stealth wallets", desc: "Sign the funding chain" },
  { id: "create", title: "Creating token", desc: "Sign the creation" },
  { id: "buys", title: "Stealth buys", desc: "Placing buys on schedule" },
  { id: "done", title: "Done", desc: "Live on the curve" },
];

function openLaunchModal(draft) {
  const steps = LP_STEPS.map(
    (s) => `<li data-step="${s.id}">
      <span class="lp-dot">○</span>
      <div class="lp-step-body"><strong>${esc(s.title)}</strong>
        <p class="tiny muted lp-note">${esc(s.desc)}</p></div>
    </li>`
  ).join("");
  document.getElementById("modal-root").innerHTML = `
    <div class="modal-backdrop">
      <div class="modal lp-modal">
        <div class="modal-head"><h3>👻 Launching <strong>${esc(draft.name)}</strong>
          <span class="muted">($${esc(draft.symbol)})</span></h3></div>
        <p class="tiny muted">Total fee: <strong>${draft.totalSol.toFixed(2)} SOL</strong>
        → <code class="tiny">${esc(shortAddr(CONFIG.paymentWallet))}</code>.
        Nothing moves until you sign.</p>
        <ol class="lp-steps">${steps}</ol>
        <div id="lp-secret" class="hidden"></div>
        <div id="lp-result" class="hidden"></div>
        <div id="lp-error" class="hidden"></div>
        <button class="btn btn-ghost btn-block" id="lp-close" disabled>Close</button>
      </div>
    </div>`;
  document.getElementById("lp-close").onclick = closeModal;
}

function lpStepEl(id) {
  return document.querySelector(`.lp-steps li[data-step="${id}"]`);
}

/** Mark a step: pending | active | done | error. Optional note replaces the desc. */
function lpSetStep(id, state, note) {
  const li = lpStepEl(id);
  if (!li) return;
  li.classList.remove("active", "done", "error");
  if (state !== "pending") li.classList.add(state);
  const dot = li.querySelector(".lp-dot");
  dot.textContent = state === "done" ? "✓" : state === "error" ? "✕" : state === "active" ? "◌" : "○";
  if (note) li.querySelector(".lp-note").textContent = note;
}

function lpShowSecret(secret) {
  const el = document.getElementById("lp-secret");
  el.classList.remove("hidden");
  el.innerHTML = `
    <div class="secret-box">
      <strong>⚠️ Save your mint secret — shown once, never stored.</strong>
      <div class="secret-row"><code id="lp-secret-val">${esc(secret)}</code>
        <button class="btn btn-ghost" id="lp-copy-secret" type="button">Copy</button></div>
      <p class="tiny muted">The app uses it to sign the creation below. Keep a copy
      for your records — the server never sees it.</p>
    </div>`;
  document.getElementById("lp-copy-secret").onclick = async () => {
    try {
      await navigator.clipboard.writeText(secret);
      toast("Mint secret copied.", "success");
    } catch {
      toast("Copy failed — select the text manually.", "error");
    }
  };
}

function lpShowResult({ mint, createSig, radarWatchId, name, symbol }) {
  lpSetStep("done", "done", "Your token is live.");
  const el = document.getElementById("lp-result");
  el.classList.remove("hidden");
  const ex = `https://explorer.solana.com/address/${mint}?cluster=devnet`;
  el.innerHTML = `
    <div class="lp-done">
      <div class="lp-mint"><span class="tiny muted">Mint address</span>
        <code>${esc(mint)}</code></div>
      <div class="lp-result-links">
        <a class="btn btn-primary" href="${ex}" target="_blank" rel="noopener">View on explorer →</a>
        <button class="btn btn-ghost" id="lp-view-curve" type="button">Check the curve</button>
      </div>
      ${radarWatchId ? `<p class="tiny muted">🛰️ Radar sniper-watch active:
        <code class="tiny">${esc(radarWatchId)}</code> — sniper &amp; whale alerts
        are being collected for this mint.</p>` : ""}
      <p class="tiny muted">Create tx:
        <a href="https://explorer.solana.com/tx/${createSig}?cluster=devnet"
           target="_blank" rel="noopener">${esc(shortAddr(createSig))}</a></p>
    </div>`;
  document.getElementById("lp-view-curve").onclick = () => {
    closeModal();
    const input = document.getElementById("liq-mint");
    if (input) {
      input.value = mint;
      document.getElementById("liquidity").scrollIntoView({ behavior: "smooth" });
      document.getElementById("liq-check").click();
    }
  };
  document.getElementById("lp-close").disabled = false;
  // Add to the ticker with a live mint so it shows a real curve bar.
  DEMO_TOKENS.unshift({ symbol, name, ago: "just now", mint });
  renderTicker();
  toast(`👻 ${name} is live behind the veil.`, "success");
}

function lpFail(e) {
  console.error("[launchpad] launch failed", e);
  const msg = e?.message || String(e);
  for (const s of LP_STEPS) {
    const li = lpStepEl(s.id);
    if (li?.classList.contains("active")) lpSetStep(s.id, "error", msg);
  }
  const el = document.getElementById("lp-error");
  el.classList.remove("hidden");
  el.innerHTML = `<div class="lp-error-box"><strong>Launch stopped.</strong>
    <p class="tiny">${esc(msg)}</p>
    <p class="tiny muted">Transactions already confirmed stay on-chain. If the fee
    was paid but no token was created, the payment is recorded — email
    <a href="mailto:ghostlaunch.api@gmail.com">ghostlaunch.api@gmail.com</a>
    with your wallet address and we'll make it right.</p></div>`;
  document.getElementById("lp-close").disabled = false;
  toast("Launch stopped: " + msg, "error");
}

/* ------------------------------ flow pieces ------------------------------ */

/** Step 1: creator signs the SOL fee transfer. Returns the signature. */
async function payLaunchFee(connection, lamports) {
  const from = wallet.publicKey;
  const to = new PublicKey(CONFIG.paymentWallet);
  const { blockhash } = await connection.getLatestBlockhash("confirmed");
  const tx = new Transaction({ feePayer: from, recentBlockhash: blockhash }).add(
    SystemProgram.transfer({ fromPubkey: from, toPubkey: to, lamports })
  );
  const signed = await wallet.signTransaction(tx);
  return sendSignedTxRaw(connection, signed);
}

async function sendSignedTxRaw(connection, signed) {
  const sig = await connection.sendRawTransaction(signed.serialize(), {
    skipPreflight: false,
    preflightCommitment: "confirmed",
  });
  await confirmSignature(connection, sig);
  return sig;
}

/** Step 2: POST the launch to the backend. The on-chain payment is the auth. */
async function postLaunchBuild(body) {
  const ctrl = new AbortController();
  const timer = setTimeout(() => ctrl.abort(), 120000);
  try {
    const res = await fetch(`${CONFIG.ghostApi}/v1/launchpad/build`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
      signal: ctrl.signal,
    });
    const data = await res.json().catch(() => ({}));
    if (!res.ok) throw new Error(data.error || `Backend error ${res.status}`);
    return data;
  } catch (e) {
    if (e.name === "AbortError") throw new Error("Backend timed out — it may be waking up. Try again in 30s.");
    throw e;
  } finally {
    clearTimeout(timer);
  }
}

/**
 * Sign one unsigned backend tx. required_signers lists pubkeys in signature
 * slot order; the creator signs via the connected wallet, ephemeral keys
 * (hop/buyer/mint) sign from the one-time secrets.
 */
async function signBackendTx(vtx, requiredSigners, ephemeralKeys, creatorStr) {
  const kps = [];
  let needsWallet = false;
  for (const s of requiredSigners) {
    if (s === creatorStr) {
      needsWallet = true;
    } else {
      const kp = ephemeralKeys.get(s);
      if (!kp) throw new Error(`Missing one-time key for ${shortAddr(s)} — cannot sign.`);
      kps.push(kp);
    }
  }
  let out = vtx;
  if (needsWallet) out = await wallet.signTransaction(vtx);
  if (kps.length) out.sign(kps); // fills slots by pubkey — never clobbers the wallet sig
  return out;
}

async function waitForSlot(connection, target, note) {
  const start = Date.now();
  for (;;) {
    const slot = await connection.getSlot("confirmed").catch(() => null);
    if (slot !== null && slot >= target) return slot;
    if (Date.now() - start > 120000) return null; // submit anyway — don't strand the launch
    if (note) note(`waiting for slot ${target.toLocaleString()}…`);
    await sleep(500);
  }
}

/* --------------------------------- run ---------------------------------- */

async function runLaunch(draft) {
  const connection = getConnection();
  const creatorStr = wallet.publicKey.toBase58();
  const feeLamports = Math.round(draft.totalSol * LAMPORTS_PER_SOL);

  // 1. Fee.
  lpSetStep("fee", "active", `Sign ${draft.totalSol.toFixed(2)} SOL in your wallet…`);
  const paymentSig = await payLaunchFee(connection, feeLamports);
  lpSetStep("fee", "done", `Paid ${draft.totalSol.toFixed(2)} SOL ✓`);

  // 2. Build.
  lpSetStep("build", "active", "Verifying payment, building stealth txs…");
  const build = await postLaunchBuild({
    payment_signature: paymentSig,
    token_name: draft.name,
    token_symbol: draft.symbol,
    token_uri: draft.tokenUri,
    token_website: draft.website || null,
    token_twitter: draft.twitter || null,
    creator_pubkey: creatorStr,
    dev_buy_sol: draft.devBuySol,
    rpc_url: CONFIG.rpcUrl,
    stealth: {
      buyer_wallets: draft.wallets,
      amount_jitter_bps: draft.jitterPct * 100,
      spread_slots: draft.spread ? 3 : 0,
    },
    addons: {
      deep_veil: draft.addons.deepVeil,
      extra_wallets: draft.addons.extraWallets,
      radar_watch: draft.addons.radarWatch,
    },
  });
  lpSetStep("build", "done", `Bundle ready — fee verified (${build.fee_paid_sol} SOL)`);
  lpShowSecret(build.mint_secret_key);
  toast("Bundle built — funding the stealth wallets next.", "success");

  const ephemeral = new Map(); // pubkey → Keypair (one-time; deleted after use)
  for (const h of build.stealth?.hop_secret_keys ?? []) {
    ephemeral.set(h.pubkey, keypairFromB58(h.secret_key));
  }
  for (const b of build.buyer_secret_keys ?? []) {
    ephemeral.set(b.pubkey, keypairFromB58(b.secret_key));
  }
  const mintKp = keypairFromB58(build.mint_secret_key);
  ephemeral.set(mintKp.publicKey.toBase58(), mintKp);

  // 3+4. Batch-sign ALL creator transactions in ONE wallet popup, then send sequentially.
  // The fee was already paid (that's the auth). Now: funding transfers + token creation
  // all get signed together — one popup instead of one per transaction.
  const funding = build.stealth?.funding_transactions ?? [];
  const createTx = build.transactions.find((t) => t.kind === "create");
  if (!createTx) throw new Error("Backend returned no create transaction.");

  // Decode and prepare everything upfront.
  const toSign = [];
  for (const ft of funding) {
    const vtx = decodeUnsignedTx(ft.base64);
    await refreshBlockhash(connection, vtx);
    toSign.push({ vtx, requiredSigners: ft.required_signers, kind: "funding" });
  }
  const createVtx = decodeUnsignedTx(createTx.base64);
  await refreshBlockhash(connection, createVtx);
  toSign.push({ vtx: createVtx, requiredSigners: createTx.required_signers, kind: "create" });

  // Single wallet popup for everything the creator must sign.
  lpSetStep("fund", "active", "Sign once in your wallet — funding + token creation…");
  const creatorTxns = toSign.filter((t) => t.requiredSigners.includes(creatorStr));
  if (creatorTxns.length) {
    const signed = await wallet.signAllTransactions(creatorTxns.map((t) => t.vtx));
    signed.forEach((stx, i) => { creatorTxns[i].vtx = stx; });
  }
  // Ephemeral keys (hop/buyer/mint) sign their slots — no wallet needed.
  for (const t of toSign) {
    const kps = [];
    for (const s of t.requiredSigners) {
      if (s !== creatorStr) {
        const kp = ephemeral.get(s);
        if (!kp) throw new Error(`Missing one-time key for ${shortAddr(s)} — cannot sign.`);
        kps.push(kp);
      }
    }
    if (kps.length) t.vtx.sign(kps);
  }

  // Send funding transfers in order.
  if (funding.length) {
    const totalFunding = build.stealth.total_funding_sol;
    lpSetStep("fund", "active", `0/${funding.length} — ~${totalFunding.toFixed(3)} SOL leaves your wallet (your dev buy + hop fees; hops end at zero)`);
    let i = 0;
    for (const t of toSign.filter((x) => x.kind === "funding")) {
      i += 1;
      lpSetStep("fund", "active", `${i}/${funding.length} — funding transfer ${i}…`);
      await sendSignedTx(connection, t.vtx);
      for (const s of t.requiredSigners) {
        if (s !== creatorStr && s !== mintKp.publicKey.toBase58()) ephemeral.delete(s);
      }
    }
    lpSetStep("fund", "done", `${funding.length} funding transfers confirmed ✓`);
  } else {
    lpSetStep("fund", "done", "No dev buy — nothing to fund");
  }

  // Send the token creation.
  lpSetStep("create", "active", "Creating your token…");
  const createSigned = toSign.find((t) => t.kind === "create").vtx;
  const createSig = await sendSignedTx(connection, createSigned);
  ephemeral.delete(mintKp.publicKey.toBase58());
  lpSetStep("create", "done", `Token created ✓`);

  // Create slot anchors the buy schedule.
  let createSlot = null;
  for (let tries = 0; tries < 8 && createSlot === null; tries++) {
    const info = await connection
      .getTransaction(createSig, { commitment: "confirmed", maxSupportedTransactionVersion: 0 })
      .catch(() => null);
    if (info?.slot) createSlot = info.slot;
    else await sleep(1500);
  }
  if (createSlot === null) createSlot = await connection.getSlot("confirmed").catch(() => 0);

  // 5. Stealth buys, per the slot schedule.
  const schedule = build.stealth?.buy_schedule ?? [];
  const txByIndex = new Map(build.transactions.map((t) => [t.index, t]));
  if (schedule.length) {
    let i = 0;
    for (const s of schedule) {
      i += 1;
      const target = createSlot + (s.target_slot_offset || 0);
      lpSetStep("buys", "active", `buy ${i}/${schedule.length} — ${s.amount_sol.toFixed(3)} SOL…`,
      );
      await waitForSlot(connection, target, (m) => lpSetStep("buys", "active", `buy ${i}/${schedule.length} — ${m}`));
      const t = txByIndex.get(s.transaction_index);
      if (!t) throw new Error(`Backend schedule points at missing tx ${s.transaction_index}.`);
      const vtx = decodeUnsignedTx(t.base64);
      await refreshBlockhash(connection, vtx);
      // Buys are signed by the disposable buyer wallets only — never the creator's wallet.
      const kps = [];
      for (const sg of t.required_signers) {
        if (sg === creatorStr) throw new Error("Buy transaction unexpectedly requires creator signature.");
        const kp = ephemeral.get(sg);
        if (!kp) throw new Error(`Missing one-time key for ${shortAddr(sg)} — cannot sign.`);
        kps.push(kp);
      }
      if (kps.length) vtx.sign(kps);
      await sendSignedTx(connection, vtx);
      ephemeral.delete(s.wallet);
    }
    lpSetStep("buys", "done", `${schedule.length} stealth buys confirmed ✓`);
  } else {
    lpSetStep("buys", "done", "No buys scheduled");
  }

  // 6. Done.
  lpShowResult({
    mint: build.mint_pubkey,
    createSig,
    radarWatchId: build.radar_watch_id || null,
    name: draft.name,
    symbol: draft.symbol,
  });
}


/* ------------------------------ just launched ------------------------------ */

// TODO: replace with a real token feed — index Pump.fun create events for
// launches made through the GhostLaunch pad (backend indexer), newest first.
// Each entry gets `mint` when known; the ticker then shows a LIVE curve
// progress bar instead of the placeholder.
const DEMO_TOKENS = [
  { symbol: "VCAT", name: "Veil Cat", ago: "2m ago", mint: null },
  { symbol: "GPEPP", name: "Ghost Pepper", ago: "14m ago", mint: null },
  { symbol: "NITE", name: "Midnight", ago: "31m ago", mint: null },
  { symbol: "SHDW", name: "Shadow", ago: "2h ago", mint: null },
  { symbol: "MIST", name: "Mist", ago: "2h ago", mint: null },
];

/** Shared mini progress bar: SOL in curve vs the ~85 SOL graduation line. */
function miniCurveBar(s) {
  const sol = s.solInCurveLamports / LAMPORTS_PER_SOL;
  if (s.graduated) {
    return `<span class="tiny liq-grad">graduated → PumpSwap</span>`;
  }
  const pct = Math.min(100, (sol / CONFIG.graduationThresholdSol) * 100);
  return `<div class="mini-bar" role="img" aria-label="${pct.toFixed(1)} percent to graduation">
      <div class="mini-fill" style="width:${pct.toFixed(1)}%"></div>
    </div>
    <span class="tiny muted">${sol.toFixed(2)} SOL · ${pct.toFixed(0)}% to grad</span>`;
}

function renderTicker() {
  const el = document.getElementById("ticker");
  if (!el) return; // ticker section removed
  el.innerHTML = DEMO_TOKENS.map(
    (t, i) => `<div class="ticker-chip" title="${esc(t.name)}">
      <div class="ticker-top">
        <span class="ticker-icon">${esc(t.symbol.slice(0, 2))}</span>
        <span class="ticker-name">$${esc(t.symbol)}</span>
        <span class="ticker-time">${esc(t.ago)}</span>
      </div>
      <div class="chip-liq" id="chip-liq-${i}">
        <span class="tiny muted">launch to see live data</span>
      </div>
    </div>`
  ).join("");
  // Live curve bars for any card that carries a mint. Demo cards have none,
  // so they keep the graceful placeholder above.
  DEMO_TOKENS.forEach((t, i) => {
    if (t.mint) loadChipCurve(i, t.mint);
  });
}

async function loadChipCurve(i, mint) {
  const el = document.getElementById(`chip-liq-${i}`);
  if (!el) return;
  el.innerHTML = `<span class="tiny muted">reading curve…</span>`;
  try {
    el.innerHTML = miniCurveBar(await fetchCurveProgress(mint));
  } catch (e) {
    el.innerHTML = `<span class="tiny muted">curve unavailable</span>`;
  }
}

/* ---------------------------- liquidity checker ---------------------------- */

function wireLiquidityChecker() {
  const btn = document.getElementById("liq-check");
  if (!btn) return;
  const run = async () => {
    const mint = document.getElementById("liq-mint").value.trim();
    const out = document.getElementById("liq-result");
    if (!mint) {
      toast("Paste a token mint address first.", "error");
      return;
    }
    btn.disabled = true;
    out.innerHTML = `<p class="tiny muted">Reading the curve…</p>`;
    try {
      const s = await fetchCurveProgress(mint);
      const sol = s.solInCurveLamports / LAMPORTS_PER_SOL;
      const pct = Math.min(100, (sol / CONFIG.graduationThresholdSol) * 100);
      out.innerHTML = s.graduated
        ? `<div class="liq-grad-banner">🎓 <strong>Graduated</strong> — this token
           has migrated and now trades on PumpSwap.</div>
           <div class="curve-labels" style="margin-top:10px">
             <span><strong>${sol.toFixed(2)} SOL</strong> in curve</span>
             <span>100% to graduation</span>
           </div>`
        : `<div class="curve-bar" role="img" aria-label="${pct.toFixed(1)} percent to graduation">
             <div class="curve-fill" style="width:${pct.toFixed(1)}%"></div>
           </div>
           <div class="curve-labels">
             <span><strong>${sol.toFixed(2)} SOL</strong> in curve</span>
             <span>${pct.toFixed(1)}% to graduation · ~${CONFIG.graduationThresholdSol} SOL line</span>
           </div>`;
    } catch (e) {
      out.innerHTML = `<p class="tiny" style="color:var(--danger)">
        Couldn't read that curve: ${esc(e.message)}</p>`;
    } finally {
      btn.disabled = false;
    }
  };
  btn.onclick = run;
  document.getElementById("liq-mint").addEventListener("keydown", (e) => {
    if (e.key === "Enter") run();
  });
}

/* ---------------------------------- boot ----------------------------------- */

document.getElementById("logo").innerHTML = `${ghostLogo(28)}<span>GhostLaunch</span>`;
document.getElementById("net-badge").textContent = CONFIG.network.toUpperCase() + "NET";

renderWalletButtons();
wireForm();
wireLiquidityChecker();
renderTicker();

// Silent reconnect — never pops a wallet window on load.
wallet.autoReconnect().catch(() => {});
