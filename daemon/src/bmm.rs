//! The BMM loop of a validator: it bids on eCash, it publishes the pair of
//! each bid to the validators, and it settles the commitments on Solana in
//! eCash height order.
//!
//! A bid commits h* = SHA-256(Solana bank hash ‖ payee) for the newest block in
//! the block record of the branch that the validator follows whose bank passes
//! rule 3. The pair goes to the validators outside any transaction, so a leader
//! cannot hide it.

use std::{
    collections::{BTreeMap, HashMap},
    future::Future,
    net::SocketAddr,
    sync::Arc,
    time::Duration,
};

use bitcoin::{hashes::Hash as _, BlockHash};
use serde_json::json;
use solana_commitment_config::CommitmentConfig;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_rpc_client_api::{client_error::ErrorKind, request::RpcRequest};
use solana_sdk::{
    instruction::Instruction,
    pubkey::Pubkey,
    signature::{Keypair, Signer as _},
    transaction::{Transaction, TransactionError},
};
use tokio::sync::Mutex;

use crate::{
    bridge::{self, Config, RecordedBlock},
    enforcer::{ActiveHeaders, Enforcer, EnforcerError},
    network::PegNetwork,
};

pub struct Settings {
    pub network: PegNetwork,
    pub enforcer_url: String,
    pub solana_rpc_url: String,
    pub sidechain_id: u8,
    pub program_id: Pubkey,
    /// N, the eCash blocks on top of a block before its settle. It must match
    /// `--bmm-confirmations` on the validators.
    pub confirmations: u64,
    /// The part of the fee income of one block that one bid offers, in percent.
    pub bid_percent: u64,
    /// The loop makes no bid below this amount.
    pub min_bid_sats: u64,
    /// No bid and no template offers more than this amount.
    pub max_bid_sats: u64,
    pub interval: Duration,
    /// The address of the JSON-RPC for BitWindow and drivechaind. `None` turns
    /// it off.
    pub rpc_listen: Option<SocketAddr>,
    /// The loop sends no bid, because drivechaind pays the bids.
    pub no_bid: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum BmmError {
    #[error(transparent)]
    Enforcer(#[from] crate::enforcer::EnforcerError),
    #[error(transparent)]
    Bridge(#[from] crate::bridge::BridgeError),
    #[error("the Solana rpc call `{call}` failed")]
    Rpc {
        call: &'static str,
        #[source]
        source: solana_rpc_client_api::client_error::Error,
    },
    #[error("the validator did not take the pair at eCash height {height}: {answers:?}")]
    PairRefused { height: u64, answers: Vec<String> },
    #[error("the bid percent is {0}, and it must be from 1 to 100")]
    BadBidPercent(u64),
    #[error("the max bid is {max} sats, and it must be at least 1 and at least the min bid of {min} sats")]
    BadMaxBid { max: u64, min: u64 },
    #[error("the validator sent a block of slot {slot} that does not decode: {reason}")]
    BadBlock { slot: u64, reason: &'static str },
    #[error("eCash height {0} does not fit a u32")]
    HeightTooLarge(u64),
    #[error("the BMM rpc cannot listen on {addr}")]
    Listen {
        addr: SocketAddr,
        #[source]
        source: std::io::Error,
    },
    #[error("the BMM rpc stopped")]
    Serve(#[source] std::io::Error),
}

/// A published pair: the eCash height of the bid, the Solana bank hash, and
/// the payee.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pair {
    pub height: u64,
    pub block: [u8; 32],
    pub payee: Pubkey,
}

impl Pair {
    pub fn commitment(&self) -> [u8; 32] {
        bridge::commitment_of(&self.block, &self.payee)
    }
}

/// The pair whose hash is the commitment at `height`.
pub fn pair_for(pairs: &[Pair], height: u64, commitment: &[u8; 32]) -> Option<Pair> {
    pairs
        .iter()
        .find(|pair| pair.height == height && &pair.commitment() == commitment)
        .copied()
}

/// The fees that the blocks of the validator paid into the treasury, as a
/// running total at each eCash tip. A transfer to the treasury is no fee, so
/// it does not count.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct FeeHistory {
    by_height: BTreeMap<u64, u64>,
}

impl FeeHistory {
    /// Records the first total that the loop sees at a tip height.
    pub fn observe(&mut self, tip_height: u64, earned: u64) {
        self.by_height.entry(tip_height).or_insert(earned);
    }

    /// The fee income of one eCash block: the median income per block since
    /// the newest tip at least `span` blocks back. One block with a high fee
    /// does not move the median. `None` means that the loop has not watched
    /// long enough.
    pub fn income_per_block(&self, tip_height: u64, earned: u64, span: u64) -> Option<u64> {
        let back = tip_height.checked_sub(span)?;
        let (&start, _) = self.by_height.range(..=back).next_back()?;
        let mut points: Vec<(u64, u64)> = self
            .by_height
            .range(start..tip_height)
            .map(|(&height, &total)| (height, total))
            .collect();
        points.push((tip_height, earned));
        let mut rates: Vec<(u64, u64)> = points
            .windows(2)
            .map(|pair| {
                let blocks = pair[1].0 - pair[0].0;
                (pair[1].1.saturating_sub(pair[0].1) / blocks, blocks)
            })
            .collect();
        rates.sort_unstable();
        let all_blocks = tip_height - start;
        let mut counted = 0u64;
        rates.into_iter().find_map(|(rate, blocks)| {
            counted += blocks;
            (counted * 2 >= all_blocks).then_some(rate)
        })
    }

    /// Drops the totals that no estimate can use any more. It keeps the
    /// newest total at or below `height`.
    pub fn forget_below(&mut self, height: u64) {
        let keep_from = self
            .by_height
            .range(..=height)
            .next_back()
            .map(|(&kept, _)| kept);
        if let Some(keep_from) = keep_from {
            self.by_height = self.by_height.split_off(&keep_from);
        }
    }
}

/// The bid for a block that earns `income_lamports`, at most `max_bid_sats`.
/// `None` means no bid.
pub fn bid_sats(
    income_lamports: u64,
    bid_percent: u64,
    min_bid_sats: u64,
    max_bid_sats: u64,
) -> Option<u64> {
    let income_sats = income_lamports / bridge::LAMPORTS_PER_SAT;
    let bid = (income_sats.saturating_mul(bid_percent) / 100).min(max_bid_sats);
    (bid >= min_bid_sats && bid > 0).then_some(bid)
}

/// True when a leader accepts a settle of `height` at eCash tip `tip_height`:
/// it needs N + 2 blocks on top.
pub fn settle_is_ready(height: u64, tip_height: u64, confirmations: u64) -> bool {
    height
        .checked_add(confirmations)
        .and_then(|needed| needed.checked_add(2))
        .is_some_and(|needed| tip_height >= needed)
}

/// The rules of a bid: the payee and the part of the fees that it offers.
#[derive(Clone, Copy, Debug)]
pub struct BidRules {
    pub payee: Pubkey,
    pub confirmations: u64,
    pub bid_percent: u64,
    pub min_bid_sats: u64,
    pub max_bid_sats: u64,
}

impl BidRules {
    /// Fails when the bid percent or the bid limits cannot give a bid.
    pub fn check(&self) -> Result<(), BmmError> {
        if !(1..=100).contains(&self.bid_percent) {
            return Err(BmmError::BadBidPercent(self.bid_percent));
        }
        if self.max_bid_sats == 0 || self.max_bid_sats < self.min_bid_sats {
            return Err(BmmError::BadMaxBid {
                max: self.max_bid_sats,
                min: self.min_bid_sats,
            });
        }
        Ok(())
    }
}

/// An inclusion answer can name a pair this many eCash blocks after its
/// height.
const REMEMBERED_HEIGHTS: u64 = 1_000;

/// The memory keeps at most this many own pairs per eCash height.
const MAX_PAIRS_PER_HEIGHT: usize = 64;

/// The validator holds at most this many unchecked pairs per eCash height
/// (`MAX_UNCHECKED_PER_HEIGHT` in the validator patch).
const VALIDATOR_PAIRS_PER_HEIGHT: usize = 16;

/// The answer of a validator that holds the most unchecked pairs at the
/// height of a pair (the `MAX_UNCHECKED_PER_HEIGHT` refusal in the patch).
pub const FULL_HEIGHT_REFUSAL: &str = "refused: too many pairs wait at this height";

/// The state that the loop and the BMM rpc share: the fee history, the bid of
/// the current tip, and the own pairs by commitment.
#[derive(Debug, Default)]
pub struct Memory {
    generation: u64,
    history: FeeHistory,
    fees: Option<(BlockHash, u64)>,
    /// The own pairs by commitment, each with its `remember` order.
    pairs: HashMap<[u8; 32], (u64, Pair)>,
    remembered: u64,
}

impl Memory {
    pub fn observe(&mut self, tip_height: u64, earned: impl Into<Earned>) {
        let earned = self.follow(earned.into());
        self.history.observe(tip_height, earned);
    }

    /// A new meter generation drops the history and the bid, because they
    /// count the fees of a branch that the chain left.
    fn follow(&mut self, earned: Earned) -> u64 {
        if earned.generation != self.generation {
            self.generation = earned.generation;
            self.history = FeeHistory::default();
            self.fees = None;
        }
        earned.total
    }

    pub fn forget_fees_below(&mut self, height: u64) {
        self.history.forget_below(height);
    }

    pub fn cached_fees(&self, tip: &BlockHash) -> Option<u64> {
        self.fees
            .and_then(|(cached, fees)| (&cached == tip).then_some(fees))
    }

    /// The bid for the block after `tip`, in sats. It stays the same for the
    /// whole tip. Zero means no bid.
    pub fn fees_at(
        &mut self,
        tip: (BlockHash, u32),
        earned: impl Into<Earned>,
        rules: &BidRules,
    ) -> u64 {
        let earned = self.follow(earned.into());
        if let Some(fees) = self.cached_fees(&tip.0) {
            return fees;
        }
        let tip_height = u64::from(tip.1);
        self.history.observe(tip_height, earned);
        let fees = self
            .history
            .income_per_block(tip_height, earned, rules.confirmations + 1)
            .and_then(|income| {
                bid_sats(
                    income,
                    rules.bid_percent,
                    rules.min_bid_sats,
                    rules.max_bid_sats,
                )
            })
            .unwrap_or(0);
        self.fees = Some((tip.0, fees));
        fees
    }

    /// Keeps `pair`, and drops the oldest pairs at its height above
    /// `MAX_PAIRS_PER_HEIGHT`.
    pub fn remember(&mut self, pair: Pair) {
        self.remembered += 1;
        self.pairs
            .insert(pair.commitment(), (self.remembered, pair));
        let mut at_height: Vec<(u64, [u8; 32])> = self
            .pairs
            .iter()
            .filter(|(_, (_, other))| other.height == pair.height)
            .map(|(commitment, (order, _))| (*order, *commitment))
            .collect();
        at_height.sort_unstable();
        let excess = at_height.len().saturating_sub(MAX_PAIRS_PER_HEIGHT);
        for (_, commitment) in &at_height[..excess] {
            self.pairs.remove(commitment);
        }
    }

    pub fn pair(&self, commitment: &[u8; 32]) -> Option<Pair> {
        self.pairs.get(commitment).map(|(_, pair)| *pair)
    }

    /// The own pairs that no settle passed yet, for a new publish at eCash tip
    /// `tip_height`. A height above the tip gives only its newest
    /// `VALIDATOR_PAIRS_PER_HEIGHT` pairs.
    pub fn pairs_to_publish(
        &mut self,
        next_height: u64,
        tip_height: u64,
        confirmations: u64,
    ) -> Vec<Pair> {
        self.pairs
            .retain(|_, (_, pair)| pair.height.saturating_add(REMEMBERED_HEIGHTS) >= tip_height);
        let mut pairs: Vec<(u64, Pair)> = self
            .pairs
            .values()
            .copied()
            .filter(|(_, pair)| {
                pair.height >= next_height && pair.height + confirmations + 2 >= tip_height
            })
            .collect();
        pairs.sort_by_key(|(order, pair)| (pair.height, std::cmp::Reverse(*order)));
        let mut sent_at_height = (0, 0);
        pairs
            .into_iter()
            .filter(|(_, pair)| {
                if pair.height <= tip_height {
                    return true;
                }
                if sent_at_height.0 != pair.height {
                    sent_at_height = (pair.height, 0);
                }
                sent_at_height.1 += 1;
                sent_at_height.1 <= VALIDATOR_PAIRS_PER_HEIGHT
            })
            .map(|(_, pair)| pair)
            .collect()
    }
}

/// An active eCash block and its commitment for the sidechain.
pub type ActiveBlock = (BlockHash, Option<[u8; 32]>);

/// Keeps each pair at a height above `tip_height`, and each pair whose
/// height has no active block yet. At a height with a block it keeps only the
/// pair that the block commits to. A height whose block read fails keeps no
/// pair, and the first read error comes back with the pairs. `pairs` must be
/// in height order.
pub async fn pairs_for_blocks<F, Fut>(
    pairs: Vec<Pair>,
    tip_height: u64,
    mut block_at: F,
) -> (Vec<Pair>, Result<(), BmmError>)
where
    F: FnMut(u64) -> Fut,
    Fut: Future<Output = Result<Option<ActiveBlock>, BmmError>>,
{
    let mut kept = Vec::with_capacity(pairs.len());
    let mut first_error = None;
    let mut block: Option<(u64, Option<Option<ActiveBlock>>)> = None;
    for pair in pairs {
        if pair.height > tip_height {
            kept.push(pair);
            continue;
        }
        if block.map(|(height, _)| height) != Some(pair.height) {
            let found = match block_at(pair.height).await {
                Ok(found) => Some(found),
                Err(error) => {
                    first_error.get_or_insert(error);
                    None
                }
            };
            block = Some((pair.height, found));
        }
        match block.and_then(|(_, found)| found) {
            None => {}
            Some(Some((_, commitment))) if commitment != Some(pair.commitment()) => {}
            Some(_) => kept.push(pair),
        }
    }
    (kept, first_error.map_or(Ok(()), Err))
}

/// The reads of eCash and Solana that a template needs, and the pair publish.
pub trait Chain: Send + Sync {
    fn tip(&self) -> impl Future<Output = Result<(BlockHash, u32), BmmError>> + Send;
    fn slot_is_active(&self) -> impl Future<Output = Result<bool, BmmError>> + Send;
    fn config(&self) -> impl Future<Output = Result<Config, BmmError>> + Send;
    /// The running total of the fees that the blocks of the validator paid
    /// into the treasury since the loop started.
    fn earned_fees(&self) -> impl Future<Output = Result<Earned, BmmError>> + Send;
    fn recorded_blocks(&self) -> impl Future<Output = Result<Vec<RecordedBlock>, BmmError>> + Send;
    /// The hash of the active eCash block at `height` below `tip`, in the
    /// internal byte order.
    fn block_hash_at(
        &self,
        tip: (BlockHash, u32),
        height: u64,
    ) -> impl Future<Output = Result<Option<[u8; 32]>, BmmError>> + Send;
    /// The active eCash block at `height` below `tip`, and its commitment for
    /// the sidechain.
    fn active_block(
        &self,
        tip: (BlockHash, u32),
        height: u64,
    ) -> impl Future<Output = Result<Option<ActiveBlock>, BmmError>> + Send;
    /// Sends pairs to the validator. It gives one answer per pair: "new",
    /// "known", or "refused: <reason>".
    fn publish(&self, pairs: &[Pair])
        -> impl Future<Output = Result<Vec<String>, BmmError>> + Send;
}

/// The chain behind the local enforcer and the local validator.
pub struct Live {
    pub rpc: Arc<RpcClient>,
    pub enforcer: Enforcer,
    pub program_id: Pubkey,
    fees: Mutex<FeeMeter>,
    cursor_headers: Mutex<ActiveHeaders>,
}

impl Live {
    pub fn new(rpc: Arc<RpcClient>, enforcer: Enforcer, program_id: Pubkey) -> Self {
        Self {
            rpc,
            enforcer,
            program_id,
            fees: Mutex::new(FeeMeter::default()),
            cursor_headers: Mutex::new(ActiveHeaders::default()),
        }
    }
}

/// The deepest eCash block below the tip that the loop asks the enforcer
/// for. The enforcer walks every header between the tip and the block.
pub const MAX_WALK: u64 = 2_016;

/// True when the loop may ask the enforcer for the block at `height`.
pub fn within_walk(tip_height: u64, height: u64) -> bool {
    tip_height
        .checked_sub(height)
        .is_some_and(|depth| depth <= MAX_WALK)
}

/// The most Solana slots that one fee read takes. A meter further behind
/// skips the older slots, so it can count too little but never too much.
pub const METER_SLOTS: u64 = 2_000;
/// The most slots that a meter catches up. Older slots stay uncounted.
pub const METER_BACKLOG: u64 = 20_000;
/// The longest time that one fee read takes. A slow read stops there, and
/// the next read goes on from the last block that it counted.
pub const METER_DEADLINE: Duration = Duration::from_secs(10);

/// The fees that the blocks paid into the treasury since the meter started.
/// A new generation starts when the confirmed branch drops a counted block.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Earned {
    pub generation: u64,
    pub total: u64,
}

impl From<u64> for Earned {
    fn from(total: u64) -> Self {
        Self {
            generation: 0,
            total,
        }
    }
}

/// One block for the fee meter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockFees {
    pub blockhash: String,
    pub previous: String,
    pub fees: u64,
}

/// Counts the fees of each confirmed block from the slot after its start.
#[derive(Debug, Default)]
pub struct FeeMeter {
    next_slot: Option<u64>,
    /// The slot and the hash of the newest counted block.
    last: Option<(u64, String)>,
    /// The ledger did not hold a block after `last`.
    gap: bool,
    earned: Earned,
}

/// What a fee read starts from. The read applies only when the meter is
/// still in this state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MeterMark {
    next_slot: Option<u64>,
    last: Option<(u64, String)>,
    gap: bool,
    earned: Earned,
}

