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

/** Waits for the faucet to pay, because a fresh chain answers slowly at first. */
export async function fundFromFaucet(connection: Connection, owner: PublicKey, lamports: number): Promise<void> {
  const signature = await connection.requestAirdrop(owner, lamports);
  await connection.confirmTransaction(signature, "confirmed");
}
