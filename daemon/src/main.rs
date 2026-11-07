use std::{os::unix::fs::PermissionsExt as _, path::PathBuf, str::FromStr as _, time::Duration};

use bitcoin::hashes::Hash as _;
use clap::{Parser, Subcommand};
use sol_drivechain_daemon::enforcer::{Declaration, Enforcer};
use sol_drivechain_daemon::network::PegNetwork;
use sol_drivechain_daemon::proto::mainchain::AckAllProposalsPolicy;
use sol_drivechain_daemon::{address, bmm, books, bridge, credit, genesis, migrate, peg, seed};
use solana_sdk::{pubkey::Pubkey, signature::read_keypair_file, signer::Signer as _};

/// The lamport count of one SOL.
const LAMPORTS_PER_SOL: u64 = 1_000_000_000;

#[derive(Parser)]
#[command(version, about = "The peg and BMM daemon of the SOL drivechain")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Args, Clone)]
struct EnforcerArgs {
    #[arg(long, default_value = "regtest")]
    network: PegNetwork,
    #[arg(long, default_value = "http://127.0.0.1:50051")]
    enforcer_url: String,
    #[arg(long)]
    slot: u8,
}

impl EnforcerArgs {
    async fn open(&self) -> Result<Enforcer, CliError> {
        let mut enforcer = Enforcer::connect(self.enforcer_url.clone(), self.slot as u32).await?;
        enforcer.check_network(self.network).await?;
        Ok(enforcer)
    }
}

#[derive(clap::Args, Clone)]
struct DeclarationArgs {
    /// BitWindow lists this title beside Thunder and BitNames, so it reads as
    /// a display name and not as a repository name.
    #[arg(long, default_value = "Solana")]
    title: String,
    #[arg(long, default_value = "A Solana sidechain")]
    description: String,
    /// The release tarball hash, 64 hex characters. Zero names no release.
    #[arg(long, default_value = "")]
    hashid1: String,
    /// The build commit hash, 40 hex characters. Zero names no commit.
    #[arg(long, default_value = "")]
    hashid2: String,
}

impl DeclarationArgs {
    fn declaration(&self) -> Result<Declaration, CliError> {
        Ok(Declaration::with_hashes(
            &self.title,
            &self.description,
            fixed_hash(&self.hashid1, "hashid1")?,
            fixed_hash(&self.hashid2, "hashid2")?,
        )?)
    }
}

/// Reads a hex hash of a fixed width. An empty value reads as all zeros.
fn fixed_hash<const N: usize>(value: &str, field: &'static str) -> Result<[u8; N], CliError> {
    let mut out = [0u8; N];
    if value.is_empty() {
        return Ok(out);
    }
    if value.len() != N * 2 {
        return Err(CliError::WrongHashLength {
            field,
            want: N * 2,
            found: value.len(),
        });
    }
    for (index, pair) in value.as_bytes().chunks(2).enumerate() {
        let text = std::str::from_utf8(pair).map_err(|_| CliError::NotHex {
            field,
            value: value.to_owned(),
        })?;
        out[index] = u8::from_str_radix(text, 16).map_err(|_| CliError::NotHex {
            field,
            value: value.to_owned(),
        })?;
    }
    Ok(out)
}

