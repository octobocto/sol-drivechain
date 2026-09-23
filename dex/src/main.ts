// The page. It holds a key, it reads the chain, and it sends the transactions
// that the visitor asks for.
import {
  Connection,
  Keypair,
  LAMPORTS_PER_SOL,
  PublicKey,
  Transaction,
  sendAndConfirmTransaction,
} from "@solana/web3.js";
import { TOKEN_PROGRAM_ID } from "@solana/spl-token";
import { quoteSwapBaseIn } from "./cpmm.js";
import { Pool, loadPools, reservesFor } from "./pools.js";
import { buildNewPool, buildNewToken, buildSwap, poolCost } from "./actions.js";
import { formatAmount, parseAmount, poolPrice } from "./amounts.js";
import { exportKey, loadWallet, replaceWallet } from "./wallet.js";
import { NATIVE, Token, mergeTokens, readSavedTokens, saveToken, shortMint, tokensOfPools } from "./tokens.js";

interface Config {
  rpcUrl: string;
  wsUrl?: string;
  tokens: Token[];
}

const el = <T extends HTMLElement>(id: string): T => {
  const found = document.getElementById(id);
  if (!found) throw new Error(`the page has no element ${id}`);
  return found as T;
};

const store = window.localStorage;
const wallet: Keypair = loadWallet(store);
let connection: Connection;
let config: Config;
let tokens: Token[] = [];
let pools: Pool[] = [];
let balances = new Map<string, bigint>();

function say(id: string, message: string, kind: "" | "good" | "bad" = ""): void {
  const node = el(id);
  node.textContent = message;
  node.className = `status ${kind}`;
}

function tokenOf(mint: string): Token {
  const found = tokens.find((token) => token.mint === mint);
  if (!found) throw new Error(`the page does not know the token ${mint}`);
  return found;
}

function balanceOf(mint: string): bigint {
  return balances.get(mint) ?? 0n;
}

async function send(transaction: Transaction, signers: Keypair[] = []): Promise<string> {
  return sendAndConfirmTransaction(connection, transaction, [wallet, ...signers], {
    commitment: "confirmed",
    skipPreflight: false,
  });
}

async function readBalances(): Promise<void> {
  const next = new Map<string, bigint>();
  next.set(NATIVE.mint, BigInt(await connection.getBalance(wallet.publicKey)));
  const accounts = await connection.getParsedTokenAccountsByOwner(wallet.publicKey, {
    programId: TOKEN_PROGRAM_ID,
  });
  for (const { account } of accounts.value) {
    const info = account.data.parsed.info;
    const mint: string = info.mint;
    const amount = BigInt(info.tokenAmount.amount);
    if (mint === NATIVE.mint) continue;
    next.set(mint, (next.get(mint) ?? 0n) + amount);
  }
  balances = next;
}

function fillSelect(select: HTMLSelectElement, keep?: string): void {
  const chosen = keep ?? select.value;
  select.innerHTML = "";
  for (const token of tokens) {
    const option = document.createElement("option");
    option.value = token.mint;
    option.textContent = token.symbol;
    select.append(option);
  }
  if (chosen && tokens.some((token) => token.mint === chosen)) select.value = chosen;
}

function renderWallet(): void {
  el("wallet-address").textContent = wallet.publicKey.toBase58();
  el("wallet-balance").textContent = formatAmount(balanceOf(NATIVE.mint), 9, 6);
}

function renderPools(): void {
  const body = el<HTMLTableSectionElement>("pool-table").querySelector("tbody");
  if (!body) return;
  body.innerHTML = "";
  if (pools.length === 0) {
    say("pool-status", "the chain holds no pool yet. Make one below.");
    return;
  }
  say("pool-status", "");
  for (const pool of pools) {
    const token0 = tokens.find((token) => token.mint === pool.state.mint0.toBase58());
    const token1 = tokens.find((token) => token.mint === pool.state.mint1.toBase58());
    if (!token0 || !token1) continue;
    const row = document.createElement("tr");
    const price = poolPrice(pool.reserve0, token0.decimals, pool.reserve1, token1.decimals);
    row.innerHTML =
      `<td>${token0.symbol} / ${token1.symbol}</td>` +
      `<td>${formatAmount(pool.reserve0, token0.decimals, 4)} ${token0.symbol}` +
      ` · ${formatAmount(pool.reserve1, token1.decimals, 4)} ${token1.symbol}</td>` +
      `<td>1 ${token0.symbol} = ${price.toPrecision(6)} ${token1.symbol}</td>`;
    const cell = document.createElement("td");
    const button = document.createElement("button");
    button.className = "ghost";
    button.textContent = "trade";
    button.onclick = () => {
      fillSelect(el<HTMLSelectElement>("swap-from"), token0.mint);
      fillSelect(el<HTMLSelectElement>("swap-to"), token1.mint);
      void refreshQuote();
      el("swap-card").scrollIntoView({ behavior: "smooth" });
    };
    cell.append(button);
    row.append(cell);
    body.append(row);
  }
}

