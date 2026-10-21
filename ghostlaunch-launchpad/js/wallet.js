/**
 * GhostLaunch Launchpad — wallet connection.
 *
 * Vanilla-JS wrapper around @solana/wallet-adapter adapters (no React).
 * Solflare is listed FIRST (Brad's preference); Phantom offered as an option.
 */
import { SolflareWalletAdapter } from "@solana/wallet-adapter-solflare";
import { PhantomWalletAdapter } from "@solana/wallet-adapter-phantom";
import { PublicKey } from "@solana/web3.js";
import { CONFIG } from "./config.js";

/**
 * In the Solflare mobile app's built-in browser, window.solflare is injected
 * directly. Talking to it directly is far more reliable than routing through
 * the wallet-adapter SDK's popup/iframe machinery (which is built for desktop
 * extensions — in the in-app browser its postMessage signing gets null back
 * and dies in Buffer.from).
 *
 * Detection is deliberately broad: the in-app browser doesn't always set the
 * isSolflare flag the way the desktop extension does.
 */
function getInjectedSolflare() {
  try {
    if (window.solflare?.isSolflare) return window.solflare;
    if (window.SolflareApp) return window.SolflareApp;
    const p = window.solflare;
    if (
      p &&
      typeof p.connect === "function" &&
      typeof p.signTransaction === "function"
    ) {
      return p;
    }
    return null;
  } catch {
    return null;
  }
}

/** Wait briefly for the in-app browser to inject its provider. */
async function waitForInjectedSolflare(timeoutMs = 2500) {
  const start = Date.now();
  for (;;) {
    const p = getInjectedSolflare();
    if (p) return p;
    if (Date.now() - start > timeoutMs) return null;
    await new Promise((r) => setTimeout(r, 100));
  }
}

/** Thin wrapper: normalizes the injected provider to our PublicKey class. */
class InjectedSolflareWallet {
  constructor(provider) {
    this.p = provider;
    this._onDisconnect = [];
    this.path = "injected";
  }
  get publicKey() {
    const pk = this.p.publicKey;
    return pk ? new PublicKey(pk.toBytes()) : null;
  }
  get connected() {
    return !!this.p.connected && !!this.p.publicKey;
  }
  async connect() {
    if (!this.p.connected) await this.p.connect();
    return this.publicKey;
  }
  async disconnect() {
    try {
      await this.p.disconnect();
    } catch {
      /* already gone */
    }
  }
  on(event, fn) {
    if (event === "disconnect" && this.p.on) this.p.on("disconnect", fn);
  }
  async signTransaction(tx) {
    return await this.p.signTransaction(tx);
  }
  async signAllTransactions(txs) {
    return await this.p.signAllTransactions(txs);
  }
  /**
   * signAndSendTransaction is the most reliable signing method inside
   * in-app browsers (single native call, no popup/iframe juggling).
   * Returns the signature string.
   */
  async signAndSendTransaction(tx) {
    if (typeof this.p.signAndSendTransaction !== "function") {
      throw new Error("signAndSendTransaction not available");
    }
    const result = await this.p.signAndSendTransaction(tx);
    const sig = result?.signature ?? result;
    if (typeof sig !== "string" || sig.length < 80) {
      throw new Error("signAndSendTransaction returned no signature");
    }
    return sig;
  }
}

const WALLET_DEFS = [
  {
    name: "Solflare",
    // Tell the wallet which cluster we're on — without this the SDK
    // defaults to mainnet-beta and signing breaks on devnet.
    make: () => new SolflareWalletAdapter({ network: CONFIG.network }),
    recommended: true,
    installUrl: "https://solflare.com",
  },
  {
    name: "Phantom",
    make: () => new PhantomWalletAdapter(),
    recommended: false,
    installUrl: "https://phantom.app",
  },
];

const STORAGE_KEY = "ghostlaunch.wallet";

export class WalletManager {
  constructor() {
    this.adapters = new Map();
    this.current = null;
    this.currentName = null;
    this.handlers = { connect: [], disconnect: [], error: [] };
  }

  /** Wallet definitions in display order (Solflare first). */
  defs() {
    return WALLET_DEFS.map((d) => ({
      name: d.name,
      recommended: d.recommended,
      installUrl: d.installUrl,
      detected: this._isDetected(d.name),
    }));
  }

  _isDetected(name) {
    try {
      if (name === "Solflare" && getInjectedSolflare()) return true;
      const a = this._adapter(name);
      // Most adapters expose `readyState`; "Installed"/"Loadable" counts.
      return a.readyState === "Installed" || a.readyState === "Loadable";
    } catch {
      return false;
    }
  }

  _adapter(name) {
    if (!this.adapters.has(name)) {
      const def = WALLET_DEFS.find((d) => d.name === name);
      if (!def) throw new Error(`Unknown wallet: ${name}`);
      this.adapters.set(name, def.make());
    }
    return this.adapters.get(name);
  }

