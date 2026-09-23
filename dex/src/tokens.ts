// The token list of the page. It joins the tokens of the deployment, the
// tokens that the visitor made, and every mint that a pool holds.
import { Connection, PublicKey } from "@solana/web3.js";
import { getMint } from "@solana/spl-token";
import { WSOL_MINT } from "./cpmm.js";
import { Pool } from "./pools.js";
import { KeyStore } from "./wallet.js";

const STORAGE_KEY = "sol-drivechain-dex.tokens";

export interface Token {
  mint: string;
  symbol: string;
  decimals: number;
}

/** The native coin of the chain. It is pegged BTC, and it wraps into a token. */
export const NATIVE: Token = { mint: WSOL_MINT.toBase58(), symbol: "BTC", decimals: 9 };

export function shortMint(mint: string): string {
  return `${mint.slice(0, 4)}…${mint.slice(-4)}`;
}

export function readSavedTokens(store: KeyStore): Token[] {
  const saved = store.getItem(STORAGE_KEY);
  if (!saved) return [];
  try {
    const parsed: unknown = JSON.parse(saved);
    if (!Array.isArray(parsed)) return [];
    return parsed.filter(
      (item): item is Token =>
        typeof item === "object" && item !== null &&
        typeof (item as Token).mint === "string" &&
        typeof (item as Token).symbol === "string" &&
        typeof (item as Token).decimals === "number",
    );
  } catch {
    return [];
  }
}

export function saveToken(store: KeyStore, token: Token): Token[] {
  const tokens = readSavedTokens(store).filter((item) => item.mint !== token.mint);
  tokens.push(token);
  store.setItem(STORAGE_KEY, JSON.stringify(tokens));
  return tokens;
}

/** Joins the lists and keeps one entry per mint. The first name wins. */
export function mergeTokens(...lists: Token[][]): Token[] {
  const byMint = new Map<string, Token>();
  for (const list of lists) {
    for (const token of list) {
      if (!byMint.has(token.mint)) byMint.set(token.mint, token);
    }
  }
  return [...byMint.values()];
}

/** Names every mint of the pools that no list names. */
export async function tokensOfPools(connection: Connection, pools: Pool[], known: Token[]): Promise<Token[]> {
  const names = new Set(known.map((token) => token.mint));
  const missing = new Set<string>();
  for (const pool of pools) {
    for (const mint of [pool.state.mint0, pool.state.mint1]) {
      const address = mint.toBase58();
      if (!names.has(address)) missing.add(address);
    }
  }
  const found: Token[] = [];
  for (const address of missing) {
    const mint = await getMint(connection, new PublicKey(address));
    found.push({ mint: address, symbol: shortMint(address), decimals: mint.decimals });
  }
  return found;
}