/// The blocks that one fee read got, oldest first. `None` is a block that
/// the ledger does not hold. `covered` is the newest slot that the read
/// finished.
#[derive(Debug, Default)]
pub struct MeterScan {
    pub blocks: Vec<(u64, Option<BlockFees>)>,
    pub covered: Option<u64>,
}

impl FeeMeter {
    pub fn earned(&self) -> Earned {
        self.earned
    }

    fn mark(&self) -> MeterMark {
        MeterMark {
            next_slot: self.next_slot,
            last: self.last.clone(),
            gap: self.gap,
            earned: self.earned,
        }
    }

    fn restart(&mut self, slot: u64) {
        self.next_slot = Some(slot.saturating_add(1));
        self.last = None;
        self.gap = false;
        self.earned = Earned {
            generation: self.earned.generation + 1,
            total: self.earned.total,
        };
    }

    /// The slots to read when the confirmed slot is `slot`, and the mark of
    /// the read. A confirmed slot below the last counted block means that
    /// the validator went back, so the meter starts a new generation.
    pub fn plan(&mut self, slot: u64) -> Option<((u64, u64), MeterMark)> {
        let Some(next) = self.next_slot else {
            self.next_slot = Some(slot.saturating_add(1));
            return None;
        };
        if self.last.as_ref().is_some_and(|(last, _)| *last > slot) {
            self.restart(slot);
            return None;
        }
        if slot < next {
            return None;
        }
        let start = next.max(slot.saturating_sub(METER_BACKLOG - 1));
        Some(((start, slot.min(start + METER_SLOTS - 1)), self.mark()))
    }

    /// Counts the blocks of a read that started at `mark`. A block that does
    /// not name the last counted block as its parent can come from another
    /// branch, so the meter starts a new generation from `confirmed`. A
    /// missing block in between hides the parent, so that also starts one.
    pub fn apply(&mut self, mark: &MeterMark, scan: MeterScan, confirmed: u64) -> Earned {
        if self.mark() != *mark {
            return self.earned;
        }
        let Some(covered) = scan.covered else {
            return self.earned;
        };
        let mut fees = 0u64;
        let mut last = self.last.clone();
        let mut gap = self.gap;
        for (slot, block) in scan.blocks.into_iter().filter(|(slot, _)| *slot <= covered) {
            let Some(block) = block else {
                gap = last.is_some();
                continue;
            };
            let linked = match &last {
                None => true,
                Some((_, hash)) => !gap && *hash == block.previous,
            };
            if !linked {
                self.restart(confirmed);
                return self.earned;
            }
            fees = fees.saturating_add(block.fees);
            gap = false;
            last = Some((slot, block.blockhash));
        }
        self.next_slot = Some(covered.saturating_add(1));
        self.last = last;
        self.gap = gap;
        self.earned.total = self.earned.total.saturating_add(fees);
        self.earned
    }
}

/// The confirmed blocks of the validator, for the fee meter.
pub trait FeeSource: Send + Sync {
    fn confirmed_slot(&self) -> impl Future<Output = Result<u64, BmmError>> + Send;
    fn blocks(
        &self,
        start: u64,
        end: u64,
    ) -> impl Future<Output = Result<Vec<u64>, BmmError>> + Send;
    /// `None` when the ledger does not hold the block.
    fn block(&self, slot: u64) -> impl Future<Output = Result<Option<BlockFees>, BmmError>> + Send;
}

