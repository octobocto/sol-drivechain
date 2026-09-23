// Helpers that the operator scripts share.
import { readFileSync } from "node:fs";
import { Connection, Keypair, PublicKey, Signer, Transaction, sendAndConfirmTransaction } from "@solana/web3.js";
import { getAccount } from "@solana/spl-token";
import { ataFor } from "../src/actions.js";

export function readKeypair(path: string): Keypair {
  const bytes = JSON.parse(readFileSync(path, "utf8")) as number[];
  return Keypair.fromSecretKey(Uint8Array.from(bytes));
}

export function connect(url: string): Connection {
  return new Connection(url, "confirmed");
}

export async function sendTransaction(
  connection: Connection,
  transaction: Transaction,
  signers: Signer[],
): Promise<string> {
  return sendAndConfirmTransaction(connection, transaction, signers, {
    commitment: "confirmed",
    skipPreflight: false,
  });
}

export async function tokenBalance(connection: Connection, mint: PublicKey, owner: PublicKey): Promise<bigint> {
  try {
    return (await getAccount(connection, ataFor(mint, owner))).amount;
  } catch {
    return 0n;
  }
}

export function need(name: string, value: string | undefined): string {
  if (!value) throw new Error(`set ${name}`);
  return value;
}

/**
 * Asks the faucet until the owner holds `lamports`. A faucet caps one request,
 * so a big target takes several drips, and a refused drip gets smaller.
 */
export async function fundFromFaucet(connection: Connection, owner: PublicKey, lamports: number): Promise<void> {
  let drip = lamports;
  for (let attempt = 0; attempt < 20; attempt += 1) {
    if ((await connection.getBalance(owner)) >= lamports) return;
    try {
      const signature = await connection.requestAirdrop(owner, drip);
      await connection.confirmTransaction(signature, "confirmed");
    } catch (error) {
      drip = Math.floor(drip / 2);
      if (drip < 100_000_000) throw new Error(`the faucet refused a drip: ${fullMessage(error)}`);
    }
  }
  throw new Error(`the faucet did not pay ${lamports} lamports to ${owner.toBase58()}`);
}

/** The message of an error and of every cause under it. */
export function fullMessage(error: unknown): string {
  const parts: string[] = [];
  let current: unknown = error;
  while (current instanceof Error) {
    parts.push(current.message);
    current = (current as Error & { cause?: unknown }).cause;
  }
  return parts.join(" <- ");
}
