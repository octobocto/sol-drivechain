//! The BMM loop of a validator: it bids on eCash, it publishes the pair of
//! each bid to the validators, and it settles the commitments on Solana in
//! eCash height order.
//!
//! A bid commits h* = SHA-256(Solana bank hash ‖ payee) for the newest block in
//! the block record of the branch that the validator follows. The pair goes to
//! the validators outside any transaction, so a leader cannot hide it.

use std::{collections::BTreeMap, time::Duration};

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

use crate::{
    bridge::{self, Config},
    enforcer::Enforcer,
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

pub async fn run(settings: Settings, identity: Keypair) -> Result<(), BmmError> {
    if !(1..=100).contains(&settings.bid_percent) {
        return Err(BmmError::BadBidPercent(settings.bid_percent));
    }
    let rpc = RpcClient::new_with_commitment(
        settings.solana_rpc_url.clone(),
        CommitmentConfig::confirmed(),
    );
    let mut enforcer = Enforcer::connect(
        settings.enforcer_url.clone(),
        u32::from(settings.sidechain_id),
    )
    .await?;
    tracing::info!(payee = %identity.pubkey(), "the BMM loop starts");

    let mut history = FeeHistory::default();
    let mut last_bid_tip: Option<BlockHash> = None;
    let mut own_pairs: Vec<Pair> = Vec::new();
    loop {
        let tip = enforcer.tip().await?;
        let tip_height = u64::from(tip.1);
        let config = read_config(&rpc, &settings.program_id).await?;
        let gross = treasury_balance(&rpc, &settings.program_id)
            .await?
            .saturating_add(config.bmm_paid_total);
        history.observe(tip_height, gross);

        if last_bid_tip != Some(tip.0) {
            if slot_is_active(&mut enforcer, settings.sidechain_id).await? {
                let result = bid(
                    &mut enforcer,
                    &settings,
                    &identity,
                    tip,
                    &config,
                    &history,
                    gross,
                    &rpc,
                )
                .await;
                match result {
                    Ok(Some(pair)) => own_pairs.push(pair),
                    Ok(None) => (),
                    Err(error) => {
                        if bid_error_is_fatal(&error) {
                            return Err(error);
                        }
                        // A bid is an offer, not a duty. The loop must keep the
                        // settles going when the eCash wallet or the enforcer
                        // refuses one bid.
                        tracing::warn!(%error, "the bid failed, and the loop goes on");
                    }
                }
            } else {
                tracing::info!(
                    slot = settings.sidechain_id,
                    "the sidechain slot is not active, so the loop makes no bid"
                );
            }
            last_bid_tip = Some(tip.0);
        }
        own_pairs.retain(|pair| {
            pair.height >= config.bmm_next_height
                && pair.height + settings.confirmations + 2 >= tip_height
        });
        // A validator that was away when the loop published gets the pair now.
        if let Err(error) = publish_pairs(&rpc, &own_pairs).await {
            tracing::warn!(%error, "the loop cannot publish its pairs");
        }
        settle(&rpc, &mut enforcer, &settings, &identity, tip, &config).await?;
        history.forget_below(tip_height.saturating_sub(settings.confirmations + 1));
        tokio::time::sleep(settings.interval).await;
    }
}

/// True when the mainchain activated the sidechain slot. A BMM request for an
/// inactive slot has no value, so the loop does not send one.
async fn slot_is_active(enforcer: &mut Enforcer, sidechain_id: u8) -> Result<bool, BmmError> {
    Ok(enforcer.sidechains().await?.iter().any(|slot| {
        slot.sidechain_number == u32::from(sidechain_id) && slot.activation_height.is_some()
    }))
}

#[allow(clippy::too_many_arguments)]
async fn bid(
    enforcer: &mut Enforcer,
    settings: &Settings,
    identity: &Keypair,
    tip: (BlockHash, u32),
    config: &Config,
    history: &FeeHistory,
    gross: u64,
    rpc: &RpcClient,
) -> Result<Option<Pair>, BmmError> {
    let height = u64::from(tip.1) + 1;
    if height < config.bmm_next_height {
        // Solana settled this height already, on a branch that eCash left.
        tracing::info!(
            height,
            "Solana settled the height, so the loop does not bid"
        );
        return Ok(None);
    }
    let span = settings.confirmations + 1;
    let Some(income) = history.income_per_block(u64::from(tip.1), gross, span) else {
        tracing::debug!(
            height,
            "the loop has not watched the fees long enough to bid"
        );
        return Ok(None);
    };
    let Some(bid) = bid_sats(income, settings.bid_percent, settings.min_bid_sats) else {
        tracing::debug!(
            height,
            income,
            "the fees of one block are too small for a bid"
        );
        return Ok(None);
    };
    let Some((slot, block)) = newest_recorded_block(rpc).await? else {
        tracing::info!(
            height,
            "the block record is empty, so the loop does not bid"
        );
        return Ok(None);
    };
    let pair = Pair {
        height,
        block,
        payee: identity.pubkey(),
    };
    match enforcer
        .create_bmm_request(bid, tip, &pair.commitment())
        .await
    {
        Ok(txid) => {
            tracing::info!(height, bid, slot, %txid, "the loop bid for an eCash block");
            if let Err(error) = publish_pairs(rpc, &[pair]).await {
                tracing::warn!(%error, height, "the loop cannot publish its pair yet");
            }
            Ok(Some(pair))
        }
        Err(error) if tip_moved(&error) => {
            // eCash found a block between the tip call and the bid. A BIP301
            // request names one block, so the loop bids again for the new tip.
            tracing::info!(
                height,
                "the eCash tip moved, so the bid waits for the new tip"
            );
            Ok(None)
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

/// The newest block in the block record of the branch that the validator
/// follows now, read at `processed`, because `confirmed` follows the stake
/// votes and can name a branch that BMM leaves.
pub async fn newest_recorded_block(rpc: &RpcClient) -> Result<Option<(u64, [u8; 32])>, BmmError> {
    let account = rpc
        .get_account_with_commitment(&bridge::BMM_BLOCKS_ID, CommitmentConfig::processed())
        .await
        .map_err(|source| BmmError::Rpc {
            call: "getAccountInfo(block record)",
            source,
        })?
        .value;
    Ok(account.and_then(|account| bridge::newest_recorded_block(&account.data)))
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