fn too_slow(call: &'static str) -> BmmError {
    BmmError::Rpc {
        call,
        source: ErrorKind::Custom(format!("the fee read took longer than {METER_DEADLINE:?}"))
            .into(),
    }
}

/// Reads the fees of the new confirmed blocks into `meter`. The lock never
/// holds over a call to the source, and the whole read ends at `deadline`.
/// An error other than a missing block stops the read, and the blocks
/// before it still count.
pub async fn read_fees<S: FeeSource>(
    meter: &Mutex<FeeMeter>,
    source: &S,
    deadline: Duration,
) -> Result<Earned, BmmError> {
    let until = tokio::time::Instant::now() + deadline;
    let confirmed = tokio::time::timeout_at(until, source.confirmed_slot())
        .await
        .map_err(|_| too_slow("getSlot"))??;
    let Some(((start, end), mark)) = meter.lock().await.plan(confirmed) else {
        return Ok(meter.lock().await.earned());
    };
    let slots = tokio::time::timeout_at(until, source.blocks(start, end))
        .await
        .map_err(|_| too_slow("getBlocks"))??;
    let mut scan = MeterScan::default();
    let mut stop = None;
    for slot in slots
        .into_iter()
        .filter(|slot| (start..=end).contains(slot))
    {
        match tokio::time::timeout_at(until, source.block(slot)).await {
            Ok(Ok(block)) => scan.blocks.push((slot, block)),
            Ok(Err(error)) => {
                stop = Some((slot, error));
                break;
            }
            Err(_) => {
                stop = Some((slot, too_slow("getBlock")));
                break;
            }
        }
    }
    scan.covered = match &stop {
        None => Some(end),
        Some((slot, _)) => slot.checked_sub(1).filter(|covered| *covered >= start),
    };
    let earned = meter.lock().await.apply(&mark, scan, confirmed);
    match stop {
        Some((_, error)) => Err(error),
        None => Ok(earned),
    }
}

/// True when an rpc error means that the ledger does not hold the block.
fn block_is_missing(error: &solana_rpc_client_api::client_error::Error) -> bool {
    use solana_rpc_client_api::{custom_error::*, request::RpcError};
    matches!(
        error.kind(),
        ErrorKind::RpcError(RpcError::RpcResponseError { code, .. })
            if [
                JSON_RPC_SERVER_ERROR_BLOCK_CLEANED_UP,
                JSON_RPC_SERVER_ERROR_BLOCK_NOT_AVAILABLE,
                JSON_RPC_SERVER_ERROR_SLOT_SKIPPED,
                JSON_RPC_SERVER_ERROR_LONG_TERM_STORAGE_SLOT_SKIPPED,
            ]
            .contains(code)
    )
}

/// The hashes and the treasury fees of one block of `getBlock`.
pub fn block_fees(
    slot: u64,
    block: &serde_json::Value,
    treasury: &Pubkey,
) -> Result<BlockFees, BmmError> {
    let bad = |reason| BmmError::BadBlock { slot, reason };
    let text = |key| {
        block
            .get(key)
            .and_then(|value| value.as_str())
            .map(str::to_owned)
            .ok_or(bad("no block hash"))
    };
    let rewards = block
        .get("rewards")
        .and_then(|rewards| rewards.as_array())
        .ok_or(bad("no rewards list"))?;
    let treasury = treasury.to_string();
    let mut fees = 0u64;
    for reward in rewards {
        if reward.get("rewardType").and_then(|kind| kind.as_str()) != Some("Fee")
            || reward.get("pubkey").and_then(|key| key.as_str()) != Some(treasury.as_str())
        {
            continue;
        }
        let lamports = reward
            .get("lamports")
            .and_then(|lamports| lamports.as_u64())
            .ok_or(bad("a fee reward without lamports"))?;
        fees = fees.checked_add(lamports).ok_or(bad("the fees overflow"))?;
    }
    Ok(BlockFees {
        blockhash: text("blockhash")?,
        previous: text("previousBlockhash")?,
        fees,
    })
}

/// The fee source behind the Solana rpc of the validator.
pub struct RpcFees<'a> {
    pub rpc: &'a RpcClient,
    pub treasury: Pubkey,
}

impl FeeSource for RpcFees<'_> {
    async fn confirmed_slot(&self) -> Result<u64, BmmError> {
        self.rpc
            .get_slot_with_commitment(CommitmentConfig::confirmed())
            .await
            .map_err(|source| BmmError::Rpc {
                call: "getSlot",
                source,
            })
    }

    async fn blocks(&self, start: u64, end: u64) -> Result<Vec<u64>, BmmError> {
        self.rpc
            .send(
                RpcRequest::GetBlocks,
                json!([start, end, {"commitment": "confirmed"}]),
            )
            .await
            .map_err(|source| BmmError::Rpc {
                call: "getBlocks",
                source,
            })
    }

    async fn block(&self, slot: u64) -> Result<Option<BlockFees>, BmmError> {
        match self
            .rpc
            .send::<serde_json::Value>(
                RpcRequest::GetBlock,
                json!([slot, {
                    "commitment": "confirmed",
                    "transactionDetails": "none",
                    "rewards": true,
                    "maxSupportedTransactionVersion": 0,
                }]),
            )
            .await
        {
            // Agave answers null for a block that it does not hold.
            Ok(serde_json::Value::Null) => Ok(None),
            Ok(block) => block_fees(slot, &block, &self.treasury).map(Some),
            Err(source) if block_is_missing(&source) => Ok(None),
            Err(source) => Err(BmmError::Rpc {
                call: "getBlock",
                source,
            }),
        }
    }
}

fn height_u32(height: u64) -> Result<u32, BmmError> {
    u32::try_from(height).map_err(|_| BmmError::HeightTooLarge(height))
}

impl Chain for Live {
    async fn tip(&self) -> Result<(BlockHash, u32), BmmError> {
        Ok(self.enforcer.clone().tip().await?)
    }

    async fn slot_is_active(&self) -> Result<bool, BmmError> {
        let mut enforcer = self.enforcer.clone();
        let sidechain_id = enforcer.sidechain_id();
        Ok(enforcer
            .sidechains()
            .await?
            .iter()
            .any(|slot| slot.sidechain_number == sidechain_id && slot.activation_height.is_some()))
    }

    async fn config(&self) -> Result<Config, BmmError> {
        read_config(&self.rpc, &self.program_id).await
    }

    async fn earned_fees(&self) -> Result<Earned, BmmError> {
        let source = RpcFees {
            rpc: &self.rpc,
            treasury: bridge::treasury_pda(&self.program_id).0,
        };
        read_fees(&self.fees, &source, METER_DEADLINE).await
    }

    async fn recorded_blocks(&self) -> Result<Vec<RecordedBlock>, BmmError> {
        recorded_blocks(&self.rpc).await
    }

