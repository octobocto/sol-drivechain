// Builds the transactions of the page: a swap, a new token, and a new pool.
import {
  Connection,
  Keypair,
  PublicKey,
  SystemProgram,
  Transaction,
  TransactionInstruction,
} from "@solana/web3.js";
import {
  ACCOUNT_SIZE,
  MINT_SIZE,
  TOKEN_PROGRAM_ID,
  createAssociatedTokenAccountIdempotentInstruction,
  createCloseAccountInstruction,
  createInitializeMint2Instruction,
  createMintToInstruction,
  createSyncNativeInstruction,
  getAssociatedTokenAddressSync,
  getMinimumBalanceForRentExemptMint,
} from "@solana/spl-token";
import {
  CREATE_POOL_FEE,
  Quote,
  WSOL_MINT,
  createPoolInstruction,
  minimumOut,
  poolLpMint,
  poolId,
  quoteSwapBaseIn,
  sortMints,
  swapBaseInInstruction,
} from "./cpmm.js";
import { Pool, loadPool, reservesFor } from "./pools.js";

export function ataFor(mint: PublicKey, owner: PublicKey): PublicKey {
  return getAssociatedTokenAddressSync(mint, owner, true);
}

function wrapInstructions(owner: PublicKey, account: PublicKey, lamports: bigint): TransactionInstruction[] {
  return [
    createAssociatedTokenAccountIdempotentInstruction(owner, account, owner, WSOL_MINT),
    SystemProgram.transfer({ fromPubkey: owner, toPubkey: account, lamports }),
    createSyncNativeInstruction(account),
  ];
}

export interface SwapPlan {
  transaction: Transaction;
  quote: Quote;
  pool: Pool;
}

/**
 * Trades `amountIn` of the input mint for the output mint. The native coin
 * goes through a wrapped account, which the transaction opens and closes.
 */
export async function buildSwap(
  connection: Connection,
  owner: PublicKey,
  inputMint: PublicKey,
  outputMint: PublicKey,
  amountIn: bigint,
  slippagePercent: number,
): Promise<SwapPlan> {
  const pool = await loadPool(connection, inputMint, outputMint);
  if (!pool) throw new Error("no pool holds this pair");
  if (pool.state.enableCreatorFee) throw new Error("this pool takes a creator fee, and the page cannot quote it");
  const { reserveIn, reserveOut } = reservesFor(pool, inputMint);
  const quote = quoteSwapBaseIn(amountIn, reserveIn, reserveOut);
  const userInput = ataFor(inputMint, owner);
  const userOutput = ataFor(outputMint, owner);

  const instructions: TransactionInstruction[] = [];
  if (inputMint.equals(WSOL_MINT)) {
    instructions.push(...wrapInstructions(owner, userInput, amountIn));
  } else {
    instructions.push(createAssociatedTokenAccountIdempotentInstruction(owner, userInput, owner, inputMint));
  }
  instructions.push(createAssociatedTokenAccountIdempotentInstruction(owner, userOutput, owner, outputMint));
  instructions.push(
    swapBaseInInstruction(
      { payer: owner, pool: pool.address, inputMint, outputMint, userInput, userOutput },
      amountIn,
      minimumOut(quote.amountOut, slippagePercent),
    ),
  );
  // A wrapped account holds lamports that the owner cannot spend. The close
  // gives them back in the same transaction.
  if (inputMint.equals(WSOL_MINT)) {
    instructions.push(createCloseAccountInstruction(userInput, owner, owner));
  }
  if (outputMint.equals(WSOL_MINT)) {
    instructions.push(createCloseAccountInstruction(userOutput, owner, owner));
  }
  return { transaction: new Transaction().add(...instructions), quote, pool };
}

export interface NewToken {
  transaction: Transaction;
  mint: Keypair;
  account: PublicKey;
}

/** Makes a token and mints the whole supply to the owner. */
export async function buildNewToken(
  connection: Connection,
  owner: PublicKey,
  decimals: number,
  supply: bigint,
): Promise<NewToken> {
  if (decimals < 0 || decimals > 9) throw new Error("the decimals stay between 0 and 9");
  if (supply <= 0n) throw new Error("a token starts with more than zero");
  const mint = Keypair.generate();
  const account = ataFor(mint.publicKey, owner);
  const rent = await getMinimumBalanceForRentExemptMint(connection);
  const transaction = new Transaction().add(
    SystemProgram.createAccount({
      fromPubkey: owner,
      newAccountPubkey: mint.publicKey,
      lamports: rent,
      space: MINT_SIZE,
      programId: TOKEN_PROGRAM_ID,
    }),
    createInitializeMint2Instruction(mint.publicKey, decimals, owner, null),
    createAssociatedTokenAccountIdempotentInstruction(owner, account, owner, mint.publicKey),
    createMintToInstruction(mint.publicKey, account, owner, supply),
  );
  return { transaction, mint, account };
}

export interface NewPool {
  transaction: Transaction;
  pool: PublicKey;
  lpMint: PublicKey;
}

/**
 * Makes a pool of two mints and moves the first liquidity into it. The price
 * of the pool starts at the ratio of the two amounts.
 */
export function buildNewPool(
  owner: PublicKey,
  mintA: PublicKey,
  amountA: bigint,
  mintB: PublicKey,
  amountB: bigint,
): NewPool {
  if (mintA.equals(mintB)) throw new Error("a pool holds two different tokens");
  if (amountA <= 0n || amountB <= 0n) throw new Error("a pool starts with more than zero of each token");
  const [mint0, mint1] = sortMints(mintA, mintB);
  const [amount0, amount1] = mint0.equals(mintA) ? [amountA, amountB] : [amountB, amountA];
  const pool = poolId(mint0, mint1);
  const token0 = ataFor(mint0, owner);
  const token1 = ataFor(mint1, owner);

  const instructions: TransactionInstruction[] = [];
  if (mint0.equals(WSOL_MINT)) instructions.push(...wrapInstructions(owner, token0, amount0));
  if (mint1.equals(WSOL_MINT)) instructions.push(...wrapInstructions(owner, token1, amount1));
  instructions.push(
    createPoolInstruction(
      {
        creator: owner,
        mint0,
        mint1,
        creatorToken0: token0,
        creatorToken1: token1,
        creatorLpToken: ataFor(poolLpMint(pool), owner),
      },
      amount0,
      amount1,
    ),
  );
  if (mint0.equals(WSOL_MINT)) instructions.push(createCloseAccountInstruction(token0, owner, owner));
  if (mint1.equals(WSOL_MINT)) instructions.push(createCloseAccountInstruction(token1, owner, owner));
  return { transaction: new Transaction().add(...instructions), pool, lpMint: poolLpMint(pool) };
}

/** The lamports that a new pool costs, beside the liquidity itself. */
export async function poolCost(connection: Connection): Promise<bigint> {
  const accountRent = await connection.getMinimumBalanceForRentExemption(ACCOUNT_SIZE);
  // The program opens two vaults, the LP mint, the pool state, and the
  // observation account, and it pays the fixed fee.
  return CREATE_POOL_FEE + BigInt(accountRent) * 3n;
}
