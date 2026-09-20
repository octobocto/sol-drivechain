# sol-drivechain

A BIP300 drivechain on eCash. The execution layer is a stock Solana network.
The money on the chain is pegged Bitcoin.

eCash is Bitcoin with drivechain enabled, so it carries BIP300 today. The chain
does not use BIP301 blind merged mining yet. Until then the operator mines the
mainchain blocks.

## Why BIP300

Nobody holds the peg keys. The Bitcoin sits in an `OP_DRIVECHAIN` output that
no private key can spend, and only a miner vote releases it. Thunder, BitNames,
BitAssets, and Photon all take that deal. Without it the chain is a bridge with
a keeper who can steal the money.

## The peg

The pegged Bitcoin is the native lamport.

- 1 SOL equals 1 BTC.
- 1 satoshi equals 10 lamports.
- A peg in and a peg out use 8 decimals. The amount is a multiple of 10
  lamports.
- A transaction on the chain uses all 9 decimals, so it can move 0.1 satoshi.

A program cannot mint a lamport. So the genesis pre-creates a vault account at
a bridge PDA and gives it the whole supply. A deposit moves lamports out of the
vault. A withdrawal moves them back. The total supply never changes.

## Only the peg changes the money supply

The peg is the one thing that may create or destroy value. That asks for one
patch to Agave, plus one genesis flag.

| What | Where | Why |
|---|---|---|
| Fee burn to 0 | `patches/agave-no-fee-burn.patch` | Upstream hard-codes a 50 percent burn of every transaction fee. A burnt fee destroys pegged Bitcoin. No flag and no feature gate turns it off. |
| VAT burn off | `--deactivate-feature VAT9…ANJ` | SIMD-0357 burns 1.6 SOL from each staked vote account at every epoch. |

Run `bash patches/build-agave.sh` to build the two binaries with the patch.
The patch is one line, so a move to a later Agave is one line to re-apply.

Nothing else is a fork.

- The BIP300 enforcer is a git submodule, pinned and never patched.
- The bridge is a deployed SBF program, not a native program.
- These `solana-genesis` flags do apply:

| Flag | Value | Why |
|---|---|---|
| `--inflation` | `none` | Inflation mints lamports that no Bitcoin backs. |
| `--fee-burn-percentage` | `0` | A burnt fee strands coins in the treasury. |
| `--rent-burn-percentage` | `0` | Same reason. |
| `--lamports-per-byte-year` | `4` | The stock 3480 makes an empty account cost 89088 satoshis. |

`--target-lamports-per-signature` and `--fee-burn-percentage` do **not** apply.
The genesis stores them and the bank ignores them. The base fee stays 5000
lamports per signature, and the patch above sends all of it to the leader.

The genesis still holds about 162 SOL that no Bitcoin backs. Of that, 160 SOL
is a hard-coded Validator Admission Ticket reserve on the vote account, and
the validator and the oracle hold 1 SOL each as a buffer. A third patch could
drop the reserve, but the runtime also filters a vote account by that balance,
so a smaller reserve risks the validator falling out of the stake set.

None of that float can peg out. The bridge counts `pegged_lamports` and
subtracts with a checked subtraction.

## Every network from the start

`--network` takes `alphanet`, `betanet`, `mainnet`, `testnet`, `signet`, or
`regtest`. The daemon asks the enforcer which network it runs, and it stops on
a mismatch.

eCash runs `chain=main`, so its networks report as mainnet. The enforcer tells
alphanet and betanet apart with `--network-preset`, not with the chain name.
The BIP300 thresholds do differ, and `status` prints them.

Numbers read from the live eCash betanet on 2026-09-20:

| Network | Slot ACK votes | ACK window | Bundle votes | Bundle max age |
|---|---|---|---|---|
| eCash betanet | 1009 | 2016 | 13151 | 26300 |
| regtest | 6 | 10 | 6 | 10 |

Only regtest is tested end to end.

## How a deposit works

1. A user asks for the deposit address of a Solana pubkey. The form is
   `s<slot>_<base58 pubkey>_<checksum>`, where the checksum is the first three
   bytes of the SHA-256 of the part before it, in hex.
