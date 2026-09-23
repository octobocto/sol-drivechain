//! The BMM loop of a validator: it bids on eCash, and it settles each eCash
//! height on Solana in order.
//!
//! The commitment is the payee pubkey, so a win needs no later step from the
//! winner. Any settle of that height pays the payee.

use std::{collections::BTreeMap, time::Duration};

use bitcoin::{hashes::Hash as _, BlockHash};
use solana_commitment_config::CommitmentConfig;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_rpc_client_api::client_error::ErrorKind;
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
                )
                .await;
                if let Err(error) = result {
                    if bid_error_is_fatal(&error) {
                        return Err(error);
                    }
                    // A bid is an offer, not a duty. The loop must keep the
                    // settles going when the eCash wallet or the enforcer
                    // refuses one bid.
                    tracing::warn!(%error, "the bid failed, and the loop goes on");
                }
            } else {
                tracing::info!(
                    slot = settings.sidechain_id,
                    "the sidechain slot is not active, so the loop makes no bid"
                );
            }
            last_bid_tip = Some(tip.0);
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

async fn bid(
    enforcer: &mut Enforcer,
    settings: &Settings,
    identity: &Keypair,
    tip: (BlockHash, u32),
    config: &Config,
    history: &FeeHistory,
    gross: u64,
) -> Result<(), BmmError> {
    let height = u64::from(tip.1) + 1;
    if height < config.bmm_next_height {
        // Solana settled this height already, on a branch that eCash left.
        tracing::info!(
            height,
            "Solana settled the height, so the loop does not bid"
        );
        return Ok(());
    }
    let span = settings.confirmations + 1;
    let Some(income) = history.income_per_block(u64::from(tip.1), gross, span) else {
        tracing::debug!(
            height,
            "the loop has not watched the fees long enough to bid"
        );
        return Ok(());
    };
    let Some(bid) = bid_sats(income, settings.bid_percent, settings.min_bid_sats) else {
        tracing::debug!(
            height,
            income,
            "the fees of one block are too small for a bid"
        );
        return Ok(());
    };
    match enforcer
        .create_bmm_request(bid, tip, &identity.pubkey().to_bytes())
        .await
    {
        Ok(txid) => {
            tracing::info!(height, bid, %txid, "the loop bid for an eCash block");
            Ok(())
        }
        Err(error) if tip_moved(&error) => {
            // eCash found a block between the tip call and the bid. A BIP301
            // request names one block, so the loop bids again for the new tip.
            tracing::info!(
                height,
                "the eCash tip moved, so the bid waits for the new tip"
            );
            Ok(())
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

async fn settle(
    rpc: &RpcClient,
    enforcer: &mut Enforcer,
    settings: &Settings,
    identity: &Keypair,
    tip: (BlockHash, u32),
    config: &Config,
) -> Result<(), BmmError> {
    let mut next = config.bmm_next_height;
    while settle_is_ready(next, u64::from(tip.1), settings.confirmations) {
        let height = u32::try_from(next).map_err(|_| BmmError::HeightTooLarge(next))?;
        let (block_hash, commitment) = enforcer.active_block(tip, height).await?;
        let winner = commitment.map(Pubkey::new_from_array);
        let payee = winner.unwrap_or_else(|| identity.pubkey());
        let before = read_config(rpc, &settings.program_id).await?.bmm_paid_total;
        let instruction = bridge::settle_bmm_ix(
            &settings.program_id,
            next,
            &block_hash.to_byte_array(),
            &payee,
        );
        match send(rpc, identity, instruction, "settle_bmm").await {
            Ok(()) => {
                let paid = read_config(rpc, &settings.program_id)
                    .await?
                    .bmm_paid_total
                    .saturating_sub(before);
                if winner == Some(identity.pubkey()) {
                    tracing::info!(height = next, paid, "the loop settled its own win");
                } else {
                    tracing::info!(height = next, paid, ?winner, "the loop settled a height");
                }
            }
            Err(BmmError::Rpc { source, .. }) if is_not_ready(&source) => {
                tracing::debug!(
                    height = next,
                    "the leader does not see the block deep enough yet"
                );
                return Ok(());
            }
            Err(error) => {
                if read_config(rpc, &settings.program_id)
                    .await?
                    .bmm_next_height
                    <= next
                {
                    return Err(error);
                }
                // Another settler got there first, which is the same result.
                tracing::debug!(height = next, %error, "another settler settled the height");
            }
        }
        next += 1;
    }
    Ok(())
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
