/**
 * GhostLaunch Launchpad — Pump.fun program integration.
 *
 * Uses the OFFICIAL Pump.fun TS SDKs (verified Oct 2026):
 *   @pump-fun/pump-sdk@4.1.0      — bonding curve (create_v2, buy_v2, sell_v2)
 *   @pump-fun/pump-swap-sdk@2.1.0  — post-migration PumpSwap trading
 * Method signatures taken from pump-public-docs:
 *   docs/instructions/COIN_CREATION.md, BUY.md, SELL.md, FEE_RECIPIENTS.md
 *
 * Everything here targets DEVNET. No mainnet execution.
 */
import {
  Connection,
  PublicKey,
  Transaction,
  Keypair,
} from "@solana/web3.js";
import { PUMP_SDK } from "@pump-fun/pump-sdk";
import { PUMP_AMM_SDK } from "@pump-fun/pump-swap-sdk";
import BN from "bn.js";
import { CONFIG, LAMPORTS_PER_SOL } from "./config.js";

const PUMP_PROGRAM_ID = new PublicKey(CONFIG.pumpProgramId);
const TOKEN_2022_PROGRAM_ID = new PublicKey(CONFIG.tokenProgram2022);
const NATIVE_MINT = new PublicKey(CONFIG.nativeMint);
// Legacy SPL Token program — quote side for SOL-paired coins (per BUY.md).
const SPL_TOKEN_PROGRAM_ID = new PublicKey(
  "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
);

// Fee recipients — official lists from pump-public-docs/docs/FEE_RECIPIENTS.md.
// Any of the 8 in each list is valid; we use index 0.
// TODO: rotate across the 8 periodically instead of pinning index 0.
const FEE_RECIPIENT_NORMAL = new PublicKey(
  "62qc2CNXwrYqQScmEdiZFFAnJR262PxWEuNQtxfafNgV"
);
const FEE_RECIPIENT_BUYBACK = new PublicKey(
  "5YxQFdt3Tr9zJLvkFccqXVUwhdTWJQc1fFg2YPbxvxeD"
);

let _connection = null;

/** Shared Connection (devnet by default — see config.js). */
export function getConnection() {
  if (!_connection) {
    _connection = new Connection(CONFIG.rpcUrl, "confirmed");
  }
  return _connection;
}

/** Bonding-curve PDA: seeds [b"bonding-curve", mint] (per COIN_CREATION.md). */
export function deriveBondingCurvePda(mint) {
  const mintPk = new PublicKey(mint);
  const [pda] = PublicKey.findProgramAddressSync(
    [Buffer.from("bonding-curve"), mintPk.toBuffer()],
    PUMP_PROGRAM_ID
  );
  return pda;
}

/**
 * Fetch + minimally decode the bonding-curve account.
 *
 * Layout assumption (first fields of the pump BondingCurve account, per the
 * public IDL — verify against pump-public-docs pump.json if decoding breaks):
 *   8  discriminator
 *   8  virtualTokenReserves (u64 LE)
 *   8  virtualSolReserves   (u64 LE)
 *   8  realTokenReserves    (u64 LE)
 *   8  realSolReserves      (u64 LE)
 *   8  tokenTotalSupply     (u64 LE)
 *   1  complete             (bool)
 *   32 creator              (pubkey)
 *
 * TODO: replace this hand decode with the SDK's account decoder once we pin
 * the exact export name in @pump-fun/pump-sdk (it ships the Anchor IDL).
 */
export async function fetchCurveState(connection, mint) {
  const pda = deriveBondingCurvePda(mint);
  const info = await connection.getAccountInfo(pda);
  if (!info) throw new Error("Bonding curve not found — token may not exist.");
  const d = info.data;
  const u64le = (off) => new BN(d.subarray(off, off + 8), "le");
  const creator = new PublicKey(d.subarray(49, 81));
  return {
    pda,
    virtualTokenReserves: u64le(8),
    virtualSolReserves: u64le(16),
    realTokenReserves: u64le(24),
    realSolReserves: u64le(32),
    tokenTotalSupply: u64le(40),
    complete: d[48] === 1,
    creator,
  };
}

/**
 * Convenience wrapper: read a token's bonding-curve progress over the
 * default (devnet) connection. READ-ONLY — no SOL spent, nothing signed.
 *
 * Returns plain numbers (all safely below 2^53):
 *   { solInCurveLamports, tokensSoldBaseUnits, graduated }
 * `graduated` mirrors the curve's `complete` flag (migrated to PumpSwap).
 * `tokensSoldBaseUnits` ≈ total supply minus tokens still sitting in the
 * curve (i.e. bought out of the bonding curve so far).
 *
 * Throws when the curve account doesn't exist (bad/unknown mint) or the
 * RPC call fails — callers should catch and show a graceful message.
 */