#[derive(Subcommand)]
enum Command {
    /// Prints what the mainchain says about this sidechain slot.
    Status {
        #[command(flatten)]
        enforcer: EnforcerArgs,
    },
    /// Sends the M1 that claims the sidechain slot.
    ProposeSlot {
        #[command(flatten)]
        enforcer: EnforcerArgs,
        #[command(flatten)]
        declared: DeclarationArgs,
    },
    /// Votes for the open proposal of this slot, and for no other.
    AckSlot {
        #[command(flatten)]
        enforcer: EnforcerArgs,
        #[command(flatten)]
        declared: DeclarationArgs,
        /// Stop the vote instead of starting it.
        #[arg(long)]
        stop: bool,
        /// Vote even when the proposal text is not the one this daemon wrote.
        #[arg(long)]
        any: bool,
    },
    /// Mines blocks whose coinbase carries the BIP300 messages. Regtest only.
    Mine {
        #[command(flatten)]
        enforcer: EnforcerArgs,
        #[arg(long, default_value_t = 1)]
        blocks: u32,
        /// The address that takes the coinbase.
        #[arg(long)]
        address: String,
        /// ACK every proposal for a slot that no sidechain holds.
        #[arg(long)]
        ack_new_slots: bool,
    },
    /// Takes a new address from the enforcer wallet.
    WalletAddress {
        #[command(flatten)]
        enforcer: EnforcerArgs,
    },
    /// Sends a deposit from the enforcer wallet to a Solana pubkey.
    Deposit {
        #[command(flatten)]
        enforcer: EnforcerArgs,
        #[arg(long)]
        pubkey: String,
        #[arg(long)]
        sats: u64,
        #[arg(long, default_value_t = 1_000)]
        fee_sats: u64,
    },
    /// Prints every peg event that the mainchain sends.
    Watch {
        #[command(flatten)]
        enforcer: EnforcerArgs,
    },
    /// Burns lamports and asks the mainchain to pay a Bitcoin address.
    Withdraw {
        #[arg(long, default_value = "http://127.0.0.1:8899")]
        solana_rpc_url: String,
        #[arg(long)]
        program_id: String,
        /// The keypair that holds the lamports.
        #[arg(long)]
        user: PathBuf,
        #[arg(long)]
        sats: u64,
        /// The mainchain fee. It comes out of the amount. It defaults to the
        /// lowest fee that the bridge takes.
        #[arg(long)]
        fee_sats: Option<u64>,
        /// The Bitcoin address that takes the payout.
        #[arg(long)]
        address: String,
        /// The network of that address.
        #[arg(long, default_value = "regtest")]
        network: PegNetwork,
    },
    /// Prints the bridge config and the vault balance.
    BridgeState {
        #[arg(long, default_value = "http://127.0.0.1:8899")]
        solana_rpc_url: String,
        #[arg(long)]
        program_id: String,
        /// Reads the branch that the validator follows, not the confirmed one.
        #[arg(long)]
        processed: bool,
        /// Prints the balance equations with each term, and fails when one
        /// does not hold. It reads eCash from the enforcer.
        #[arg(long, requires_all = ["slot", "network"])]
        books: bool,
        #[arg(long)]
        network: Option<PegNetwork>,
        #[arg(long, default_value = "http://127.0.0.1:50051")]
        enforcer_url: String,
        #[arg(long)]
        slot: Option<u8>,
    },
    /// Sends the lamports to the vault that the genesis float still needs, so
    /// withdrawals open.
    BackFloat {
        #[arg(long, default_value = "http://127.0.0.1:8899")]
        solana_rpc_url: String,
        #[arg(long)]
        program_id: String,
        /// The keypair that pays the gift.
        #[arg(long)]
        payer: PathBuf,
    },
    /// Reads the state of a halted chain into a file for the genesis of its
    /// successor: the system balances, the withdrawal records, and the deposit
    /// cursor.
    MigrateState {
        /// The RPC of a validator that runs with HALT=1 on the old ledger.
        #[arg(long)]
        solana_rpc_url: String,
        #[arg(long)]
        program_id: String,
        #[arg(long)]
        out: PathBuf,
        /// Leaves this many lamports of ownerless accounts behind. It must
        /// equal the count that the export finds.
        #[arg(long)]
        accept_ownerless_lamports: Option<u64>,
    },
    /// Prints the lamports of one account in a state file, or 0.
    StateLamports {
        #[arg(long)]
        state: PathBuf,
        #[arg(long)]
        pubkey: String,
    },
    /// Credits the deposits of each eCash height in order. Anyone can run it.
    Credit {
        #[arg(long)]
        network: PegNetwork,
        #[arg(long, default_value = "http://127.0.0.1:50051")]
        enforcer_url: String,
        #[arg(long, default_value = "http://127.0.0.1:8899")]
        solana_rpc_url: String,
        #[arg(long)]
        slot: u8,
        #[arg(long)]
        program_id: String,
        /// The keypair that signs and pays the fees.
        #[arg(long)]
        payer: PathBuf,
    },
    /// Prints the address of one account of a BIP39 seed phrase.
    SeedPubkey {
        /// The file that holds the seed phrase. BitWindow keeps the same one.
        #[arg(long)]
        mnemonic_file: PathBuf,
        #[arg(long, default_value_t = 0)]
        account: u32,
        /// How many accounts to print, starting at `account`.
        #[arg(long, default_value_t = 1)]
        count: u32,
    },
    /// Writes a Solana keypair file for one account of a seed phrase.
    SeedKeypair {
        #[arg(long)]
        mnemonic_file: PathBuf,
        #[arg(long, default_value_t = 0)]
        account: u32,
        #[arg(long)]
        out: PathBuf,
    },
    /// Prints the program addresses that the bridge derives.
    Derive {
        #[arg(long)]
        program_id: String,
    },
    /// Prints the mainchain deposit address for a Solana pubkey.
    Address {
        #[arg(long)]
        slot: u8,
        #[arg(long)]
        pubkey: String,
    },
    /// Prints the JSON that asks a miner to claim and activate the slot.
    ActivationRequest {
        #[arg(long)]
        slot: u8,
        #[command(flatten)]
        declared: DeclarationArgs,
    },
    /// Writes the primordial accounts file: the vault, the treasury, the
    /// bridge config, the block record, and the oracle. With `--state`, it
    /// adds the balances, the withdrawal records, and the deposit cursor of a
    /// halted chain.
    Genesis {
        #[arg(long)]
        program_id: String,
        /// The supply of the chain. The vault holds it at the start.
        #[arg(long, default_value_t = 21_000_000)]
        vault_sol: u64,
        /// The key that seals a bundle and marks it paid.
        #[arg(long)]
        oracle: String,
        /// The lamports that the oracle of a new chain gets to pay its tx fees.
        #[arg(long)]
        oracle_lamports: u64,
        /// The spendable lamports that `solana-genesis` adds and no deposit
        /// backs: the identity, the vote and stake accounts, and the faucet.
        #[arg(long)]
        float_lamports: u64,
        /// The eCash blocks that the credits must pass above a commitment
        /// without a pair before the BMM cursor skips it.
        #[arg(long)]
        bmm_skip_blocks: u64,
        /// The DEX accounts file. A carried account of the same key wins.
        #[arg(long)]
        dex_accounts: Option<PathBuf>,
        /// The bootstrap vote account. No carried account may hold its key.
        #[arg(long)]
        bootstrap_vote: Option<String>,
        /// The bootstrap stake account. No carried account may hold its key.
        #[arg(long)]
        bootstrap_stake: Option<String>,
        /// The rent of the genesis. Each account gets its rent-exempt minimum.
        #[arg(long)]
        lamports_per_byte_year: u64,
        /// The eCash tip when the chain starts. BMM settles from it, and a new
        /// chain counts the deposits up to it as credited.
        #[arg(long)]
        ecash_start_height: u64,
        /// D, the eCash confirmations before a credit.
        #[arg(long)]
        deposit_confirmations: u64,
        /// A BMM checkpoint at eCash height H counts only when its bank
        /// credited every deposit through H minus this lag.
        #[arg(long)]
        deposit_lag: u64,
        /// The state file of a halted chain, from `migrate-state`.
        #[arg(long)]
        state: Option<PathBuf>,
        #[arg(long)]
        out: PathBuf,
    },
    /// Prints how many headers and BMM commitments the enforcer gives for
    /// one call. A test uses it.
    EcashWalk {
        #[command(flatten)]
        enforcer: EnforcerArgs,
        #[arg(long, default_value_t = 1000)]
        max_ancestors: u32,
    },
    /// Prints the balance of the enforcer wallet, in satoshis.
    WalletBalance {
        #[command(flatten)]
        enforcer: EnforcerArgs,
    },
    /// Prints the height and the hash of the eCash tip.
    EcashTip {
        #[command(flatten)]
        enforcer: EnforcerArgs,
    },
    /// Sends one BIP301 bid for the next eCash block, for the newest block in
    /// the block record of the validator that passes rule 3, and publishes the
    /// pair to the validator. The payee takes the treasury if the bid settles.
    /// An operator or a test uses it.
    BidBmm {
        #[command(flatten)]
        enforcer: EnforcerArgs,
        #[arg(long, default_value = "http://127.0.0.1:8899")]
        solana_rpc_url: String,
        /// The pubkey that takes the payout.
        #[arg(long, required_unless_present = "commitment")]
        payee: Option<String>,
        #[arg(long)]
        sats: u64,
        /// Bids this h*, in hex, and publishes no pair. A test uses it for a
        /// commitment that no pair matches.
        #[arg(long, conflicts_with = "payee")]
        commitment: Option<String>,
        /// Bids, but keeps the pair back.
        #[arg(long)]
        withhold: bool,
        /// Bids for this Solana bank hash, in base58, and not for the newest
        /// recorded block. It skips the rule 3 check. A test uses it.
        #[arg(long, conflicts_with = "commitment")]
        block: Option<String>,
        #[arg(long)]
        program_id: Option<String>,
        /// How long the bid waits for a recorded block that passes rule 3.
        #[arg(long, default_value_t = 240)]
        wait_secs: u64,
    },
    /// Publishes one BMM pair to a validator.
    PublishPair {
        #[arg(long, default_value = "http://127.0.0.1:8899")]
        solana_rpc_url: String,
        #[arg(long)]
        height: u64,
        /// The Solana bank hash, in base58.
        #[arg(long)]
        block: String,
        #[arg(long)]
        payee: String,
    },
    /// Prints the slot and the bank hash of the newest block in the block
    /// record of the validator.
    BmmBlock {
        #[arg(long, default_value = "http://127.0.0.1:8899")]
        solana_rpc_url: String,
    },
    /// Settles one BMM commitment by hand. A test or an operator uses it.
    SettleBmm {
        #[arg(long, default_value = "http://127.0.0.1:8899")]
        solana_rpc_url: String,
        #[arg(long)]
        program_id: String,
        /// The keypair that signs and pays the fee.
        #[arg(long)]
        identity: PathBuf,
        #[arg(long)]
        height: u64,
        /// The eCash block hash in the hex that the explorers show.
        #[arg(long)]
        block_hash: String,
        /// The Solana bank hash of the pair, in base58.
        #[arg(long)]
        solana_block: String,
        /// The payee of the pair. It defaults to the signer.
        #[arg(long)]
        payee: Option<String>,
    },
    /// Bids for eCash blocks, and settles each eCash height on Solana.
    Bmm {
        #[arg(long)]
        network: PegNetwork,
        #[arg(long, default_value = "http://127.0.0.1:50051")]
        enforcer_url: String,
        #[arg(long, default_value = "http://127.0.0.1:8899")]
        solana_rpc_url: String,
        #[arg(long)]
        slot: u8,
        #[arg(long)]
        program_id: String,
        /// The keypair that signs, and whose pubkey is the payee of each bid.
        /// Normally the validator identity.
        #[arg(long)]
        identity: PathBuf,
        /// N. It must match `--bmm-confirmations` on the validators.
        #[arg(long, default_value_t = 6)]
        confirmations: u64,
        /// The part of the fee income of one block that one bid offers.
        #[arg(long, default_value_t = 90)]
        bid_percent: u64,
        #[arg(long, default_value_t = 1_000)]
        min_bid_sats: u64,
        /// No bid and no template offers more sats than this.
        #[arg(long)]
        max_bid_sats: u64,
        #[arg(long, default_value_t = 2)]
        interval_secs: u64,
        /// Serves the JSON-RPC for BitWindow and drivechaind on this address.
        #[arg(long)]
        rpc_listen: Option<std::net::SocketAddr>,
        /// Sends no bid. drivechaind pays the bids through the JSON-RPC.
        #[arg(long)]
        no_bid: bool,
    },
    /// Runs the peg. It credits deposits, it proposes withdrawal bundles, and
    /// it marks the paid withdrawals.
    Run {
        #[arg(long, default_value = "regtest")]
        network: PegNetwork,
        #[arg(long, default_value = "http://127.0.0.1:50051")]
        enforcer_url: String,
        #[arg(long, default_value = "http://127.0.0.1:8899")]
        solana_rpc_url: String,
        #[arg(long)]
        slot: u8,
        #[arg(long)]
        program_id: String,
        #[arg(long)]
        oracle: PathBuf,
        #[arg(long, default_value_t = 30)]
        bundle_interval_secs: u64,
        /// The file of the bundles that the peg handed to the enforcer. It
        /// defaults to `<oracle>.bundles.json` beside the oracle key.
        #[arg(long)]
        bundle_log: Option<PathBuf>,
        /// Starts a new bundle file while the bridge holds withdrawals. Pass
        /// it only when no copy of the old file exists.
        #[arg(long)]
        new_bundle_log: bool,
    },
}