    async fn block_hash_at(
        &self,
        tip: (BlockHash, u32),
        height: u64,
    ) -> Result<Option<[u8; 32]>, BmmError> {
        if !within_walk(u64::from(tip.1), height) {
            return Ok(None);
        }
        match self
            .enforcer
            .clone()
            .block_at(tip, height_u32(height)?)
            .await
        {
            Ok((hash, _)) => Ok(Some(hash.to_byte_array())),
            Err(EnforcerError::NoBlockAtHeight(_)) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    async fn active_block(
        &self,
        tip: (BlockHash, u32),
        height: u64,
    ) -> Result<Option<ActiveBlock>, BmmError> {
        if !within_walk(u64::from(tip.1), height) {
            return Ok(None);
        }
        match self
            .enforcer
            .clone()
            .active_block(tip, height_u32(height)?)
            .await
        {
            Ok(found) => Ok(Some(found)),
            Err(EnforcerError::NoBlockAtHeight(_)) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    async fn publish(&self, pairs: &[Pair]) -> Result<Vec<String>, BmmError> {
        publish_pairs(&self.rpc, pairs).await
    }
}

pub async fn run(settings: Settings, identity: Keypair) -> Result<(), BmmError> {
    let rules = BidRules {
        payee: identity.pubkey(),
        confirmations: settings.confirmations,
        bid_percent: settings.bid_percent,
        min_bid_sats: settings.min_bid_sats,
        max_bid_sats: settings.max_bid_sats,
    };
    rules.check()?;
    let mut enforcer = Enforcer::connect(
        settings.enforcer_url.clone(),
        u32::from(settings.sidechain_id),
    )
    .await?;
    enforcer.check_network(settings.network).await?;
    let live = Arc::new(Live::new(
        Arc::new(RpcClient::new_with_commitment(
            settings.solana_rpc_url.clone(),
            CommitmentConfig::confirmed(),
        )),
        enforcer,
        settings.program_id,
    ));
    let memory = Arc::new(Mutex::new(Memory::default()));
    tracing::info!(payee = %rules.payee, no_bid = settings.no_bid, "the BMM loop starts");
    let rounds = rounds(&settings, &identity, &live, &memory, &rules);
    let Some(addr) = settings.rpc_listen else {
        return rounds.await;
    };
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|source| BmmError::Listen { addr, source })?;
    tracing::info!(%addr, "the BMM rpc listens");
    let server = crate::bmm_rpc::Server {
        chain: live.clone(),
        memory: memory.clone(),
        rules,
        sidechain_id: settings.sidechain_id,
    };
    tokio::select! {
        result = rounds => result,
        result = crate::bmm_rpc::serve(listener, server) => result,
    }
}

async fn rounds(
    settings: &Settings,
    identity: &Keypair,
    live: &Live,
    memory: &Mutex<Memory>,
    rules: &BidRules,
) -> Result<(), BmmError> {
    let mut last_bid_tip: Option<BlockHash> = None;
    loop {
        let result = round(settings, identity, live, memory, rules, &mut last_bid_tip).await;
        if let Err(error) = result {
            if round_error_is_fatal(&error) {
                return Err(error);
            }
            tracing::warn!(%error, "the round failed, and the loop goes on");
        }
        tokio::time::sleep(settings.interval).await;
    }
}

async fn round(
    settings: &Settings,
    identity: &Keypair,
    live: &Live,
    memory: &Mutex<Memory>,
    rules: &BidRules,
    last_bid_tip: &mut Option<BlockHash>,
) -> Result<(), BmmError> {
    let rpc = &live.rpc;
    let mut enforcer = live.enforcer.clone();
    let tip = enforcer.tip().await?;
    let tip_height = u64::from(tip.1);
    let config = read_config(rpc, &settings.program_id).await?;
    match live.earned_fees().await {
        Ok(earned) => memory.lock().await.observe(tip_height, earned),
        Err(error) => tracing::warn!(%error, "the loop cannot read the fees of the blocks"),
    }

    let pairs = memory.lock().await.pairs_to_publish(
        config.bmm_next_height,
        tip_height,
        settings.confirmations,
    );
    // A validator that was away when the loop published gets the pair now.
    // After the eCash block only the won pair goes out, because the
    // validator holds few pairs per height.
    let published = async {
        let (pairs, read) =
            pairs_for_blocks(pairs, tip_height, |height| live.active_block(tip, height)).await;
        publish_pairs(rpc, &pairs).await?;
        read
    };
    if let Err(error) = published.await {
        tracing::warn!(%error, "the loop cannot publish its pairs");
    }
    memory
        .lock()
        .await
        .forget_fees_below(tip_height.saturating_sub(settings.confirmations + 1));
    // The settles and the pairs go first, so a slow bid never holds them back.
    let settled = settle(live, settings, identity, tip).await;
    // The settles can take long, and a bid must name the newest tip.
    let tip = enforcer.tip().await?;
    if bids_for(settings.no_bid, *last_bid_tip, tip.0) {
        let result = bid(live, &mut enforcer, memory, rules, tip).await;
        if let Err(error) = &result {
            if round_error_is_fatal(error) {
                return result.map(|_| ());
            }
            // A bid is an offer, not a duty. The loop must keep the settles
            // going when the eCash wallet or the enforcer refuses one bid.
            tracing::warn!(%error, "the bid failed, and the next round tries the tip again");
        }
        let retry = bids_again(&result);
        if !retry {
            *last_bid_tip = Some(tip.0);
        }
    }
    settled
}

/// What one round of the loop did about the bid for the next eCash block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bid {
    Sent(Pair),
    /// No bid for this tip.
    Declined,
    /// No recorded block of the followed branch passes rule 3 yet.
    NoBlock,
    /// The node or the wallet refused the bid tx for sure, so no tx went out.
    Rejected,
    /// The bid tx may have gone out, and its answer was lost.
    Unknown,
}

/// Keeps and publishes the pair of a bid before the bid goes out. A bid with
/// a lost answer can still win, and its commitment then needs the pair.
pub async fn keep_pair(memory: &Mutex<Memory>, rpc: &RpcClient, pair: Pair) {
    memory.lock().await.remember(pair);
    if let Err(error) = publish_pairs(rpc, &[pair]).await {
        tracing::warn!(%error, height = pair.height, "the loop cannot publish its pair yet");
    }
}

/// True when a checkpoint at eCash height `height` for `block` passes rule 3:
/// the bank credited every deposit through `height - lag`, and the active
/// eCash chain holds the credited block and the block of the open parts.
/// `active_block` gives the hash of the active eCash block at a height.
pub fn passes_rule_3(
    block: &RecordedBlock,
    height: u64,
    lag: u64,
    mut active_block: impl FnMut(u64) -> Option<[u8; 32]>,
) -> bool {
    let next = block.credited_height.saturating_add(1);
    block.credited_height.saturating_add(lag) >= height
        && [
            (block.credited_height, block.credited_block),
            (next, block.part_block),
        ]
        .into_iter()
        .all(|(at, hash)| hash == [0u8; 32] || active_block(at) == Some(hash))
}

/// `passes_rule_3` with the active eCash blocks of `chain` below `tip`.
/// `active` keeps the blocks that an earlier check read.
pub async fn passes_rule_3_on<C: Chain>(
    chain: &C,
    tip: (BlockHash, u32),
    block: &RecordedBlock,
    height: u64,
    lag: u64,
    active: &mut HashMap<u64, Option<[u8; 32]>>,
) -> Result<bool, BmmError> {
    if block.credited_height.saturating_add(lag) < height {
        return Ok(false);
    }
    let next = block.credited_height.saturating_add(1);
    for (at, hash) in [
        (block.credited_height, block.credited_block),
        (next, block.part_block),
    ] {
        if hash != [0u8; 32] && !active.contains_key(&at) {
            let found = chain.block_hash_at(tip, at).await?;
            active.insert(at, found);
        }
    }
    Ok(passes_rule_3(block, height, lag, |at| {
        active.get(&at).copied().flatten()
    }))
}

/// The newest block in the block record of the followed branch that passes
/// rule 3 for a checkpoint at eCash height `height`, below eCash tip `tip`.
pub async fn bid_block<C: Chain>(
    chain: &C,
    tip: (BlockHash, u32),
    height: u64,
    lag: u64,
) -> Result<Option<RecordedBlock>, BmmError> {
    let mut active = HashMap::new();
    for block in chain.recorded_blocks().await? {
        if passes_rule_3_on(chain, tip, &block, height, lag, &mut active).await? {
            return Ok(Some(block));
        }
    }
    Ok(None)
}

/// The bid for the eCash block after a tip: the pair of the newest block that
/// passes rule 3, and the sats that the fees allow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Template {
    pub tip: (BlockHash, u32),
    pub pair: Pair,
    /// The Solana slot of the block.
    pub slot: u64,
    pub fees_sats: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum NoTemplate {
    #[error("the sidechain slot is not active on eCash")]
    SlotInactive,
    #[error("Solana settled eCash height {0} already")]
    Settled(u64),
    #[error("no recorded block of the followed branch passes rule 3 for eCash height {0} yet")]
    NoBlock(u64),
    #[error(transparent)]
    Chain(#[from] BmmError),
}

/// The template for the eCash block after `tip`. It sends nothing.
pub async fn template<C: Chain>(
    chain: &C,
    memory: &Mutex<Memory>,
    rules: &BidRules,
    tip: (BlockHash, u32),
) -> Result<Template, NoTemplate> {
    if !chain.slot_is_active().await? {
        return Err(NoTemplate::SlotInactive);
    }
    let config = chain.config().await?;
    let height = u64::from(tip.1) + 1;
    if height < config.bmm_next_height {
        return Err(NoTemplate::Settled(height));
    }
    let cached = memory.lock().await.cached_fees(&tip.0);
    let fees_sats = match cached {
        Some(fees) => fees,
        None => {
            let earned = chain.earned_fees().await?;
            memory.lock().await.fees_at(tip, earned, rules)
        }
    };
    let block = bid_block(chain, tip, height, config.deposit_lag)
        .await?
        .ok_or(NoTemplate::NoBlock(height))?;
    Ok(Template {
        tip,
        pair: Pair {
            height,
            block: block.bank_hash,
            payee: rules.payee,
        },
        slot: block.slot,
        fees_sats,
    })
}

async fn bid(
    live: &Live,
    enforcer: &mut Enforcer,
    memory: &Mutex<Memory>,
    rules: &BidRules,
    tip: (BlockHash, u32),
) -> Result<Bid, BmmError> {
    let template = match template(live, memory, rules, tip).await {
        Ok(template) => template,
        Err(NoTemplate::Chain(error)) => return Err(error),
        Err(refusal @ NoTemplate::NoBlock(_)) => {
            tracing::info!(%refusal, "the loop waits");
            return Ok(Bid::NoBlock);
        }
        Err(refusal) => {
            tracing::info!(%refusal, "the loop makes no bid");
            return Ok(Bid::Declined);
        }
    };
    let Template {
        pair,
        slot,
        fees_sats: bid,
        ..
    } = template;
    let height = pair.height;
    if bid == 0 {
        tracing::debug!(height, "the fees of one block give no bid");
        return Ok(Bid::Declined);
    }
    keep_pair(memory, &live.rpc, pair).await;
    match enforcer
        .create_bmm_request(bid, tip, &pair.commitment())
        .await
    {
        Ok(txid) => {
            tracing::info!(height, bid, slot, %txid, "the loop bid for an eCash block");
            Ok(Bid::Sent(pair))
        }
        Err(error) if tip_moved(&error) => {
            // eCash found a block between the tip call and the bid. A BIP301
            // request names one block, so the loop bids again for the new tip.
            tracing::info!(
                height,
                "the eCash tip moved, so the bid waits for the new tip"
            );
            Ok(Bid::Declined)
        }
        Err(error) if crate::enforcer::bid_rejected(&error) => {
            tracing::warn!(
                %error,
                height,
                "the node refused the bid, so the wallet scans the chain, and the next round bids again"
            );
            let mut scanner = enforcer.clone();
            start_scan(async move { scanner.full_scan().await });
            Ok(Bid::Rejected)
        }
        Err(error) => {
            // The bid tx may be out. A second one could pay twice, so the
            // loop makes no other bid for this tip.
            tracing::warn!(%error, height, "the bid has no clear answer, and the tip gets no other bid");
            Ok(Bid::Unknown)
        }
    }
}

/// True while a wallet scan runs.
static SCAN_RUNS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Starts `scan` in the background unless a scan runs, and gives true when it
/// started one.
fn start_scan(
    scan: impl Future<Output = Result<(), crate::enforcer::EnforcerError>> + Send + 'static,
) -> bool {
    use std::sync::atomic::Ordering;
    if SCAN_RUNS.swap(true, Ordering::SeqCst) {
        return false;
    }
    tokio::spawn(async move {
        if let Err(error) = scan.await {
            tracing::error!(%error, "the wallet scan failed");
        }
        SCAN_RUNS.store(false, Ordering::SeqCst);
    });
    true
}

/// True when the round bids for the eCash tip `tip`, read after the settles:
/// bids are on, and the loop made no final bid for that tip yet.
pub fn bids_for(no_bid: bool, last_bid_tip: Option<BlockHash>, tip: BlockHash) -> bool {
    !no_bid && last_bid_tip != Some(tip)
}

/// True when the next round bids for the same tip again: no recorded block
/// passes rule 3 yet, the node refused the bid tx, or the round failed before
/// any bid tx. A bid whose outcome is unknown gets no second try.
pub fn bids_again(result: &Result<Bid, BmmError>) -> bool {
    matches!(result, Ok(Bid::NoBlock | Bid::Rejected) | Err(_))
}

/// True when a failed round must stop the loop. The enforcer and the Solana
/// rpc may fail for one round, and the next round reads them again.
pub fn round_error_is_fatal(error: &BmmError) -> bool {
    !matches!(
        error,
        BmmError::Enforcer(_) | BmmError::Rpc { .. } | BmmError::BadBlock { .. }
    )
}

/// True when the enforcer refused a bid because its `prev_bytes` names a block
/// that is no longer the tip.
pub fn tip_moved(error: &crate::enforcer::EnforcerError) -> bool {
    match error {
        crate::enforcer::EnforcerError::Call { source, .. } => {
            source.code() == tonic::Code::InvalidArgument && source.message().contains("prev_bytes")
        }
        _ => false,
    }
}

/// The most heights that one settle round moves the cursor past.
pub const SETTLE_STEPS: usize = 16;

/// The most header pages that one settle round reads from the enforcer.
pub const SETTLE_WALK_PAGES: usize = 8;

/// What the loop sends for the next height to settle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettleStep {
    /// The height is not deep enough, or its pair is unknown and the credits
    /// have not passed the skip blocks.
    Wait,
    /// A settle with this Solana block and payee. An empty height or a skip
    /// takes any pair, and it pays nothing.
    Send {
        block_hash: BlockHash,
        solana_block: [u8; 32],
        payee: Pubkey,
    },
}

/// The step for the next height `next`. The bridge settles heights only in
/// order, so the loop settles each height, an empty one too.
pub fn next_settle(
    next: u64,
    tip_height: u64,
    confirmations: u64,
    block: Option<ActiveBlock>,
    pairs: &[Pair],
    skip_ready: bool,
    sender: Pubkey,
) -> SettleStep {
    if !settle_is_ready(next, tip_height, confirmations) {
        return SettleStep::Wait;
    }
    let Some((block_hash, commitment)) = block else {
        return SettleStep::Wait;
    };
    let any = SettleStep::Send {
        block_hash,
        solana_block: [0u8; 32],
        payee: sender,
    };
    match commitment {
        None => any,
        Some(commitment) => match pair_for(pairs, next, &commitment) {
            Some(pair) => SettleStep::Send {
                block_hash,
                solana_block: pair.block,
                payee: pair.payee,
            },
            None if skip_ready => any,
            None => SettleStep::Wait,
        },
    }
}

/// The settles from the cursor `next` up, at most `SETTLE_STEPS`, as each
/// one moves the cursor by one. A commitment without a pair can skip once
/// `credited_height` reaches its height plus `skip_blocks` and `grace_passed`
/// holds for the cursor. It stops at the first wait.
#[allow(clippy::too_many_arguments)]
pub async fn plan_settles<F, Fut>(
    next: u64,
    tip_height: u64,
    confirmations: u64,
    pairs: &[Pair],
    skip_blocks: u64,
    credited_height: u64,
    grace_passed: bool,
    sender: Pubkey,
    mut block_at: F,
) -> Result<Vec<(u64, SettleStep)>, BmmError>
where
    F: FnMut(u64) -> Fut,
    Fut: Future<Output = Result<Option<ActiveBlock>, BmmError>>,
{
    let mut plan = Vec::new();
    for height in next..next.saturating_add(SETTLE_STEPS as u64) {
        if !settle_is_ready(height, tip_height, confirmations) {
            break;
        }
        let block = block_at(height).await?;
        let step = next_settle(
            height,
            tip_height,
            confirmations,
            block,
            pairs,
            credited_height >= height.saturating_add(skip_blocks) && grace_passed && height == next,
            sender,
        );
        if step == SettleStep::Wait {
            break;
        }
        plan.push((height, step));
    }
    Ok(plan)
}

/// Settles the heights from the cursor up, one at a time, in eCash height
/// order. The headers from the tip down to the cursor stay between rounds,
/// and a round reads at most `SETTLE_WALK_PAGES` pages of them.
async fn settle(
    live: &Live,
    settings: &Settings,
    identity: &Keypair,
    tip: (BlockHash, u32),
) -> Result<(), BmmError> {
    let rpc = &live.rpc;
    let tip_height = u64::from(tip.1);
    let config = read_config(rpc, &settings.program_id).await?;
    if !settle_is_ready(config.bmm_next_height, tip_height, settings.confirmations) {
        return Ok(());
    }
    let pairs = fetch_pairs(rpc, config.bmm_next_height).await?;
    let mut headers = live.cursor_headers.lock().await;
    headers
        .follow(
            &mut live.enforcer.clone(),
            tip,
            height_u32(config.bmm_next_height)?,
            SETTLE_WALK_PAGES,
        )
        .await?;
    let slot = rpc.get_slot().await.map_err(|source| BmmError::Rpc {
        call: "getSlot",
        source,
    })?;
    let plan = plan_settles(
        config.bmm_next_height,
        tip_height,
        settings.confirmations,
        &pairs,
        config.bmm_skip_blocks,
        config.credited_height,
        slot >= config
            .bmm_next_since_slot
            .saturating_add(bridge::SKIP_GRACE_SLOTS),
        identity.pubkey(),
        |height| {
            let header = u32::try_from(height)
                .ok()
                .and_then(|height| headers.get(height));
            let mut enforcer = live.enforcer.clone();
            async move {
                match header {
                    Some(header) => Ok(Some((
                        header.hash,
                        enforcer.commitment_at(header.hash).await?,
                    ))),
                    None => Ok(None),
                }
            }
        },
    )
    .await?;
    drop(headers);
    let mut paid_before = config.bmm_paid_total;
    for (height, step) in plan {
        let SettleStep::Send {
            block_hash,
            solana_block,
            payee,
        } = step
        else {
            return Ok(());
        };
        let instruction = bridge::settle_bmm_ix(
            &settings.program_id,
            height,
            &block_hash.to_byte_array(),
            &solana_block,
            &payee,
        );
        match send(rpc, identity, instruction, "settle_bmm").await {
            Ok(()) => {
                let paid_total = read_config(rpc, &settings.program_id).await?.bmm_paid_total;
                let paid = paid_total.saturating_sub(paid_before);
                paid_before = paid_total;
                if solana_block == [0u8; 32] {
                    tracing::info!(
                        height,
                        "the loop moved the cursor past a height with no win"
                    );
                } else if payee == identity.pubkey() {
                    tracing::info!(height, paid, "the loop settled its own win");
                } else {
                    tracing::info!(height, paid, %payee, "the loop settled a height");
                }
            }
            Err(BmmError::Rpc { source, .. }) if is_not_ready(&source) => {
                // A height below the view of the validator waits for its
                // backfill, and the next round sends it again.
                tracing::debug!(height, "the leader does not see the block deep enough yet");
                return Ok(());
            }
            Err(error) => {
                // Another settler moved the cursor first, or the block left
                // this branch. The next round reads again.
                tracing::info!(height, %error, "the settle did not land");
                return Ok(());
            }
        }
    }
    Ok(())
}

/// The blocks in the block record of the branch that the validator follows
/// now, newest first. The read is at `processed`, because `confirmed` follows
/// the stake votes and can name a branch that BMM leaves.
pub async fn recorded_blocks(rpc: &RpcClient) -> Result<Vec<RecordedBlock>, BmmError> {
    let account = rpc
        .get_account_with_commitment(&bridge::BMM_BLOCKS_ID, CommitmentConfig::processed())
        .await
        .map_err(|source| BmmError::Rpc {
            call: "getAccountInfo(block record)",
            source,
        })?
        .value;
    Ok(account
        .map(|account| bridge::recorded_blocks(&account.data))
        .unwrap_or_default())
}

/// Publishes `pair`, and fails unless the validator holds it.
pub async fn publish_one(chain: &impl Chain, pair: Pair) -> Result<(), BmmError> {
    held(pair.height, chain.publish(&[pair]).await?)
}

/// Fails unless `answers` is the answer "new" or "known" for one pair at
/// `height`.
pub fn held(height: u64, answers: Vec<String>) -> Result<(), BmmError> {
    match answers.as_slice() {
        [answer] if answer == "new" || answer == "known" => Ok(()),
        _ => Err(BmmError::PairRefused { height, answers }),
    }
}

/// Sends pairs to the validator. It gives one result per pair: "new",
/// "known", or the reason of a refusal.
pub async fn publish_pairs(rpc: &RpcClient, pairs: &[Pair]) -> Result<Vec<String>, BmmError> {
    if pairs.is_empty() {
        return Ok(Vec::new());
    }
    let list: Vec<serde_json::Value> = pairs
        .iter()
        .map(|pair| {
            json!([
                pair.height,
                Pubkey::new_from_array(pair.block).to_string(),
                pair.payee.to_string()
            ])
        })
        .collect();
    rpc.send(
        RpcRequest::Custom {
            method: "bmmPublishPairs",
        },
        json!([list]),
    )
    .await
    .map_err(|source| BmmError::Rpc {
        call: "bmmPublishPairs",
        source,
    })
}

/// The pairs that the validator holds at or above `from_height`.
pub async fn fetch_pairs(rpc: &RpcClient, from_height: u64) -> Result<Vec<Pair>, BmmError> {
    let list: Vec<(u64, String, String)> = rpc
        .send(
            RpcRequest::Custom {
                method: "bmmGetPairs",
            },
            json!([from_height]),
        )
        .await
        .map_err(|source| BmmError::Rpc {
            call: "bmmGetPairs",
            source,
        })?;
    Ok(parse_pairs(list))
}

/// The pairs of a `bmmGetPairs` answer. Anyone can publish a pair, so a pair
/// with a bad key drops out and the others stay.
pub fn parse_pairs(list: Vec<(u64, String, String)>) -> Vec<Pair> {
    list.into_iter()
        .filter_map(|(height, block, payee)| {
            Some(Pair {
                height,
                block: block.parse::<Pubkey>().ok()?.to_bytes(),
                payee: payee.parse::<Pubkey>().ok()?,
            })
        })
        .collect()
}

/// True when the validator did not take the settle because its enforcer does
/// not see the block deep enough yet. The loop tries again at the next tick.
fn is_not_ready(error: &solana_rpc_client_api::client_error::Error) -> bool {
    let transaction_error = match error.kind() {
        ErrorKind::TransactionError(error) => Some(error.clone()),
        ErrorKind::RpcError(_) => error.get_transaction_error(),
        _ => None,
    };
    matches!(
        transaction_error,
        Some(TransactionError::ProgramExecutionTemporarilyRestricted { .. })
    )
}

async fn read_config(rpc: &RpcClient, program_id: &Pubkey) -> Result<Config, BmmError> {
    let data = rpc
        .get_account_data(&bridge::config_pda(program_id).0)
        .await
        .map_err(|source| BmmError::Rpc {
            call: "getAccountInfo",
            source,
        })?;
    Ok(Config::decode(&data)?)
}

async fn send(
    rpc: &RpcClient,
    payer: &Keypair,
    instruction: Instruction,
    call: &'static str,
) -> Result<(), BmmError> {
    let blockhash = rpc
        .get_latest_blockhash()
        .await
        .map_err(|source| BmmError::Rpc {
            call: "getLatestBlockhash",
            source,
        })?;
    let transaction = Transaction::new_signed_with_payer(
        &[instruction],
        Some(&payer.pubkey()),
        &[payer],
        blockhash,
    );
    rpc.send_and_confirm_transaction(&transaction)
        .await
        .map_err(|source| BmmError::Rpc { call, source })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_enforcer_or_rpc_failure_does_not_stop_the_loop() {
        let refused = BmmError::Enforcer(crate::enforcer::EnforcerError::Call {
            call: "CreateBmmCriticalDataTransaction",
            source: tonic::Status::unknown("bad-txns-inputs-missingorspent"),
        });
        assert!(!round_error_is_fatal(&refused));
        let rpc = BmmError::Rpc {
            call: "getSlot",
            source: ErrorKind::Custom("the validator restarts".to_owned()).into(),
        };
        assert!(!round_error_is_fatal(&rpc));
        let block = BmmError::BadBlock {
            slot: 9,
            reason: "no rewards list",
        };
        assert!(!round_error_is_fatal(&block));
        assert!(round_error_is_fatal(&BmmError::BadBidPercent(0)));
        assert!(round_error_is_fatal(&BmmError::HeightTooLarge(1)));
    }

    #[tokio::test]
    async fn a_pair_is_kept_and_published_before_its_bid() {
        let memory = Mutex::new(Memory::default());
        let rpc = RpcClient::new_mock("succeeds".to_owned());
        let pair = pair_at(500, 9);
        keep_pair(&memory, &rpc, pair).await;
        // The bid then has a lost answer, and the pair stays for the publish.
        assert!(!bids_again(&Ok(Bid::Unknown)));
        let kept = memory.lock().await.pairs_to_publish(400, 505, 6);
        assert_eq!(kept, vec![pair]);
    }

    #[tokio::test]
    async fn one_wallet_scan_runs_at_a_time() {
        let (done, wait) = tokio::sync::oneshot::channel::<()>();
        assert!(start_scan(async move {
            let _ = wait.await;
            Ok(())
        }));
        assert!(!start_scan(async { Ok(()) }), "a scan runs");
        done.send(()).unwrap();
        let mut started = false;
        for _ in 0..100 {
            tokio::task::yield_now().await;
            if start_scan(async { Err(crate::enforcer::EnforcerError::MissingField("scan")) }) {
                started = true;
                break;
            }
        }
        assert!(started, "the end of a scan clears the gate");
        for _ in 0..100 {
            tokio::task::yield_now().await;
            if !SCAN_RUNS.load(std::sync::atomic::Ordering::SeqCst) {
                return;
            }
        }
        panic!("a failed scan clears the gate");
    }

    #[test]
    fn the_bid_names_the_tip_after_the_settles() {
        let before = BlockHash::from_byte_array([1u8; 32]);
        let after = BlockHash::from_byte_array([2u8; 32]);
        // A tip that moved during the settles gets its bid in this round.
        assert!(bids_for(false, Some(before), after));
        assert!(!bids_for(false, Some(after), after));
        assert!(!bids_for(true, None, after));
    }

    #[test]
    fn a_failed_bid_tries_the_tip_again_in_the_next_round() {
        let stale = BmmError::Enforcer(crate::enforcer::EnforcerError::Call {
            call: "CreateBmmCriticalDataTransaction",
            source: tonic::Status::unknown("bad-txns-inputs-missingorspent"),
        });
        assert!(bids_again(&Err(stale)));
        assert!(bids_again(&Ok(Bid::NoBlock)));
        assert!(bids_again(&Ok(Bid::Rejected)));
        assert!(
            !bids_again(&Ok(Bid::Unknown)),
            "a lost answer gets no second bid"
        );
        assert!(!bids_again(&Ok(Bid::Declined)));
        assert!(!bids_again(&Ok(Bid::Sent(Pair {
            height: 1,
            block: [1u8; 32],
            payee: Pubkey::new_from_array([2u8; 32]),
        }))));
    }

    #[test]
    fn a_refused_prev_bytes_means_the_tip_moved() {
        let moved = crate::enforcer::EnforcerError::Call {
            call: "CreateBmmCriticalDataTransaction",
            source: tonic::Status::invalid_argument("invalid prev_bytes 75 78: expected 3a 0c"),
        };
        assert!(tip_moved(&moved));
    }

    #[test]
    fn another_enforcer_error_is_not_a_moved_tip() {
        let other = crate::enforcer::EnforcerError::Call {
            call: "CreateBmmCriticalDataTransaction",
            source: tonic::Status::invalid_argument("the wallet holds no coins"),
        };
        assert!(!tip_moved(&other));
        assert!(!tip_moved(&crate::enforcer::EnforcerError::MissingField(
            "tip"
        )));
    }

    #[test]
    fn the_pair_for_a_commitment_has_its_height_and_its_hash() {
        let payee = Pubkey::new_from_array([7u8; 32]);
        let pair = Pair {
            height: 105,
            block: [3u8; 32],
            payee,
        };
        let other = Pair {
            height: 105,
            block: [4u8; 32],
            payee,
        };
        let pairs = [other, pair];
        assert_eq!(pair_for(&pairs, 105, &pair.commitment()), Some(pair));
        assert_eq!(pair_for(&pairs, 106, &pair.commitment()), None);
        assert_eq!(pair_for(&pairs, 105, &[0u8; 32]), None);
        assert_eq!(pair.commitment(), bridge::commitment_of(&[3u8; 32], &payee));
    }

    fn a_block(credited_height: u64, credited_block: [u8; 32]) -> RecordedBlock {
        RecordedBlock {
            slot: 640,
            bank_hash: [9u8; 32],
            credited_height,
            credited_block,
            part_block: [0u8; 32],
        }
    }

    #[test]
    fn rule_3_holds_at_exactly_h_minus_lag() {
        let block = a_block(95, [1u8; 32]);
        assert!(passes_rule_3(&block, 100, 5, |_| Some([1u8; 32])));
        assert!(!passes_rule_3(&block, 101, 5, |_| Some([1u8; 32])));
    }

    #[test]
    fn a_credit_of_a_block_that_the_active_chain_dropped_fails_rule_3() {
        let block = a_block(95, [1u8; 32]);
        assert!(!passes_rule_3(&block, 96, 5, |_| Some([2u8; 32])));
        assert!(!passes_rule_3(&block, 96, 5, |_| None));
    }

    #[test]
    fn open_parts_of_a_dropped_block_fail_rule_3() {
        let block = RecordedBlock {
            part_block: [3u8; 32],
            ..a_block(95, [1u8; 32])
        };
        let active = |part: [u8; 32]| move |at: u64| Some(if at == 96 { part } else { [1u8; 32] });
        assert!(passes_rule_3(&block, 100, 5, active([3u8; 32])));
        assert!(!passes_rule_3(&block, 100, 5, active([4u8; 32])));
    }

    #[test]
    fn a_bank_without_a_credit_needs_no_active_block() {
        let block = a_block(95, [0u8; 32]);
        assert!(passes_rule_3(&block, 100, 5, |_| None));
        assert!(!passes_rule_3(&block, 101, 5, |_| None));
    }

    #[test]
    fn a_bid_offers_its_percent_of_one_block_of_fees_in_sats() {
        assert_eq!(bid_sats(10_000_000, 90, 1, u64::MAX), Some(900_000));
    }

    #[test]
    fn fees_below_the_minimum_give_no_bid() {
        assert_eq!(bid_sats(10_000, 90, 1_000, u64::MAX), None);
    }

    #[test]
    fn no_fees_give_no_bid() {
        assert_eq!(bid_sats(0, 100, 0, u64::MAX), None);
    }

    #[test]
    fn the_max_bid_caps_each_bid() {
        assert_eq!(bid_sats(10_000_000, 90, 1, 5_000), Some(5_000));
        assert_eq!(bid_sats(u64::MAX, 100, 1, 5_000), Some(5_000));
    }

    #[test]
    fn the_rules_refuse_a_max_bid_that_allows_no_bid() {
        assert!(rules().check().is_ok());
        let zero = BidRules {
            max_bid_sats: 0,
            min_bid_sats: 0,
            ..rules()
        };
        assert!(matches!(zero.check(), Err(BmmError::BadMaxBid { .. })));
        let below = BidRules {
            max_bid_sats: 10,
            min_bid_sats: 11,
            ..rules()
        };
        assert!(matches!(below.check(), Err(BmmError::BadMaxBid { .. })));
        let percent = BidRules {
            bid_percent: 101,
            ..rules()
        };
        assert!(matches!(percent.check(), Err(BmmError::BadBidPercent(101))));
    }

    #[test]
    fn a_settle_waits_for_n_plus_two_blocks_on_top() {
        assert!(!settle_is_ready(100, 107, 6));
        assert!(settle_is_ready(100, 108, 6));
    }

    #[test]
    fn the_income_is_the_growth_per_block_over_the_span() {
        let mut history = FeeHistory::default();
        history.observe(100, 1_000);
        history.observe(103, 4_000);
        assert_eq!(history.income_per_block(107, 8_000, 7), Some(1_000));
    }

    #[test]
    fn a_short_history_gives_no_income() {
        let mut history = FeeHistory::default();
        history.observe(105, 1_000);
        assert_eq!(history.income_per_block(107, 8_000, 7), None);
    }

    #[test]
    fn the_first_total_at_a_height_stays() {
        let mut history = FeeHistory::default();
        history.observe(100, 1_000);
        history.observe(100, 9_000);
        assert_eq!(history.income_per_block(101, 2_000, 1), Some(1_000));
    }

    #[test]
    fn a_missed_tip_still_gives_an_estimate_from_an_older_one() {
        let mut history = FeeHistory::default();
        history.observe(90, 0);
        assert_eq!(history.income_per_block(100, 5_000, 7), Some(500));
    }

    fn rules() -> BidRules {
        BidRules {
            payee: Pubkey::new_from_array([7u8; 32]),
            confirmations: 6,
            bid_percent: 50,
            min_bid_sats: 1,
            max_bid_sats: 1_000_000,
        }
    }

    #[test]
    fn the_fees_of_a_tip_come_from_its_first_total() {
        let mut memory = Memory::default();
        memory.observe(93, 0);
        let tip = (BlockHash::from_byte_array([1u8; 32]), 100);
        assert_eq!(memory.fees_at(tip, 7_000_000, &rules()), 50_000);
        assert_eq!(memory.fees_at(tip, 70_000_000, &rules()), 50_000);
        assert_eq!(memory.cached_fees(&tip.0), Some(50_000));
        let next = (BlockHash::from_byte_array([2u8; 32]), 101);
        assert_eq!(memory.cached_fees(&next.0), None);
        assert_eq!(memory.fees_at(next, 8_000_000, &rules()), 50_000);
    }

    #[test]
    fn no_history_gives_zero_fees() {
        let mut memory = Memory::default();
        let tip = (BlockHash::from_byte_array([1u8; 32]), 100);
        assert_eq!(memory.fees_at(tip, 7_000_000, &rules()), 0);
    }

    #[test]
    fn a_settled_pair_is_not_published_but_stays_known() {
        let mut memory = Memory::default();
        let pair = Pair {
            height: 100,
            block: [3u8; 32],
            payee: Pubkey::new_from_array([7u8; 32]),
        };
        memory.remember(pair);
        assert_eq!(memory.pairs_to_publish(100, 105, 6), [pair]);
        assert!(memory.pairs_to_publish(101, 105, 6).is_empty());
        assert_eq!(memory.pair(&pair.commitment()), Some(pair));
        assert!(memory.pairs_to_publish(101, 1_101, 6).is_empty());
        assert_eq!(memory.pair(&pair.commitment()), None);
    }

    fn pairs_at(height: u64, count: u16) -> Vec<Pair> {
        (0..count)
            .map(|index| {
                let mut block = [0u8; 32];
                block[..2].copy_from_slice(&index.to_le_bytes());
                Pair {
                    height,
                    block,
                    payee: Pubkey::new_from_array([7u8; 32]),
                }
            })
            .collect()
    }

    #[test]
    fn a_height_keeps_more_pairs_than_the_validator_holds() {
        let mut memory = Memory::default();
        let pairs = pairs_at(100, 17);
        for pair in &pairs {
            memory.remember(*pair);
        }
        for pair in &pairs {
            assert_eq!(memory.pair(&pair.commitment()), Some(*pair));
        }
    }

    #[test]
    fn a_height_without_a_block_publishes_only_the_newest_pairs_that_the_validator_holds() {
        let mut memory = Memory::default();
        let open = pairs_at(106, 20);
        let mut mined = pairs_at(104, 20);
        for pair in &mut mined {
            pair.block[2] = 1;
        }
        for pair in open.iter().chain(&mined) {
            memory.remember(*pair);
        }
        let published = memory.pairs_to_publish(100, 105, 6);
        let commitments = |pairs: &mut dyn Iterator<Item = &Pair>| {
            let mut found: Vec<[u8; 32]> = pairs.map(Pair::commitment).collect();
            found.sort_unstable();
            found
        };
        let at = |height| commitments(&mut published.iter().filter(|pair| pair.height == height));
        assert_eq!(at(106), commitments(&mut open[4..].iter()));
        assert_eq!(at(104), commitments(&mut mined.iter()));
    }

    #[tokio::test]
    async fn a_failed_block_read_still_publishes_the_pairs_above_the_tip() {
        let mut pairs = pairs_at(100, 2);
        pairs.extend(pairs_at(101, 2));
        pairs.extend(pairs_at(102, 2));
        let (kept, read) = pairs_for_blocks(pairs.clone(), 101, |height| async move {
            match height {
                100 => Err(BmmError::HeightTooLarge(height)),
                _ => Ok(None),
            }
        })
        .await;
        assert_eq!(kept, pairs[2..]);
        assert!(matches!(read, Err(BmmError::HeightTooLarge(100))));
    }

    #[tokio::test]
    async fn after_its_block_a_height_publishes_only_the_committed_pair() {
        let mut pairs = pairs_at(100, 3);
        pairs.extend(pairs_at(101, 2));
        pairs.extend(pairs_at(102, 2));
        pairs.extend(pairs_at(103, 2));
        let won = pairs[1];
        let hash = BlockHash::from_byte_array([1u8; 32]);
        let (kept, read) = pairs_for_blocks(pairs.clone(), 102, |height| async move {
            Ok(match height {
                100 => Some((hash, Some(won.commitment()))),
                101 => Some((hash, None)),
                _ => None,
            })
        })
        .await;
        read.unwrap();
        assert_eq!(kept, [&[won][..], &pairs[5..]].concat());
    }

    #[test]
    fn a_height_keeps_only_the_newest_pairs_up_to_the_bound() {
        let mut memory = Memory::default();
        let pairs = pairs_at(100, MAX_PAIRS_PER_HEIGHT as u16 + 1);
        for pair in &pairs {
            memory.remember(*pair);
        }
        assert_eq!(memory.pair(&pairs[0].commitment()), None);
        for pair in &pairs[1..] {
            assert_eq!(memory.pair(&pair.commitment()), Some(*pair));
        }
    }

    #[test]
    fn forget_below_keeps_the_newest_old_total() {
        let mut history = FeeHistory::default();
        history.observe(90, 0);
        history.observe(95, 500);
        history.observe(99, 900);
        history.forget_below(96);
        assert_eq!(history.income_per_block(103, 1_300, 7), Some(100));
        assert_eq!(history.income_per_block(103, 1_300, 12), None);
    }

    #[test]
    fn a_fee_spike_in_one_block_does_not_move_the_median() {
        let mut history = FeeHistory::default();
        let mut earned = 0;
        for height in 100..107 {
            history.observe(height, earned);
            earned += if height == 103 { 1_000_000_000 } else { 1_000 };
        }
        assert_eq!(history.income_per_block(107, earned, 7), Some(1_000));
    }

    #[test]
    fn fees_in_most_blocks_set_the_median() {
        let mut history = FeeHistory::default();
        let mut earned = 0;
        for height in 100..107 {
            history.observe(height, earned);
            earned += if height % 2 == 0 { 5_000 } else { 0 };
        }
        assert_eq!(history.income_per_block(107, earned, 7), Some(5_000));
    }

    #[test]
    fn the_max_bid_caps_the_fees_of_a_tip() {
        let mut memory = Memory::default();
        memory.observe(93, 0);
        let tip = (BlockHash::from_byte_array([1u8; 32]), 100);
        let rules = BidRules {
            max_bid_sats: 20_000,
            ..rules()
        };
        assert_eq!(memory.fees_at(tip, 7_000_000, &rules), 20_000);
    }

    fn treasury() -> Pubkey {
        Pubkey::new_from_array([5u8; 32])
    }

    fn a_block_json(rewards: serde_json::Value) -> serde_json::Value {
        json!({"blockhash": "B", "previousBlockhash": "A", "rewards": rewards})
    }

    #[test]
    fn only_a_fee_reward_to_the_treasury_counts() {
        let block = a_block_json(json!([
            {"pubkey": treasury().to_string(), "lamports": 7_000, "rewardType": "Fee"},
            {"pubkey": treasury().to_string(), "lamports": 9_000, "rewardType": "Rent"},
            {"pubkey": Pubkey::new_from_array([6u8; 32]).to_string(), "lamports": 9_000, "rewardType": "Fee"},
            {"pubkey": treasury().to_string(), "lamports": 3_000, "rewardType": "Fee"},
        ]));
        assert_eq!(
            block_fees(9, &block, &treasury()).unwrap(),
            BlockFees {
                blockhash: "B".to_owned(),
                previous: "A".to_owned(),
                fees: 10_000
            }
        );
        assert_eq!(
            block_fees(9, &a_block_json(json!([])), &treasury())
                .unwrap()
                .fees,
            0
        );
    }

    #[test]
    fn a_block_without_rewards_or_hashes_does_not_decode() {
        assert!(matches!(
            block_fees(
                9,
                &json!({"blockhash": "B", "previousBlockhash": "A"}),
                &treasury()
            ),
            Err(BmmError::BadBlock { slot: 9, .. })
        ));
        assert!(block_fees(9, &json!({"rewards": []}), &treasury()).is_err());
        let negative = a_block_json(json!([
            {"pubkey": treasury().to_string(), "lamports": -1, "rewardType": "Fee"},
        ]));
        assert!(block_fees(9, &negative, &treasury()).is_err());
    }

    #[test]
    fn a_missing_block_is_only_a_missing_block_error() {
        use solana_rpc_client_api::request::{RpcError, RpcResponseErrorData};
        let response = |code| {
            solana_rpc_client_api::client_error::Error::from(ErrorKind::RpcError(
                RpcError::RpcResponseError {
                    code,
                    message: String::new(),
                    data: RpcResponseErrorData::Empty,
                },
            ))
        };
        assert!(block_is_missing(&response(-32007)));
        assert!(block_is_missing(&response(-32004)));
        assert!(
            !block_is_missing(&response(-32011)),
            "no history is a setup error"
        );
        assert!(!block_is_missing(
            &ErrorKind::Custom("refused".to_owned()).into()
        ));
    }

    fn fees_block(hash: &str, previous: &str, fees: u64) -> Option<BlockFees> {
        Some(BlockFees {
            blockhash: hash.to_owned(),
            previous: previous.to_owned(),
            fees,
        })
    }

    fn scan(blocks: Vec<(u64, Option<BlockFees>)>, covered: u64) -> MeterScan {
        MeterScan {
            blocks,
            covered: Some(covered),
        }
    }

    #[test]
    fn the_meter_counts_a_chain_of_blocks() {
        let mut meter = FeeMeter::default();
        assert!(meter.plan(500).is_none());
        let ((start, end), mark) = meter.plan(510).unwrap();
        assert_eq!((start, end), (501, 510));
        let read = scan(
            vec![
                (505, None),
                (507, fees_block("d", "c", 30)),
                (509, fees_block("e", "d", 40)),
            ],
            510,
        );
        assert_eq!(meter.apply(&mark, read, 510).total, 70);
        let ((start, _), mark) = meter.plan(520).unwrap();
        assert_eq!(start, 511);
        let next = scan(vec![(512, fees_block("f", "e", 5))], 520);
        assert_eq!(
            meter.apply(&mark, next, 520),
            Earned {
                generation: 0,
                total: 75
            }
        );
    }

    #[test]
    fn a_block_of_another_branch_starts_a_new_generation() {
        let mut meter = FeeMeter::default();
        meter.plan(500);
        let (_, mark) = meter.plan(510).unwrap();
        meter.apply(
            &mark,
            scan(vec![(508, fees_block("x", "w", 900))], 510),
            510,
        );
        let (_, mark) = meter.plan(520).unwrap();
        let other = scan(vec![(512, fees_block("y2", "w2", 5))], 520);
        let earned = meter.apply(&mark, other, 520);
        assert_eq!(earned.generation, 1);
        assert_eq!(meter.plan(520), None, "the new generation starts after 520");
        let (range, _) = meter.plan(530).unwrap();
        assert_eq!(range, (521, 530));
    }

    fn after_a_gap(next: Option<BlockFees>) -> Earned {
        let mut meter = FeeMeter::default();
        meter.plan(500);
        let (_, mark) = meter.plan(501).unwrap();
        meter.apply(
            &mark,
            scan(vec![(501, fees_block("h501", "h500", 10))], 501),
            501,
        );
        let (_, mark) = meter.plan(502).unwrap();
        meter.apply(&mark, scan(vec![(502, None)], 502), 502);
        let (_, mark) = meter.plan(503).unwrap();
        meter.apply(&mark, scan(vec![(503, next)], 503), 503)
    }

    #[test]
    fn a_block_after_a_missing_parent_starts_a_new_generation() {
        assert_eq!(after_a_gap(fees_block("x503", "x502", 7)).generation, 1);
        // The honest case resets too: no read proves that 502 descends from 501.
        assert_eq!(after_a_gap(fees_block("h503", "h502", 7)).generation, 1);
    }

    #[tokio::test]
    async fn a_null_block_is_a_missing_block() {
        let rpc = RpcClient::new_mock_with_mocks(
            "succeeds".to_owned(),
            HashMap::from([(RpcRequest::GetBlock, serde_json::Value::Null)]),
        );
        let source = RpcFees {
            rpc: &rpc,
            treasury: treasury(),
        };
        assert_eq!(source.block(9).await.unwrap(), None);
    }

    #[test]
    fn a_rollback_below_the_last_block_starts_a_new_generation() {
        let mut meter = FeeMeter::default();
        meter.plan(500);
        let (_, mark) = meter.plan(510).unwrap();
        meter.apply(
            &mark,
            scan(vec![(509, fees_block("x", "w", 900))], 510),
            510,
        );
        assert_eq!(meter.plan(505), None);
        assert_eq!(meter.earned().generation, 1);
        let (range, _) = meter.plan(515).unwrap();
        assert_eq!(range, (506, 515));
    }

    #[test]
    fn a_new_generation_drops_the_fee_history() {
        let mut memory = Memory::default();
        memory.observe(93, 0);
        let tip = (BlockHash::from_byte_array([1u8; 32]), 100);
        assert_eq!(memory.fees_at(tip, 7_000_000, &rules()), 50_000);
        let rolled = Earned {
            generation: 1,
            total: 7_000_000,
        };
        assert_eq!(memory.fees_at(tip, rolled, &rules()), 0);
        let next = (BlockHash::from_byte_array([2u8; 32]), 101);
        assert_eq!(memory.fees_at(next, rolled, &rules()), 0);
    }

    #[test]
    fn a_stale_read_does_not_apply() {
        let mut meter = FeeMeter::default();
        meter.plan(500);
        let (_, mark) = meter.plan(510).unwrap();
        let (_, same) = meter.plan(510).unwrap();
        meter.apply(&mark, scan(vec![(509, fees_block("x", "w", 9))], 510), 510);
        let late = meter.apply(&same, scan(vec![(509, fees_block("x", "w", 9))], 510), 510);
        assert_eq!(
            late.total, 9,
            "the second read of the same slots counts nothing"
        );
    }

    #[test]
    fn the_meter_reads_a_bounded_range() {
        let mut meter = FeeMeter::default();
        meter.plan(0);
        let ((start, end), mark) = meter.plan(5_000).unwrap();
        assert_eq!((start, end), (1, METER_SLOTS));
        meter.apply(&mark, scan(Vec::new(), end), 5_000);
        let far = 1_000_000;
        let ((start, end), _) = meter.plan(far).unwrap();
        assert_eq!(start, far - METER_BACKLOG + 1);
        assert_eq!(end, start + METER_SLOTS - 1);
    }

    struct FakeFees {
        meter: Arc<Mutex<FeeMeter>>,
        confirmed: u64,
        blocks: Vec<u64>,
        fail_at: Option<u64>,
        slow_at: Option<u64>,
    }

    impl FeeSource for FakeFees {
        async fn confirmed_slot(&self) -> Result<u64, BmmError> {
            Ok(self.confirmed)
        }
        async fn blocks(&self, start: u64, end: u64) -> Result<Vec<u64>, BmmError> {
            Ok(self
                .blocks
                .iter()
                .copied()
                .filter(|slot| (start..=end).contains(slot))
                .collect())
        }
        async fn block(&self, slot: u64) -> Result<Option<BlockFees>, BmmError> {
            assert!(
                self.meter.try_lock().is_ok(),
                "the read holds the meter lock"
            );
            if self.fail_at == Some(slot) {
                return Err(BmmError::Rpc {
                    call: "getBlock",
                    source: ErrorKind::Custom("connection reset".to_owned()).into(),
                });
            }
            if self.slow_at == Some(slot) {
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
            Ok(fees_block(&slot.to_string(), &(slot - 1).to_string(), 10))
        }
    }

    fn fake_fees(fail_at: Option<u64>, slow_at: Option<u64>) -> FakeFees {
        FakeFees {
            meter: Arc::new(Mutex::new(FeeMeter::default())),
            confirmed: 100,
            blocks: (101..=110).collect(),
            fail_at,
            slow_at,
        }
    }

    #[tokio::test]
    async fn a_transport_error_stops_the_read_and_keeps_the_blocks_before_it() {
        let mut source = fake_fees(Some(105), None);
        let meter = source.meter.clone();
        read_fees(&meter, &source, METER_DEADLINE).await.unwrap();
        source.confirmed = 110;
        assert!(read_fees(&meter, &source, METER_DEADLINE).await.is_err());
        assert_eq!(meter.lock().await.earned().total, 40);
        source.fail_at = None;
        let earned = read_fees(&meter, &source, METER_DEADLINE).await.unwrap();
        assert_eq!(earned.total, 100);
        assert_eq!(earned.generation, 0);
    }

    #[tokio::test]
    async fn a_slow_read_stops_at_its_deadline() {
        let mut source = fake_fees(None, Some(103));
        let meter = source.meter.clone();
        read_fees(&meter, &source, METER_DEADLINE).await.unwrap();
        source.confirmed = 110;
        let started = std::time::Instant::now();
        assert!(read_fees(&meter, &source, Duration::from_millis(200))
            .await
            .is_err());
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(meter.lock().await.earned().total, 20);
    }

    #[test]
    fn the_walk_stops_at_its_depth() {
        assert!(within_walk(5_000, 5_000));
        assert!(within_walk(5_000, 5_000 - MAX_WALK));
        assert!(!within_walk(5_000, 5_000 - MAX_WALK - 1));
        assert!(!within_walk(5_000, 0));
        assert!(!within_walk(5_000, 5_001));
    }

    fn pair_at(height: u64, block: u8) -> Pair {
        Pair {
            height,
            block: [block; 32],
            payee: Pubkey::new_from_array([7u8; 32]),
        }
    }

    #[test]
    fn the_loop_settles_each_height_in_order() {
        let sender = Pubkey::new_from_array([8u8; 32]);
        let won = pair_at(100, 9);
        let hash = BlockHash::from_byte_array([1u8; 32]);
        assert_eq!(
            next_settle(
                100,
                107,
                6,
                Some((hash, Some(won.commitment()))),
                &[won],
                false,
                sender
            ),
            SettleStep::Wait,
            "not deep enough"
        );
        assert_eq!(
            next_settle(
                100,
                108,
                6,
                Some((hash, Some(won.commitment()))),
                &[won],
                false,
                sender
            ),
            SettleStep::Send {
                block_hash: hash,
                solana_block: won.block,
                payee: won.payee
            }
        );
        assert_eq!(
            next_settle(100, 108, 6, Some((hash, None)), &[], false, sender),
            SettleStep::Send {
                block_hash: hash,
                solana_block: [0u8; 32],
                payee: sender
            },
            "an empty height settles"
        );
        assert_eq!(
            next_settle(100, 108, 6, None, &[won], false, sender),
            SettleStep::Wait
        );
    }

    #[tokio::test]
    async fn a_cursor_far_below_the_tip_catches_up_to_the_tip() {
        let sender = Pubkey::new_from_array([8u8; 32]);
        let tip_height = 9_000u64;
        let won = pair_at(4_000, 9);
        let mut next = tip_height - 3 * MAX_WALK;
        let mut rounds = 0;
        loop {
            let plan = plan_settles(
                next,
                tip_height,
                6,
                &[won],
                6,
                0,
                true,
                sender,
                |height| async move {
                    let hash = BlockHash::from_byte_array([height as u8; 32]);
                    let commitment = (height == 4_000).then(|| won.commitment());
                    Ok(Some((hash, commitment)))
                },
            )
            .await
            .unwrap();
            if plan.is_empty() {
                break;
            }
            assert!(plan.len() <= SETTLE_STEPS);
            for (height, _) in &plan {
                assert_eq!(*height, next);
                next += 1;
            }
            rounds += 1;
        }
        assert_eq!(
            next,
            tip_height - 7,
            "every height up to the deepest ready one settled"
        );
        assert!(rounds >= (3 * MAX_WALK as usize) / SETTLE_STEPS);
    }

    #[tokio::test]
    async fn a_plan_skips_only_the_cursor_and_only_after_the_grace_slots() {
        let sender = Pubkey::new_from_array([8u8; 32]);
        let plan = |grace_passed| {
            plan_settles(
                100,
                200,
                6,
                &[],
                6,
                110,
                grace_passed,
                sender,
                |height| async move {
                    let hash = BlockHash::from_byte_array([height as u8; 32]);
                    Ok(Some((hash, Some([5u8; 32]))))
                },
            )
        };
        assert!(plan(false).await.unwrap().is_empty());
        let skipped = plan(true).await.unwrap();
        assert_eq!(
            skipped
                .iter()
                .map(|(height, _)| *height)
                .collect::<Vec<_>>(),
            [100],
            "each skip starts a new grace, so the plan stops after the first one"
        );
        assert!(matches!(
            skipped[0].1,
            SettleStep::Send { solana_block, .. } if solana_block == [0u8; 32]
        ));
    }

    #[tokio::test]
    async fn a_cursor_far_below_the_credits_skips_only_after_the_grace_slots() {
        let sender = Pubkey::new_from_array([8u8; 32]);
        let credited_height = 10_000u64;
        let next = 100;
        let plan = |grace_passed| {
            plan_settles(
                next,
                credited_height + 6,
                6,
                &[],
                6,
                credited_height,
                grace_passed,
                sender,
                |height| async move {
                    let hash = BlockHash::from_byte_array([height as u8; 32]);
                    Ok(Some((hash, Some([5u8; 32]))))
                },
            )
        };
        assert!(plan(false).await.unwrap().is_empty());
        let skipped = plan(true).await.unwrap();
        assert_eq!(
            skipped
                .iter()
                .map(|(height, _)| *height)
                .collect::<Vec<_>>(),
            [next]
        );
    }

    #[test]
    fn a_commitment_without_a_pair_waits_for_the_skip_blocks() {
        let sender = Pubkey::new_from_array([8u8; 32]);
        let hash = BlockHash::from_byte_array([1u8; 32]);
        let garbage = Some((hash, Some([5u8; 32])));
        let other = pair_at(100, 9);
        assert_eq!(
            next_settle(100, 200, 6, garbage, &[other], false, sender),
            SettleStep::Wait
        );
        assert_eq!(
            next_settle(100, 200, 6, garbage, &[other], true, sender),
            SettleStep::Send {
                block_hash: hash,
                solana_block: [0u8; 32],
                payee: sender
            }
        );
    }

    #[test]
    fn a_pair_with_a_bad_key_drops_out() {
        let good = pair_at(9, 1);
        let pairs = parse_pairs(vec![
            (9, "not a key".to_owned(), good.payee.to_string()),
            (
                9,
                Pubkey::new_from_array(good.block).to_string(),
                good.payee.to_string(),
            ),
            (
                9,
                Pubkey::new_from_array(good.block).to_string(),
                "x".to_owned(),
            ),
        ]);
        assert_eq!(pairs, [good]);
    }
}
