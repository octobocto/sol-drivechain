//! The peg loop. It seals one bundle at a time, proposes it to the
//! enforcer, and marks it paid only after eCash paid it on the active chain.
//!
//! Every pass reads its state again from Solana and from eCash, so a restart,
//! a lost event, or a reorg leaves nothing behind.

use std::{ops::Range, time::Duration};

use bitcoin::{hashes::Hash as _, ScriptBuf, Transaction as BitcoinTransaction, Txid};
use solana_commitment_config::CommitmentConfig;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    account::Account,
    instruction::Instruction,
    pubkey::Pubkey,
    signature::{Keypair, Signer as _},
    transaction::Transaction as SolanaTransaction,
};

use crate::{
    bridge::{self, Config, WithdrawalRecord},
    credit,
    enforcer::{BundleEvent, Enforcer, PegEvent},
    loaded,
    m6::{self, Payout},
    proto::mainchain::WithdrawalBundlePolicy,
};

/// The most records that one close tx takes. Records with large owners take
/// fewer, so the tx stays below the loaded data limit.
const CLOSE_BATCH: usize = 10;

pub struct Settings {
    pub network: crate::network::PegNetwork,
    pub enforcer_url: String,
    pub solana_rpc_url: String,
    pub sidechain_id: u8,
    pub program_id: Pubkey,
    pub bundle_interval: Duration,
    /// The file of the bundles that the daemon handed to its enforcer.
    pub bundle_log: std::path::PathBuf,
    /// Starts a new bundle file while the bridge holds withdrawals.
    pub new_bundle_log: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum PegError {
    #[error(transparent)]
    Enforcer(#[from] crate::enforcer::EnforcerError),
    #[error(transparent)]
    Bridge(#[from] crate::bridge::BridgeError),
    #[error(transparent)]
    M6(#[from] crate::m6::M6Error),
    #[error(transparent)]
    Credit(#[from] crate::credit::CreditError),
    #[error("the Solana rpc call `{call}` failed")]
    Rpc {
        call: &'static str,
        #[source]
        source: solana_rpc_client_api::client_error::Error,
    },
    #[error("the live bundle holds record {0}, and the chain holds no such record")]
    MissingRecord(u64),
    #[error("eCash height {0} does not fit a u32")]
    HeightTooLarge(u64),
    #[error(transparent)]
    Proposals(#[from] crate::proposals::ProposalsError),
    #[error(
        "records {start} to {end} overlap bundle {m6id}, which the enforcer keeps and eCash can \
         still pay or paid. The peg seals and proposes nothing until an operator acts"
    )]
    Overlap { m6id: String, start: u64, end: u64 },
    #[error(
        "records from {start} can repeat a paid bundle below record {dropped_through}, so the peg \
         seals and proposes nothing until an operator acts"
    )]
    BelowDropped { start: u64, dropped_through: u64 },
    #[error("the proposal file holds `{0}`, which is no M6ID")]
    BadM6id(String),
    #[error(
        "the finalized records of the live bundle give M6ID {built}, but the bridge sealed \
         {sealed}. eCash never pays the sealed M6ID, so the peg stops until an operator acts"
    )]
    SealedM6idMismatch { built: Txid, sealed: Txid },
}

/// The record that `account` holds at `index`, if it is one: the program
/// owns it, it decodes, and it names the same index. Anyone can send
/// lamports to the address of a closed record, so anything else is skipped.
pub fn record_of(program_id: &Pubkey, index: u64, account: &Account) -> Option<WithdrawalRecord> {
    if account.owner != *program_id {
        return None;
    }
    let record = WithdrawalRecord::decode(&account.data).ok()?;
    (record.index == index).then_some(record)
}

/// Reads the withdrawal records in `range`, lowest index first, and skips
/// each address that holds no record.
pub async fn read_records(
    rpc: &RpcClient,
    program_id: &Pubkey,
    range: Range<u64>,
    commitment: CommitmentConfig,
) -> Result<Vec<WithdrawalRecord>, PegError> {
    let indices: Vec<u64> = range.collect();
    let mut records = Vec::new();
    for chunk in indices.chunks(100) {
        let keys: Vec<Pubkey> = chunk
            .iter()
            .map(|index| bridge::withdrawal_pda(program_id, *index).0)
            .collect();
        let accounts = rpc
            .get_multiple_accounts_with_commitment(&keys, commitment)
            .await
            .map_err(|source| PegError::Rpc {
                call: "getMultipleAccounts",
                source,
            })?
            .value;
        for (index, account) in chunk.iter().zip(accounts) {
            let Some(account) = account else { continue };
            match record_of(program_id, *index, &account) {
                Some(record) => records.push(record),
                None => tracing::warn!(
                    index,
                    owner = %account.owner,
                    "the address of a withdrawal record holds no record, so the peg skips it"
                ),
            }
        }
    }
    Ok(records)
}

/// The records of the live bundle. Each one must be there, because a bundle
/// without one has another M6ID.
pub async fn live_records(
    rpc: &RpcClient,
    program_id: &Pubkey,
    live: Range<u64>,
    commitment: CommitmentConfig,
) -> Result<Vec<WithdrawalRecord>, PegError> {
    let records = read_records(rpc, program_id, live.clone(), commitment).await?;
    complete(live, records)
}

/// Checks that `records` holds each index of `live` in order.
pub fn complete(
    live: Range<u64>,
    records: Vec<WithdrawalRecord>,
) -> Result<Vec<WithdrawalRecord>, PegError> {
    let mut found = records.iter().map(|record| record.index);
    for index in live {
        if found.next() != Some(index) {
            return Err(PegError::MissingRecord(index));
        }
    }
    Ok(records)
}

/// Builds the blinded M6 of a bundle, the payouts in record order. The same
/// records always give the same M6ID. The lock time holds the index of the
/// first record, so two bundles with equal payouts still get two M6IDs.
pub fn bundle_of(records: &[WithdrawalRecord]) -> Result<BitcoinTransaction, crate::m6::M6Error> {
    let mut fee_sats: u64 = 0;
    let mut payouts = Vec::with_capacity(records.len());
    for record in records {
        fee_sats = fee_sats
            .checked_add(record.fee_sats)
            .ok_or(crate::m6::M6Error::PayoutOverflow)?;
        payouts.push(Payout {
            script_pubkey: ScriptBuf::from_bytes(record.script_pubkey.clone()),
            sats: record.payout_sats,
        });
    }
    let mut blinded = m6::build_blinded_m6(fee_sats, &payouts)?;
    let first = records.first().map_or(0, |record| record.index);
    // Each treasury input of the M6 has the final sequence, so any lock time
    // is final.
    let first_u32 =
        u32::try_from(first).map_err(|_| crate::m6::M6Error::FirstRecordTooLarge(first))?;
    blinded.lock_time = bitcoin::absolute::LockTime::from_consensus(first_u32);
    Ok(blinded)
}

