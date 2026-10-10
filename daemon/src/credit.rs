//! The credit loop. It sends `credit_deposits` for the next eCash height that
//! the bridge config of the followed branch has not credited. Anyone can run
//! it, and the peg runs it for liveness.

use std::time::Duration;

use bitcoin::hashes::Hash as _;
use solana_commitment_config::CommitmentConfig;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    pubkey::Pubkey,
    signature::{Keypair, Signer as _},
    transaction::Transaction,
};

use crate::{
    address,
    bridge::{self, Config},
    enforcer::{DepositEvent, Enforcer, EnforcerError},
};

/// The most deposits in one credit. Each deposit with a target adds one
/// account to the tx.
pub const MAX_PART: usize = 16;

const IDLE: Duration = Duration::from_secs(1);

#[derive(Debug, thiserror::Error)]
pub enum CreditError {
    #[error(transparent)]
    Enforcer(#[from] EnforcerError),
    #[error(transparent)]
    Bridge(#[from] bridge::BridgeError),
    #[error("the Solana rpc call `{call}` failed")]
    Rpc {
        call: &'static str,
        #[source]
        source: solana_rpc_client_api::client_error::Error,
    },
    #[error("eCash height {0} does not fit a u32")]
    HeightTooLarge(u64),
}

/// One credit: a part of the deposits of the block at `height`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Part {
    pub height: u64,
    pub first: u32,
    pub count: u8,
    pub total: usize,
    /// The pubkey of each deposit of the part that names one, in order.
    pub targets: Vec<Pubkey>,
}

/// The next part of the deposits of a block, from the credit index of the
/// config. A deposit whose OP_RETURN names no pubkey adds no target.
pub fn next_part(height: u64, credit_index: u64, deposits: &[DepositEvent]) -> Part {
    let first = usize::try_from(credit_index)
        .unwrap_or(usize::MAX)
        .min(deposits.len());
    let part = &deposits[first..deposits.len().min(first.saturating_add(MAX_PART))];
    Part {
        height,
        first: u32::try_from(first).unwrap_or(u32::MAX),
        count: u8::try_from(part.len()).unwrap_or(u8::MAX),
        total: deposits.len(),
        targets: part.iter().filter_map(target).collect(),
    }
}

/// The pubkey that a deposit OP_RETURN names. The validator reads it the same
/// way.
pub fn target(deposit: &DepositEvent) -> Option<Pubkey> {
    let text = std::str::from_utf8(&deposit.address).ok()?;
    address::parse_deposit_recipient(text).ok()
}

/// True when a leader takes a credit of `height` at eCash tip `tip_height`:
/// the block needs D confirmations, and the leader asks for one more.
pub fn credit_is_ready(height: u64, tip_height: u64, deposit_confirmations: u64) -> bool {
    height
        .checked_add(deposit_confirmations)
        .is_some_and(|needed| tip_height >= needed)
}

pub async fn run(
    enforcer: &mut Enforcer,
    solana_rpc_url: String,
    program_id: Pubkey,
    payer: &Keypair,
) -> Result<(), CreditError> {
    // The followed branch can be one that the stake does not confirm yet.
    let rpc = RpcClient::new_with_commitment(solana_rpc_url, CommitmentConfig::processed());
    tracing::info!(payer = %payer.pubkey(), "the credit loop starts");
    loop {
        if !credit_next(enforcer, &rpc, &program_id, payer).await? {
            tokio::time::sleep(IDLE).await;
        }
    }
}

/// Sends the next credit, and gives true when it landed.
async fn credit_next(
    enforcer: &mut Enforcer,
    rpc: &RpcClient,
    program_id: &Pubkey,
    payer: &Keypair,
) -> Result<bool, CreditError> {
    let config = read_config(rpc, program_id).await?;
    let height = config.credited_height.saturating_add(1);
    let tip = enforcer.tip().await?;
    if !credit_is_ready(height, u64::from(tip.1), config.deposit_confirmations) {
        return Ok(false);
    }
    let at = u32::try_from(height).map_err(|_| CreditError::HeightTooLarge(height))?;
    let (block_hash, prev_hash) = enforcer.block_at(tip, at).await?;
    if config.credited_block != [0u8; 32] && config.credited_block != prev_hash.to_byte_array() {
        tracing::warn!(
            height,
            "the followed branch credited an eCash block that the active chain dropped, so \
             nobody can credit on it"
        );
        return Ok(false);
    }
    let deposits = enforcer.block_deposits(block_hash, prev_hash).await?;
    let part = next_part(height, config.credit_index, &deposits);
    let instruction = bridge::credit_deposits_ix(
        program_id,
        height,
        &block_hash.to_byte_array(),
        part.first,
        part.count,
        &part.targets,
    );
    let blockhash = rpc
        .get_latest_blockhash()
        .await
        .map_err(|source| CreditError::Rpc {
            call: "getLatestBlockhash",
            source,
        })?;
    let transaction = Transaction::new_signed_with_payer(
        &[instruction],
        Some(&payer.pubkey()),
        &[payer],
        blockhash,
    );
    match rpc.send_and_confirm_transaction(&transaction).await {
        Ok(_) => {
            tracing::info!(
                height,
                %block_hash,
                first = part.first,
                count = part.count,
                total = part.total,
                "the loop credited deposits"
            );
            Ok(true)
        }
        Err(error) => {
            // Another sender can credit the same height first, and a fork
            // switch can drop the tx. The next round reads the config again.
            tracing::warn!(height, %error, "the credit did not land, and the loop goes on");
            Ok(false)
        }
    }
}

async fn read_config(rpc: &RpcClient, program_id: &Pubkey) -> Result<Config, CreditError> {
    let data = rpc
        .get_account_data(&bridge::config_pda(program_id).0)
        .await
        .map_err(|source| CreditError::Rpc {
            call: "getAccountInfo(config)",
            source,
        })?;
    Ok(Config::decode(&data)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a_deposit(sequence_number: u64, address: &[u8]) -> DepositEvent {
        DepositEvent {
            sequence_number,
            address: address.to_vec(),
            value_sats: 100_000,
        }
    }

    #[test]
    fn a_credit_waits_for_d_confirmations_and_one_more_block() {
        assert!(!credit_is_ready(100, 105, 6));
        assert!(credit_is_ready(100, 106, 6));
        assert!(credit_is_ready(100, 101, 1));
        assert!(!credit_is_ready(100, 100, 1));
    }

    #[test]
    fn a_part_starts_at_the_credit_index_and_holds_at_most_the_limit() {
        let key = Pubkey::new_from_array([7u8; 32]);
        let deposits: Vec<DepositEvent> = (0..20)
            .map(|index| a_deposit(index, key.to_string().as_bytes()))
            .collect();
        let first = next_part(50, 0, &deposits);
        assert_eq!((first.first, first.count, first.total), (0, 16, 20));
        assert_eq!(first.targets.len(), 16);
        let last = next_part(50, 16, &deposits);
        assert_eq!((last.first, last.count), (16, 4));
    }

    #[test]
    fn a_deposit_without_a_pubkey_adds_no_target() {
        let key = Pubkey::new_from_array([7u8; 32]);
        let deposits = [
            a_deposit(0, b"s8_abc_000000"),
            a_deposit(1, key.to_string().as_bytes()),
            a_deposit(2, &[0xff]),
        ];
        let part = next_part(50, 0, &deposits);
        assert_eq!(part.count, 3);
        assert_eq!(part.targets, vec![key]);
    }

    #[test]
    fn an_empty_block_gives_an_empty_part() {
        let part = next_part(50, 0, &[]);
        assert_eq!((part.first, part.count, part.total), (0, 0, 0));
        assert!(part.targets.is_empty());
    }
}
