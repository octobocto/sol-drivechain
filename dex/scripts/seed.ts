// Makes the demo tokens and their pools, and writes the token list of the page.
//
//   RPC_URL=... KEYPAIR=/path/to/id.json node dist/scripts/seed.mjs
//
// The script pays from KEYPAIR. That key holds pegged BTC, which comes from a
// deposit on eCash.
import { writeFileSync } from "node:fs";
import { LAMPORTS_PER_SOL } from "@solana/web3.js";
import { WSOL_MINT } from "../src/cpmm.js";
import { buildNewPool, buildNewToken } from "../src/actions.js";
import { loadPool } from "../src/pools.js";
import { fullMessage, connect, need, readKeypair, sendTransaction } from "./common.js";

interface Plan {
  symbol: string;
  decimals: number;
  supply: bigint;
  /** The lamports of pegged BTC that the pool takes. */
  poolLamports: bigint;
  /** The tokens that the pool takes, in base units. */
  poolTokens: bigint;
}

const PLANS: Plan[] = [
  {
    symbol: "USDX",
    decimals: 6,
    supply: 10_000_000_000_000n,
    poolLamports: 2n * BigInt(LAMPORTS_PER_SOL),
    poolTokens: 200_000_000_000n,
  },
  {
    symbol: "PEPE",
    decimals: 6,
    supply: 420_000_000_000_000n,
    poolLamports: BigInt(LAMPORTS_PER_SOL),
    poolTokens: 69_000_000_000n,
  },
  {
    symbol: "GOLD",
    decimals: 9,
    supply: 1_000_000_000_000_000n,
    poolLamports: BigInt(LAMPORTS_PER_SOL) / 2n,
    poolTokens: 25_000_000_000_000n,
  },
];

async function main(): Promise<void> {
  const connection = connect(need("RPC_URL", process.env.RPC_URL));
  const payer = readKeypair(need("KEYPAIR", process.env.KEYPAIR));
  const out = process.env.TOKENS_OUT;

  const balance = await connection.getBalance(payer.publicKey);
  console.log(`the payer is ${payer.publicKey.toBase58()} with ${balance / LAMPORTS_PER_SOL} BTC`);
  const tokens: Array<{ mint: string; symbol: string; decimals: number }> = [];

  for (const plan of PLANS) {
    const token = await buildNewToken(connection, payer.publicKey, plan.decimals, plan.supply);
    await sendTransaction(connection, token.transaction, [payer, token.mint]);
    console.log(`${plan.symbol} is ${token.mint.publicKey.toBase58()}`);
    tokens.push({ mint: token.mint.publicKey.toBase58(), symbol: plan.symbol, decimals: plan.decimals });

    const pool = buildNewPool(
      payer.publicKey,
      WSOL_MINT,
      plan.poolLamports,
      token.mint.publicKey,
      plan.poolTokens,
    );
    await sendTransaction(connection, pool.transaction, [payer]);
    const live = await loadPool(connection, WSOL_MINT, token.mint.publicKey);
    if (!live) throw new Error(`the pool of ${plan.symbol} did not appear`);
    console.log(`the BTC/${plan.symbol} pool is ${pool.pool.toBase58()}`);
  }

  if (out) {
    writeFileSync(out, `${JSON.stringify(tokens, null, 2)}\n`);
    console.log(`the token list is in ${out}`);
  }
}

main().catch((error: unknown) => {
  console.error(`the seed failed: ${fullMessage(error)}`);
  process.exit(1);
});