/// What eCash did with a bundle, from its newest event on the active chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BundleStatus {
    Unknown,
    Submitted { height: u32 },
    Failed { height: u32 },
    Succeeded { height: u32 },
}

/// The status of bundle `m6id` after `events`, which hold the peg events of
/// the active chain, oldest first. A payment is final: a miner can propose a
/// paid M6ID again, and that later M3 or its failure changes nothing.
pub fn bundle_status(events: &[PegEvent], m6id: Txid) -> BundleStatus {
    let mut status = BundleStatus::Unknown;
    for event in events {
        let PegEvent::Connect {
            height, bundles, ..
        } = event
        else {
            continue;
        };
        for bundle in bundles {
            status = match bundle {
                BundleEvent::Succeeded { m6id: id, .. } if *id == m6id => {
                    return BundleStatus::Succeeded { height: *height };
                }
                BundleEvent::Submitted(id) if *id == m6id => {
                    BundleStatus::Submitted { height: *height }
                }
                BundleEvent::Failed(id) if *id == m6id => BundleStatus::Failed { height: *height },
                _ => status,
            };
        }
    }
    status
}

/// The status of `m6id` after `prev` and then `events`. A payment stays.
pub fn advance(prev: BundleStatus, events: &[PegEvent], m6id: Txid) -> BundleStatus {
    if matches!(prev, BundleStatus::Succeeded { .. }) {
        return prev;
    }
    match bundle_status(events, m6id) {
        BundleStatus::Unknown => prev,
        status => status,
    }
}

/// Reads what the active eCash chain below `tip` did with bundle `m6id`. The
/// M3 of a bundle comes after its seal, so the read starts at `sealed_height`.
pub async fn bundle_status_at(
    enforcer: &mut Enforcer,
    tip: (bitcoin::BlockHash, u32),
    sealed_height: u64,
    m6id: Txid,
) -> Result<BundleStatus, PegError> {
    let start = height_u32(sealed_height)?;
    let events = enforcer.peg_data_from(tip, start.min(tip.1)).await?;
    Ok(bundle_status(&events, m6id))
}

fn height_u32(height: u64) -> Result<u32, PegError> {
    u32::try_from(height).map_err(|_| PegError::HeightTooLarge(height))
}

/// The part of the eCash history of the live bundle that a pass read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scan {
    pub m6id: Txid,
    /// The newest block that the scan read.
    pub height: u32,
    pub hash: bitcoin::BlockHash,
    pub status: BundleStatus,
}

/// Reads the status of `m6id` up to `tip`. A scan of an earlier pass counts
/// only while the active chain still holds its newest block, so a reorg
/// reads the whole history again.
async fn scan_bundle(
    enforcer: &mut Enforcer,
    tip: (bitcoin::BlockHash, u32),
    sealed_height: u64,
    m6id: Txid,
    last: Option<Scan>,
) -> Result<Scan, PegError> {
    let kept = match last {
        Some(last) if last.m6id == m6id && last.height <= tip.1 => {
            let (hash, _) = enforcer.block_at(tip, last.height).await?;
            (hash == last.hash).then_some(last)
        }
        _ => None,
    };
    let (from, prev) = match kept {
        Some(last) => (last.height, last.status),
        None => (height_u32(sealed_height)?.min(tip.1), BundleStatus::Unknown),
    };
    let events = enforcer.peg_data_from(tip, from).await?;
    Ok(Scan {
        m6id,
        height: tip.1,
        hash: tip.0,
        status: advance(prev, &events, m6id),
    })
}

/// The next step of the peg.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Wait,
    Seal,
    Propose,
    MarkPaid { through: u64, height: u32 },
}

/// What one pass of the peg sees.
#[derive(Clone, Debug)]
pub struct View {
    /// The config of the branch that the validator follows.
    pub processed: Config,
    /// The config at the BMM root. Only a final bundle goes to eCash.
    pub finalized: Config,
    /// What eCash did with the final live bundle.
    pub status: BundleStatus,
    pub tip_height: u32,
}

/// Picks the next step. One bundle is live at a time, and a record never
/// leaves its bundle, so the M6ID of a bundle never changes.
pub fn decide(view: &View) -> Action {
    let processed = &view.processed;
    if processed.live_bundle().is_none() {
        if processed.withdrawal_count > processed.bundled_through {
            return Action::Seal;
        }
        return Action::Wait;
    }
    let Some(live) = view.finalized.live_bundle() else {
        return Action::Wait;
    };
    if processed.paid_through >= live.end {
        return Action::Wait;
    }
    match view.status {
        BundleStatus::Succeeded { height } => {
            let confirmations = u64::from(view.tip_height.saturating_sub(height)) + 1;
            if confirmations >= view.finalized.deposit_confirmations.max(1) {
                Action::MarkPaid {
                    through: live.end,
                    height,
                }
            } else {
                Action::Wait
            }
        }
        // The local enforcer must hold the blinded M6 while it is pending, or
        // the KNOWN policy never votes for it. A repeat proposal changes
        // nothing, and one after a failure queues the bundle again.
        _ => Action::Propose,
    }
}

pub async fn run(settings: Settings, oracle: Keypair) -> Result<(), PegError> {
    let mut enforcer = Enforcer::connect(
        settings.enforcer_url.clone(),
        u32::from(settings.sidechain_id),
    )
    .await?;
    enforcer.check_network(settings.network).await?;
    tracing::info!(
        network = settings.network.name(),
        "the peg credits deposits and proposes withdrawal bundles"
    );
    enforcer
        .set_withdrawal_bundle_policy(WithdrawalBundlePolicy::Known)
        .await?;

    let rpc = RpcClient::new_with_commitment(
        settings.solana_rpc_url.clone(),
        CommitmentConfig::processed(),
    );
    let mut proposals = crate::proposals::Proposals::load(settings.bundle_log.clone())?;
    let config = read_config(&rpc, &settings.program_id, CommitmentConfig::processed()).await?;
    crate::proposals::check_journal(&proposals, config.withdrawal_count, settings.new_bundle_log)?;
    proposals.persist()?;
    tracing::info!(
        kept = proposals.list().len(),
        "the peg reads the bundles that it handed to the enforcer"
    );
    let mut credit_enforcer = enforcer.clone();
    tokio::select! {
        error = peg_loop(&mut enforcer, &rpc, &settings, &oracle, proposals) => Err(error),
        result = credit::run(
            &mut credit_enforcer,
            settings.solana_rpc_url.clone(),
            settings.program_id,
            &oracle,
            settings.sidechain_id,
        ) => result.map_err(PegError::from),
    }
}