2. The user pays that address through `CreateDepositTransaction`. The enforcer
   writes the string into the OP_RETURN with no change.
3. The mainchain sends a deposit event with a gapless sequence number.
4. After enough confirmations the daemon credits `value_sats * 10` lamports.
   The sequence number is the replay guard, on chain.

## How a withdrawal works

1. A user calls `withdraw` with a Bitcoin script pubkey and a fee. It burns
   the lamports into the vault and writes a record.
2. The daemon packs every open record into one blinded M6 and calls
   `ProposeWithdrawalBundle`.
3. It sets the withdrawal bundle policy to `KNOWN`, so a miner upvotes only a
   bundle whose transaction that node holds.
4. Miners vote. On `Succeeded` the daemon reads the paid M6, matches each
   payout to a record, and closes it.
5. A bundle that expires leaves its records open, and the next bundle carries
   them again. No money disappears.

## The trust model

The mainchain releases the Bitcoin, not a key. The oracle key only credits
lamports for a deposit that the mainchain already confirmed, and the bridge
counts `pegged_lamports` so the validator's genesis lamports can never peg out.

## Layout

| Path | What it holds |
|---|---|
| `programs/bridge` | The Anchor bridge program, plus its SVM tests. |
| `daemon` | The oracle daemon and the enforcer client. |
| `genesis` | The genesis scripts and the primordial accounts file. |
| `scripts` | The regtest stack helpers. |
| `bip300301_enforcer` | A submodule. Excluded from both workspaces. |

Two Cargo workspaces, because `cargo-build-sbf` carries its own older rustc.

## Build and test

```sh
git submodule update --init --recursive
cargo-build-sbf --manifest-path programs/bridge/Cargo.toml --sbf-out-dir target/deploy
cargo test --all-targets
cargo test --manifest-path daemon/Cargo.toml --all-targets
```

The program tests load `target/deploy/sol_drivechain_bridge.so`, so build the
SBF artifact first.

## A regtest chain

The stack is a drivechain-patched `bitcoind` plus the enforcer. Stock Core does
not relay an `OP_DRIVECHAIN` output, so an unpatched node asks for
`ACCEPT_NONSTD=1`.

```sh
bash scripts/regtest.sh start
D=daemon/target/debug/sol-drivechain-daemon
E="--network regtest --enforcer-url http://127.0.0.1:19551 --slot 8"

$D propose-slot $E
ADDRESS=$($D wallet-address $E | head -1)
$D mine $E --blocks 7 --address "$ADDRESS" --ack-new-slots
$D status $E                       # the slot activates

$D mine $E --blocks 101 --address "$ADDRESS"
$D deposit $E --pubkey <PUBKEY> --sats 5000000
$D mine $E --blocks 1 --address "$ADDRESS"
$D watch $E                        # the deposit event names the account
```

## Reach the chain from a wallet

The validator binds `127.0.0.1` for its RPC, and
`genesis/close-validator-ports.sh` drops outside traffic to ports 8000 to 8020.
So the only way in is a reverse proxy. `genesis/caddy/sol-rpc.caddy` holds the
block, and it takes POST only.

```sh
solana config set --url https://seed.alpha.ecash.eu.com/sol/
```

```js
new Connection("https://seed.alpha.ecash.eu.com/sol/", {
  wsEndpoint: "wss://seed.alpha.ecash.eu.com/sol-ws/",
});
```

Pass `wsEndpoint`, because a wallet guesses the websocket address from the
http one, and a path proxy breaks that guess.

The genesis hash names the cluster:
`CW6Q1sLumxtmhLBz9DnDWaSidP6A5a8CEKiH18EKQH4i`.

Every wallet prints the unit as SOL. One SOL is one BTC here.

## Prove the whole peg

`scripts/regtest-peg.sh` builds its own Bitcoin node, its own enforcer, and its
own Solana chain, then it claims a slot, pegs in, and pegs out. It never
touches a chain that already runs on the host.