function renderSwapBalances(): void {
  const from = tokenOf(el<HTMLSelectElement>("swap-from").value);
  el("swap-from-balance").textContent =
    `you hold ${formatAmount(balanceOf(from.mint), from.decimals, 6)} ${from.symbol}`;
}

async function refreshQuote(): Promise<void> {
  const out = el<HTMLOutputElement>("swap-out");
  const fromMint = el<HTMLSelectElement>("swap-from").value;
  const toMint = el<HTMLSelectElement>("swap-to").value;
  renderSwapBalances();
  const text = el<HTMLInputElement>("swap-amount").value;
  if (!text) {
    out.textContent = "0";
    say("swap-detail", "");
    return;
  }
  try {
    const from = tokenOf(fromMint);
    const to = tokenOf(toMint);
    if (from.mint === to.mint) throw new Error("pick two different tokens");
    const amountIn = parseAmount(text, from.decimals);
    if (amountIn === 0n) throw new Error("pick an amount above zero");
    const pool = pools.find((item) => {
      const pair = [item.state.mint0.toBase58(), item.state.mint1.toBase58()];
      return pair.includes(from.mint) && pair.includes(to.mint);
    });
    if (!pool) throw new Error("no pool holds this pair");
    const { reserveIn, reserveOut } = reservesFor(pool, new PublicKey(from.mint));
    const quote = quoteSwapBaseIn(amountIn, reserveIn, reserveOut);
    out.textContent = formatAmount(quote.amountOut, to.decimals, 6);
    say(
      "swap-detail",
      `fee ${formatAmount(quote.feeAmount, from.decimals, 6)} ${from.symbol}` +
        ` · price impact ${(quote.priceImpact * 100).toFixed(2)}%`,
    );
  } catch (error) {
    out.textContent = "0";
    say("swap-detail", (error as Error).message, "bad");
  }
}

async function refreshChain(): Promise<void> {
  const known = mergeTokens([NATIVE], config.tokens, readSavedTokens(store));
  el("chain-status").textContent = "the page reads the pools…";
  const [slot, pooled] = await Promise.all([connection.getSlot(), loadPools(connection)]);
  pools = pooled;
  el("chain-status").textContent = "the page reads the tokens…";
  tokens = mergeTokens(known, await tokensOfPools(connection, pools, known));
  el("chain-status").textContent = "the page reads your balances…";
  await readBalances();
  el("chain-status").textContent = `slot ${slot} · ${pools.length} pools · ${tokens.length} tokens`;
  for (const id of ["swap-from", "swap-to", "pool-mint-a", "pool-mint-b"]) {
    fillSelect(el<HTMLSelectElement>(id));
  }
  for (const pair of [
    ["swap-from", "swap-to"],
    ["pool-mint-a", "pool-mint-b"],
  ]) {
    const first = el<HTMLSelectElement>(pair[0]);
    const second = el<HTMLSelectElement>(pair[1]);
    if (first.value === second.value && tokens.length > 1) {
      second.value = tokens.find((token) => token.mint !== first.value)?.mint ?? second.value;
    }
  }
  renderWallet();
  renderPools();
  await refreshQuote();
}

async function guard(button: HTMLButtonElement, statusId: string, work: () => Promise<void>): Promise<void> {
  button.disabled = true;
  try {
    await work();
  } catch (error) {
    say(statusId, (error as Error).message, "bad");
  } finally {
    button.disabled = false;
  }
}

function wireWallet(): void {
  el<HTMLButtonElement>("copy-address").onclick = async () => {
    await navigator.clipboard.writeText(wallet.publicKey.toBase58());
    say("wallet-status", "the page copied your address", "good");
  };
  el<HTMLButtonElement>("show-key").onclick = () => {
    say("wallet-status", exportKey(wallet));
  };
  el<HTMLButtonElement>("import-key").onclick = () => {
    const secret = window.prompt("Paste a secret key in base58. The page keeps it in this browser.");
    if (!secret) return;
    replaceWallet(store, secret);
    window.location.reload();
  };
  const airdrop = el<HTMLButtonElement>("airdrop");
  airdrop.onclick = () =>
    guard(airdrop, "wallet-status", async () => {
      say("wallet-status", "the faucet sends test BTC…");
      const signature = await connection.requestAirdrop(wallet.publicKey, LAMPORTS_PER_SOL);
      await connection.confirmTransaction(signature, "confirmed");
      await readBalances();
      renderWallet();
      say("wallet-status", "the faucet paid 1 BTC", "good");
    });
}

