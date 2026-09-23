import { describe, expect, it } from "vitest";
import { Keypair } from "@solana/web3.js";
import { KeyStore, exportKey, forgetWallet, importKey, loadWallet, replaceWallet } from "../src/wallet.js";

function memoryStore(start: Record<string, string> = {}): KeyStore {
  const values = new Map(Object.entries(start));
  return {
    getItem: (key) => values.get(key) ?? null,
    setItem: (key, value) => void values.set(key, value),
    removeItem: (key) => void values.delete(key),
  };
}

describe("the wallet", () => {
  it("makes a key one time and keeps it", () => {
    const store = memoryStore();
    const first = loadWallet(store);
    const second = loadWallet(store);
    expect(second.publicKey.toBase58()).toBe(first.publicKey.toBase58());
  });

  it("makes a new key when the saved one is broken", () => {
    const store = memoryStore({ "sol-drivechain-dex.key": "not a key" });
    const wallet = loadWallet(store);
    expect(wallet.publicKey.toBase58()).toHaveLength(44);
    expect(loadWallet(store).publicKey.toBase58()).toBe(wallet.publicKey.toBase58());
  });

  it("writes a key out and reads it back", () => {
    const keypair = Keypair.generate();
    expect(importKey(exportKey(keypair)).publicKey.toBase58()).toBe(keypair.publicKey.toBase58());
    expect(importKey(` ${exportKey(keypair)} `).publicKey.toBase58()).toBe(keypair.publicKey.toBase58());
  });

  it("refuses a secret of the wrong length", () => {
    expect(() => importKey(exportKey(Keypair.generate()).slice(0, 10))).toThrow();
  });

  it("takes a key from the visitor, and forgets a key", () => {
    const store = memoryStore();
    const first = loadWallet(store);
    const other = Keypair.generate();
    expect(replaceWallet(store, exportKey(other)).publicKey.toBase58()).toBe(other.publicKey.toBase58());
    expect(loadWallet(store).publicKey.toBase58()).toBe(other.publicKey.toBase58());
    forgetWallet(store);
    expect(loadWallet(store).publicKey.toBase58()).not.toBe(first.publicKey.toBase58());
  });
});
