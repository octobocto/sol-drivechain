// Every key and every number here goes against the official Raydium SDK. The
// page carries its own small client, and these tests hold it to the SDK.
import { describe, expect, it } from "vitest";
import { Keypair, PublicKey } from "@solana/web3.js";
import BN from "bn.js";
import {
  CREATE_CPMM_POOL_PROGRAM,
  CpmmPoolInfoLayout,
  CurveCalculator,
  getCpmmPdaAmmConfigId,
  getCpmmPdaPoolId,
  getPdaLpMint,
  getPdaObservationId,
  getPdaPoolAuthority,
  getPdaVault,
} from "@raydium-io/raydium-sdk-v2";
import {
  CPMM_CONFIG_ID,
  CPMM_PROGRAM_ID,
  POOL_STATE_LEN,
  TRADE_FEE_RATE,
  decodePoolState,
  minimumOut,
  poolAuthority,
  poolId,
  poolLpMint,
  poolObservation,
  poolVault,
  quoteSwapBaseIn,
  reserve,
  sortMints,
  swapBaseInInstruction,
  createPoolInstruction,
} from "../src/cpmm.js";

const mintA = new PublicKey("So11111111111111111111111111111111111111112");
const mintB = new PublicKey("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");

describe("the program keys", () => {
  it("names the same program as the SDK", () => {
    expect(CPMM_PROGRAM_ID.toBase58()).toBe(CREATE_CPMM_POOL_PROGRAM.toBase58());
  });

  it("names the fee config at index 0", () => {
    expect(CPMM_CONFIG_ID.toBase58()).toBe(
      getCpmmPdaAmmConfigId(CREATE_CPMM_POOL_PROGRAM, 0).publicKey.toBase58(),
    );
  });

  it("derives the same authority", () => {
    expect(poolAuthority().toBase58()).toBe(getPdaPoolAuthority(CREATE_CPMM_POOL_PROGRAM).publicKey.toBase58());
  });

  it("derives the same pool, lp mint, vaults, and observation", () => {
    const [mint0, mint1] = sortMints(mintA, mintB);
    const pool = poolId(mint0, mint1);
    expect(pool.toBase58()).toBe(
      getCpmmPdaPoolId(CREATE_CPMM_POOL_PROGRAM, CPMM_CONFIG_ID, mint0, mint1).publicKey.toBase58(),
    );
    expect(poolLpMint(pool).toBase58()).toBe(getPdaLpMint(CREATE_CPMM_POOL_PROGRAM, pool).publicKey.toBase58());
    expect(poolVault(pool, mint0).toBase58()).toBe(
      getPdaVault(CREATE_CPMM_POOL_PROGRAM, pool, mint0).publicKey.toBase58(),
    );
    expect(poolObservation(pool).toBase58()).toBe(
      getPdaObservationId(CREATE_CPMM_POOL_PROGRAM, pool).publicKey.toBase58(),
    );
  });

  it("puts the two mints of a pool in byte order", () => {
    const [mint0, mint1] = sortMints(mintB, mintA);
    expect(Buffer.compare(mint0.toBuffer(), mint1.toBuffer())).toBeLessThan(0);
    expect(sortMints(mintA, mintB)[0].toBase58()).toBe(mint0.toBase58());
  });
});

describe("the pool state", () => {
  it("reads the same fields as the SDK layout", () => {
    const data = Buffer.alloc(POOL_STATE_LEN);
    const fields = {
      configId: CPMM_CONFIG_ID,
      poolCreator: Keypair.generate().publicKey,
      vaultA: Keypair.generate().publicKey,
      vaultB: Keypair.generate().publicKey,
      mintLp: Keypair.generate().publicKey,
      mintA,
      mintB,
      mintProgramA: Keypair.generate().publicKey,
      mintProgramB: Keypair.generate().publicKey,
      observationId: Keypair.generate().publicKey,
      bump: 254,
      status: 0,
      lpDecimals: 9,
      mintDecimalA: 9,
      mintDecimalB: 6,
      lpAmount: new BN(123456789),
      protocolFeesMintA: new BN(11),
      protocolFeesMintB: new BN(22),
      fundFeesMintA: new BN(33),
      fundFeesMintB: new BN(44),
      openTime: new BN(1700000000),
      epoch: new BN(500),
      feeOn: 0,
      enableCreatorFee: false,
      padding1: Array(6).fill(0),
      creatorFeesMintA: new BN(55),
      creatorFeesMintB: new BN(66),
      padding: Array(28).fill(new BN(0)),
    };
    CpmmPoolInfoLayout.encode(fields, data);

    const state = decodePoolState(data);
    expect(state.configId.toBase58()).toBe(CPMM_CONFIG_ID.toBase58());
    expect(state.vault0.toBase58()).toBe(fields.vaultA.toBase58());
    expect(state.vault1.toBase58()).toBe(fields.vaultB.toBase58());
    expect(state.mintLp.toBase58()).toBe(fields.mintLp.toBase58());
    expect(state.mint0.toBase58()).toBe(mintA.toBase58());
    expect(state.mint1.toBase58()).toBe(mintB.toBase58());
    expect(state.observationId.toBase58()).toBe(fields.observationId.toBase58());
    expect(state.decimals0).toBe(9);
    expect(state.decimals1).toBe(6);
    expect(state.lpAmount).toBe(123456789n);
    expect(state.protocolFees0).toBe(11n);
    expect(state.protocolFees1).toBe(22n);
    expect(state.fundFees0).toBe(33n);
    expect(state.fundFees1).toBe(44n);
    expect(state.openTime).toBe(1700000000n);
    expect(state.enableCreatorFee).toBe(false);
    expect(state.creatorFees0).toBe(55n);
    expect(state.creatorFees1).toBe(66n);
  });

  it("takes the same span as the SDK layout", () => {
    expect(POOL_STATE_LEN).toBe(CpmmPoolInfoLayout.span);
  });

  it("refuses a short account", () => {
    expect(() => decodePoolState(new Uint8Array(100))).toThrow(/bytes/);
  });

  it("keeps the fees out of the reserve", () => {
    expect(reserve(1000n, 10n, 5n, 5n)).toBe(980n);
    expect(reserve(10n, 20n, 0n, 0n)).toBe(0n);
  });
});