function wireSwap(): void {
  el<HTMLInputElement>("swap-amount").oninput = () => void refreshQuote();
  el<HTMLSelectElement>("swap-from").onchange = () => void refreshQuote();
  el<HTMLSelectElement>("swap-to").onchange = () => void refreshQuote();
  const swap = el<HTMLButtonElement>("swap");
  swap.onclick = () =>
    guard(swap, "swap-status", async () => {
      const from = tokenOf(el<HTMLSelectElement>("swap-from").value);
      const to = tokenOf(el<HTMLSelectElement>("swap-to").value);
      const amountIn = parseAmount(el<HTMLInputElement>("swap-amount").value, from.decimals);
      const slippage = Number(el<HTMLSelectElement>("swap-slippage").value);
      say("swap-status", "the page builds the trade…");
      const plan = await buildSwap(
        connection,
        wallet.publicKey,
        new PublicKey(from.mint),
        new PublicKey(to.mint),
        amountIn,
        slippage,
      );
      const signature = await send(plan.transaction);
      say(
        "swap-status",
        `the trade paid ${formatAmount(plan.quote.amountOut, to.decimals, 6)} ${to.symbol} · ${signature.slice(0, 12)}…`,
        "good",
      );
      await refreshChain();
    });
}

function wireMakeToken(): void {
  const make = el<HTMLButtonElement>("make-token");
  make.onclick = () =>
    guard(make, "token-status", async () => {
      const symbol = el<HTMLInputElement>("token-symbol").value.trim().toUpperCase();
      if (!/^[A-Z0-9]{2,10}$/.test(symbol)) throw new Error("a symbol holds 2 to 10 letters or digits");
      const decimals = Number(el<HTMLSelectElement>("token-decimals").value);
      const supply = parseAmount(el<HTMLInputElement>("token-supply").value || "1000000", decimals);
      say("token-status", "the page makes the token…");
      const token = await buildNewToken(connection, wallet.publicKey, decimals, supply);
      await send(token.transaction, [token.mint]);
      saveToken(store, { mint: token.mint.publicKey.toBase58(), symbol, decimals });
      say("token-status", `${symbol} is yours: ${token.mint.publicKey.toBase58()}`, "good");
      await refreshChain();
    });
}

function wireMakePool(): void {
  const make = el<HTMLButtonElement>("make-pool");
  make.onclick = () =>
    guard(make, "pool-make-status", async () => {
      const mintA = tokenOf(el<HTMLSelectElement>("pool-mint-a").value);
      const mintB = tokenOf(el<HTMLSelectElement>("pool-mint-b").value);
      const amountA = parseAmount(el<HTMLInputElement>("pool-amount-a").value, mintA.decimals);
      const amountB = parseAmount(el<HTMLInputElement>("pool-amount-b").value, mintB.decimals);
      const cost = await poolCost(connection);
      if (balanceOf(NATIVE.mint) < cost + amountA) {
        throw new Error(`a new pool costs about ${formatAmount(cost, 9, 4)} BTC. Ask the faucet first.`);
      }
      say("pool-make-status", "the page makes the pool…");
      const plan = buildNewPool(
        wallet.publicKey,
        new PublicKey(mintA.mint),
        amountA,
        new PublicKey(mintB.mint),
        amountB,
      );
      const signature = await send(plan.transaction);
      say("pool-make-status", `the pool trades now · ${signature.slice(0, 12)}…`, "good");
      await refreshChain();
    });
}

async function start(): Promise<void> {
  config = (await (await fetch("config.json", { cache: "no-store" })).json()) as Config;
  connection = new Connection(config.rpcUrl, {
    commitment: "confirmed",
    wsEndpoint: config.wsUrl,
  });
  el("endpoints").textContent = `RPC ${config.rpcUrl} · your address ${shortMint(wallet.publicKey.toBase58())}`;
  wireWallet();
  wireSwap();
  wireMakeToken();
  wireMakePool();
  await refreshChain();
  window.setInterval(() => void refreshChain().catch(() => undefined), 15_000);
}

start().catch((error: Error) => {
  el("chain-status").textContent = `the chain did not answer: ${error.message}`;
});
