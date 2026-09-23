// Proves the DEX against a live chain: a token, a pool, a trade out, and a
// trade back. Every step checks the chain, so a wrong account order or a wrong
// curve fails here.
//
//   RPC_URL=... node dist/scripts/trade.mjs
//
// The script takes its money from the faucet of the chain. KEYPAIR pays when
// the chain runs no faucet.
import { Keypair, LAMPORTS_PER_SOL } from "@solana/web3.js";
import { WSOL_MINT, minimumOut, quoteSwapBaseIn } from "../src/cpmm.js";
import { buildNewPool, buildNewToken, buildSwap } from "../src/actions.js";
import { loadPool, loadPools, reservesFor } from "../src/pools.js";
import { fullMessage, connect, fundFromFaucet, need, readKeypair, sendTransaction, tokenBalance } from "./common.js";

function check(claim: boolean, message: string): void {
  if (!claim) throw new Error(message);
  console.log(`  ok: ${message}`);
}

async function main(): Promise<void> {
  const connection = connect(need("RPC_URL", process.env.RPC_URL));
  const payer = process.env.KEYPAIR ? readKeypair(process.env.KEYPAIR) : Keypair.generate();
  console.log(`the trader is ${payer.publicKey.toBase58()}`);

  if (!process.env.KEYPAIR) {
    await fundFromFaucet(connection, payer.publicKey, 5 * LAMPORTS_PER_SOL);
  }
  const start = await connection.getBalance(payer.publicKey);
  check(start > LAMPORTS_PER_SOL, `the trader holds ${start / LAMPORTS_PER_SOL} BTC`);

  console.log("makes a token");
  const supply = 1_000_000_000_000n;
  const token = await buildNewToken(connection, payer.publicKey, 6, supply);
  await sendTransaction(connection, token.transaction, [payer, token.mint]);
  const mint = token.mint.publicKey;
  check((await tokenBalance(connection, mint, payer.publicKey)) === supply, "the whole supply is on the trader");

  console.log("makes a pool");
  const poolLamports = BigInt(LAMPORTS_PER_SOL);
  const poolTokens = 100_000_000_000n;
  const plan = buildNewPool(payer.publicKey, WSOL_MINT, poolLamports, mint, poolTokens);
  await sendTransaction(connection, plan.transaction, [payer]);
  const pool = await loadPool(connection, WSOL_MINT, mint);
  check(pool !== null, "the pool answers on the chain");
  if (!pool) return;
  const sideA = pool.state.mint0.equals(WSOL_MINT) ? pool.reserve0 : pool.reserve1;
  const sideB = pool.state.mint0.equals(WSOL_MINT) ? pool.reserve1 : pool.reserve0;
  check(sideA === poolLamports, `the pool holds ${poolLamports} lamports of pegged BTC`);
  check(sideB === poolTokens, `the pool holds ${poolTokens} token units`);
  check((await loadPools(connection)).some((item) => item.address.equals(pool.address)), "the pool list holds it");

  console.log("trades BTC for the token");
  const amountIn = BigInt(LAMPORTS_PER_SOL) / 10n;
  const before = await tokenBalance(connection, mint, payer.publicKey);
  const swap = await buildSwap(connection, payer.publicKey, WSOL_MINT, mint, amountIn, 1);
  await sendTransaction(connection, swap.transaction, [payer]);
  const after = await tokenBalance(connection, mint, payer.publicKey);
  check(
    after - before === swap.quote.amountOut,
    `the chain paid ${after - before} units, and the quote said ${swap.quote.amountOut}`,
  );
  check(swap.quote.amountOut >= minimumOut(swap.quote.amountOut, 1), "the output clears the slippage limit");

  console.log("trades the token back for BTC");
  const lamportsBefore = await connection.getBalance(payer.publicKey);
  const backIn = (after - before) / 2n;
  const back = await buildSwap(connection, payer.publicKey, mint, WSOL_MINT, backIn, 1);
  await sendTransaction(connection, back.transaction, [payer]);
  const lamportsAfter = await connection.getBalance(payer.publicKey);
  check(lamportsAfter > lamportsBefore, `the trader got ${lamportsAfter - lamportsBefore} lamports back`);

  const moved = await loadPool(connection, WSOL_MINT, mint);
  if (!moved) throw new Error("the pool left the chain");
  const movedA = moved.state.mint0.equals(WSOL_MINT) ? moved.reserve0 : moved.reserve1;
  check(movedA > poolLamports, "the pool keeps the fees of the two trades");

  const recheck = reservesFor(moved, WSOL_MINT);
  const nextQuote = quoteSwapBaseIn(amountIn, recheck.reserveIn, recheck.reserveOut);
  check(nextQuote.amountOut > 0n, "the pool quotes the next trade");

  console.log("the DEX works");
}

main().catch((error: unknown) => {
  console.error(`the trade proof failed: ${fullMessage(error)}`);
  process.exit(1);
});
