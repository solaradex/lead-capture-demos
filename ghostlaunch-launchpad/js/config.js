/**
 * GhostLaunch Launchpad — central configuration.
 *
 * DEVNET ONLY. Flip `network` to "mainnet" only with Brad's explicit approval.
 * No mainnet execution, no real SOL spent from this scaffold.
 */
export const CONFIG = {
  network: "devnet",

  // RPC endpoints. Public devnet RPC is rate-limited — TODO: swap in a
  // Helius devnet key (same pattern as the launch-api backend) for production.
  rpcUrl:
    "devnet" === "devnet"
      ? "https://api.devnet.solana.com"
      : "https://api.mainnet-beta.solana.com",

  // Pump.fun live programs (verified Oct 2026, pump-public-docs).
  pumpProgramId: "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P",
  pumpAmmProgramId: "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA",
  tokenProgram2022: "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb",
  nativeMint: "So11111111111111111111111111111111111111112", // wSOL

  // Pricing — approved by Brad Oct 19-20, 2026.
  launchFeeSol: 0.2, // flat per creator launch, stealth included

  // Graduation: ~85 SOL in the bonding curve migrates the token to a
  // PumpSwap pool. Used for the liquidity progress bars.
  graduationThresholdSol: 85,
  paymentWallet: "2q4EwekHtit3BtzL25NmGbWJ5khPkmypYbba2t3A4XtM",

  // The 0.30% creator fee on every trade is pointed here (platform revenue).
  // The 0.95% protocol fee always goes to Pump.fun — unavoidable.
  // Set to the user's wallet instead if we ever want creators to keep it.
  creatorFeeWallet: "2q4EwekHtit3BtzL25NmGbWJ5khPkmypYbba2t3A4XtM",

  // GhostLaunch backend API (key pool, billing, radar).
  ghostApi: "https://ghostlaunch-api.onrender.com",

  // Token metadata constraints (Pump.fun create_v2).
  maxNameLen: 32,
  maxSymbolLen: 13,
  maxUriLen: 200,
  tokenDecimals: 6,

  brand: {
    name: "GhostLaunch",
    slogan: "Launch behind the veil",
    site: "https://ghostlaunch.framer.website/",
  },

  ipfsGateway: "https://ipfs.io/ipfs/",

  // Token metadata URI used until IPFS pinning ships (TODO).
  // The form lets the user paste an image URL instead; a file upload only
  // previews locally for now.
  placeholderTokenUri: "https://ghostlaunch.framer.website/meta/placeholder.json",
};

export const LAMPORTS_PER_SOL = 1_000_000_000;
