use std::{os::unix::fs::PermissionsExt as _, path::PathBuf, str::FromStr as _, time::Duration};

use clap::{Parser, Subcommand};
use sol_drivechain_daemon::enforcer::{Declaration, Enforcer};
use sol_drivechain_daemon::network::PegNetwork;
use sol_drivechain_daemon::proto::mainchain::AckAllProposalsPolicy;
use sol_drivechain_daemon::{address, bmm, bridge, peg, seed};
use solana_sdk::{pubkey::Pubkey, signature::read_keypair_file, signer::Signer as _};

/// The lamport count of one SOL.
const LAMPORTS_PER_SOL: u64 = 1_000_000_000;

#[derive(Parser)]
#[command(version, about = "The oracle daemon of the SOL drivechain")]
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
    /// Creates the bridge config account. Run this one time per chain.
    Initialize {
        #[arg(long, default_value = "http://127.0.0.1:8899")]
        solana_rpc_url: String,
        #[arg(long)]
        program_id: String,
        /// The keypair that pays the rent of the config account.
        #[arg(long)]
        payer: PathBuf,
        /// The keypair whose pubkey credits every deposit.
        #[arg(long)]
        oracle: PathBuf,
        /// The first eCash height that BMM settles, normally the current tip.
        #[arg(long)]
        bmm_start_height: u64,
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
        /// The mainchain fee. It comes out of the amount.
        #[arg(long, default_value_t = 10_000)]
        fee_sats: u64,
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
    /// Writes the primordial accounts file for the genesis.
    Genesis {
        #[arg(long)]
        program_id: String,
        #[arg(long, default_value_t = 21_000_000)]
        vault_sol: u64,
        /// The oracle account. It pays the rent of every deposit claim.
        #[arg(long)]
        oracle: Option<String>,
        #[arg(long, default_value_t = 10)]
        oracle_sol: u64,
        /// The rent reserve of the empty treasury account. Every fee goes to
        /// the treasury, so it must exist from the genesis on.
        #[arg(long)]
        treasury_lamports: u64,
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
    /// Prints the height and the hash of the eCash tip.
    EcashTip {
        #[command(flatten)]
        enforcer: EnforcerArgs,
    },
    /// Settles one eCash height by hand. A test or an operator uses it.
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
        /// The block hash in the hex that the explorers show.
        #[arg(long)]
        block_hash: String,
        /// The payee in the BMM commitment. It defaults to the signer.
        #[arg(long)]
        payee: Option<String>,
    },
    /// Bids for eCash blocks, and settles each eCash height on Solana.
    Bmm {
        #[arg(long, default_value = "http://127.0.0.1:50051")]
        enforcer_url: String,
        #[arg(long, default_value = "http://127.0.0.1:8899")]
        solana_rpc_url: String,
        #[arg(long)]
        slot: u8,
        #[arg(long)]
        program_id: String,
        /// The keypair that signs, and whose pubkey is the BMM commitment and
        /// the payee. Normally the validator identity.
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
        #[arg(long, default_value_t = 2)]
        interval_secs: u64,
    },
    /// Runs the peg. It credits deposits and it proposes withdrawal bundles.
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
        /// Defaults to the safe count of the network.
        #[arg(long)]
        confirmations: Option<u32>,
        #[arg(long, default_value_t = 30)]
        bundle_interval_secs: u64,
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
            println!("{}", enforcer.new_wallet_address().await?);
            println!("balance {confirmed} sats confirmed, {pending} sats pending");
            Ok(())
        }),
        Command::Deposit {
            enforcer,
            pubkey,
            sats,
            fee_sats,
        } => {
            let pubkey = parse_pubkey(&pubkey)?;
            let slot = enforcer.slot;
            runtime.block_on(async {
                let mut enforcer = enforcer.open().await?;
                // The enforcer writes this string into the OP_RETURN with no
                // change, so the caller adds the slot and the checksum.
                let deposit_address = address::format_deposit_address(slot, &pubkey);
                let txid = enforcer
                    .create_deposit(&deposit_address, sats, fee_sats)
                    .await?;
                println!("txid {txid}");
                println!("the mainchain carries {deposit_address}");
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
                                let parsed = address::parse_deposit_address(slot, &text);
                                println!(
                                    "block {height} deposit seq {} value {} sats address {text}",
                                    deposit.sequence_number, deposit.value_sats
                                );
                                match parsed {
                                    Ok(pubkey) => println!("  the daemon credits {pubkey}"),
                                    Err(error) => println!("  the address fails: {error}"),
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
        Command::Initialize {
            solana_rpc_url,
            program_id,
            payer,
            oracle,
            bmm_start_height,
        } => {
            let program_id = parse_pubkey(&program_id)?;
            let payer_key = read_keypair_file(&payer).map_err(|_| CliError::NoOracleKey {
                path: payer.clone(),
            })?;
            let oracle_key = read_keypair_file(&oracle).map_err(|_| CliError::NoOracleKey {
                path: oracle.clone(),
            })?;
            let rpc = blocking_rpc(&solana_rpc_url);
            let instruction = bridge::initialize_ix(
                &program_id,
                &payer_key.pubkey(),
                &oracle_key.pubkey(),
                bmm_start_height,
            );
            send_one(&rpc, &payer_key, instruction, "initialize")?;
            println!("the bridge config holds oracle {}", oracle_key.pubkey());
            println!("BMM settles from eCash height {bmm_start_height}");
            println!("config {}", bridge::config_pda(&program_id).0);
            Ok(())
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
        } => {
            let program_id = parse_pubkey(&program_id)?;
            let rpc = blocking_rpc(&solana_rpc_url);
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
            Ok(())
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
            std::fs::write(&out, body).map_err(|source| CliError::Write {
                path: out.clone(),
                source,
            })?;
            let readable = std::fs::Permissions::from_mode(0o600);
            std::fs::set_permissions(&out, readable).map_err(|source| CliError::Write {
                path: out.clone(),
                source,
            })?;
            println!("account {account} is {}", keypair.pubkey());
            println!("the keypair is in {}", out.display());
            Ok(())
        }
        Command::Derive { program_id } => {
            let program_id = parse_pubkey(&program_id)?;
            println!("program {program_id}");
            println!("config  {}", bridge::config_pda(&program_id).0);
            println!("vault   {}", bridge::vault_pda(&program_id).0);
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
            oracle_sol,
            treasury_lamports,
            out,
        } => {
            let program_id = parse_pubkey(&program_id)?;
            let lamports = vault_sol
                .checked_mul(LAMPORTS_PER_SOL)
                .ok_or(CliError::VaultTooLarge(vault_sol))?;
            let vault = bridge::vault_pda(&program_id).0;
            let mut body = primordial_yaml(&vault, lamports);
            let treasury = bridge::treasury_pda(&program_id).0;
            body.push_str(&primordial_yaml(&treasury, treasury_lamports));
            println!("treasury {treasury} holds {treasury_lamports} lamports");
            if let Some(oracle) = oracle {
                let oracle = parse_pubkey(&oracle)?;
                let oracle_lamports = oracle_sol
                    .checked_mul(LAMPORTS_PER_SOL)
                    .ok_or(CliError::VaultTooLarge(oracle_sol))?;
                body.push_str(&primordial_yaml(&oracle, oracle_lamports));
                println!("oracle {oracle} holds {oracle_lamports} lamports");
            }
            std::fs::write(&out, body).map_err(|source| CliError::Write {
                path: out.clone(),
                source,
            })?;
            println!(
                "vault {vault} holds {lamports} lamports in {}",
                out.display()
            );
            Ok(())
        }
        Command::Run {
            network,
            enforcer_url,
            solana_rpc_url,
            slot,
            program_id,
            oracle,
            confirmations,
            bundle_interval_secs,
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
                confirmations: confirmations.unwrap_or_else(|| network.default_confirmations()),
                bundle_interval: Duration::from_secs(bundle_interval_secs.max(1)),
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
        Command::EcashTip { enforcer } => runtime.block_on(async {
            let mut enforcer = enforcer.open().await?;
            let (hash, height) = enforcer.tip().await?;
            println!("{height} {hash}");
            Ok(())
        }),
        Command::SettleBmm {
            solana_rpc_url,
            program_id,
            identity,
            height,
            block_hash,
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
            let rpc = blocking_rpc(&solana_rpc_url);
            let instruction = bridge::settle_bmm_ix(&program_id, height, &internal, &payee);
            send_one(&rpc, &signer, instruction, "settle_bmm")?;
            println!("eCash height {height} settled, and the payee is {payee}");
            Ok(())
        }
        Command::Bmm {
            enforcer_url,
            solana_rpc_url,
            slot,
            program_id,
            identity,
            confirmations,
            bid_percent,
            min_bid_sats,
            interval_secs,
        } => {
            let program_id = parse_pubkey(&program_id)?;
            let keypair = read_keypair_file(&identity).map_err(|_| CliError::NoIdentityKey {
                path: identity.clone(),
            })?;
            let settings = bmm::Settings {
                enforcer_url,
                solana_rpc_url,
                sidechain_id: slot,
                program_id,
                confirmations,
                bid_percent,
                min_bid_sats,
                interval: Duration::from_secs(interval_secs.max(1)),
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

/// Builds the `--primordial-accounts-file` body that `solana-genesis` reads.
///
/// The vault is a system account with no data, so a program signs a transfer
/// out of it with the vault seeds.
fn primordial_yaml(vault: &Pubkey, lamports: u64) -> String {
    format!(
        "{vault}:\n  balance: {lamports}\n  owner: {}\n  data: \"\"\n  executable: false\n",
        bridge::SYSTEM_PROGRAM_ID
    )
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn the_primordial_file_names_the_vault_and_the_system_program() {
        let vault = Pubkey::new_from_array([2u8; 32]);
        let yaml = primordial_yaml(&vault, 21_000_000_000_000_000);
        assert!(yaml.starts_with(&format!("{vault}:")));
        assert!(yaml.contains("owner: 11111111111111111111111111111111"));
        assert!(yaml.contains("balance: 21000000000000000"));
        assert!(yaml.contains("executable: false"));
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
            confirmations,
            bundle_interval_secs,
            ..
        } = cli.command
        else {
            panic!("the parser did not pick the run command");
        };
        assert_eq!(network, PegNetwork::Regtest);
        assert_eq!(slot, 8);
        assert_eq!(confirmations, None);
        assert_eq!(bundle_interval_secs, 30);
    }
}