  on(event, fn) {
    if (this.handlers[event]) this.handlers[event].push(fn);
  }

  _emit(event, data) {
    for (const fn of this.handlers[event] || []) {
      try {
        fn(data);
      } catch (e) {
        console.error("wallet handler error", e);
      }
    }
  }

  get publicKey() {
    return this.current?.publicKey ?? null;
  }

  get connected() {
    return !!this.current?.connected;
  }

  get walletName() {
    return this.currentName;
  }

  /** Connect to a named wallet. Throws a user-friendly Error on failure. */
  async connect(name) {
    // Prefer the injected provider (Solflare mobile in-app browser) —
    // direct, no popup/iframe machinery.
    if (name === "Solflare") {
      const injected = await waitForInjectedSolflare();
      if (injected) {
        try {
          const w = new InjectedSolflareWallet(injected);
          await w.connect();
          this.current = w;
          this.currentName = name;
          try {
            localStorage.setItem(STORAGE_KEY, name);
          } catch {
            /* private mode — ignore */
          }
          w.on("disconnect", () => {
            this.current = null;
            this.currentName = null;
            this._emit("disconnect");
          });
          this._emit("connect", { publicKey: w.publicKey, name });
          try {
            toast("Connected via in-app wallet", "success");
          } catch {}
          return w.publicKey;
        } catch (e) {
          throw new Error(`Could not connect to ${name}: ${e?.message || e}`);
        }
      }
      try {
        toast("In-app wallet not found — using extension mode", "warning");
      } catch {}
    }
    const adapter = this._adapter(name);
    if (!adapter.connected) {
      try {
        await adapter.connect();
      } catch (e) {
        const msg = String(e?.message || e);
        if (/not found|not installed|no wallet/i.test(msg)) {
          const def = WALLET_DEFS.find((d) => d.name === name);
          throw new Error(
            `${name} wallet not detected. Install it here: ${def.installUrl}`
          );
        }
        throw new Error(`Could not connect to ${name}: ${msg}`);
      }
    }
    this.current = adapter;
    this.currentName = name;
    try {
      localStorage.setItem(STORAGE_KEY, name);
    } catch {
      /* private mode — ignore */
    }
    adapter.on("disconnect", () => {
      this.current = null;
      this.currentName = null;
      try {
        localStorage.removeItem(STORAGE_KEY);
      } catch {
        /* ignore */
      }
      this._emit("disconnect");
    });
    this._emit("connect", { publicKey: adapter.publicKey, name });
    return adapter.publicKey;
  }

  async disconnect() {
    if (this.current) {
      try {
        await this.current.disconnect();
      } catch {
        /* already gone */
      }
    }
    this.current = null;
    this.currentName = null;
    try {
      localStorage.removeItem(STORAGE_KEY);
    } catch {
      /* ignore */
    }
    this._emit("disconnect");
  }

  /** Attempt silent reconnect with the last-used wallet (no popup). */
  async autoReconnect() {
    let saved = null;
    try {
      saved = localStorage.getItem(STORAGE_KEY);
    } catch {
      return null;
    }
    if (!saved) return null;
    try {
      // Injected provider first (mobile in-app browser). If the provider
      // exists at all, use it — don't fall through to the adapter just
      // because it isn't "connected" yet on a fresh page load. The adapter
      // path is broken inside in-app browsers (its postMessage signing gets
      // null back), so falling through strands the user on a dead path.
      if (saved === "Solflare") {
        const injected = await waitForInjectedSolflare(1500);
        if (injected) {
          const w = new InjectedSolflareWallet(injected);
          // If the provider is already connected (returning user), adopt it
          // silently. If not, leave it for the user to connect via the
          // Connect button — but do NOT fall through to the adapter.
          if (injected.connected && injected.publicKey) {
            this.current = w;
            this.currentName = saved;
            this._emit("connect", { publicKey: w.publicKey, name: saved });
            return w.publicKey;
          }
          // Provider exists but not connected: clear the stale auto-connect
          // so the next explicit connect() takes the injected path.
          return null;
        }
      }
      const adapter = this._adapter(saved);
      // Only attempt if the extension is present; never force a popup.
      if (adapter.readyState !== "Installed") return null;
      await adapter.connect();
      this.current = adapter;
      this.currentName = saved;
      adapter.on("disconnect", () => {
        this.current = null;
        this.currentName = null;
        this._emit("disconnect");
      });
      this._emit("connect", { publicKey: adapter.publicKey, name: saved });
      return adapter.publicKey;
    } catch {
      return null;
    }
  }

  async signTransaction(tx) {
    if (!this.connected) throw new Error("Connect a wallet first.");
    return this.current.signTransaction(tx);
  }

  async signAllTransactions(txs) {
    if (!this.connected) throw new Error("Connect a wallet first.");
    return this.current.signAllTransactions(txs);
  }
}

export const wallet = new WalletManager();