export async function fetchCurveProgress(mint) {
  const curve = await fetchCurveState(getConnection(), mint);
  const solInCurveLamports = curve.realSolReserves.toNumber();
  const tokensSoldBaseUnits = curve.tokenTotalSupply
    .sub(curve.realTokenReserves)
    .toNumber();
  return {
    solInCurveLamports,
    tokensSoldBaseUnits,
    graduated: curve.complete,
  };
}

/**
 * Constant-product buy quote on virtual reserves:
 *   out = Vt - (Vt * Vs) / (Vs + dx)
 * Returns estimated base-token units (6 decimals) for `solLamports` in.
 *
 * TODO: verify fee handling against the SDK — Pump.fun takes 1.25% per trade
 * (0.95% protocol + 0.30% creator). This quote is pre-fee; the UI applies a
 * slippage buffer on top via maxSolCost.
 */
export function quoteBuyTokens(solLamportsBN, curve) {
  const Vt = curve.virtualTokenReserves;
  const Vs = curve.virtualSolReserves;
  if (Vs.isZero() || Vt.isZero()) throw new Error("Curve has no liquidity.");
  const dx = new BN(solLamportsBN);
  const numerator = Vt.mul(Vs);
  const denominator = Vs.add(dx);
  const out = Vt.sub(numerator.div(denominator));
  return out; // base-token units (6 decimals)
}

/** Inverse quote: SOL out for selling `tokenAmountBN` base units. */
export function quoteSellSol(tokenAmountBN, curve) {
  const Vt = curve.virtualTokenReserves;
  const Vs = curve.virtualSolReserves;
  const dy = new BN(tokenAmountBN);
  const numerator = Vs.mul(dy);
  const denominator = Vt.add(dy);
  return Vs.sub(numerator.div(denominator)); // lamports, pre-fee
}

/**
 * Build an UNSIGNED create_v2 transaction (per COIN_CREATION.md).
 *
 * IMPORTANT — scaffold flow: the real launch goes through the GhostLaunch
 * backend proxy (POST /v1/bundle/build), which verifies the 0.2 SOL payment
 * to the payment wallet and applies Cloak stealth funding BEFORE the create
 * lands. This direct-SDK path exists for devnet testing of the instruction
 * shape only.
 *
 * Returns { transaction, signers } — caller adds blockhash, gets the wallet
 * to sign, then has mintKeypair sign too.
 */
export async function buildCreateTransaction({
  user, // base58
  name,
  symbol,
  uri,
  mintKeypair, // Keypair — generated client-side per launch
}) {
  if (name.length > CONFIG.maxNameLen)
    throw new Error(`Name too long (max ${CONFIG.maxNameLen} chars).`);
  if (symbol.length > CONFIG.maxSymbolLen)
    throw new Error(`Symbol too long (max ${CONFIG.maxSymbolLen} chars).`);
  if (uri.length > CONFIG.maxUriLen)
    throw new Error(`URI too long (max ${CONFIG.maxUriLen} chars).`);

  const ix = await PUMP_SDK.createV2Instruction({
    mint: mintKeypair.publicKey,
    name,
    symbol,
    uri,
    // The 0.30% creator fee on every trade goes to the platform wallet.
    // (See CONFIG.creatorFeeWallet — set to the user's wallet if we ever
    // want creators to keep their trade fees.)
    creator: new PublicKey(CONFIG.creatorFeeWallet),
    user: new PublicKey(user),
    mayhemMode: false,
    holderReward: false,
  });

  const tx = new Transaction().add(ix);
  tx.feePayer = new PublicKey(user);
  return { transaction: tx, signers: [mintKeypair] };
}

/** Attach a fresh blockhash to a transaction. */
export async function withBlockhash(connection, tx) {
  const { blockhash, lastValidBlockHeight } =
    await connection.getLatestBlockhash("confirmed");
  tx.recentBlockhash = blockhash;
  tx.lastValidBlockHeight = lastValidBlockHeight;
  return tx;
}

/**
 * Build an UNSIGNED buy_v2 transaction (per BUY.md) for a SOL-paired coin
 * still on the bonding curve.
 *
 * `solLamports` — how much SOL the user wants to spend.
 * Slippage: maxSolCost = solLamports * (1 + slippageBps/10000).
 */
