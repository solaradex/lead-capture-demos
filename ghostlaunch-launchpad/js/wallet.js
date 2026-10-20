/**
 * GhostLaunch Launchpad — wallet connection.
 *
 * Vanilla-JS wrapper around @solana/wallet-adapter adapters (no React).
 * Solflare is listed FIRST (Brad's preference); Phantom offered as an option.
 */
import { SolflareWalletAdapter } from "@solana/wallet-adapter-solflare";
import { PhantomWalletAdapter } from "@solana/wallet-adapter-phantom";
import { CONFIG } from "./config.js";

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
