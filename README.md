# sol-drivechain

A BIP300 sidechain. The execution layer is a stock Solana network. The money
is pegged Bitcoin.

The chain does not use BIP301 blind merged mining. It uses only the BIP300
deposit and withdrawal path.

## The peg

The pegged Bitcoin is the native lamport.

- 1 SOL equals 1 BTC.
- 1 satoshi equals 10 lamports.
- A peg in and a peg out use 8 decimals. The amount is a multiple of 10
  lamports.
- A transaction on the chain uses all 9 decimals.

## No fork

This repo patches no upstream project.

- The validator is a stock `agave-validator`.
- Every Solana constant comes from a `solana-genesis` flag.
- The bridge is a deployed SBF program, not a native program.
- The enforcer is a git submodule.

## Layout

| Path | What it holds |
|---|---|
| `programs/bridge` | The Anchor bridge program. |
| `daemon` | The oracle daemon. It talks to the enforcer and to Solana. |
| `genesis` | The genesis scripts and the primordial accounts file. |
| `integration_tests` | A regtest round trip: bitcoind, the enforcer, a validator. |
| `bip300301_enforcer` | A submodule. Excluded from both workspaces. |