/// The memory of the loop. The proposals live in a file, and the rest only
/// spares repeat reads.
struct Memory {
    proposals: crate::proposals::Proposals,
    /// Every record below this index is closed at the BMM root.
    closed_through: u64,
    /// The eCash history of the live bundle that the last pass read.
    scan: Option<Scan>,
}

/// Runs a pass at each tick. It returns only the error of a live bundle that
/// eCash can never pay.
async fn peg_loop(
    enforcer: &mut Enforcer,
    rpc: &RpcClient,
    settings: &Settings,
    oracle: &Keypair,
    proposals: crate::proposals::Proposals,
) -> PegError {
    let mut ticker = tokio::time::interval(settings.bundle_interval);
    let mut memory = Memory {
        proposals,
        closed_through: 0,
        scan: None,
    };
    loop {
        ticker.tick().await;
        match pass(enforcer, rpc, settings, oracle, &mut memory).await {
            Ok(()) => (),
            Err(error @ PegError::SealedM6idMismatch { .. }) => return error,
            Err(PegError::Rpc { source, .. }) if finalized_unavailable(&source) => {
                tracing::warn!(
                    "the validator holds no BMM finalized state, so the peg waits for a BMM \
                     checkpoint"
                );
            }
            Err(error) => {
                tracing::error!(
                    %error,
                    cause = ?std::error::Error::source(&error),
                    "the peg pass failed, and the next pass tries again"
                )
            }
        }
    }
}

/// True for the RPC error of a `finalized` read while the validator holds no
/// BMM finalized state, as in a long fallback.
pub fn finalized_unavailable(error: &solana_rpc_client_api::client_error::Error) -> bool {
    use solana_rpc_client_api::{client_error::ErrorKind, request::RpcError};
    matches!(
        error.kind(),
        ErrorKind::RpcError(RpcError::RpcResponseError { code: -32004, .. })
    ) || error.to_string().contains("Block not available")
}

/// Reads the bridge config and the vault balance from one bank.
pub async fn config_and_vault(
    rpc: &RpcClient,
    program_id: &Pubkey,
    commitment: CommitmentConfig,
) -> Result<(Config, u64), PegError> {
    let keys = [
        bridge::config_pda(program_id).0,
        bridge::vault_pda(program_id).0,
    ];
    let accounts = rpc
        .get_multiple_accounts_with_commitment(&keys, commitment)
        .await
        .map_err(|source| PegError::Rpc {
            call: "getMultipleAccounts(config, vault)",
            source,
        })?
        .value;
    let data = accounts
        .first()
        .and_then(Option::as_ref)
        .map(|account| account.data.as_slice())
        .unwrap_or_default();
    let vault = accounts
        .get(1)
        .and_then(Option::as_ref)
        .map_or(0, |account| account.lamports);
    Ok((Config::decode(data)?, vault))
}

async fn read_config(
    rpc: &RpcClient,
    program_id: &Pubkey,
    commitment: CommitmentConfig,
) -> Result<Config, PegError> {
    let data = rpc
        .get_account_with_commitment(&bridge::config_pda(program_id).0, commitment)
        .await
        .map_err(|source| PegError::Rpc {
            call: "getAccountInfo(config)",
            source,
        })?
        .value
        .map(|account| account.data)
        .unwrap_or_default();
    Ok(Config::decode(&data)?)
}

async fn pass(
    enforcer: &mut Enforcer,
    rpc: &RpcClient,
    settings: &Settings,
    oracle: &Keypair,
    memory: &mut Memory,
) -> Result<(), PegError> {
    let program_id = &settings.program_id;
    let processed = read_config(rpc, program_id, CommitmentConfig::processed()).await?;
    let finalized = read_config(rpc, program_id, CommitmentConfig::finalized()).await?;
    let mut view = View {
        processed,
        finalized,
        status: BundleStatus::Unknown,
        tip_height: 0,
    };
    memory.proposals.drop_paid(finalized.paid_through)?;
    let mut bundle = None;
    if let (Some(_), Some(live)) = (processed.live_bundle(), finalized.live_bundle()) {
        let records =
            live_records(rpc, program_id, live.clone(), CommitmentConfig::finalized()).await?;
        let blinded = bundle_of(&records)?;
        let m6id = m6::m6id(&blinded);
        let sealed = Txid::from_byte_array(finalized.bundle_m6id);
        if m6id != sealed {
            return Err(PegError::SealedM6idMismatch {
                built: m6id,
                sealed,
            });
        }
        let tip = enforcer.tip().await?;
        let scan = scan_bundle(
            enforcer,
            tip,
            finalized.bundle_sealed_height,
            m6id,
            memory.scan,
        )
        .await?;
        memory.scan = Some(scan);
        view.status = scan.status;
        view.tip_height = tip.1;
        bundle = Some((blinded, m6id, live, tip));
    }

    match decide(&view) {
        Action::Wait => {}
        Action::Seal => {
            // Records never change, so the root holds the same records as
            // the followed branch.
            let start = processed.paid_through;
            let end = start
                .saturating_add(bridge::MAX_BUNDLE_RECORDS)
                .min(finalized.withdrawal_count);
            if end > start {
                check_overlap(enforcer, &memory.proposals, start..end, None).await?;
                let records =
                    live_records(rpc, program_id, start..end, CommitmentConfig::finalized())
                        .await?;
                let m6id = m6::m6id(&bundle_of(&records)?);
                send(
                    rpc,
                    oracle,
                    bridge::seal_bundle_ix(
                        program_id,
                        &oracle.pubkey(),
                        start,
                        end,
                        &m6id.to_byte_array(),
                    ),
                )
                .await?;
                tracing::info!(
                    first = start,
                    "the peg sealed the open records into a bundle"
                );
            }
        }
        Action::Propose => {
            if let Some((blinded, m6id, live, _)) = bundle {
                let id = m6id.to_string();
                check_overlap(enforcer, &memory.proposals, live.clone(), Some(&id)).await?;
                let proposal = crate::proposals::Proposal {
                    start: live.start,
                    end: live.end,
                    m6id: id,
                    sealed_height: finalized.bundle_sealed_height,
                    // The enforcer queues the bundle at its own tip, which can
                    // pass this one. Until the tip after the call is known, no
                    // failure frees the range.
                    proposed_at: u32::MAX,
                };
                memory.proposals.add(proposal.clone())?;
                enforcer.propose_withdrawal_bundle(&blinded).await?;
                let after = enforcer.tip().await?;
                memory.proposals.add(crate::proposals::Proposal {
                    proposed_at: view.tip_height.max(after.1),
                    ..proposal
                })?;
                tracing::debug!(
                    %m6id,
                    first = live.start,
                    end = live.end,
                    status = ?view.status,
                    "the enforcer holds the live bundle"
                );
            }
        }
        Action::MarkPaid { through, height } => {
            if let Some((_, m6id, live, tip)) = bundle {
                let (block_hash, _) = enforcer.block_at(tip, height).await?;
                send(
                    rpc,
                    oracle,
                    bridge::bundle_paid_ix(
                        program_id,
                        &oracle.pubkey(),
                        through,
                        u64::from(height),
                        &block_hash.to_byte_array(),
                        &m6id.to_byte_array(),
                    ),
                )
                .await?;
                tracing::info!(%m6id, first = live.start, end = live.end, "eCash paid the bundle");
            }
        }
    }
    // Anyone can close a paid record, so a close can fail after the read.
    close_paid(rpc, program_id, oracle, &processed, &finalized, memory).await
}

