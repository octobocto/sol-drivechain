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
    enforcer::{Enforcer, EnforcerError},
};

pub struct Settings {
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
    #[error("the bid percent is {0}, and it must be from 1 to 100")]
    BadBidPercent(u64),
    #[error("eCash height {0} does not fit a u32")]
    HeightTooLarge(u64),
    #[error("the validator gave a pair with a bad key: {0}")]
    BadPair(String),
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

/// The gross fee income that the loop saw at each eCash tip: the treasury
/// balance plus every payout. Payouts do not hide income in this sum.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct FeeHistory {
    by_height: BTreeMap<u64, u64>,
}

impl FeeHistory {
    /// Records the first total that the loop sees at a tip height.
    pub fn observe(&mut self, tip_height: u64, gross: u64) {
        self.by_height.entry(tip_height).or_insert(gross);
    }

    /// The fee income of one eCash block, from the growth of the total since
    /// the newest tip at least `span` blocks back. `None` means that the loop
    /// has not watched long enough.
    pub fn income_per_block(&self, tip_height: u64, gross: u64, span: u64) -> Option<u64> {
        let back = tip_height.checked_sub(span)?;
        let (&old_height, &old_gross) = self.by_height.range(..=back).next_back()?;
        let blocks = tip_height.checked_sub(old_height)?;
        gross.checked_sub(old_gross)?.checked_div(blocks)
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

/// The bid for a block that earns `income_lamports`. `None` means no bid.
pub fn bid_sats(income_lamports: u64, bid_percent: u64, min_bid_sats: u64) -> Option<u64> {
    let income_sats = income_lamports / bridge::LAMPORTS_PER_SAT;
    let bid = income_sats.checked_mul(bid_percent)? / 100;
    (bid >= min_bid_sats && bid > 0).then_some(bid)
}

/// True when a leader accepts a settle of `height` at eCash tip `tip_height`:
/// it needs N + 1 blocks on top.
pub fn settle_is_ready(height: u64, tip_height: u64, confirmations: u64) -> bool {
    height
        .checked_add(confirmations)
        .and_then(|needed| needed.checked_add(1))
        .is_some_and(|needed| tip_height >= needed)
}

/// The rules of a bid: the payee and the part of the fees that it offers.
#[derive(Clone, Copy, Debug)]
pub struct BidRules {
    pub payee: Pubkey,
    pub confirmations: u64,
    pub bid_percent: u64,
    pub min_bid_sats: u64,
}

/// An inclusion answer can name a pair this many eCash blocks after its
/// height.
const REMEMBERED_HEIGHTS: u64 = 1_000;

/// The state that the loop and the BMM rpc share: the fee history, the bid of
/// the current tip, and the own pairs by commitment.
#[derive(Debug, Default)]
pub struct Memory {
    history: FeeHistory,
    fees: Option<(BlockHash, u64)>,
    pairs: HashMap<[u8; 32], Pair>,
}

impl Memory {
    pub fn observe(&mut self, tip_height: u64, gross: u64) {
        self.history.observe(tip_height, gross);
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
    pub fn fees_at(&mut self, tip: (BlockHash, u32), gross: u64, rules: &BidRules) -> u64 {
        if let Some(fees) = self.cached_fees(&tip.0) {
            return fees;
        }
        let tip_height = u64::from(tip.1);
        self.history.observe(tip_height, gross);
        let fees = self
            .history
            .income_per_block(tip_height, gross, rules.confirmations + 1)
            .and_then(|income| bid_sats(income, rules.bid_percent, rules.min_bid_sats))
            .unwrap_or(0);
        self.fees = Some((tip.0, fees));
        fees
    }

    pub fn remember(&mut self, pair: Pair) {
        self.pairs.insert(pair.commitment(), pair);
    }

    pub fn pair(&self, commitment: &[u8; 32]) -> Option<Pair> {
        self.pairs.get(commitment).copied()
    }

    /// The own pairs that no settle passed yet, for a new publish at eCash tip
    /// `tip_height`.
    pub fn pairs_to_publish(
        &mut self,
        next_height: u64,
        tip_height: u64,
        confirmations: u64,
    ) -> Vec<Pair> {
        self.pairs
            .retain(|_, pair| pair.height.saturating_add(REMEMBERED_HEIGHTS) >= tip_height);
        let mut pairs: Vec<Pair> = self
            .pairs
            .values()
            .filter(|pair| {
                pair.height >= next_height && pair.height + confirmations + 2 >= tip_height
            })
            .copied()
            .collect();
        pairs.sort_by_key(|pair| pair.height);
        pairs
    }
}

/// An active eCash block and its commitment for the sidechain.
pub type ActiveBlock = (BlockHash, Option<[u8; 32]>);

/// The reads of eCash and Solana that a template needs, and the pair publish.
pub trait Chain: Send + Sync {
    fn tip(&self) -> impl Future<Output = Result<(BlockHash, u32), BmmError>> + Send;
    fn slot_is_active(&self) -> impl Future<Output = Result<bool, BmmError>> + Send;
    fn config(&self) -> impl Future<Output = Result<Config, BmmError>> + Send;
    fn treasury(&self) -> impl Future<Output = Result<u64, BmmError>> + Send;
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
    fn publish(&self, pairs: &[Pair]) -> impl Future<Output = Result<(), BmmError>> + Send;
}

/// The chain behind the local enforcer and the local validator.
pub struct Live {
    pub rpc: Arc<RpcClient>,
    pub enforcer: Enforcer,
    pub program_id: Pubkey,
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

    async fn treasury(&self) -> Result<u64, BmmError> {
        treasury_balance(&self.rpc, &self.program_id).await
    }

    async fn recorded_blocks(&self) -> Result<Vec<RecordedBlock>, BmmError> {
        recorded_blocks(&self.rpc).await
    }

    async fn block_hash_at(
        &self,
        tip: (BlockHash, u32),
        height: u64,
    ) -> Result<Option<[u8; 32]>, BmmError> {
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

    async fn publish(&self, pairs: &[Pair]) -> Result<(), BmmError> {
        publish_pairs(&self.rpc, pairs).await.map(|_| ())
    }
}

pub async fn run(settings: Settings, identity: Keypair) -> Result<(), BmmError> {
    if !(1..=100).contains(&settings.bid_percent) {
        return Err(BmmError::BadBidPercent(settings.bid_percent));
    }
    let live = Arc::new(Live {
        rpc: Arc::new(RpcClient::new_with_commitment(
            settings.solana_rpc_url.clone(),
            CommitmentConfig::confirmed(),
        )),
        enforcer: Enforcer::connect(
            settings.enforcer_url.clone(),
            u32::from(settings.sidechain_id),
        )
        .await?,
        program_id: settings.program_id,
    });
    let memory = Arc::new(Mutex::new(Memory::default()));
    let rules = BidRules {
        payee: identity.pubkey(),
        confirmations: settings.confirmations,
        bid_percent: settings.bid_percent,
        min_bid_sats: settings.min_bid_sats,
    };
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
    let rpc = &live.rpc;
    let mut enforcer = live.enforcer.clone();
    let mut last_bid_tip: Option<BlockHash> = None;
    loop {
        let tip = enforcer.tip().await?;
        let tip_height = u64::from(tip.1);
        let config = read_config(rpc, &settings.program_id).await?;
        let gross = treasury_balance(rpc, &settings.program_id)
            .await?
            .saturating_add(config.bmm_paid_total);
        memory.lock().await.observe(tip_height, gross);

        if !settings.no_bid && last_bid_tip != Some(tip.0) {
            let retry = match bid(live, &mut enforcer, memory, rules, tip).await {
                // The credits of the branch can catch up before the next
                // eCash block, so the next round tries the same tip again.
                Ok(Bid::NoBlock) => true,
                Ok(_) => false,
                Err(error) => {
                    if bid_error_is_fatal(&error) {
                        return Err(error);
                    }
                    // A bid is an offer, not a duty. The loop must keep the
                    // settles going when the eCash wallet or the enforcer
                    // refuses one bid.
                    tracing::warn!(%error, "the bid failed, and the loop goes on");
                    false
                }
            };
            if !retry {
                last_bid_tip = Some(tip.0);
            }
        }
        let pairs = memory.lock().await.pairs_to_publish(
            config.bmm_next_height,
            tip_height,
            settings.confirmations,
        );
        // A validator that was away when the loop published gets the pair now.
        if let Err(error) = publish_pairs(rpc, &pairs).await {
            tracing::warn!(%error, "the loop cannot publish its pairs");
        }
        settle(rpc, &mut enforcer, settings, identity, tip, &config).await?;
        memory
            .lock()
            .await
            .forget_fees_below(tip_height.saturating_sub(settings.confirmations + 1));
        tokio::time::sleep(settings.interval).await;
    }
}

/// What one round of the loop did about the bid for the next eCash block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bid {
    Sent(Pair),
    /// No bid for this tip.
    Declined,
    /// No recorded block of the followed branch passes rule 3 yet.
    NoBlock,
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
            let gross = chain
                .treasury()
                .await?
                .saturating_add(config.bmm_paid_total);
            memory.lock().await.fees_at(tip, gross, rules)
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
    match enforcer
        .create_bmm_request(bid, tip, &pair.commitment())
        .await
    {
        Ok(txid) => {
            tracing::info!(height, bid, slot, %txid, "the loop bid for an eCash block");
            memory.lock().await.remember(pair);
            if let Err(error) = publish_pairs(&live.rpc, &[pair]).await {
                tracing::warn!(%error, height, "the loop cannot publish its pair yet");
            }
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
        Err(error) => Err(error.into()),
    }
}

/// True when a failed bid must stop the loop. Only the eCash side of a bid
/// may fail without a stop, because the loop settles without it.
pub fn bid_error_is_fatal(error: &BmmError) -> bool {
    !matches!(error, BmmError::Enforcer(_))
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

/// Settles each commitment with a known pair from the cursor up, in eCash
/// height order. A commitment without a pair stays unsettled, and a later
/// settle moves the cursor past it.
async fn settle(
    rpc: &RpcClient,
    enforcer: &mut Enforcer,
    settings: &Settings,
    identity: &Keypair,
    tip: (BlockHash, u32),
    config: &Config,
) -> Result<(), BmmError> {
    let tip_height = u64::from(tip.1);
    if !settle_is_ready(config.bmm_next_height, tip_height, settings.confirmations) {
        return Ok(());
    }
    let pairs = fetch_pairs(rpc, config.bmm_next_height).await?;
    let mut heights: Vec<u64> = pairs.iter().map(|pair| pair.height).collect();
    heights.dedup();
    for height in heights {
        if !settle_is_ready(height, tip_height, settings.confirmations) {
            break;
        }
        let height_u32 = u32::try_from(height).map_err(|_| BmmError::HeightTooLarge(height))?;
        let (block_hash, commitment) = enforcer.active_block(tip, height_u32).await?;
        let Some(pair) = commitment.and_then(|commitment| pair_for(&pairs, height, &commitment))
        else {
            continue;
        };
        let before = read_config(rpc, &settings.program_id).await?.bmm_paid_total;
        let instruction = bridge::settle_bmm_ix(
            &settings.program_id,
            height,
            &block_hash.to_byte_array(),
            &pair.block,
            &pair.payee,
        );
        match send(rpc, identity, instruction, "settle_bmm").await {
            Ok(()) => {
                let paid = read_config(rpc, &settings.program_id)
                    .await?
                    .bmm_paid_total
                    .saturating_sub(before);
                if pair.payee == identity.pubkey() {
                    tracing::info!(height, paid, "the loop settled its own win");
                } else {
                    tracing::info!(height, paid, payee = %pair.payee, "the loop settled a height");
                }
            }
            Err(BmmError::Rpc { source, .. }) if is_not_ready(&source) => {
                tracing::debug!(height, "the leader does not see the block deep enough yet");
                return Ok(());
            }
            Err(error) => {
                // The Solana block is not on this branch, or another settler
                // moved the cursor first.
                tracing::info!(height, %error, "the settle did not land");
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
    list.into_iter()
        .map(|(height, block, payee)| {
            let block = block
                .parse::<Pubkey>()
                .map_err(|_| BmmError::BadPair(block.clone()))?;
            let payee = payee
                .parse::<Pubkey>()
                .map_err(|_| BmmError::BadPair(payee.clone()))?;
            Ok(Pair {
                height,
                block: block.to_bytes(),
                payee,
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

async fn treasury_balance(rpc: &RpcClient, program_id: &Pubkey) -> Result<u64, BmmError> {
    rpc.get_balance(&bridge::treasury_pda(program_id).0)
        .await
        .map_err(|source| BmmError::Rpc {
            call: "getBalance",
            source,
        })
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
    fn an_ecash_bid_failure_does_not_stop_the_loop() {
        let refused = BmmError::Enforcer(crate::enforcer::EnforcerError::Call {
            call: "CreateBmmCriticalDataTransaction",
            source: tonic::Status::unknown("bad-txns-inputs-missingorspent"),
        });
        assert!(!bid_error_is_fatal(&refused));
        assert!(bid_error_is_fatal(&BmmError::BadBidPercent(0)));
        assert!(bid_error_is_fatal(&BmmError::HeightTooLarge(1)));
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
        assert_eq!(bid_sats(10_000_000, 90, 1), Some(900_000));
    }

    #[test]
    fn fees_below_the_minimum_give_no_bid() {
        assert_eq!(bid_sats(10_000, 90, 1_000), None);
    }

    #[test]
    fn no_fees_give_no_bid() {
        assert_eq!(bid_sats(0, 100, 0), None);
    }

    #[test]
    fn a_settle_waits_for_n_plus_one_blocks_on_top() {
        assert!(!settle_is_ready(100, 106, 6));
        assert!(settle_is_ready(100, 107, 6));
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
}
