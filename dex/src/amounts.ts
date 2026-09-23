// Turns the base units of a token into text, and text back into base units.

export function parseAmount(text: string, decimals: number): bigint {
  const clean = text.trim();
  if (!/^\d*\.?\d*$/.test(clean) || clean === "" || clean === ".") {
    throw new Error(`"${text}" is not an amount`);
  }
  const [whole, fraction = ""] = clean.split(".");
  if (fraction.length > decimals) {
    throw new Error(`this token keeps ${decimals} decimals`);
  }
  const padded = fraction.padEnd(decimals, "0");
  return BigInt(`${whole === "" ? "0" : whole}${padded}`);
}

export function formatAmount(amount: bigint, decimals: number, maxFraction = decimals): string {
  const negative = amount < 0n;
  const value = negative ? -amount : amount;
  const unit = 10n ** BigInt(decimals);
  const whole = value / unit;
  const fraction = (value % unit).toString().padStart(decimals, "0").slice(0, maxFraction).replace(/0+$/, "");
  const text = fraction === "" ? whole.toString() : `${whole}.${fraction}`;
  return negative ? `-${text}` : text;
}

/** The price of one whole unit of the input, in whole units of the output. */
export function poolPrice(
  reserveIn: bigint,
  decimalsIn: number,
  reserveOut: bigint,
  decimalsOut: number,
): number {
  if (reserveIn === 0n) return 0;
  const inUnits = Number(reserveIn) / 10 ** decimalsIn;
  const outUnits = Number(reserveOut) / 10 ** decimalsOut;
  return outUnits / inUnits;
}