export async function buildBuyTransaction({
  connection,
  user, // base58
  mint, // base58
  solLamports, // number | string | BN
  slippageBps = 200, // 2% default
}) {
  const curve = await fetchCurveState(connection, mint);
  if (curve.complete)
    throw new Error(
      "Token migrated to PumpSwap — use the PumpSwap trade path (TODO)."
    );

  const dx = new BN(solLamports.toString());
  const amountOut = quoteBuyTokens(dx, curve);
  if (amountOut.isZero())
    throw new Error("Quote too small — increase the SOL amount.");
  const maxSolCost = dx.mul(new BN(10000 + slippageBps)).div(new BN(10000));

  const ix = await PUMP_SDK.getBuyV2InstructionRaw({
    user: new PublicKey(user),
    mint: new PublicKey(mint),
    creator: curve.creator,
    amount: amountOut,
    quoteAmount: maxSolCost,
    tokenProgram: TOKEN_2022_PROGRAM_ID,
    quoteMint: NATIVE_MINT,
    quoteTokenProgram: SPL_TOKEN_PROGRAM_ID,
    feeRecipient: FEE_RECIPIENT_NORMAL,
    buybackFeeRecipient: FEE_RECIPIENT_BUYBACK,
  });

  const tx = new Transaction().add(ix);
  tx.feePayer = new PublicKey(user);
  return { transaction: tx, signers: [], quote: { tokensOut: amountOut, maxSolCost } };
}

/**
 * Build an UNSIGNED sell_v2 transaction (per SELL.md).
 * `tokenAmount` — base-token units (6 decimals) to sell.
 * `minSolOut` — slippage floor in lamports.
 */
export async function buildSellTransaction({
  connection,
  user,
  mint,
  tokenAmount, // base units
  slippageBps = 200,
}) {
  const curve = await fetchCurveState(connection, mint);
  if (curve.complete)
    throw new Error(
      "Token migrated to PumpSwap — use the PumpSwap trade path (TODO)."
    );

  const dy = new BN(tokenAmount.toString());
  const grossOut = quoteSellSol(dy, curve);
  const minSolOut = grossOut.mul(new BN(10000 - slippageBps)).div(new BN(10000));

  const ix = await PUMP_SDK.getSellV2InstructionRaw({
    user: new PublicKey(user),
    mint: new PublicKey(mint),
    creator: curve.creator,
    amount: dy,
    quoteAmount: minSolOut,
    tokenProgram: TOKEN_2022_PROGRAM_ID,
    quoteMint: NATIVE_MINT,
    quoteTokenProgram: SPL_TOKEN_PROGRAM_ID,
    feeRecipient: FEE_RECIPIENT_NORMAL,
    buybackFeeRecipient: FEE_RECIPIENT_BUYBACK,
  });

  const tx = new Transaction().add(ix);
  tx.feePayer = new PublicKey(user);
  return { transaction: tx, signers: [], quote: { minSolOut } };
}

/**
 * Sign with the connected wallet (+ extra signers), send, and confirm.
 * Returns the signature.
 */
export async function signAndSend({ connection, wallet, transaction, signers = [] }) {
  await withBlockhash(connection, transaction);
  const signed = await wallet.signTransaction(transaction);
  if (signers.length) signed.partialSign(...signers);
  const sig = await connection.sendRawTransaction(signed.serialize(), {
    skipPreflight: false,
    preflightCommitment: "confirmed",
  });
  await connection.confirmTransaction(
    {
      signature: sig,
      blockhash: transaction.recentBlockhash,
      lastValidBlockHeight: transaction.lastValidBlockHeight,
    },
    "confirmed"
  );
  return sig;
}

/**
 * Post-migration trading goes through PumpSwap — placeholder wiring.
 * TODO: implement with PUMP_AMM_SDK (pool lookup by mint, then swap
 * instructions). Reference: pump-public-docs "PumpSwap Trades V2".
 */
export function assertNotMigrated(curve) {
  if (curve.complete) {
    throw new Error(
      "This token has migrated to PumpSwap. PumpSwap trading is not wired yet — see TODO in pump.js."
    );
  }
}

export { PUMP_SDK, PUMP_AMM_SDK };
export const solToLamports = (sol) =>
  new BN(Math.floor(Number(sol) * LAMPORTS_PER_SOL).toString());
export const lamportsToSol = (lamports) =>
  new BN(lamports.toString()).toNumber() / LAMPORTS_PER_SOL;
