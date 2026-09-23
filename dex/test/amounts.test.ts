import { describe, expect, it } from "vitest";
import { formatAmount, parseAmount, poolPrice } from "../src/amounts.js";

describe("an amount", () => {
  it("reads whole units into base units", () => {
    expect(parseAmount("1", 9)).toBe(1_000_000_000n);
    expect(parseAmount("0.5", 9)).toBe(500_000_000n);
    expect(parseAmount(".25", 4)).toBe(2_500n);
    expect(parseAmount("12.", 2)).toBe(1_200n);
    expect(parseAmount("0", 6)).toBe(0n);
  });

  it("refuses text that is not an amount", () => {
    expect(() => parseAmount("", 9)).toThrow();
    expect(() => parseAmount("1.2.3", 9)).toThrow();
    expect(() => parseAmount("-1", 9)).toThrow();
    expect(() => parseAmount("1e9", 9)).toThrow();
    expect(() => parseAmount("0.1234", 2)).toThrow(/decimals/);
  });

  it("writes base units back", () => {
    expect(formatAmount(1_000_000_000n, 9)).toBe("1");
    expect(formatAmount(1_500_000_000n, 9)).toBe("1.5");
    expect(formatAmount(1n, 9)).toBe("0.000000001");
    expect(formatAmount(0n, 9)).toBe("0");
    expect(formatAmount(1_234_567_890n, 9, 2)).toBe("1.23");
  });

  it("goes back to the same number", () => {
    for (const text of ["0.1", "123.456", "7", "0.000001"]) {
      expect(formatAmount(parseAmount(text, 6), 6)).toBe(text.replace(/^0+(?=\d)/, ""));
    }
  });

  it("prices one side of a pool in the other", () => {
    expect(poolPrice(1_000_000_000n, 9, 2_000_000n, 6)).toBe(2);
    expect(poolPrice(0n, 9, 5n, 6)).toBe(0);
  });
});