describe("the quote", () => {
  const cases: Array<[bigint, bigint, bigint]> = [
    [1_000_000n, 1_000_000_000n, 2_000_000_000n],
    [1n, 1_000_000_000n, 2_000_000_000n],
    [500_000_000n, 1_000_000_000n, 2_000_000_000n],
    [7_777n, 123_456_789n, 987_654_321n],
    [999_999_999n, 1_000_000_000_000n, 3_333n],
  ];

  it.each(cases)("matches the SDK for %s in", (amountIn, reserveIn, reserveOut) => {
    const mine = quoteSwapBaseIn(amountIn, reserveIn, reserveOut);
    const theirs = CurveCalculator.swapBaseInput(
      new BN(amountIn.toString()),
      new BN(reserveIn.toString()),
      new BN(reserveOut.toString()),
      new BN(TRADE_FEE_RATE.toString()),
      new BN(0),
      new BN(120000),
      new BN(0),
      false,
    );
    expect(mine.amountOut.toString()).toBe(theirs.outputAmount.toString());
    expect(mine.feeAmount.toString()).toBe(theirs.tradeFee.toString());
  });

  it("refuses an empty pool and an empty trade", () => {
    expect(() => quoteSwapBaseIn(0n, 1n, 1n)).toThrow(/more than zero/);
    expect(() => quoteSwapBaseIn(1n, 0n, 1n)).toThrow(/liquidity/);
  });

  it("shows a bigger impact for a bigger trade", () => {
    const small = quoteSwapBaseIn(1_000n, 1_000_000_000n, 1_000_000_000n);
    const large = quoteSwapBaseIn(500_000_000n, 1_000_000_000n, 1_000_000_000n);
    expect(large.priceImpact).toBeGreaterThan(small.priceImpact);
    expect(small.priceImpact).toBeLessThan(0.01);
  });

  it("cuts the output by the slippage", () => {
    expect(minimumOut(1_000_000n, 1)).toBe(990_000n);
    expect(minimumOut(1_000_000n, 0)).toBe(1_000_000n);
    expect(minimumOut(1_000_000n, 0.5)).toBe(995_000n);
    expect(() => minimumOut(1n, 100)).toThrow(/slippage/);
  });
});

describe("the instructions", () => {
  const owner = Keypair.generate().publicKey;

  it("puts the swap accounts in the order of the program", () => {
    const [mint0, mint1] = sortMints(mintA, mintB);
    const pool = poolId(mint0, mint1);
    const instruction = swapBaseInInstruction(
      {
        payer: owner,
        pool,
        inputMint: mint0,
        outputMint: mint1,
        userInput: Keypair.generate().publicKey,
        userOutput: Keypair.generate().publicKey,
      },
      1_000n,
      900n,
    );
    expect(instruction.keys).toHaveLength(13);
    expect(instruction.keys[0].pubkey.toBase58()).toBe(owner.toBase58());
    expect(instruction.keys[0].isSigner).toBe(true);
    expect(instruction.keys[3].pubkey.toBase58()).toBe(pool.toBase58());
    expect(instruction.keys[6].pubkey.toBase58()).toBe(poolVault(pool, mint0).toBase58());
    expect(instruction.keys[7].pubkey.toBase58()).toBe(poolVault(pool, mint1).toBase58());
    expect(instruction.keys[12].pubkey.toBase58()).toBe(poolObservation(pool).toBase58());
    expect([...instruction.data.subarray(0, 8)]).toEqual([143, 190, 90, 218, 196, 30, 51, 222]);
    expect(instruction.data.readBigUInt64LE(8)).toBe(1_000n);
    expect(instruction.data.readBigUInt64LE(16)).toBe(900n);
  });

  it("puts the pool accounts in the order of the program", () => {
    const [mint0, mint1] = sortMints(mintA, mintB);
    const pool = poolId(mint0, mint1);
    const instruction = createPoolInstruction(
      {
        creator: owner,
        mint0,
        mint1,
        creatorToken0: Keypair.generate().publicKey,
        creatorToken1: Keypair.generate().publicKey,
        creatorLpToken: Keypair.generate().publicKey,
      },
      10n,
      20n,
    );
    expect(instruction.keys).toHaveLength(20);
    expect(instruction.keys[3].pubkey.toBase58()).toBe(pool.toBase58());
    expect(instruction.keys[6].pubkey.toBase58()).toBe(poolLpMint(pool).toBase58());
    expect(instruction.keys[13].pubkey.toBase58()).toBe(poolObservation(pool).toBase58());
    expect([...instruction.data.subarray(0, 8)]).toEqual([175, 175, 109, 31, 13, 152, 155, 237]);
    expect(instruction.data.readBigUInt64LE(8)).toBe(10n);
    expect(instruction.data.readBigUInt64LE(16)).toBe(20n);
    expect(instruction.data.readBigUInt64LE(24)).toBe(0n);
  });
});