/// Fails when `range` overlaps a bundle of another M6ID that the enforcer
/// keeps and eCash can still pay or paid. Two such bundles could pay one
/// record twice after a rollback of Solana below its root.
async fn check_overlap(
    enforcer: &mut Enforcer,
    proposals: &crate::proposals::Proposals,
    range: Range<u64>,
    m6id: Option<&str>,
) -> Result<(), PegError> {
    if range.start < proposals.dropped_through() {
        return Err(PegError::BelowDropped {
            start: range.start,
            dropped_through: proposals.dropped_through(),
        });
    }
    let overlaps: Vec<_> = proposals.overlapping(&range, m6id).collect();
    if overlaps.is_empty() {
        return Ok(());
    }
    let tip = enforcer.tip().await?;
    let mut statuses = Vec::with_capacity(overlaps.len());
    for proposal in overlaps {
        let id: Txid = proposal
            .m6id
            .parse()
            .map_err(|_| PegError::BadM6id(proposal.m6id.clone()))?;
        let status = bundle_status_at(enforcer, tip, proposal.sealed_height, id).await?;
        statuses.push((proposal, status));
    }
    match crate::proposals::blocking(&statuses, tip.1) {
        Some(proposal) => Err(PegError::Overlap {
            m6id: proposal.m6id.clone(),
            start: range.start,
            end: range.end,
        }),
        None => Ok(()),
    }
}

/// Closes the paid records, so each owner gets the rent back. The cursor
/// moves only when the BMM root holds no record below it, so a fork that
/// drops a close never skips that record.
async fn close_paid(
    rpc: &RpcClient,
    program_id: &Pubkey,
    oracle: &Keypair,
    processed: &Config,
    finalized: &Config,
    memory: &mut Memory,
) -> Result<(), PegError> {
    if memory.closed_through >= processed.paid_through {
        return Ok(());
    }
    let records = read_records(
        rpc,
        program_id,
        memory.closed_through..processed.paid_through,
        CommitmentConfig::processed(),
    )
    .await?;
    let instructions: Vec<Instruction> = records
        .iter()
        .map(|record| bridge::close_record_ix(program_id, &record.owner, record.index))
        .collect();
    let lens = loaded::loaded_lens(
        rpc,
        &loaded::tx_keys(&oracle.pubkey(), &instructions),
        CommitmentConfig::processed(),
    )
    .await
    .map_err(|source| PegError::Rpc {
        call: "getMultipleAccounts(close accounts)",
        source,
    })?;
    for batch in close_batches(&oracle.pubkey(), &instructions, &lens) {
        send_all(rpc, oracle, batch).await?;
    }
    let left = read_records(
        rpc,
        program_id,
        memory.closed_through..finalized.paid_through,
        CommitmentConfig::finalized(),
    )
    .await?;
    memory.closed_through = closed_through(memory.closed_through, finalized.paid_through, &left);
    Ok(())
}

/// Splits the close instructions into txs of at most `CLOSE_BATCH` each,
/// and each tx stays below the loaded data budget.
pub fn close_batches<'a>(
    payer: &Pubkey,
    instructions: &'a [Instruction],
    lens: &std::collections::HashMap<Pubkey, u64>,
) -> Vec<&'a [Instruction]> {
    let items: Vec<Vec<Pubkey>> = instructions.iter().map(loaded::instruction_keys).collect();
    let mut batches = Vec::new();
    let mut start = 0;
    while start < instructions.len() {
        let count = loaded::fitting(
            &[*payer],
            &items[start..],
            lens,
            loaded::LOADED_DATA_BUDGET,
            CLOSE_BATCH,
        );
        batches.push(&instructions[start..start + count]);
        start += count;
    }
    batches
}

/// The new close cursor: the lowest record of `left` that the BMM root still
/// holds below `final_paid_through`, or `final_paid_through`.
pub fn closed_through(cursor: u64, final_paid_through: u64, left: &[WithdrawalRecord]) -> u64 {
    left.iter()
        .map(|record| record.index)
        .filter(|index| *index < final_paid_through)
        .min()
        .unwrap_or(final_paid_through)
        .max(cursor)
}

async fn send(rpc: &RpcClient, oracle: &Keypair, instruction: Instruction) -> Result<(), PegError> {
    send_all(rpc, oracle, &[instruction]).await
}

