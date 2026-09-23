// Reads the CP-Swap pools of the chain.
import { Connection, PublicKey } from "@solana/web3.js";
import { AccountLayout } from "@solana/spl-token";
import {
  CPMM_CONFIG_ID,
  CPMM_PROGRAM_ID,
  POOL_STATE_LEN,
  PoolState,
  decodePoolState,
  poolId,
  reserve,
  sortMints,
} from "./cpmm.js";

export interface Pool {
  address: PublicKey;
  state: PoolState;
  reserve0: bigint;
  reserve1: bigint;
}

function tokenAmount(data: Uint8Array): bigint {
  return AccountLayout.decode(data).amount;
}

function withReserves(address: PublicKey, state: PoolState, vault0: Uint8Array, vault1: Uint8Array): Pool {
  return {
    address,
    state,
    reserve0: reserve(tokenAmount(vault0), state.protocolFees0, state.fundFees0, state.creatorFees0),
    reserve1: reserve(tokenAmount(vault1), state.protocolFees1, state.fundFees1, state.creatorFees1),
  };
}

/** Reads one pool of two mints. It returns null when the pool does not exist. */
export async function loadPool(connection: Connection, mintA: PublicKey, mintB: PublicKey): Promise<Pool | null> {
  const [mint0, mint1] = sortMints(mintA, mintB);
  const address = poolId(mint0, mint1);
  const account = await connection.getAccountInfo(address);
  if (!account) return null;
  const state = decodePoolState(account.data);
  const vaults = await connection.getMultipleAccountsInfo([state.vault0, state.vault1]);
  if (!vaults[0] || !vaults[1]) throw new Error(`the pool ${address.toBase58()} lost a vault`);
  return withReserves(address, state, vaults[0].data, vaults[1].data);
}

/** Reads every pool of the chain. The chain is small, so it reads them all. */
export async function loadPools(connection: Connection): Promise<Pool[]> {
  const accounts = await connection.getProgramAccounts(CPMM_PROGRAM_ID, {
    filters: [
      { dataSize: POOL_STATE_LEN },
      { memcmp: { offset: 8, bytes: CPMM_CONFIG_ID.toBase58() } },
    ],
  });
  const states = accounts.map(({ pubkey, account }) => ({ pubkey, state: decodePoolState(account.data) }));
  if (states.length === 0) return [];
  const vaultKeys = states.flatMap(({ state }) => [state.vault0, state.vault1]);
  const vaults = await connection.getMultipleAccountsInfo(vaultKeys);
  return states.map(({ pubkey, state }, index) => {
    const vault0 = vaults[index * 2];
    const vault1 = vaults[index * 2 + 1];
    if (!vault0 || !vault1) throw new Error(`the pool ${pubkey.toBase58()} lost a vault`);
    return withReserves(pubkey, state, vault0.data, vault1.data);
  });
}

/** The reserves of a pool, in the order that the caller asks for. */
export function reservesFor(pool: Pool, inputMint: PublicKey): { reserveIn: bigint; reserveOut: bigint } {
  return pool.state.mint0.equals(inputMint)
    ? { reserveIn: pool.reserve0, reserveOut: pool.reserve1 }
    : { reserveIn: pool.reserve1, reserveOut: pool.reserve0 };
}
