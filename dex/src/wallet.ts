// The wallet of the page. The key stays in the browser, and the page never
// sends it anywhere. The chain is a test chain, so a key in the browser is
// safe enough for a visitor who wants to trade at once.
import { Keypair } from "@solana/web3.js";
import bs58 from "bs58";

const STORAGE_KEY = "sol-drivechain-dex.key";

export interface KeyStore {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
  removeItem(key: string): void;
}

export function importKey(secret: string): Keypair {
  const bytes = bs58.decode(secret.trim());
  if (bytes.length !== 64) throw new Error("a secret key holds 64 bytes in base58");
  return Keypair.fromSecretKey(bytes);
}

export function exportKey(keypair: Keypair): string {
  return bs58.encode(keypair.secretKey);
}

/** Reads the key of the browser, or makes one and keeps it. */
export function loadWallet(store: KeyStore): Keypair {
  const saved = store.getItem(STORAGE_KEY);
  if (saved) {
    try {
      return importKey(saved);
    } catch {
      store.removeItem(STORAGE_KEY);
    }
  }
  const keypair = Keypair.generate();
  store.setItem(STORAGE_KEY, exportKey(keypair));
  return keypair;
}

export function replaceWallet(store: KeyStore, secret: string): Keypair {
  const keypair = importKey(secret);
  store.setItem(STORAGE_KEY, exportKey(keypair));
  return keypair;
}

export function forgetWallet(store: KeyStore): void {
  store.removeItem(STORAGE_KEY);
}