async fn send_all(
    rpc: &RpcClient,
    oracle: &Keypair,
    instructions: &[Instruction],
) -> Result<(), PegError> {
    let blockhash = rpc
        .get_latest_blockhash()
        .await
        .map_err(|source| PegError::Rpc {
            call: "getLatestBlockhash",
            source,
        })?;
    let transaction = SolanaTransaction::new_signed_with_payer(
        instructions,
        Some(&oracle.pubkey()),
        &[oracle],
        blockhash,
    );
    rpc.send_and_confirm_transaction(&transaction)
        .await
        .map_err(|source| PegError::Rpc {
            call: "sendTransaction",
            source,
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use bitcoin::{absolute::LockTime, transaction::Version, Amount, BlockHash, TxOut};
    use std::{future::Future, pin::Pin, sync::Arc};

    use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
    use solana_rpc_client::{
        mock_sender::MockSender,
        rpc_client::RpcClientConfig,
        rpc_sender::{RpcSender, RpcTransportStats},
    };
    use solana_rpc_client_api::{
        client_error::Result as ClientResult,
        request::{RpcError as ClientRpcError, RpcRequest},
    };
    use solana_sdk::signature::Signature;

    fn a_record(index: u64, payout_sats: u64, script: u8) -> WithdrawalRecord {
        WithdrawalRecord {
            index,
            owner: Pubkey::new_from_array([index as u8; 32]),
            burned_lamports: (payout_sats + 1_000) * 10,
            payout_sats,
            fee_sats: 1_000,
            script_pubkey: [&[0x00, 0x14][..], &[script; 20]].concat(),
            bump: 255,
        }
    }

    fn a_record_account(program_id: Pubkey, record: &WithdrawalRecord) -> Account {
        let mut data = bridge::discriminator("account", "WithdrawalRecord").to_vec();
        data.extend_from_slice(&record.index.to_le_bytes());
        data.extend_from_slice(record.owner.as_ref());
        for value in [record.burned_lamports, record.payout_sats, record.fee_sats] {
            data.extend_from_slice(&value.to_le_bytes());
        }
        data.extend_from_slice(&(record.script_pubkey.len() as u32).to_le_bytes());
        data.extend_from_slice(&record.script_pubkey);
        data.push(record.bump);
        Account {
            lamports: 1_000,
            data,
            owner: program_id,
            executable: false,
            rent_epoch: 0,
        }
    }

    fn a_paid_m6(outputs: Vec<TxOut>) -> BitcoinTransaction {
        let mut output = vec![TxOut {
            value: Amount::from_sat(1_000_000),
            script_pubkey: ScriptBuf::from_bytes(vec![0xb5]),
        }];
        output.extend(outputs);
        BitcoinTransaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: Vec::new(),
            output,
        }
    }

    fn a_config(withdrawal_count: u64, paid_through: u64, bundled_through: u64) -> Config {
        Config {
            oracle: Pubkey::new_from_array([7u8; 32]),
            deposit_high_water: 0,
            pegged_lamports: 0,
            withdrawal_count,
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
            vault_supply: 0,
            float_lamports: 0,
            paid_through,
            bundled_through,
            bundle_sealed_height: 50,
            bmm_skip_blocks: 0,
            bmm_next_since_slot: 0,
            bundle_m6id: [0u8; 32],
        }
    }

    fn a_view(processed: Config, finalized: Config, status: BundleStatus) -> View {
        View {
            processed,
            finalized,
            status,
            tip_height: 100,
        }
    }

    fn a_block(height: u32, bundles: Vec<BundleEvent>) -> PegEvent {
        PegEvent::Connect {
            block_hash: BlockHash::from_byte_array([height as u8; 32]),
            height,
            deposits: Vec::new(),
            bundles,
        }
    }

    fn an_m6id(byte: u8) -> Txid {
        Txid::from_byte_array([byte; 32])
    }

    /// A Solana RPC that holds one config and its records by address, and
    /// refuses each close tx.
    struct Bank {
        mock: MockSender,
        config: Config,
        finalized: Config,
        program_id: Pubkey,
        records: Vec<(Pubkey, Account)>,
        finalized_records: Vec<(Pubkey, Account)>,
        sent: Arc<std::sync::Mutex<Vec<&'static str>>>,
        /// The data of each seal after its discriminator.
        sealed: Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
    }

    fn ui_account(account: &Account) -> serde_json::Value {
        serde_json::json!({
            "lamports": account.lamports,
            "data": [BASE64.encode(&account.data), "base64"],
            "owner": account.owner.to_string(),
            "executable": false,
            "rentEpoch": 0,
            "space": account.data.len(),
        })
    }

    impl Bank {
        async fn answer(
            &self,
            request: RpcRequest,
            params: serde_json::Value,
        ) -> ClientResult<serde_json::Value> {
            let context = serde_json::json!({"slot": 1});
            let finalized = params[1]["commitment"] == "finalized";
            let (config, records) = if finalized {
                (&self.finalized, &self.finalized_records)
            } else {
                (&self.config, &self.records)
            };
            match request {
                RpcRequest::GetAccountInfo => Ok(serde_json::json!({
                    "context": context,
                    "value": ui_account(&Account {
                        lamports: 1_000,
                        data: config.encode(),
                        owner: self.program_id,
                        executable: false,
                        rent_epoch: 0,
                    }),
                })),
                RpcRequest::GetMultipleAccounts => {
                    let value: Vec<serde_json::Value> = params[0]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|key| {
                            records
                                .iter()
                                .find(|(at, _)| at.to_string() == key.as_str().unwrap())
                                .map_or(serde_json::Value::Null, |(_, account)| ui_account(account))
                        })
                        .collect();
                    Ok(serde_json::json!({"context": context, "value": value}))
                }
                RpcRequest::SendTransaction => {
                    let tx = BASE64.decode(params[0].as_str().unwrap()).unwrap();
                    let holds = |name: &str| {
                        let discriminator = bridge::discriminator("global", name);
                        tx.windows(8).any(|window| window == discriminator)
                    };
                    let name = ["seal_bundle", "close_record"]
                        .into_iter()
                        .find(|name| holds(name))
                        .unwrap();
                    self.sent.lock().unwrap().push(name);
                    if name == "seal_bundle" {
                        let discriminator = bridge::discriminator("global", name);
                        let at = tx
                            .windows(8)
                            .position(|window| window == discriminator)
                            .unwrap();
                        self.sealed
                            .lock()
                            .unwrap()
                            .push(tx[at + 8..at + 56].to_vec());
                    }
                    if name == "close_record" {
                        return Err(ClientRpcError::ForUser(
                            "a user closed the record first".to_owned(),
                        )
                        .into());
                    }
                    let signature = Signature::try_from(&tx[1..65]).unwrap();
                    Ok(serde_json::Value::String(signature.to_string()))
                }
                request => self.mock.send(request, params).await,
            }
        }
    }

    impl RpcSender for Bank {
        fn send<'life0, 'async_trait>(
            &'life0 self,
            request: RpcRequest,
            params: serde_json::Value,
        ) -> Pin<Box<dyn Future<Output = ClientResult<serde_json::Value>> + Send + 'async_trait>>
        where
            'life0: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(self.answer(request, params))
        }

        fn get_transport_stats(&self) -> RpcTransportStats {
            RpcTransportStats::default()
        }

        fn url(&self) -> String {
            "bank".to_owned()
        }
    }

    type Sent = Arc<std::sync::Mutex<Vec<&'static str>>>;
    type Sealed = Arc<std::sync::Mutex<Vec<Vec<u8>>>>;

    fn record_accounts(program_id: Pubkey, records: &[WithdrawalRecord]) -> Vec<(Pubkey, Account)> {
        records
            .iter()
            .map(|record| {
                (
                    bridge::withdrawal_pda(&program_id, record.index).0,
                    a_record_account(program_id, record),
                )
            })
            .collect()
    }

    /// A bank with `processed` and `finalized`, each a config and its records.
    fn a_bank(
        program_id: Pubkey,
        processed: (Config, &[WithdrawalRecord]),
        finalized: (Config, &[WithdrawalRecord]),
    ) -> (RpcClient, Sent, Sealed) {
        let sent = Sent::default();
        let sealed = Sealed::default();
        let rpc = RpcClient::new_sender(
            Bank {
                mock: MockSender::new("succeeds"),
                config: processed.0,
                finalized: finalized.0,
                program_id,
                records: record_accounts(program_id, processed.1),
                finalized_records: record_accounts(program_id, finalized.1),
                sent: Arc::clone(&sent),
                sealed: Arc::clone(&sealed),
            },
            RpcClientConfig::with_commitment(CommitmentConfig::processed()),
        );
        (rpc, sent, sealed)
    }

    fn a_settings(program_id: Pubkey, name: &str) -> Settings {
        let settings = Settings {
            network: crate::network::PegNetwork::Alphanet,
            enforcer_url: "http://127.0.0.1:1".to_owned(),
            solana_rpc_url: String::new(),
            sidechain_id: 8,
            program_id,
            bundle_interval: Duration::from_secs(1),
            bundle_log: std::env::temp_dir().join(format!(
                "sol-drivechain-peg-{}-{name}.json",
                std::process::id()
            )),
            new_bundle_log: false,
        };
        let _ = std::fs::remove_file(&settings.bundle_log);
        settings
    }

    async fn a_pass(rpc: &RpcClient, settings: &Settings) -> Result<(), PegError> {
        let mut enforcer = Enforcer::connect(settings.enforcer_url.clone(), 8)
            .await
            .unwrap();
        pass(
            &mut enforcer,
            rpc,
            settings,
            &Keypair::new(),
            &mut Memory {
                proposals: crate::proposals::Proposals::load(settings.bundle_log.clone()).unwrap(),
                closed_through: 0,
                scan: None,
            },
        )
        .await
    }

    #[tokio::test]
    async fn a_failed_close_does_not_stop_the_seal_of_the_m6id_of_the_records() {
        let program_id = Pubkey::new_from_array([3u8; 32]);
        let records: Vec<WithdrawalRecord> =
            (0..3).map(|index| a_record(index, 5_000, 1)).collect();
        let config = a_config(3, 1, 1);
        let (rpc, sent, sealed) = a_bank(program_id, (config, &records), (config, &records));
        let error = a_pass(&rpc, &a_settings(program_id, "close"))
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            PegError::Rpc {
                call: "sendTransaction",
                ..
            }
        ));
        assert_eq!(*sent.lock().unwrap(), ["seal_bundle", "close_record"]);
        let m6id = m6::m6id(&bundle_of(&records[1..]).unwrap());
        assert_eq!(
            *sealed.lock().unwrap(),
            [[
                &1u64.to_le_bytes()[..],
                &3u64.to_le_bytes(),
                &m6id.to_byte_array()
            ]
            .concat()]
        );
    }

    #[tokio::test]
    async fn the_seal_takes_the_end_and_the_m6id_from_the_finalized_records() {
        let program_id = Pubkey::new_from_array([3u8; 32]);
        let followed: Vec<WithdrawalRecord> =
            (0..3).map(|index| a_record(index, 6_000, 2)).collect();
        let rooted: Vec<WithdrawalRecord> = (0..2).map(|index| a_record(index, 5_000, 1)).collect();
        let (rpc, sent, sealed) = a_bank(
            program_id,
            (a_config(3, 1, 1), &followed),
            (a_config(2, 1, 1), &rooted),
        );
        a_pass(&rpc, &a_settings(program_id, "finalized-seal"))
            .await
            .unwrap_err();
        assert_eq!(sent.lock().unwrap().first(), Some(&"seal_bundle"));
        let m6id = m6::m6id(&bundle_of(&rooted[1..]).unwrap());
        assert_eq!(
            *sealed.lock().unwrap(),
            [[
                &1u64.to_le_bytes()[..],
                &2u64.to_le_bytes(),
                &m6id.to_byte_array()
            ]
            .concat()]
        );
    }

    #[tokio::test]
    async fn the_seal_waits_until_the_root_holds_an_open_record() {
        let program_id = Pubkey::new_from_array([3u8; 32]);
        let records: Vec<WithdrawalRecord> =
            (0..3).map(|index| a_record(index, 5_000, 1)).collect();
        let (rpc, sent, sealed) = a_bank(
            program_id,
            (a_config(3, 1, 1), &records),
            (a_config(1, 0, 0), &records[..1]),
        );
        a_pass(&rpc, &a_settings(program_id, "root-behind"))
            .await
            .unwrap_err();
        assert_eq!(*sent.lock().unwrap(), ["close_record"]);
        assert!(sealed.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_live_bundle_of_another_m6id_stops_the_peg() {
        let program_id = Pubkey::new_from_array([3u8; 32]);
        let records: Vec<WithdrawalRecord> =
            (0..3).map(|index| a_record(index, 5_000, 1)).collect();
        let mut config = a_config(3, 1, 3);
        config.bundle_m6id = [9u8; 32];
        let (rpc, _, _) = a_bank(program_id, (config, &records), (config, &records));
        let settings = a_settings(program_id, "m6id-mismatch");
        let error = a_pass(&rpc, &settings).await.unwrap_err();
        assert!(
            matches!(error, PegError::SealedM6idMismatch { .. }),
            "{error:?}"
        );
        let mut enforcer = Enforcer::connect(settings.enforcer_url.clone(), 8)
            .await
            .unwrap();
        let proposals = crate::proposals::Proposals::load(settings.bundle_log.clone()).unwrap();
        let stopped = tokio::time::timeout(
            Duration::from_secs(10),
            peg_loop(&mut enforcer, &rpc, &settings, &Keypair::new(), proposals),
        )
        .await
        .expect("the loop stops");
        assert!(matches!(stopped, PegError::SealedM6idMismatch { .. }));
    }

    #[test]
    fn a_missing_finalized_state_is_a_wait_and_not_a_failure() {
        use solana_rpc_client_api::{
            client_error::{Error, ErrorKind},
            request::{RpcError, RpcResponseErrorData},
        };
        let response = |code, message: &str| {
            Error::from(ErrorKind::RpcError(RpcError::RpcResponseError {
                code,
                message: message.to_owned(),
                data: RpcResponseErrorData::Empty,
            }))
        };
        assert!(finalized_unavailable(&response(
            -32004,
            "Block not available for slot 0"
        )));
        assert!(!finalized_unavailable(&response(
            -32002,
            "Transaction simulation failed"
        )));
    }

    #[test]
    fn a_bundle_adds_every_fee() {
        let records = [a_record(0, 50_000, 1), a_record(1, 60_000, 2)];
        let bundle = bundle_of(&records).unwrap();
        let fee: [u8; 8] = bundle.output[0].script_pubkey.as_bytes()[2..]
            .try_into()
            .unwrap();
        assert_eq!(u64::from_be_bytes(fee), 2_000);
    }

    #[test]
    fn a_bundle_holds_one_payout_for_each_record() {
        let records = [a_record(0, 50_000, 1), a_record(1, 60_000, 2)];
        let bundle = bundle_of(&records).unwrap();
        assert_eq!(bundle.output.len(), 3);
        assert_eq!(bundle.output[1].value, Amount::from_sat(50_000));
        assert_eq!(bundle.output[2].value, Amount::from_sat(60_000));
    }

    #[test]
    fn the_same_records_give_the_same_m6id() {
        let records = [a_record(3, 50_000, 1), a_record(4, 60_000, 2)];
        assert_eq!(
            m6::m6id(&bundle_of(&records).unwrap()),
            m6::m6id(&bundle_of(&records.clone()).unwrap())
        );
        assert_ne!(
            m6::m6id(&bundle_of(&records).unwrap()),
            m6::m6id(&bundle_of(&records[..1]).unwrap())
        );
    }

    #[test]
    fn an_empty_record_set_gives_no_bundle() {
        assert!(bundle_of(&[]).is_err());
    }

    #[test]
    fn a_system_account_at_a_record_address_is_no_record() {
        let program_id = Pubkey::new_from_array([3u8; 32]);
        let record = a_record(4, 50_000, 1);
        let account = a_record_account(program_id, &record);
        assert_eq!(record_of(&program_id, 4, &account), Some(record.clone()));

        let gift = Account {
            lamports: 512,
            data: Vec::new(),
            owner: bridge::SYSTEM_PROGRAM_ID,
            executable: false,
            rent_epoch: 0,
        };
        assert_eq!(record_of(&program_id, 4, &gift), None);
        let mut foreign = account.clone();
        foreign.owner = Pubkey::new_from_array([9u8; 32]);
        assert_eq!(record_of(&program_id, 4, &foreign), None);
        assert_eq!(record_of(&program_id, 5, &account), None);
        let mut garbage = account;
        garbage.data[0] ^= 1;
        assert_eq!(record_of(&program_id, 4, &garbage), None);
    }

    #[test]
    fn a_live_bundle_with_a_missing_record_fails() {
        let records = vec![a_record(4, 50_000, 1), a_record(6, 50_000, 1)];
        assert!(matches!(
            complete(4..7, records),
            Err(PegError::MissingRecord(5))
        ));
        let records = vec![a_record(4, 50_000, 1), a_record(5, 50_000, 1)];
        assert_eq!(complete(4..6, records.clone()).unwrap(), records);
        assert!(matches!(
            complete(4..7, records),
            Err(PegError::MissingRecord(6))
        ));
    }

    #[test]
    fn the_newest_event_of_the_bundle_gives_its_status() {
        let mine = an_m6id(1);
        let other = an_m6id(2);
        let events = [
            a_block(10, vec![BundleEvent::Submitted(mine)]),
            a_block(11, vec![BundleEvent::Submitted(other)]),
            a_block(20, vec![BundleEvent::Failed(mine)]),
            a_block(21, vec![BundleEvent::Submitted(mine)]),
        ];
        assert_eq!(
            bundle_status(&events[..1], mine),
            BundleStatus::Submitted { height: 10 }
        );
        assert_eq!(
            bundle_status(&events[..3], mine),
            BundleStatus::Failed { height: 20 }
        );
        assert_eq!(
            bundle_status(&events, mine),
            BundleStatus::Submitted { height: 21 }
        );
        assert_eq!(bundle_status(&events, an_m6id(3)), BundleStatus::Unknown);
        let paid = [a_block(
            30,
            vec![BundleEvent::Succeeded {
                m6id: mine,
                paid: a_paid_m6(Vec::new()),
            }],
        )];
        assert_eq!(
            bundle_status(&paid, mine),
            BundleStatus::Succeeded { height: 30 }
        );
        assert_eq!(bundle_status(&paid, other), BundleStatus::Unknown);
    }

    #[test]
    fn a_scan_in_parts_keeps_the_newest_status_and_the_payment() {
        let mine = an_m6id(1);
        let submitted = [a_block(10, vec![BundleEvent::Submitted(mine)])];
        let paid = [a_block(
            30,
            vec![BundleEvent::Succeeded {
                m6id: mine,
                paid: a_paid_m6(Vec::new()),
            }],
        )];
        let again = [a_block(31, vec![BundleEvent::Submitted(mine)])];
        let status = advance(BundleStatus::Unknown, &submitted, mine);
        assert_eq!(status, BundleStatus::Submitted { height: 10 });
        assert_eq!(advance(status, &[], mine), status);
        let status = advance(status, &paid, mine);
        assert_eq!(status, BundleStatus::Succeeded { height: 30 });
        assert_eq!(advance(status, &again, mine), status);
    }

    #[test]
    fn a_new_proposal_of_a_paid_m6id_keeps_the_payment() {
        let mine = an_m6id(1);
        let events = [
            a_block(10, vec![BundleEvent::Submitted(mine)]),
            a_block(
                30,
                vec![BundleEvent::Succeeded {
                    m6id: mine,
                    paid: a_paid_m6(Vec::new()),
                }],
            ),
            a_block(31, vec![BundleEvent::Submitted(mine)]),
            a_block(45, vec![BundleEvent::Failed(mine)]),
        ];
        let status = bundle_status(&events, mine);
        assert_eq!(status, BundleStatus::Succeeded { height: 30 });
        let config = a_config(3, 1, 3);
        let mut view = a_view(config, config, status);
        view.tip_height = 31;
        assert_eq!(decide(&view), Action::Wait);
        view.tip_height = 35;
        assert_eq!(
            decide(&view),
            Action::MarkPaid {
                through: 3,
                height: 30
            }
        );
    }

    #[test]
    fn open_records_without_a_live_bundle_get_sealed() {
        let config = a_config(3, 1, 1);
        assert_eq!(
            decide(&a_view(config, config, BundleStatus::Unknown)),
            Action::Seal
        );
        let empty = a_config(1, 1, 1);
        assert_eq!(
            decide(&a_view(empty, empty, BundleStatus::Unknown)),
            Action::Wait
        );
    }

    #[test]
    fn a_live_bundle_blocks_a_second_seal() {
        // New records wait while a bundle is live, whatever eCash does.
        let config = a_config(9, 1, 3);
        for status in [
            BundleStatus::Unknown,
            BundleStatus::Submitted { height: 60 },
            BundleStatus::Failed { height: 70 },
            BundleStatus::Succeeded { height: 99 },
        ] {
            assert_ne!(decide(&a_view(config, config, status)), Action::Seal);
        }
    }

    #[test]
    fn a_seal_goes_to_ecash_only_once_it_is_final() {
        let processed = a_config(3, 1, 3);
        let finalized = a_config(3, 1, 1);
        assert_eq!(
            decide(&a_view(processed, finalized, BundleStatus::Unknown)),
            Action::Wait
        );
        assert_eq!(
            decide(&a_view(processed, processed, BundleStatus::Unknown)),
            Action::Propose
        );
    }

    #[test]
    fn a_failed_bundle_goes_back_to_the_enforcer_unchanged() {
        let config = a_config(3, 1, 3);
        let view = a_view(config, config, BundleStatus::Failed { height: 70 });
        assert_eq!(decide(&view), Action::Propose);
    }

    #[test]
    fn a_pending_bundle_goes_to_the_local_enforcer_again() {
        // A fresh enforcer never got the blinded M6, and KNOWN votes only
        // for a bundle that it holds.
        let config = a_config(3, 1, 3);
        let view = a_view(config, config, BundleStatus::Submitted { height: 60 });
        assert_eq!(decide(&view), Action::Propose);
    }

    #[test]
    fn two_bundles_with_equal_payouts_get_two_m6ids() {
        let first = [a_record(0, 50_000, 1), a_record(1, 50_000, 1)];
        let second = [a_record(2, 50_000, 1), a_record(3, 50_000, 1)];
        let first = bundle_of(&first).unwrap();
        let second = bundle_of(&second).unwrap();
        assert_eq!(first.output, second.output);
        assert_ne!(m6::m6id(&first), m6::m6id(&second));
        assert_eq!(second.lock_time.to_consensus_u32(), 2);
        // The payment of the first bundle tells nothing about the second.
        let events = [a_block(
            90,
            vec![BundleEvent::Succeeded {
                m6id: m6::m6id(&first),
                paid: a_paid_m6(Vec::new()),
            }],
        )];
        let status = bundle_status(&events, m6::m6id(&second));
        assert_eq!(status, BundleStatus::Unknown);
        let config = a_config(4, 2, 4);
        assert_eq!(decide(&a_view(config, config, status)), Action::Propose);
    }

    #[test]
    fn a_first_record_above_the_lock_time_fails() {
        let record = a_record(u64::from(u32::MAX) + 1, 50_000, 1);
        assert!(matches!(
            bundle_of(&[record]),
            Err(crate::m6::M6Error::FirstRecordTooLarge(_))
        ));
    }

    #[test]
    fn a_close_batch_stays_under_the_loaded_data_limit() {
        const MIB: u64 = 1024 * 1024;
        let program_id = Pubkey::new_from_array([3u8; 32]);
        let payer = Pubkey::new_from_array([4u8; 32]);
        let records: Vec<WithdrawalRecord> =
            (0..14).map(|index| a_record(index, 5_000, 1)).collect();
        let instructions: Vec<Instruction> = records
            .iter()
            .map(|record| bridge::close_record_ix(&program_id, &record.owner, record.index))
            .collect();
        let mut lens: std::collections::HashMap<Pubkey, u64> =
            loaded::tx_keys(&payer, &instructions)
                .into_iter()
                .map(|key| (key, loaded::ACCOUNT_BASE_LEN + 200))
                .collect();
        lens.insert(program_id, loaded::ACCOUNT_BASE_LEN + MIB);
        for record in &records[..7] {
            lens.insert(record.owner, loaded::ACCOUNT_BASE_LEN + 10 * MIB);
        }
        let batches = close_batches(&payer, &instructions, &lens);
        let sizes: Vec<usize> = batches.iter().map(|batch| batch.len()).collect();
        assert_eq!(sizes, [5, 9]);
        for batch in batches {
            let mut keys = loaded::tx_keys(&payer, batch);
            keys.sort();
            keys.dedup();
            let total: u64 = keys.iter().map(|key| lens[key]).sum();
            assert!(total <= loaded::LOADED_DATA_BUDGET, "{total}");
        }
    }

    #[test]
    fn the_close_cursor_moves_only_past_records_that_the_root_dropped() {
        assert_eq!(closed_through(0, 5, &[]), 5);
        let left = [a_record(2, 1, 1), a_record(4, 1, 1)];
        assert_eq!(closed_through(0, 5, &left), 2);
        assert_eq!(closed_through(3, 5, &left[1..]), 4);
        assert_eq!(closed_through(4, 3, &[]), 4);
    }

    #[test]
    fn a_paid_bundle_is_marked_paid_after_d_confirmations() {
        let config = a_config(3, 1, 3);
        let mut view = a_view(config, config, BundleStatus::Succeeded { height: 96 });
        assert_eq!(decide(&view), Action::Wait, "five confirmations");
        view.tip_height = 101;
        assert_eq!(
            decide(&view),
            Action::MarkPaid {
                through: 3,
                height: 96
            }
        );
        // A reorg that drops the payment takes the status back.
        view.status = BundleStatus::Submitted { height: 60 };
        assert_eq!(decide(&view), Action::Propose);
    }

    #[test]
    fn a_paid_bundle_is_never_proposed_again() {
        let config = a_config(3, 1, 3);
        let view = a_view(config, config, BundleStatus::Succeeded { height: 100 });
        assert_eq!(decide(&view), Action::Wait);
    }

    #[test]
    fn a_payment_on_the_followed_branch_is_not_sent_twice() {
        let finalized = a_config(3, 1, 3);
        let status = BundleStatus::Succeeded { height: 80 };
        let paid = a_config(3, 3, 3);
        assert_eq!(decide(&a_view(paid, finalized, status)), Action::Wait);
        let resealed = a_config(5, 3, 5);
        assert_eq!(decide(&a_view(resealed, finalized, status)), Action::Wait);
        let open = a_config(5, 3, 3);
        assert_eq!(decide(&a_view(open, finalized, status)), Action::Seal);
    }
}
