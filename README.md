# sol-drivechain

A BIP300 drivechain on eCash. The execution layer is a stock Solana network.
The money on the chain is pegged Bitcoin.

eCash is Bitcoin with drivechain enabled, so it carries BIP300 and BIP301
today. Every Solana fee goes to a treasury, and validators win it back through
BIP301 blind merged mining. So the fees go to eCash miners.

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

The peg is the one thing that may create or destroy value. That asks for a
patch to Agave, plus one genesis flag.

| What | Where | Why |
|---|---|---|
| Fee burn to 0 | `patches/agave-sol-drivechain.patch` | Upstream hard-codes a 50 percent burn of every transaction fee. A burnt fee destroys pegged Bitcoin. No flag and no feature gate turns it off. |
| VAT burn off | `--deactivate-feature VAT9…ANJ` | SIMD-0357 burns 1.6 SOL from each staked vote account at every epoch. |

The same patch holds the BMM changes. [BMM](#bmm) lists them. Run
`bash patches/build-agave.sh` to build the two binaries with the patch.

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
lamports per signature, and the patch sends all of it to the treasury.

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

## BMM

Validators get fees only when they win a BIP301 bid on eCash. The commitment
on eCash is the payee pubkey, and that is the whole h*. Each validator reads
eCash from its own enforcer.

1. **Fees.** The patched runtime sends every fee to the treasury PDA.
2. **Bid.** For each new eCash tip T, a validator sends one BMM request with
   its payee pubkey and a bid in sats. The bid is at most the fee income of
   one block interval, from the growth of `treasury + bmm_paid_total`.
3. **Win.** The miner of block H = T + 1 takes the top bid. It puts that
   pubkey in the coinbase, and the request pays the bid. A losing request is
   invalid, so it costs nothing.
4. **Settle.** When block H + N + 1 arrives, any node sends
   `settle_bmm(H, hash(H))`. The winner does not need to be the sender. The
   program pays the whole treasury above its rent reserve to the pubkey in
   the coinbase, and it adds that amount to `bmm_paid_total`. A height with
   no commitment pays nobody, and its fees go to the next winner.

Heights settle one at a time, in order. Each winner gets the fees between the
settle of H − 1 and the settle of H, about one block interval.

The Agave patch adds these parts:

| Part | What it does |
|---|---|
| Treasury | Every fee goes to the treasury, not to the leader. |
| Vote fee | A simple vote pays no signature fee. The genesis keeps Alpenglow off, so votes are txs. |
| `sol-drivechain-bmm` | A view of the active eCash chain, filled from the local enforcer. |
| Pre-check | For each tx with a top-level `settle_bmm(H, M)`, the check asks the view. It passes only when M is the active block at H. A leader needs N + 1 blocks on top, and replay needs N. |
| `BmmAnswer` | The runtime builds this account for the tx, the same way as the instructions sysvar. It holds the height, the block hash, and the commitment. The bridge reads it. |
| Flags | `--bmm-enforcer-url`, `--bmm-sidechain-slot`, and `--bmm-confirmations`. |

A "not ready" settle waits in the leader queue and goes into a later block.
On replay, the check waits up to 30 seconds for the local enforcer, and then
the block is dead. A leader needs one more block than replay, so a healthy
node never waits.

A settle names its eCash block, and the check takes only the active chain. So
a settle for a block on another branch never lands, however long that branch
is. Solana runs a settle one time and never asks again, so a later eCash
reorg changes no Solana state.

A payee that cannot take the payout gets nothing, and the fees go to the next
winner. The settle still lands. That holds for an account below its rent
reserve, for an executable account, and for an account that the runtime
demotes to read only. A settle that failed for such a payee would stop every
later height, so a bid with a bad payee would stop the whole chain.

This plan carries no checkpoint of Solana state on eCash. To add one later,
make the commitment `hash(payee, slot, slot hash)` and add a claim
instruction.

## Two depths: D for a deposit, N for a BMM settle

A deposit waits D confirmations, and a BMM settle waits N. They are not the
same number, because the damage is not the same.

| | Depth | Why |
|---|---|---|
| Deposit | D = 6 on alphanet and betanet, 100 on mainnet | A reorg that drops a credited deposit mints lamports that no Bitcoin backs. Only a restart can undo it. A test network takes the smaller wait. |
| BMM settle | N = 6 | A reorg that drops a settled block costs nobody. Solana keeps the payout, and the miner of the stale block loses its own reward. |

Solana never rolls back by itself. A node runs a settle one time and never
asks again, so a later eCash reorg changes no Solana state.

## The disaster restart

A reorg deeper than D can remove a credited deposit from eCash. No consensus
rule handles that. The cluster then does a coordinated restart, the same way
Solana restarts after an outage.

1. Find the last slot whose eCash parts survive. Every credited deposit and
   every settled block must be on the new eCash branch.
2. Start every validator from a snapshot at or before that slot, with
   `--hard-fork <slot>` and `--wait-for-supermajority <slot>`.
3. Let the daemon replay the peg from eCash. It does that at every start. A
   peg-out that eCash paid stays paid.
4. Bidders bid again from the `bmm next height` of the restarted chain.

`run-validator.sh` keeps ten full snapshots, 25000 slots apart, which covers
about 28 hours. So step 2 always finds a snapshot inside the D window.

## Join the chain

Anyone can run a validator. The genesis deactivates SIMD-0357, so no vote
account holds an admission ticket, and a validator can stake any amount.

1. Take `agave-validator` and `solana-genesis` from a release, or build them
   with `bash patches/build-agave.sh`. A stock Agave forks off at the first
   fee, so the patched binaries are necessary.
2. Run an eCash node and an enforcer beside the validator. A validator without
   an enforcer cannot check a block that holds a BMM settle.
3. Start the node. It takes the genesis and a snapshot from the RPC of the
   entrypoint, and then it follows the chain.

```sh
mkdir -p ~/sol-drivechain/ledger
LEDGER=~/sol-drivechain/ledger KEYS=~/sol-drivechain/keys \
ENFORCER_URL=http://127.0.0.1:50051 SIDECHAIN_SLOT=8 \
ENTRYPOINT=204.168.254.113:8001 \
KNOWN_VALIDATOR=AYJy9KVJtgSnMSknAcFqhXtLhtSyBNFZJ5W1zDEWDeVg \
EXPECTED_GENESIS_HASH=2EfKjqtXSu7YZaLiBLUE1xx3Tt1XMmjzKwQ967KzneUx \
bash genesis/run-validator.sh
```

4. To vote, create a vote account, peg in Bitcoin, and delegate stake to it.
   The stake activates at the next epoch. Stake is pegged Bitcoin, so the
   security of the chain is the Bitcoin that validators stake.

Read the genesis hash from the chain: `solana -u <rpc> genesis-hash`.

A validator advertises the address that it binds. The first validator of a
chain has no entrypoint, so it cannot learn its own address. Its operator sets
`BIND_ADDRESS` to the address that the world sees, or no joiner reaches it.

## The trust model

The mainchain releases the Bitcoin, not a key. The oracle key only credits
lamports for a deposit that the mainchain already confirmed, and the bridge
counts `pegged_lamports` so the validator's genesis lamports can never peg out.

No key controls BMM. The coinbase names the payee, and any user can send the
settle that pays it.

## Layout

| Path | What it holds |
|---|---|
| `programs/bridge` | The Anchor bridge program, plus its SVM tests. |
| `daemon` | The oracle daemon and the enforcer client. |
| `genesis` | The genesis scripts and the primordial accounts file. |
| `scripts` | The regtest stack helpers. |
| `dex` | The CP-Swap client, its tests, and the trade page. |
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

The validator binds `127.0.0.1` for its RPC, so the only way in is a reverse
proxy. `genesis/caddy/sol-rpc.caddy` holds the block, and it takes POST only.
The gossip ports stay open, because any user may join the chain.

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
`2EfKjqtXSu7YZaLiBLUE1xx3Tt1XMmjzKwQ967KzneUx`.

Every wallet prints the unit as SOL. One SOL is one BTC here.

## Prove the whole peg

`scripts/regtest-peg.sh` builds its own Bitcoin node, its own enforcer, and its
own Solana chain, then it claims a slot, pegs in, and pegs out. Last, a second
validator joins, and a BMM loop bids until a win settles and pays the
treasury. Four more stages prove the hard cases:

- A settle for a block that is not N + 1 deep never lands.
- A settle for a block that a reorg dropped never lands.
- The loop settles the new block at that height after the reorg.
- Both validators run after a reorg deeper than N.

It never touches a chain that already runs on the host.

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

## A standing regtest chain

`scripts/regtest-peg.sh` proves the chain one time and takes it down.
`scripts/regtest-chain.sh` keeps one up: a Bitcoin node, an enforcer, a
validator, the peg, the BMM loop, and a miner that adds a block every
minute. The sidechain slot is active there, so bids win and settles pay,
again and again.

```sh
bash scripts/regtest-chain.sh up
bash scripts/regtest-chain.sh status
bash scripts/regtest-chain.sh mine 5
bash scripts/regtest-chain.sh down
```

`genesis/systemd/sol-regtest.service` runs the same chain as a service, so it
comes back after a restart of the host. One chain of this shape runs today,
and builders use it while betanet waits for the slot:

```sh
solana config set --url https://seed.alpha.ecash.eu.com/sol-regtest/
```

```js
new Connection("https://seed.alpha.ecash.eu.com/sol-regtest/", {
  wsEndpoint: "wss://seed.alpha.ecash.eu.com/sol-regtest-ws/",
});
```

Its genesis hash is `HNnsusSnk6VAhTwRqM3FpdLdV27y9wkbyYLbkRaeiTgZ`. The chain
runs a faucet, and the faucet holds pegged BTC from one eCash deposit. A
wallet takes test coins with the stock airdrop call:

```sh
solana airdrop 1
```

One limit of regtest: the eCash node relays the first deposit only. A later
deposit spends a drivechain output, and the mempool policy of a regtest node
refuses that spend. The faucet covers every builder until betanet opens.

## The DEX

The regtest chain carries four Solana mainnet programs at their mainnet
addresses: SPL Token, the associated token account program, Memo, and the
Raydium CP-Swap AMM. `genesis/fetch-dex-programs.sh` copies each program from
a mainnet RPC, and it pins the bytes in `genesis/dex-programs.sha256`.

CP-Swap keeps its fees in an `AmmConfig` account that only its admin makes.
That account is a PDA of the program, so the genesis carries a copy of the
mainnet account at the same address. The genesis also carries the wrapped SOL
mint and the token account that takes the pool creation fee.

A trade page runs on those pools at `https://seed.alpha.ecash.eu.com/dex/`.
The page lives in the `sol-dex` repository. This repository serves the built
files from `/var/www/sol-dex`, and it holds no page code.

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
ENFORCER_URL=http://127.0.0.1:50051 SIDECHAIN_SLOT=8 bash genesis/run-validator.sh
daemon/target/debug/sol-drivechain-daemon run \
  --network regtest --slot 8 \
  --program-id "$(cat keys/bridge-program.pubkey)" \
  --oracle keys/oracle.json
daemon/target/debug/sol-drivechain-daemon bmm \
  --slot 8 --program-id "$(cat keys/bridge-program.pubkey)" \
  --identity keys/validator-identity.json
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
| `bridge-state` | Prints the bridge config, the vault, the treasury, and the BMM totals. |
| `initialize` | Creates the bridge config account. |
| `run` | Runs the peg. |
| `bmm` | Bids for eCash blocks, and settles each eCash height. |
| `settle-bmm` | Settles one eCash height by hand. |
| `ecash-tip` | Prints the height and the hash of the eCash tip. |
| `ecash-walk` | Prints how far the enforcer walks back in one call. |

## Still open

- A full end-to-end run across the enforcer and a live validator. It waits for
  `agave-validator`, which the macOS release does not carry.
- `cargo deny`. The CI runs fmt, clippy, the SBF build, and both test suites.
