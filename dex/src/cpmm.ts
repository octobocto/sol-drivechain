// The client of the Raydium CP-Swap program. The chain carries the mainnet
// binary at its mainnet address, so these keys, discriminators, and account
// orders are the mainnet ones.
import { PublicKey, SystemProgram, SYSVAR_RENT_PUBKEY, TransactionInstruction } from "@solana/web3.js";
import { ASSOCIATED_TOKEN_PROGRAM_ID, TOKEN_PROGRAM_ID } from "@solana/spl-token";

export const CPMM_PROGRAM_ID = new PublicKey("CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C");
export const CPMM_CONFIG_ID = new PublicKey("D4FPEruKEHrG5TenZ2mpDGEfu1iUvTiqBxvpU8HLBvC2");
export const CPMM_CREATE_FEE_ID = new PublicKey("DNXgeM9EiiaAbaWvwjHj9fQQLAX5ZsfHyvmYUNRAdNC8");
export const WSOL_MINT = new PublicKey("So11111111111111111111111111111111111111112");

/** The fee of the config at index 0, in parts of a million. 2500 is 0.25%. */
export const TRADE_FEE_RATE = 2500n;
export const FEE_DENOMINATOR = 1_000_000n;
/** CP-Swap takes this many lamports for each new pool. */
export const CREATE_POOL_FEE = 150_000_000n;

const AUTH_SEED = Buffer.from("vault_and_lp_mint_auth_seed", "utf8");
const POOL_SEED = Buffer.from("pool", "utf8");
const POOL_LP_MINT_SEED = Buffer.from("pool_lp_mint", "utf8");
const POOL_VAULT_SEED = Buffer.from("pool_vault", "utf8");
const OBSERVATION_SEED = Buffer.from("observation", "utf8");

const INITIALIZE = Buffer.from([175, 175, 109, 31, 13, 152, 155, 237]);
const SWAP_BASE_INPUT = Buffer.from([143, 190, 90, 218, 196, 30, 51, 222]);

export const POOL_STATE_LEN = 637;

function pda(seeds: Buffer[]): PublicKey {
  return PublicKey.findProgramAddressSync(seeds, CPMM_PROGRAM_ID)[0];
}

export function poolAuthority(): PublicKey {
  return pda([AUTH_SEED]);
}

/** CP-Swap holds the two mints of a pool in byte order. */
export function sortMints(a: PublicKey, b: PublicKey): [PublicKey, PublicKey] {
  return Buffer.compare(a.toBuffer(), b.toBuffer()) < 0 ? [a, b] : [b, a];
}

export function poolId(mint0: PublicKey, mint1: PublicKey): PublicKey {
  return pda([POOL_SEED, CPMM_CONFIG_ID.toBuffer(), mint0.toBuffer(), mint1.toBuffer()]);
}

export function poolLpMint(pool: PublicKey): PublicKey {
  return pda([POOL_LP_MINT_SEED, pool.toBuffer()]);
}

export function poolVault(pool: PublicKey, mint: PublicKey): PublicKey {
  return pda([POOL_VAULT_SEED, pool.toBuffer(), mint.toBuffer()]);
}

export function poolObservation(pool: PublicKey): PublicKey {
  return pda([OBSERVATION_SEED, pool.toBuffer()]);
}

export interface PoolState {
  configId: PublicKey;
  vault0: PublicKey;
  vault1: PublicKey;
  mintLp: PublicKey;
  mint0: PublicKey;
  mint1: PublicKey;
  program0: PublicKey;
  program1: PublicKey;
  observationId: PublicKey;
  status: number;
  lpDecimals: number;
  decimals0: number;
  decimals1: number;
  lpAmount: bigint;
  protocolFees0: bigint;
  protocolFees1: bigint;
  fundFees0: bigint;
  fundFees1: bigint;
  openTime: bigint;
  /** A pool of the `initialize` call keeps this false, and takes no creator fee. */
  enableCreatorFee: boolean;
  creatorFees0: bigint;
  creatorFees1: bigint;
}

export function decodePoolState(data: Uint8Array): PoolState {
  if (data.length < POOL_STATE_LEN) {
    throw new Error(`a pool account holds ${data.length} bytes, and the state takes ${POOL_STATE_LEN}`);
  }
  const view = new DataView(data.buffer, data.byteOffset, data.byteLength);
  const key = (offset: number) => new PublicKey(data.subarray(offset, offset + 32));
  const u64 = (offset: number) => view.getBigUint64(offset, true);
  return {
    configId: key(8),
    vault0: key(72),
    vault1: key(104),
    mintLp: key(136),
    mint0: key(168),
    mint1: key(200),
    program0: key(232),
    program1: key(264),
    observationId: key(296),
    status: data[329],
    lpDecimals: data[330],
    decimals0: data[331],
    decimals1: data[332],
    lpAmount: u64(333),
    protocolFees0: u64(341),
    protocolFees1: u64(349),
    fundFees0: u64(357),
    fundFees1: u64(365),
    openTime: u64(373),
    enableCreatorFee: data[390] !== 0,
    creatorFees0: u64(397),
    creatorFees1: u64(405),
  };
}

/**
 * The tradable balance of one side. The vault also holds the fees that nobody
 * collected yet, and those are not part of the curve.
 */
export function reserve(vaultAmount: bigint, protocolFees: bigint, fundFees: bigint, creatorFees: bigint): bigint {
  const fees = protocolFees + fundFees + creatorFees;
  return vaultAmount > fees ? vaultAmount - fees : 0n;
}

function ceilDiv(amount: bigint, numerator: bigint, denominator: bigint): bigint {
  if (amount === 0n) return 0n;
  return (amount * numerator + denominator - 1n) / denominator;
}