#[derive(Debug, thiserror::Error)]
enum CliError {
    #[error("`{0}` is not a Solana pubkey")]
    NotAPubkey(String),
    #[error("`{field}` holds {found} characters, and the field takes {want}")]
    WrongHashLength {
        field: &'static str,
        want: usize,
        found: usize,
    },
    #[error("`{field}` is not hex: `{value}`")]
    NotHex { field: &'static str, value: String },
    #[error(transparent)]
    BitcoinAddress(#[from] sol_drivechain_daemon::network::AddressError),
    #[error("the daemon cannot read the oracle keypair at `{path}`")]
    NoOracleKey { path: PathBuf },
    #[error("the daemon cannot read the identity keypair at `{path}`")]
    NoIdentityKey { path: PathBuf },
    #[error("the daemon cannot read `{path}`")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Seed(#[from] sol_drivechain_daemon::seed::SeedError),
    #[error("the daemon cannot write `{path}`")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("the daemon cannot start its runtime")]
    NoRuntime(#[source] std::io::Error),
    #[error("the block record of the validator is empty")]
    EmptyBlockRecord,
    #[error("a bid needs a payee or a commitment")]
    NoCommitment,
    #[error(transparent)]
    Enforcer(#[from] sol_drivechain_daemon::enforcer::EnforcerError),
    #[error("the Solana rpc call `{call}` failed")]
    Rpc {
        call: &'static str,
        #[source]
        source: solana_rpc_client_api::client_error::Error,
    },
    #[error(transparent)]
    Bridge(#[from] sol_drivechain_daemon::bridge::BridgeError),
    #[error("a vault of {0} SOL overflows a u64 lamport count")]
    VaultTooLarge(u64),
    #[error(transparent)]
    Address(#[from] address::AddressError),
    #[error(transparent)]
    Peg(#[from] peg::PegError),
    #[error(transparent)]
    Bmm(#[from] bmm::BmmError),
    #[error(transparent)]
    Credit(#[from] credit::CreditError),
    #[error(transparent)]
    Genesis(#[from] genesis::GenesisError),
    #[error(transparent)]
    Migrate(#[from] migrate::MigrateError),
    #[error(transparent)]
    Books(#[from] books::BooksError),
    #[error("the books do not close")]
    BooksOpen,
    #[error("a fee of {fee_sats} sats is below the {quote} sats that the bridge takes")]
    FeeBelowQuote { fee_sats: u64, quote: u64 },
    #[error("the vault holds less than the supply less the pegged lamports, so no gift is safe")]
    VaultShort,
    #[error("the state file `{path}` is not valid")]
    BadState {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("a bid without --block needs --program-id")]
    NoProgramId,
    #[error("the books need --slot")]
    NoSlot,
    #[error("no recorded block passed rule 3 for eCash height {height} in {secs} seconds")]
    NoBidBlock { height: u64, secs: u64 },
}

fn main() -> Result<(), CliError> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let runtime = tokio::runtime::Runtime::new().map_err(CliError::NoRuntime)?;

    match Cli::parse().command {
        Command::Status { enforcer } => runtime.block_on(async {
            let mut enforcer = enforcer.open().await?;
            let info = enforcer.chain_info().await?;
            println!("network                {:?}", info.network);
            println!(
                "slot activation votes  {}",
                info.unused_slot_activation_threshold + 1
            );
            println!(
                "slot proposal window   {}",
                info.unused_slot_proposal_max_age
            );
            println!("bundle max age         {}", info.withdrawal_bundle_max_age);
            println!(
                "bundle votes           {}",
                info.withdrawal_bundle_inclusion_threshold + 1
            );
            println!("tip                    {}", enforcer.chain_tip().await?);
            match enforcer.ctip().await? {
                Some(ctip) => println!(
                    "treasury               {} sats, sequence {}",
                    ctip.value_sats, ctip.sequence_number
                ),
                None => println!("treasury               none, the slot is not active"),
            }
            for proposal in enforcer.sidechain_proposals().await? {
                println!(
                    "proposal slot {} votes {} height {} age {} hash {}",
                    proposal.sidechain_number,
                    proposal.vote_count,
                    proposal.proposal_height,
                    proposal.proposal_age,
                    proposal.description_hash
                );
            }
            for slot in enforcer.sidechains().await? {
                println!(
                    "slot {} votes {} proposed at {} active at {:?} title {:?}",
                    slot.sidechain_number,
                    slot.vote_count,
                    slot.proposal_height,
                    slot.activation_height,
                    slot.title.as_deref().unwrap_or("")
                );
                println!(
                    "    tarball {} commit {}",
                    slot.hash_id_1.as_deref().unwrap_or("none"),
                    slot.hash_id_2.as_deref().unwrap_or("none")
                );
            }
            Ok(())
        }),
        Command::ProposeSlot { enforcer, declared } => runtime.block_on(async {
            let declared = declared.declaration()?;
            let slot = enforcer.slot;
            let mut enforcer = enforcer.open().await?;
            enforcer.submit_sidechain_proposal(&declared).await?;
            let hash = declared.proposal_hash();
            println!("the proposal for slot {slot} waits for the next block");
            println!("proposal {hash}");
            // The enforcer drops an ACK for a proposal that no block carries,
            // so the vote waits for the M1 block.
            println!("run `ack-slot --slot {slot}` after that block, to start the vote");
            Ok(())
        }),
        Command::AckSlot {
            enforcer,
            declared,
            stop,
            any,
        } => {
            let slot = enforcer.slot;
            runtime.block_on(async {
                let declared = declared.declaration()?;
                let mut enforcer = enforcer.open().await?;
                let proposals = enforcer.sidechain_proposals().await?;
                let Some(proposal) = proposals
                    .iter()
                    .find(|proposal| proposal.sidechain_number == slot as u32)
                else {
                    // The enforcer drops an ACK for a proposal that no block
                    // carries, so the vote waits for the M1 block.
                    println!("no block carries a proposal for slot {slot} yet");
                    return Ok(());
                };
                // A stranger can propose the same slot. Vote only for the
                // proposal whose description this daemon wrote.
                let mine = declared.proposal_hash();
                if proposal.description_hash != mine && !any {
                    println!("slot {slot} carries proposal {}", proposal.description_hash);
                    println!("this title and this description give {mine}");
                    println!("that proposal belongs to somebody else. Pass --any to vote anyway.");
                    return Ok(());
                }
                enforcer
                    .set_sidechain_ack(slot as u32, &proposal.description_hash, !stop)
                    .await?;
                let word = if stop { "stops" } else { "starts" };
                println!(
                    "the miner {word} the vote for slot {slot}, proposal {}",
                    proposal.description_hash
                );
                println!("it holds {} votes today", proposal.vote_count);
                Ok(())
            })
        }
        Command::Mine {
            enforcer,
            blocks,
            address,
            ack_new_slots,
        } => runtime.block_on(async {
            let mut enforcer = enforcer.open().await?;
            let policy = if ack_new_slots {
                AckAllProposalsPolicy::NewSlots
            } else {
                AckAllProposalsPolicy::None
            };
            enforcer.set_ack_all_proposals(policy).await?;
            let hashes = enforcer.generate_to_address(blocks, &address).await?;
            for hash in hashes {
                println!("{hash}");
            }
            Ok(())
        }),
        Command::WalletAddress { enforcer } => runtime.block_on(async {
            let mut enforcer = enforcer.open().await?;
            let (confirmed, pending) = enforcer.wallet_balance().await?;
            // Stdout holds the address alone, so a script captures it whole.
            println!("{}", enforcer.new_wallet_address().await?);
            eprintln!("balance {confirmed} sats confirmed, {pending} sats pending");
            Ok(())
        }),
        Command::Deposit {
            enforcer,
            pubkey,
            sats,
            fee_sats,
        } => {
            let pubkey = parse_pubkey(&pubkey)?;
            runtime.block_on(async {
                let mut enforcer = enforcer.open().await?;
                let recipient = pubkey.to_string();
                let txid = enforcer.create_deposit(&recipient, sats, fee_sats).await?;
                println!("txid {txid}");
                println!("the mainchain carries {recipient}");
                Ok(())
            })
        }
        Command::Watch { enforcer } => {
            let slot = enforcer.slot;
            runtime.block_on(async {
                use futures::StreamExt as _;

                let mut enforcer = enforcer.open().await?;
                let mut stream = enforcer.subscribe_events().await?;
                println!("watching slot {slot}");
                while let Some(item) = stream.next().await {
                    let response = item.map_err(|status| {
                        sol_drivechain_daemon::enforcer::EnforcerError::Call {
                            call: "SubscribeEvents",
                            source: status,
                        }
                    })?;
                    match sol_drivechain_daemon::enforcer::peg_event(response)? {
                        sol_drivechain_daemon::enforcer::PegEvent::Disconnect { block_hash } => {
                            println!("disconnect {block_hash}");
                        }
                        sol_drivechain_daemon::enforcer::PegEvent::Connect {
                            height,
                            deposits,
                            bundles,
                            ..
                        } => {
                            for deposit in deposits {
                                let text = String::from_utf8_lossy(&deposit.address);
                                let parsed = address::deposit_target(&text, slot);
                                println!(
                                    "block {height} deposit seq {} value {} sats address {text}",
                                    deposit.sequence_number, deposit.value_sats
                                );
                                match parsed {
                                    Some(pubkey) => println!("  the target is {pubkey}"),
                                    None => println!("  the address names no target"),
                                }
                            }
                            for bundle in bundles {
                                println!("block {height} bundle {bundle:?}");
                            }
                        }
                    }
                }
                Ok(())
            })
        }
        Command::Withdraw {
            solana_rpc_url,
            program_id,
            user,
            sats,
            fee_sats,
            address,
            network,
        } => {
            let program_id = parse_pubkey(&program_id)?;
            let user_key = read_keypair_file(&user)
                .map_err(|_| CliError::NoOracleKey { path: user.clone() })?;
            let script = sol_drivechain_daemon::network::script_pubkey_of(&address, network)?;
            let rpc = blocking_rpc(&solana_rpc_url);
            let data = rpc
                .get_account_data(&bridge::config_pda(&program_id).0)
                .map_err(|source| CliError::Rpc {
                    call: "getAccountInfo(config)",
                    source,
                })?;
            let config = bridge::Config::decode(&data)?;
            let quote = bridge::MIN_FEE_SATS;
            let fee_sats = fee_sats.unwrap_or(quote);
            if fee_sats < quote {
                return Err(CliError::FeeBelowQuote { fee_sats, quote });
            }
            let lamports = sats
                .checked_mul(bridge::LAMPORTS_PER_SAT)
                .ok_or(CliError::VaultTooLarge(sats))?;
            let instruction = bridge::withdraw_ix(
                &program_id,
                &user_key.pubkey(),
                config.withdrawal_count,
                lamports,
                fee_sats,
                &script,
            );
            send_one(&rpc, &user_key, instruction, "withdraw")?;
            println!(
                "withdrawal {} burns {lamports} lamports, pays {} sats to {address}",
                config.withdrawal_count,
                sats - fee_sats
            );
            Ok(())
        }
        Command::BridgeState {
            solana_rpc_url,
            program_id,
            processed,
            books,
            network,
            enforcer_url,
            slot,
        } => {
            let program_id = parse_pubkey(&program_id)?;
            let commitment = if processed {
                solana_commitment_config::CommitmentConfig::processed()
            } else {
                solana_commitment_config::CommitmentConfig::confirmed()
            };
            let rpc = solana_rpc_client::rpc_client::RpcClient::new_with_commitment(
                solana_rpc_url.clone(),
                commitment,
            );
            let (config_key, _) = bridge::config_pda(&program_id);
            let (vault_key, _) = bridge::vault_pda(&program_id);
            let data = rpc
                .get_account_data(&config_key)
                .map_err(|source| CliError::Rpc {
                    call: "getAccountInfo(config)",
                    source,
                })?;
            let config = bridge::Config::decode(&data)?;
            let vault = rpc
                .get_balance(&vault_key)
                .map_err(|source| CliError::Rpc {
                    call: "getBalance(vault)",
                    source,
                })?;
            let treasury = rpc
                .get_balance(&bridge::treasury_pda(&program_id).0)
                .map_err(|source| CliError::Rpc {
                    call: "getBalance(treasury)",
                    source,
                })?;
            println!("oracle            {}", config.oracle);
            println!("deposit high water {}", config.deposit_high_water);
            println!("pegged lamports   {}", config.pegged_lamports);
            println!("withdrawals       {}", config.withdrawal_count);
            println!("vault lamports    {vault}");
            println!("treasury lamports {treasury}");
            println!("bmm next height   {}", config.bmm_next_height);
            println!("bmm paid total    {}", config.bmm_paid_total);
            println!("credited height   {}", config.credited_height);
            println!(
                "credited block    {}",
                bitcoin::BlockHash::from_byte_array(config.credited_block)
            );
            println!("credit index      {}", config.credit_index);
            println!(
                "credit part block {}",
                bitcoin::BlockHash::from_byte_array(config.credit_part_block)
            );
            println!("stranded lamports {}", config.stranded_lamports);
            println!("deposit confirmations {}", config.deposit_confirmations);
            println!("deposit lag       {}", config.deposit_lag);
            println!("vault supply      {}", config.vault_supply);
            println!("float lamports    {}", config.float_lamports);
            println!(
                "float backed      {}",
                if config.float_backed(vault) {
                    "yes"
                } else {
                    "no"
                }
            );
            println!("paid through      {}", config.paid_through);
            println!("bundled through   {}", config.bundled_through);
            println!("bundle sealed at  {}", config.bundle_sealed_height);
            println!("withdrawal fee    {} sats", bridge::MIN_FEE_SATS);
            if !books {
                return Ok(());
            }
            let slot = slot.ok_or(CliError::NoSlot)?;
            let books = runtime.block_on(async {
                let rpc =
                    solana_rpc_client::nonblocking::rpc_client::RpcClient::new_with_commitment(
                        solana_rpc_url,
                        commitment,
                    );
                let mut enforcer = Enforcer::connect(enforcer_url, u32::from(slot)).await?;
                if let Some(network) = network {
                    enforcer.check_network(network).await?;
                }
                Ok::<_, CliError>(books::read(&rpc, &mut enforcer, &program_id, commitment).await?)
            })?;
            print!("{}", books.report());
            if books.hold() {
                Ok(())
            } else {
                Err(CliError::BooksOpen)
            }
        }
        Command::BackFloat {
            solana_rpc_url,
            program_id,
            payer,
        } => {
            let program_id = parse_pubkey(&program_id)?;
            let payer_key = read_keypair_file(&payer).map_err(|_| CliError::NoIdentityKey {
                path: payer.clone(),
            })?;
            let vault_key = bridge::vault_pda(&program_id).0;
            let (config, vault) = runtime.block_on(async {
                let rpc = solana_rpc_client::nonblocking::rpc_client::RpcClient::new(
                    solana_rpc_url.clone(),
                );
                peg::config_and_vault(
                    &rpc,
                    &program_id,
                    solana_commitment_config::CommitmentConfig::processed(),
                )
                .await
            })?;
            let gift = float_shortfall(&config, vault).ok_or(CliError::VaultShort)?;
            if gift == 0 {
                println!("the float is backed, and withdrawals are open");
                return Ok(());
            }
            let instruction = bridge::transfer_ix(&payer_key.pubkey(), &vault_key, gift);
            send_one(
                &blocking_rpc(&solana_rpc_url),
                &payer_key,
                instruction,
                "transfer",
            )?;
            println!("sent {gift} lamports to the vault, and withdrawals are open");
            Ok(())
        }
        Command::MigrateState {
            solana_rpc_url,
            program_id,
            out,
            accept_ownerless_lamports,
        } => {
            let program_id = parse_pubkey(&program_id)?;
            let state = runtime.block_on(async {
                let rpc =
                    solana_rpc_client::nonblocking::rpc_client::RpcClient::new(solana_rpc_url);
                Ok::<_, CliError>(
                    migrate::export(&rpc, &program_id, accept_ownerless_lamports).await?,
                )
            })?;
            let body =
                serde_json::to_string_pretty(&state).map_err(|source| CliError::BadState {
                    path: out.clone(),
                    source,
                })?;
            std::fs::write(&out, body).map_err(|source| CliError::Write {
                path: out.clone(),
                source,
            })?;
            let copied: u64 = state
                .balances
                .values()
                .chain(state.accounts.iter().map(|account| &account.lamports))
                .fold(0, |sum, lamports| sum.saturating_add(*lamports));
            println!("slot              {}", state.slot);
            println!("pegged lamports   {}", state.cursor.pegged_lamports);
            println!("credited height   {}", state.cursor.credited_height);
            println!(
                "credited block    {}",
                bitcoin::BlockHash::from_byte_array(state.cursor.credited_block)
            );
            println!("credit index      {}", state.cursor.credit_index);
            println!("deposit high water {}", state.cursor.deposit_high_water);
            println!("withdrawals       {}", state.cursor.withdrawal_count);
            println!(
                "open withdrawals  {}",
                state
                    .cursor
                    .withdrawal_count
                    .saturating_sub(state.cursor.paid_through)
            );
            println!("carried accounts  {}", state.accounts.len());
            println!("system accounts   {}", state.balances.len());
            println!(
                "copied lamports   {copied}, and {} in programs and sysvars that the genesis \
                 makes again, of a supply of {}",
                state.fixed_lamports, state.supply_lamports
            );
            println!(
                "ownerless lamports {}, which no program can spend",
                state.ownerless_lamports
            );
            println!("the state is in {}", out.display());
            Ok(())
        }
        Command::StateLamports { state, pubkey } => {
            let state = read_state(&state)?;
            let pubkey = parse_pubkey(&pubkey)?;
            println!(
                "{}",
                state
                    .balances
                    .get(&pubkey.to_string())
                    .copied()
                    .unwrap_or(0)
            );
            Ok(())
        }
        Command::Credit {
            network,
            enforcer_url,
            solana_rpc_url,
            slot,
            program_id,
            payer,
        } => {
            let program_id = parse_pubkey(&program_id)?;
            let payer_key = read_keypair_file(&payer).map_err(|_| CliError::NoIdentityKey {
                path: payer.clone(),
            })?;
            runtime.block_on(async {
                let mut enforcer = sol_drivechain_daemon::enforcer::Enforcer::connect(
                    enforcer_url,
                    u32::from(slot),
                )
                .await?;
                enforcer.check_network(network).await?;
                credit::run(&mut enforcer, solana_rpc_url, program_id, &payer_key, slot).await?;
                Ok(())
            })
        }
        Command::SeedPubkey {
            mnemonic_file,
            account,
            count,
        } => {
            let phrase = read_mnemonic(&mnemonic_file)?;
            for index in account..account.saturating_add(count.max(1)) {
                let pubkey = seed::pubkey_from_mnemonic(&phrase, "", index)?;
                println!("m/44'/501'/{index}'/0'  {pubkey}");
            }
            Ok(())
        }
        Command::SeedKeypair {
            mnemonic_file,
            account,
            out,
        } => {
            let phrase = read_mnemonic(&mnemonic_file)?;
            let keypair = seed::keypair_from_mnemonic(&phrase, "", account)?;
            let body = serde_json::to_string(&seed::keypair_file_bytes(&keypair))
                .expect("a byte array always serializes");
            write_secret(&out, body.as_bytes()).map_err(|source| CliError::Write {
                path: out.clone(),
                source,
            })?;
            println!("account {account} is {}", keypair.pubkey());
            println!("the keypair is in {}", out.display());
            Ok(())
        }
        Command::Derive { program_id } => {
            let program_id = parse_pubkey(&program_id)?;
            println!("program  {program_id}");
            println!("config   {}", bridge::config_pda(&program_id).0);
            println!("vault    {}", bridge::vault_pda(&program_id).0);
            println!("treasury {}", bridge::treasury_pda(&program_id).0);
            println!("bmm answer {}", bridge::BMM_ANSWER_ID);
            println!("block record {}", bridge::BMM_BLOCKS_ID);
            Ok(())
        }
        Command::Address { slot, pubkey } => {
            let pubkey = parse_pubkey(&pubkey)?;
            println!("{}", address::format_deposit_address(slot, &pubkey));
            Ok(())
        }
        Command::ActivationRequest { slot, declared } => {
            println!("{}", activation_request(slot, &declared.declaration()?));
            Ok(())
        }
        Command::Genesis {
            program_id,
            vault_sol,
            oracle,
            oracle_lamports,
            float_lamports,
            bmm_skip_blocks,
            dex_accounts,
            bootstrap_vote,
            bootstrap_stake,
            lamports_per_byte_year,
            ecash_start_height,
            deposit_confirmations,
            deposit_lag,
            state,
            out,
        } => {
            let params = genesis::Params {
                program_id: parse_pubkey(&program_id)?,
                supply_lamports: vault_sol
                    .checked_mul(LAMPORTS_PER_SOL)
                    .ok_or(CliError::VaultTooLarge(vault_sol))?,
                oracle: parse_pubkey(&oracle)?,
                oracle_lamports,
                float_lamports,
                bmm_skip_blocks,
                lamports_per_byte_year,
                ecash_start_height,
                deposit_confirmations,
                deposit_lag,
                bootstrap_keys: [
                    bootstrap_vote.as_deref().map(parse_pubkey).transpose()?,
                    bootstrap_stake.as_deref().map(parse_pubkey).transpose()?,
                ],
            };
            let state = state.as_ref().map(read_state).transpose()?;
            let extra = match &dex_accounts {
                Some(path) => {
                    let text = std::fs::read_to_string(path).map_err(|source| CliError::Read {
                        path: path.clone(),
                        source,
                    })?;
                    genesis::parse_yaml(&text)?
                }
                None => Vec::new(),
            };
            let accounts = genesis::accounts(&params, state.as_ref(), &extra)?;
            std::fs::write(&out, genesis::yaml(&accounts)).map_err(|source| CliError::Write {
                path: out.clone(),
                source,
            })?;
            let config = bridge::config_pda(&params.program_id).0;
            for account in &accounts {
                let name = if account.pubkey == bridge::vault_pda(&params.program_id).0 {
                    "vault"
                } else if account.pubkey == bridge::treasury_pda(&params.program_id).0 {
                    "treasury"
                } else if account.pubkey == config {
                    "config"
                } else if account.pubkey == bridge::BMM_BLOCKS_ID {
                    "block record"
                } else if account.pubkey == params.oracle {
                    "oracle"
                } else {
                    continue;
                };
                println!(
                    "{name} {} holds {} lamports",
                    account.pubkey, account.lamports
                );
            }
            if let Some(state) = &state {
                println!(
                    "the state of slot {} adds {} balances and {} carried accounts",
                    state.slot,
                    state.balances.len(),
                    state.accounts.len()
                );
            }
            println!(
                "D is {deposit_confirmations}, the lag is {deposit_lag}, and BMM settles from \
                 eCash height {}",
                ecash_start_height.max(
                    state
                        .as_ref()
                        .map_or(0, |state| state.cursor.bmm_next_height)
                )
            );
            println!("{} accounts are in {}", accounts.len(), out.display());
            Ok(())
        }
        Command::Run {
            network,
            enforcer_url,
            solana_rpc_url,
            slot,
            program_id,
            oracle,
            bundle_interval_secs,
            bundle_log,
            new_bundle_log,
        } => {
            let program_id = parse_pubkey(&program_id)?;
            let keypair = read_keypair_file(&oracle).map_err(|_| CliError::NoOracleKey {
                path: oracle.clone(),
            })?;
            let settings = peg::Settings {
                network,
                enforcer_url,
                solana_rpc_url,
                sidechain_id: slot,
                program_id,
                bundle_interval: Duration::from_secs(bundle_interval_secs.max(1)),
                bundle_log: bundle_log.unwrap_or_else(|| oracle.with_extension("bundles.json")),
                new_bundle_log,
            };
            runtime.block_on(peg::run(settings, keypair))?;
            Ok(())
        }
        Command::EcashWalk {
            enforcer,
            max_ancestors,
        } => runtime.block_on(async {
            let mut enforcer = enforcer.open().await?;
            let (hash, height) = enforcer.tip().await?;
            let (headers, commitments) = enforcer
                .headers_and_commitments(&hash, max_ancestors)
                .await?;
            println!("tip height {height}");
            println!("headers    {headers}");
            println!("commitments {commitments}");
            Ok(())
        }),
        Command::WalletBalance { enforcer } => runtime.block_on(async {
            let mut enforcer = enforcer.open().await?;
            let (confirmed, pending) = enforcer.wallet_balance().await?;
            println!("confirmed {confirmed}");
            println!("pending   {pending}");
            Ok(())
        }),
        Command::EcashTip { enforcer } => runtime.block_on(async {
            let mut enforcer = enforcer.open().await?;
            let (hash, height) = enforcer.tip().await?;
            println!("{height} {hash}");
            Ok(())
        }),
        Command::BidBmm {
            enforcer,
            solana_rpc_url,
            payee,
            sats,
            commitment,
            withhold,
            block,
            program_id,
            wait_secs,
        } => runtime.block_on(async {
            let rpc = std::sync::Arc::new(
                solana_rpc_client::nonblocking::rpc_client::RpcClient::new_with_commitment(
                    solana_rpc_url,
                    solana_commitment_config::CommitmentConfig::confirmed(),
                ),
            );
            let mut enforcer = enforcer.open().await?;
            let mut tip = enforcer.tip().await?;
            let pair = match payee {
                Some(payee) => {
                    let block = match block {
                        Some(block) => {
                            let block = parse_pubkey(&block)?.to_bytes();
                            if let Some(found) = bmm::recorded_blocks(&rpc)
                                .await?
                                .iter()
                                .find(|recorded| recorded.bank_hash == block)
                            {
                                println!("slot {}", found.slot);
                            }
                            block
                        }
                        None => {
                            let program_id =
                                parse_pubkey(&program_id.ok_or(CliError::NoProgramId)?)?;
                            let found =
                                wait_for_bid_block(&rpc, &mut enforcer, &program_id, wait_secs)
                                    .await?;
                            tip = found.0;
                            println!("slot {}", found.1.slot);
                            found.1.bank_hash
                        }
                    };
                    Some(bmm::Pair {
                        height: u64::from(tip.1) + 1,
                        block,
                        payee: parse_pubkey(&payee)?,
                    })
                }
                None => None,
            };
            let height = u64::from(tip.1) + 1;
            let h_star = match (&pair, &commitment) {
                (Some(pair), _) => pair.commitment(),
                (None, Some(commitment)) => fixed_hash::<32>(commitment, "commitment")?,
                (None, None) => return Err(CliError::NoCommitment),
            };
            println!("commitment {}", sol_drivechain_daemon::hex::encode(&h_star));
            if let Some(pair) = pair {
                println!("block {}", Pubkey::new_from_array(pair.block));
                println!("payee {}", pair.payee);
            }
            let publish = |pair: bmm::Pair| {
                let rpc = rpc.clone();
                async move {
                    let answers = bmm::publish_pairs(&rpc, &[pair]).await?;
                    for outcome in &answers {
                        println!("pair {outcome}");
                    }
                    Ok::<_, CliError>(answers)
                }
            };
            let request =
                async { Ok::<_, CliError>(enforcer.create_bmm_request(sats, tip, &h_star).await?) };
            let txid = publish_before_bid(pair, withhold, publish, request).await?;
            println!("bid {sats} sats for eCash height {height} as {txid}");
            Ok(())
        }),
        Command::PublishPair {
            solana_rpc_url,
            height,
            block,
            payee,
        } => runtime.block_on(async {
            let rpc = solana_rpc_client::nonblocking::rpc_client::RpcClient::new_with_commitment(
                solana_rpc_url,
                solana_commitment_config::CommitmentConfig::confirmed(),
            );
            let pair = bmm::Pair {
                height,
                block: parse_pubkey(&block)?.to_bytes(),
                payee: parse_pubkey(&payee)?,
            };
            for outcome in bmm::publish_pairs(&rpc, &[pair]).await? {
                println!("pair {outcome}");
            }
            Ok(())
        }),
        Command::BmmBlock { solana_rpc_url } => runtime.block_on(async {
            let rpc = solana_rpc_client::nonblocking::rpc_client::RpcClient::new(solana_rpc_url);
            let newest = *bmm::recorded_blocks(&rpc)
                .await?
                .first()
                .ok_or(CliError::EmptyBlockRecord)?;
            println!(
                "{} {}",
                newest.slot,
                Pubkey::new_from_array(newest.bank_hash)
            );
            Ok(())
        }),
        Command::SettleBmm {
            solana_rpc_url,
            program_id,
            identity,
            height,
            block_hash,
            solana_block,
            payee,
        } => {
            let program_id = parse_pubkey(&program_id)?;
            let signer = read_keypair_file(&identity).map_err(|_| CliError::NoIdentityKey {
                path: identity.clone(),
            })?;
            let hash = fixed_hash::<32>(&block_hash, "block_hash")?;
            // The hex of an explorer is the reverse of the internal bytes.
            let mut internal = hash;
            internal.reverse();
            let payee = match payee {
                Some(payee) => parse_pubkey(&payee)?,
                None => signer.pubkey(),
            };
            let solana_block = parse_pubkey(&solana_block)?.to_bytes();
            let rpc = blocking_rpc(&solana_rpc_url);
            let instruction =
                bridge::settle_bmm_ix(&program_id, height, &internal, &solana_block, &payee);
            send_one(&rpc, &signer, instruction, "settle_bmm")?;
            println!("eCash height {height} settled, and the payee is {payee}");
            Ok(())
        }
        Command::Bmm {
            network,
            enforcer_url,
            solana_rpc_url,
            slot,
            program_id,
            identity,
            confirmations,
            bid_percent,
            min_bid_sats,
            max_bid_sats,
            interval_secs,
            rpc_listen,
            no_bid,
        } => {
            let program_id = parse_pubkey(&program_id)?;
            let keypair = read_keypair_file(&identity).map_err(|_| CliError::NoIdentityKey {
                path: identity.clone(),
            })?;
            let settings = bmm::Settings {
                network,
                enforcer_url,
                solana_rpc_url,
                sidechain_id: slot,
                program_id,
                confirmations,
                bid_percent,
                min_bid_sats,
                max_bid_sats,
                interval: Duration::from_secs(interval_secs.max(1)),
                rpc_listen,
                no_bid,
            };
            runtime.block_on(bmm::run(settings, keypair))?;
            Ok(())
        }
    }
}

fn blocking_rpc(url: &str) -> solana_rpc_client::rpc_client::RpcClient {
    solana_rpc_client::rpc_client::RpcClient::new_with_commitment(
        url.to_owned(),
        solana_commitment_config::CommitmentConfig::confirmed(),
    )
}

fn send_one(
    rpc: &solana_rpc_client::rpc_client::RpcClient,
    payer: &solana_sdk::signature::Keypair,
    instruction: solana_sdk::instruction::Instruction,
    call: &'static str,
) -> Result<(), CliError> {
    let blockhash = rpc.get_latest_blockhash().map_err(|source| CliError::Rpc {
        call: "getLatestBlockhash",
        source,
    })?;
    let transaction = solana_sdk::transaction::Transaction::new_signed_with_payer(
        &[instruction],
        Some(&payer.pubkey()),
        &[payer],
        blockhash,
    );
    rpc.send_and_confirm_transaction(&transaction)
        .map_err(|source| CliError::Rpc { call, source })?;
    Ok(())
}

/// The lamports that a gift to the vault must add to back the float, read
/// from one bank. `None` when the vault holds less than the books allow.
fn float_shortfall(config: &bridge::Config, vault: u64) -> Option<u64> {
    let gifts = config.vault_surplus(vault)?;
    Some(config.float_lamports.saturating_sub(gifts))
}

/// Writes a secret file that only the owner reads. The file must be new, and
/// it has mode 600 before the first byte lands.
fn write_secret(path: &std::path::Path, body: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(body)?;
    file.sync_all()
}

/// Reads a seed phrase out of a file, and keeps it out of the process list.
fn read_mnemonic(path: &PathBuf) -> Result<String, CliError> {
    std::fs::read_to_string(path)
        .map(|text| text.trim().to_owned())
        .map_err(|source| CliError::Read {
            path: path.clone(),
            source,
        })
}

/// Builds the JSON that asks a miner to claim and activate a sidechain slot.
///
/// A miner needs two blocks of work. The M1 claims the slot, and it lands only
/// in a block that the miner builds. The M2 votes, and it must land in enough
/// later blocks. This prints every form of both, so the miner uses the one
/// that fits the software they run.
fn activation_request(slot: u8, declared: &Declaration) -> String {
    let hash = declared.proposal_hash();
    let declaration = sol_drivechain_daemon::hex::encode(&declared.bytes());
    let title = declared.title();
    let description = declared.description();
    let hashid1 = sol_drivechain_daemon::hex::encode(declared.hash_id_1());
    let hashid2 = sol_drivechain_daemon::hex::encode(declared.hash_id_2());

    serde_json::json!({
        "sidechain": {
            "slot": slot,
            "title": title,
            "description": description,
            "version": 0,
            "hashid1": hashid1,
            "hashid2": hashid2,
            "declaration_hex": declaration,
            "proposal_hash": hash,
        },
        "bitwindow": {
            "method": "drivechain.v1.DrivechainService/ProposeSidechain",
            "request": {
                "slot": slot,
                "title": title,
                "description": description,
                "version": 0,
                "hashid1": hashid1,
                "hashid2": hashid2,
            },
        },
        "enforcer": {
            "propose": {
                "method": "cusf.mainchain.v1.BlockProducerService/CreateSidechainProposal",
                "request": {
                    "sidechain_id": slot,
                    "declaration": {
                        "v0": {
                            "title": title,
                            "description": description,
                            "hash_id_1": { "hex": hashid1 },
                            "hash_id_2": { "hex": hashid2 },
                        },
                    },
                },
            },
            "ack": {
                "method": "cusf.mainchain.v1.BlockProducerService/SetSidechainAck",
                "request": {
                    "sidechain_number": slot,
                    "description_sha256d_hash": { "hex": hash },
                    "ack": true,
                },
            },
        },
        "coinbase": {
            "m1_propose_script_hex": declared.m1_script(slot),
            "m2_ack_script_hex": declared.m2_script(slot),
        },
    })
    .to_string()
}

fn parse_pubkey(text: &str) -> Result<Pubkey, CliError> {
    Pubkey::from_str(text).map_err(|_| CliError::NotAPubkey(text.to_owned()))
}

fn read_state(path: &PathBuf) -> Result<migrate::State, CliError> {
    let text = std::fs::read_to_string(path).map_err(|source| CliError::Read {
        path: path.clone(),
        source,
    })?;
    serde_json::from_str(&text).map_err(|source| CliError::BadState {
        path: path.clone(),
        source,
    })
}

/// Publishes `pair` unless `withhold`, and sends `bid` only after the
/// validator holds the pair.
async fn publish_before_bid<T, E, P, PF, B>(
    pair: Option<bmm::Pair>,
    withhold: bool,
    publish: P,
    bid: B,
) -> Result<T, E>
where
    E: From<bmm::BmmError>,
    P: FnOnce(bmm::Pair) -> PF,
    PF: std::future::Future<Output = Result<Vec<String>, E>>,
    B: std::future::Future<Output = Result<T, E>>,
{
    if let Some(pair) = pair.filter(|_| !withhold) {
        bmm::held(pair.height, publish(pair).await?)?;
    }
    bid.await
}

/// Waits for a recorded block of the followed branch that passes rule 3 for
/// the next eCash block. Gives the eCash tip of the check and the block.
async fn wait_for_bid_block(
    rpc: &std::sync::Arc<solana_rpc_client::nonblocking::rpc_client::RpcClient>,
    enforcer: &mut Enforcer,
    program_id: &Pubkey,
    wait_secs: u64,
) -> Result<((bitcoin::BlockHash, u32), bridge::RecordedBlock), CliError> {
    let live = bmm::Live::new(rpc.clone(), enforcer.clone(), *program_id);
    let config_key = bridge::config_pda(program_id).0;
    let mut height = 0;
    for _ in 0..=wait_secs {
        let tip = enforcer.tip().await?;
        height = u64::from(tip.1) + 1;
        let data = rpc
            .get_account_with_commitment(
                &config_key,
                solana_commitment_config::CommitmentConfig::processed(),
            )
            .await
            .map_err(|source| CliError::Rpc {
                call: "getAccountInfo(config)",
                source,
            })?
            .value
            .map(|account| account.data)
            .unwrap_or_default();
        let lag = bridge::Config::decode(&data)?.deposit_lag;
        if let Some(block) = bmm::bid_block(&live, tip, height, lag).await? {
            return Ok((tip, block));
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    Err(CliError::NoBidBlock {
        height,
        secs: wait_secs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_cli_publishes_its_pair_before_the_bid_and_withhold_keeps_it_back() {
        let pair = bmm::Pair {
            height: 7,
            block: [1; 32],
            payee: Pubkey::new_unique(),
        };
        for withhold in [false, true] {
            let events = std::sync::Mutex::new(Vec::new());
            let publish = |pair: bmm::Pair| {
                events
                    .lock()
                    .unwrap()
                    .push(format!("publish {}", pair.height));
                async { Ok::<_, CliError>(vec!["new".to_owned()]) }
            };
            let bid = async {
                events.lock().unwrap().push("bid".to_string());
                Err::<(), CliError>(CliError::NoCommitment)
            };
            assert!(publish_before_bid(Some(pair), withhold, publish, bid)
                .await
                .is_err());
            let expected = if withhold {
                vec!["bid"]
            } else {
                vec!["publish 7", "bid"]
            };
            assert_eq!(*events.lock().unwrap(), expected);
        }
    }

    #[tokio::test]
    async fn the_cli_sends_no_bid_after_the_validator_refused_its_pair() {
        let pair = bmm::Pair {
            height: 7,
            block: [1; 32],
            payee: Pubkey::new_unique(),
        };
        let bid_sent = std::sync::atomic::AtomicBool::new(false);
        let publish = |_| async { Ok::<_, CliError>(vec![bmm::FULL_HEIGHT_REFUSAL.to_owned()]) };
        let bid = async {
            bid_sent.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok::<_, CliError>(())
        };
        let refused = publish_before_bid(Some(pair), false, publish, bid).await;
        assert!(!bid_sent.load(std::sync::atomic::Ordering::SeqCst));
        assert!(matches!(
            refused,
            Err(CliError::Bmm(bmm::BmmError::PairRefused { height: 7, .. }))
        ));
    }

    #[test]
    fn an_empty_hash_reads_as_zeros() {
        assert_eq!(fixed_hash::<32>("", "hashid1").unwrap(), [0u8; 32]);
        assert_eq!(fixed_hash::<20>("", "hashid2").unwrap(), [0u8; 20]);
    }

    #[test]
    fn a_full_hash_reads() {
        let hex = "c46e3297057b396024798ccfe05c4d1488dbc9f14f47af5858f1a81479e3d742";
        let bytes = fixed_hash::<32>(hex, "hashid1").unwrap();
        assert_eq!(bytes[0], 0xc4);
        assert_eq!(bytes[31], 0x42);
    }

    #[test]
    fn a_short_hash_fails() {
        let error = fixed_hash::<32>("abcd", "hashid1").unwrap_err();
        assert!(matches!(error, CliError::WrongHashLength { want: 64, .. }));
    }

    #[test]
    fn a_hash_that_is_not_hex_fails() {
        let error = fixed_hash::<20>(&"z".repeat(40), "hashid2").unwrap_err();
        assert!(matches!(error, CliError::NotHex { .. }));
    }

    #[test]
    fn the_activation_request_names_both_hashes() {
        let declared = Declaration::with_hashes(
            "sol-drivechain",
            "A Solana sidechain",
            [0xaa; 32],
            [0xbb; 20],
        )
        .unwrap();
        let json: serde_json::Value =
            serde_json::from_str(&activation_request(8, &declared)).unwrap();
        assert_eq!(json["sidechain"]["hashid1"], "aa".repeat(32));
        assert_eq!(json["sidechain"]["hashid2"], "bb".repeat(20));
        assert_eq!(json["bitwindow"]["request"]["hashid1"], "aa".repeat(32));
        assert_eq!(
            json["enforcer"]["propose"]["request"]["declaration"]["v0"]["hash_id_2"]["hex"],
            "bb".repeat(20)
        );
    }

    #[test]
    fn the_activation_request_carries_both_coinbase_scripts() {
        let declared = Declaration::new("sol-drivechain", "A Solana sidechain").unwrap();
        let json: serde_json::Value =
            serde_json::from_str(&activation_request(8, &declared)).unwrap();
        assert_eq!(
            json["coinbase"]["m1_propose_script_hex"],
            declared.m1_script(8)
        );
        assert_eq!(json["coinbase"]["m2_ack_script_hex"], declared.m2_script(8));
        assert_eq!(json["sidechain"]["proposal_hash"], declared.proposal_hash());
    }

    #[test]
    fn twenty_one_million_sol_fits_a_u64() {
        assert_eq!(
            21_000_000u64.checked_mul(LAMPORTS_PER_SOL),
            Some(21_000_000_000_000_000)
        );
    }

    #[test]
    fn the_run_command_takes_every_network() {
        for want in PegNetwork::ALL {
            let cli = Cli::parse_from([
                "daemon",
                "run",
                "--network",
                want.name(),
                "--slot",
                "8",
                "--program-id",
                "ARA8mQfWk85ULDLAujy3b8gLknFF2QzrsAKHYbc8beu",
                "--oracle",
                "/tmp/oracle.json",
            ]);
            let Command::Run { network, .. } = cli.command else {
                panic!("the parser did not pick the run command");
            };
            assert_eq!(network, want);
        }
    }

    #[test]
    fn a_bad_pubkey_fails() {
        assert!(matches!(
            parse_pubkey("not a pubkey"),
            Err(CliError::NotAPubkey(_))
        ));
    }

    #[test]
    fn the_cli_parses_the_run_command() {
        let cli = Cli::parse_from([
            "daemon",
            "run",
            "--slot",
            "8",
            "--program-id",
            "ARA8mQfWk85ULDLAujy3b8gLknFF2QzrsAKHYbc8beu",
            "--oracle",
            "/tmp/oracle.json",
        ]);
        let Command::Run {
            network,
            slot,
            bundle_interval_secs,
            ..
        } = cli.command
        else {
            panic!("the parser did not pick the run command");
        };
        assert_eq!(network, PegNetwork::Regtest);
        assert_eq!(slot, 8);
        assert_eq!(bundle_interval_secs, 30);
    }

    fn a_config(pegged_lamports: u64, float_lamports: u64) -> bridge::Config {
        bridge::Config {
            oracle: Pubkey::default(),
            deposit_high_water: 0,
            pegged_lamports,
            withdrawal_count: 0,
            bmm_next_height: 0,
            bmm_paid_total: 0,
            deposit_confirmations: 6,
            deposit_lag: 100,
            credited_height: 0,
            credited_block: [0u8; 32],
            credit_index: 0,
            credit_part_block: [0u8; 32],
            stranded_lamports: 0,
            bump: 0,
            vault_bump: 0,
            treasury_bump: 0,
            vault_supply: 1_000_000,
            float_lamports,
            paid_through: 0,
            bundled_through: 0,
            bundle_sealed_height: 0,
            bmm_skip_blocks: 0,
            bmm_next_since_slot: 0,
            bundle_m6id: [0u8; 32],
        }
    }

    #[test]
    fn the_shortfall_is_the_float_less_the_gifts() {
        let config = a_config(400_000, 5_000);
        assert_eq!(float_shortfall(&config, 600_000), Some(5_000));
        assert_eq!(float_shortfall(&config, 603_000), Some(2_000));
        assert_eq!(float_shortfall(&config, 605_000), Some(0));
        assert_eq!(float_shortfall(&config, 700_000), Some(0));
        assert!(config.float_backed(600_000 + 5_000));
    }

    #[test]
    fn a_credit_between_two_reads_gives_no_extra_gift() {
        // A credit moves lamports from the vault to pegged in one bank, so
        // the shortfall of one bank stays the same.
        let before = a_config(400_000, 5_000);
        let mut after = before;
        after.pegged_lamports += 30_000;
        assert_eq!(float_shortfall(&before, 600_000), Some(5_000));
        assert_eq!(float_shortfall(&after, 570_000), Some(5_000));
        // The config of the old bank and the vault of the new one look short,
        // so no gift goes out.
        assert_eq!(float_shortfall(&before, 570_000), None);
    }

    fn a_temp_path(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("sol-drivechain-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("secret.json");
        let _ = std::fs::remove_file(&path);
        path
    }

    #[test]
    fn a_secret_file_lands_with_mode_600() {
        let path = a_temp_path("mode");
        write_secret(&path, b"[1,2,3]").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(std::fs::read(&path).unwrap(), b"[1,2,3]");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn a_secret_file_never_replaces_a_file() {
        let path = a_temp_path("exists");
        std::fs::write(&path, b"keep").unwrap();
        assert!(write_secret(&path, b"[1,2,3]").is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"keep");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn each_enforcer_command_asks_for_the_network() {
        const ID: &str = "ARA8mQfWk85ULDLAujy3b8gLknFF2QzrsAKHYbc8beu";
        let bmm = [
            "daemon",
            "bmm",
            "--slot",
            "8",
            "--program-id",
            ID,
            "--identity",
            "/tmp/id.json",
            "--max-bid-sats",
            "5000",
        ];
        assert!(Cli::try_parse_from(bmm).is_err());
        let cli = Cli::try_parse_from(bmm.iter().chain(&["--network", "ecash"])).unwrap();
        let Command::Bmm {
            network,
            max_bid_sats,
            ..
        } = cli.command
        else {
            panic!("the parser did not pick the bmm command");
        };
        assert_eq!((network, max_bid_sats), (PegNetwork::Ecash, 5000));
        let no_cap = [
            "daemon",
            "bmm",
            "--network",
            "ecash",
            "--slot",
            "8",
            "--program-id",
            ID,
            "--identity",
            "/tmp/id.json",
        ];
        assert!(Cli::try_parse_from(no_cap).is_err());
        let credit = [
            "daemon",
            "credit",
            "--slot",
            "8",
            "--program-id",
            ID,
            "--payer",
            "/tmp/p.json",
        ];
        assert!(Cli::try_parse_from(credit).is_err());
        let books = [
            "daemon",
            "bridge-state",
            "--program-id",
            ID,
            "--books",
            "--slot",
            "8",
        ];
        assert!(Cli::try_parse_from(books).is_err());
        assert!(Cli::try_parse_from(books.iter().chain(&["--network", "ecash"])).is_ok());
    }
}