```sh
BITCOIND=<a drivechain-patched bitcoind> \
ENFORCER=<the enforcer> \
bash scripts/regtest-peg.sh
```

A run on 2026-09-20 gave this:

```
PASS: the peg in credited 50000000 lamports for 5000000 sats
withdrawal 0 burns 20000000 lamports, pays 1990000 sats to bcrt1q3cls...
PASS: the payout address went from 0.00000000 BTC to 0.01990000 BTC

pegged lamports   30000000
treasury          3000000 sats
```

The last two lines are the whole point. 30,000,000 lamports is 3,000,000
satoshis, and the mainchain treasury holds 3,000,000 satoshis.

## Two hosts on one regtest chain

`scripts/regtest-two-hosts.sh` starts a stack here and a stack on a remote
host, and joins them through an SSH tunnel. The remote host opens no port.

```sh
REMOTE=alphanet bash scripts/regtest-two-hosts.sh
```

It mines here, checks that the remote height grows, and both enforcers then
report the same tip, the same treasury, and the same active slot.

## The live eCash chain

Point the daemon at the enforcer that already runs beside your eCash node:

```sh
D=daemon/target/debug/sol-drivechain-daemon
$D status --network betanet --enforcer-url http://127.0.0.1:50051 --slot 8
```

Pick a slot that no sidechain holds. `status` lists every active slot and every
open proposal. Then claim it and mine until it activates:

```sh
$D propose-slot --network betanet --enforcer-url http://127.0.0.1:50051 --slot 8
```

## Start the Solana chain

`solana-genesis` and `agave-validator` are not in the macOS release of the
Agave CLI. Use the Linux release, or build both from an agave checkout.

```sh
bash genesis/build-genesis.sh
bash genesis/run-validator.sh
daemon/target/debug/sol-drivechain-daemon run \
  --network regtest --slot 8 \
  --program-id "$(cat keys/bridge-program.pubkey)" \
  --oracle keys/oracle.json
```

## One seed for every sidechain

BitWindow keeps one BIP39 seed phrase, and every sidechain derives its keys
from it. This chain takes the same phrase and uses Solana's own path,
`m/44'/501'/<account>'/0'`.

```sh
D=daemon/target/debug/sol-drivechain-daemon
$D seed-pubkey --mnemonic-file <phrase file> --account 0 --count 5
$D seed-keypair --mnemonic-file <phrase file> --account 0 --out wallet.json
```

Because the path is Solana's own, the same phrase also opens the same accounts
in `solana-keygen recover`, in Phantom, and in Solflare. A test in
`daemon/src/seed.rs` pins two addresses that `solana-keygen recover` gave for
the public BIP39 test phrase, so a change to the derivation breaks the build.

The phrase comes from a file, never from a command line argument, so it stays
out of the shell history and out of the process list. The keypair file lands
with mode 600.

## Commands

| Command | What it does |
|---|---|
| `status` | Prints the network, the thresholds, the treasury, and the slot. |
| `propose-slot` | Sends the M1 that claims the sidechain slot. |
| `mine` | Mines blocks with the BIP300 coinbase. Regtest only. |
| `wallet-address` | Takes an address from the enforcer wallet. |
| `deposit` | Sends a deposit to a Solana pubkey. |
| `watch` | Prints every peg event that the mainchain sends. |
| `address` | Prints the deposit address of a Solana pubkey. |
| `derive` | Prints the bridge program addresses. |
| `genesis` | Writes the primordial accounts file for the vault. |
| `seed-pubkey` | Prints the addresses of a BIP39 seed phrase. |
| `seed-keypair` | Writes a Solana keypair file from that phrase. |
| `withdraw` | Burns lamports and asks the mainchain to pay an address. |
| `bridge-state` | Prints the bridge config and the vault balance. |
| `initialize` | Creates the bridge config account. |
| `run` | Runs the peg. |

## Still open

- A full end-to-end run across the enforcer and a live validator. It waits for
  `agave-validator`, which the macOS release does not carry.
- `cargo deny`. The CI runs fmt, clippy, the SBF build, and both test suites.