export interface Quote {
  amountIn: bigint;
  feeAmount: bigint;
  amountOut: bigint;
  /** The part of the price that the trade itself moves, as a fraction of one. */
  priceImpact: number;
}

/** The output of a trade that names its input. It is the on-chain curve. */
export function quoteSwapBaseIn(
  amountIn: bigint,
  reserveIn: bigint,
  reserveOut: bigint,
  tradeFeeRate: bigint = TRADE_FEE_RATE,
): Quote {
  if (amountIn <= 0n) throw new Error("a trade moves more than zero");
  if (reserveIn <= 0n || reserveOut <= 0n) throw new Error("the pool holds no liquidity");
  const feeAmount = ceilDiv(amountIn, tradeFeeRate, FEE_DENOMINATOR);
  const amountInAfterFee = amountIn - feeAmount;
  const amountOut = (reserveOut * amountInAfterFee) / (reserveIn + amountInAfterFee);
  const before = Number(reserveOut) / Number(reserveIn);
  const after = Number(amountOut) / Number(amountInAfterFee);
  const priceImpact = before === 0 ? 0 : Math.max(0, (before - after) / before);
  return { amountIn, feeAmount, amountOut, priceImpact };
}

/** The smallest output that a trade accepts, after the slippage of the user. */
export function minimumOut(amountOut: bigint, slippagePercent: number): bigint {
  if (slippagePercent < 0 || slippagePercent >= 100) {
    throw new Error("the slippage stays between 0 and 100 percent");
  }
  const keep = BigInt(Math.round((100 - slippagePercent) * 100));
  return (amountOut * keep) / 10_000n;
}

function u64Bytes(value: bigint): Buffer {
  if (value < 0n || value > 0xffffffffffffffffn) throw new Error(`${value} does not fit in a u64`);
  const out = Buffer.alloc(8);
  out.writeBigUInt64LE(value);
  return out;
}

export interface CreatePoolAccounts {
  creator: PublicKey;
  mint0: PublicKey;
  mint1: PublicKey;
  creatorToken0: PublicKey;
  creatorToken1: PublicKey;
  creatorLpToken: PublicKey;
}

/** Makes a pool and moves the first liquidity into it. */
export function createPoolInstruction(
  accounts: CreatePoolAccounts,
  amount0: bigint,
  amount1: bigint,
  openTime: bigint = 0n,
): TransactionInstruction {
  const pool = poolId(accounts.mint0, accounts.mint1);
  const keys = [
    { pubkey: accounts.creator, isSigner: true, isWritable: true },
    { pubkey: CPMM_CONFIG_ID, isSigner: false, isWritable: false },
    { pubkey: poolAuthority(), isSigner: false, isWritable: false },
    { pubkey: pool, isSigner: false, isWritable: true },
    { pubkey: accounts.mint0, isSigner: false, isWritable: false },
    { pubkey: accounts.mint1, isSigner: false, isWritable: false },
    { pubkey: poolLpMint(pool), isSigner: false, isWritable: true },
    { pubkey: accounts.creatorToken0, isSigner: false, isWritable: true },
    { pubkey: accounts.creatorToken1, isSigner: false, isWritable: true },
    { pubkey: accounts.creatorLpToken, isSigner: false, isWritable: true },
    { pubkey: poolVault(pool, accounts.mint0), isSigner: false, isWritable: true },
    { pubkey: poolVault(pool, accounts.mint1), isSigner: false, isWritable: true },
    { pubkey: CPMM_CREATE_FEE_ID, isSigner: false, isWritable: true },
    { pubkey: poolObservation(pool), isSigner: false, isWritable: true },
    { pubkey: TOKEN_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: TOKEN_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: TOKEN_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: ASSOCIATED_TOKEN_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: SystemProgram.programId, isSigner: false, isWritable: false },
    { pubkey: SYSVAR_RENT_PUBKEY, isSigner: false, isWritable: false },
  ];
  return new TransactionInstruction({
    programId: CPMM_PROGRAM_ID,
    keys,
    data: Buffer.concat([INITIALIZE, u64Bytes(amount0), u64Bytes(amount1), u64Bytes(openTime)]),
  });
}

export interface SwapAccounts {
  payer: PublicKey;
  pool: PublicKey;
  inputMint: PublicKey;
  outputMint: PublicKey;
  userInput: PublicKey;
  userOutput: PublicKey;
}

/** Trades a known input amount for at least `minimumAmountOut`. */
export function swapBaseInInstruction(
  accounts: SwapAccounts,
  amountIn: bigint,
  minimumAmountOut: bigint,
): TransactionInstruction {
  const keys = [
    { pubkey: accounts.payer, isSigner: true, isWritable: false },
    { pubkey: poolAuthority(), isSigner: false, isWritable: false },
    { pubkey: CPMM_CONFIG_ID, isSigner: false, isWritable: false },
    { pubkey: accounts.pool, isSigner: false, isWritable: true },
    { pubkey: accounts.userInput, isSigner: false, isWritable: true },
    { pubkey: accounts.userOutput, isSigner: false, isWritable: true },
    { pubkey: poolVault(accounts.pool, accounts.inputMint), isSigner: false, isWritable: true },
    { pubkey: poolVault(accounts.pool, accounts.outputMint), isSigner: false, isWritable: true },
    { pubkey: TOKEN_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: TOKEN_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: accounts.inputMint, isSigner: false, isWritable: false },
    { pubkey: accounts.outputMint, isSigner: false, isWritable: false },
    { pubkey: poolObservation(accounts.pool), isSigner: false, isWritable: true },
  ];
  return new TransactionInstruction({
    programId: CPMM_PROGRAM_ID,
    keys,
    data: Buffer.concat([SWAP_BASE_INPUT, u64Bytes(amountIn), u64Bytes(minimumAmountOut)]),
  });
}
