use std::time::Duration;

use bitcoin::{Amount, ScriptBuf, Transaction as BitcoinTransaction};
use futures::StreamExt as _;
use solana_commitment_config::CommitmentConfig;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    pubkey::Pubkey,
    signature::{Keypair, Signer as _},
    transaction::Transaction as SolanaTransaction,
};

use crate::{
    address, bridge,
    enforcer::{BundleEvent, Enforcer, PegEvent},
    m6::{self, Payout},
    pending::PendingDeposits,
    proto::mainchain::WithdrawalBundlePolicy,
};

pub struct Settings {
    pub network: crate::network::PegNetwork,
    pub enforcer_url: String,
    pub solana_rpc_url: String,
    pub sidechain_id: u8,
    pub program_id: Pubkey,
    pub confirmations: u32,
    pub bundle_interval: Duration,
}

#[derive(Debug, thiserror::Error)]
pub enum PegError {
    #[error(transparent)]
    Enforcer(#[from] crate::enforcer::EnforcerError),
    #[error(transparent)]
    Bridge(#[from] crate::bridge::BridgeError),
    #[error(transparent)]
    M6(#[from] crate::m6::M6Error),
    #[error("the Solana rpc call `{call}` failed")]
    Rpc {
        call: &'static str,
        #[source]
        source: solana_rpc_client_api::client_error::Error,
    },
    #[error("the enforcer event stream stopped")]
    StreamClosed,
    #[error("the enforcer stream failed")]
    Stream(#[source] tonic::Status),
}

/// Reads the open withdrawal records, lowest index first.
async fn read_config(rpc: &RpcClient, program_id: &Pubkey) -> Result<bridge::Config, PegError> {
    let (config_key, _) = bridge::config_pda(program_id);
    let config_data = rpc
        .get_account_data(&config_key)
        .await
        .map_err(|source| PegError::Rpc {
            call: "getAccountInfo(config)",
            source,
        })?;
    Ok(bridge::Config::decode(&config_data)?)
}

pub async fn open_records(
    rpc: &RpcClient,
    program_id: &Pubkey,
) -> Result<Vec<bridge::WithdrawalRecord>, PegError> {
    let config = read_config(rpc, program_id).await?;

    let keys: Vec<Pubkey> = (0..config.withdrawal_count)
        .map(|index| bridge::withdrawal_pda(program_id, index).0)
        .collect();

    let mut records = Vec::new();
    for chunk in keys.chunks(100) {
        let accounts = rpc
            .get_multiple_accounts(chunk)
            .await
            .map_err(|source| PegError::Rpc {
                call: "getMultipleAccounts",
                source,
            })?;
        for account in accounts.into_iter().flatten() {
            records.push(bridge::WithdrawalRecord::decode(&account.data)?);
        }
    }
    Ok(records)
}

/// Builds one blinded M6 out of every open record.
pub fn bundle_of(
    records: &[bridge::WithdrawalRecord],
) -> Result<BitcoinTransaction, crate::m6::M6Error> {
    let mut fee_sats: u64 = 0;
    let mut payouts = Vec::with_capacity(records.len());
    for record in records {
        fee_sats = fee_sats.saturating_add(record.fee_sats);
        payouts.push(Payout {
            script_pubkey: ScriptBuf::from_bytes(record.script_pubkey.clone()),
            sats: record.payout_sats,
        });
    }
    m6::build_blinded_m6(fee_sats, &payouts)
}

/// Finds the records that a paid M6 covers.
///
/// The real M6 replaces output 0 with the treasury output, so the payouts start
/// at index 1. Each output matches at most one record.
pub fn records_paid_by(
    records: &[bridge::WithdrawalRecord],
    paid: &BitcoinTransaction,
) -> Vec<u64> {
    let mut left: Vec<&bridge::WithdrawalRecord> = records.iter().collect();
    let mut indices = Vec::new();
    for output in paid.output.iter().skip(1) {
        let found = left.iter().position(|record| {
            output.value == Amount::from_sat(record.payout_sats)
                && output.script_pubkey.as_bytes() == record.script_pubkey
        });
        if let Some(at) = found {
            indices.push(left.remove(at).index);
        }
    }
    indices.sort_unstable();
    indices
}

pub async fn run(settings: Settings, oracle: Keypair) -> Result<(), PegError> {
    let rpc = RpcClient::new_with_commitment(
        settings.solana_rpc_url.clone(),
        CommitmentConfig::confirmed(),
    );
    let mut enforcer = Enforcer::connect(
        settings.enforcer_url.clone(),
        u32::from(settings.sidechain_id),
    )
    .await?;
    enforcer.check_network(settings.network).await?;
    enforcer
        .set_withdrawal_bundle_policy(WithdrawalBundlePolicy::Known)
        .await?;

    let bundle_enforcer = enforcer.clone();
    let bundle_rpc = RpcClient::new_with_commitment(
        settings.solana_rpc_url.clone(),
        CommitmentConfig::confirmed(),
    );
    let program_id = settings.program_id;
    let interval = settings.bundle_interval;
    let bundles = tokio::spawn(async move {
        bundle_loop(bundle_enforcer, bundle_rpc, program_id, interval).await
    });

    let result = deposit_loop(&mut enforcer, &rpc, &settings, &oracle).await;
    bundles.abort();
    result
}

async fn deposit_loop(
    enforcer: &mut Enforcer,
    rpc: &RpcClient,
    settings: &Settings,
    oracle: &Keypair,
) -> Result<(), PegError> {
    let mut pending = PendingDeposits::new(settings.confirmations);
    // Subscribe before the backfill, so no block can fall between the two. A
    // block that both carry goes into the queue twice, and the high-water
    // check in `credit_deposit` skips the second credit.
    let mut stream = enforcer.subscribe_events().await?;

    // The stream starts at the next block, and the queue lives in memory. A
    // restart would lose every deposit that still waits, and every block that
    // connected while the daemon was down.
    let tip = enforcer.chain_tip().await?;
    let history = enforcer.two_way_peg_data(tip).await?;
    tracing::info!(blocks = history.len(), %tip, "the daemon replays the peg history");
    for event in history {
        if let PegEvent::Connect {
            height, deposits, ..
        } = event
        {
            for deposit in pending.connect(height, deposits) {
                credit_deposit(rpc, settings, oracle, &deposit).await?;
            }
        }
    }

    while let Some(item) = stream.next().await {
        let response = item.map_err(PegError::Stream)?;
        match crate::enforcer::peg_event(response)? {
            PegEvent::Disconnect { block_hash } => {
                tracing::warn!(%block_hash, "the mainchain disconnected a block");
                pending.disconnect();
            }
            PegEvent::Connect {
                height,
                deposits,
                bundles,
                ..
            } => {
                let ready = pending.connect(height, deposits);
                tracing::trace!(
                    height,
                    waiting = pending.waiting(),
                    "the daemon read a block"
                );
                for deposit in ready {
                    credit_deposit(rpc, settings, oracle, &deposit).await?;
                }
                for bundle in bundles {
                    close_paid_records(rpc, settings, oracle, bundle).await?;
                }
            }
        }
    }
    Err(PegError::StreamClosed)
}

async fn credit_deposit(
    rpc: &RpcClient,
    settings: &Settings,
    oracle: &Keypair,
    deposit: &crate::enforcer::DepositEvent,
) -> Result<(), PegError> {
    let text = match std::str::from_utf8(&deposit.address) {
        Ok(text) => text,
        Err(error) => {
            tracing::warn!(
                sequence_number = deposit.sequence_number,
                %error,
                "the deposit address is not utf8, so the daemon skips it"
            );
            return Ok(());
        }
    };
    let recipient = match address::parse_deposit_address(settings.sidechain_id, text) {
        Ok(recipient) => recipient,
        Err(error) => {
            tracing::warn!(
                sequence_number = deposit.sequence_number,
                address = text,
                %error,
                "the deposit address does not parse, so the daemon skips it"
            );
            return Ok(());
        }
    };

    // A replay after a restart sees deposits the bridge credited long ago. The
    // bridge would refuse them, and the refusal would stop the daemon.
    let high_water = read_config(rpc, &settings.program_id)
        .await?
        .deposit_high_water;
    if already_credited(deposit.sequence_number, high_water) {
        tracing::debug!(
            sequence_number = deposit.sequence_number,
            high_water,
            "the bridge already credited this deposit"
        );
        return Ok(());
    }

    let instruction = bridge::deposit_ix(
        &settings.program_id,
        &oracle.pubkey(),
        &recipient,
        deposit.sequence_number,
        deposit.value_sats,
    );
    send(rpc, oracle, instruction, "deposit").await?;
    tracing::info!(
        sequence_number = deposit.sequence_number,
        value_sats = deposit.value_sats,
        %recipient,
        "the daemon credited a deposit"
    );
    Ok(())
}

/// True when the bridge already applied this mainchain sequence number.
///
/// The bridge holds the lowest number a deposit may still use, and it moves
/// only forward.
fn already_credited(sequence_number: u64, high_water: u64) -> bool {
    sequence_number < high_water
}

async fn close_paid_records(
    rpc: &RpcClient,
    settings: &Settings,
    oracle: &Keypair,
    bundle: BundleEvent,
) -> Result<(), PegError> {
    let (m6id, paid) = match bundle {
        BundleEvent::Submitted(m6id) => {
            tracing::info!(%m6id, "the mainchain carries the bundle proposal");
            return Ok(());
        }
        BundleEvent::Failed(m6id) => {
            tracing::warn!(%m6id, "the bundle expired, so the records stay open");
            return Ok(());
        }
        BundleEvent::Succeeded { m6id, paid } => (m6id, paid),
    };

    let records = open_records(rpc, &settings.program_id).await?;
    for index in records_paid_by(&records, &paid) {
        let Some(record) = records.iter().find(|record| record.index == index) else {
            continue;
        };
        let instruction =
            bridge::mark_paid_ix(&settings.program_id, &oracle.pubkey(), &record.owner, index);
        send(rpc, oracle, instruction, "mark_paid").await?;
        tracing::info!(index, %m6id, "the mainchain paid a withdrawal");
    }
    Ok(())
}

async fn bundle_loop(
    mut enforcer: Enforcer,
    rpc: RpcClient,
    program_id: Pubkey,
    interval: Duration,
) -> Result<(), PegError> {
    let mut ticker = tokio::time::interval(interval);
    loop {
        ticker.tick().await;
        let records = open_records(&rpc, &program_id).await?;
        if records.is_empty() {
            continue;
        }
        let blinded = bundle_of(&records)?;
        enforcer.propose_withdrawal_bundle(&blinded).await?;
        tracing::info!(
            m6id = %m6::m6id(&blinded),
            records = records.len(),
            "the daemon proposed a withdrawal bundle"
        );
    }
}

async fn send(
    rpc: &RpcClient,
    oracle: &Keypair,
    instruction: solana_sdk::instruction::Instruction,
    call: &'static str,
) -> Result<(), PegError> {
    let blockhash = rpc
        .get_latest_blockhash()
        .await
        .map_err(|source| PegError::Rpc {
            call: "getLatestBlockhash",
            source,
        })?;
    let transaction = SolanaTransaction::new_signed_with_payer(
        &[instruction],
        Some(&oracle.pubkey()),
        &[oracle],
        blockhash,
    );
    rpc.send_and_confirm_transaction(&transaction)
        .await
        .map_err(|source| PegError::Rpc { call, source })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_number_below_the_high_water_is_already_credited() {
        assert!(already_credited(4, 5));
        assert!(already_credited(0, 1));
    }

    #[test]
    fn the_high_water_itself_is_still_open() {
        // The bridge stores the lowest number a deposit may still use.
        assert!(!already_credited(5, 5));
    }

    #[test]
    fn a_number_above_the_high_water_is_open() {
        // A paid withdrawal also moves the mainchain counter, so gaps occur.
        assert!(!already_credited(9, 5));
    }

    #[test]
    fn nothing_is_credited_on_a_fresh_bridge() {
        assert!(!already_credited(0, 0));
    }
    use bitcoin::{absolute::LockTime, transaction::Version, TxOut};

    fn a_record(index: u64, payout_sats: u64, script: u8) -> bridge::WithdrawalRecord {
        bridge::WithdrawalRecord {
            index,
            owner: Pubkey::new_from_array([index as u8; 32]),
            burned_lamports: (payout_sats + 100) * 10,
            payout_sats,
            fee_sats: 100,
            script_pubkey: vec![0x00, 0x14, script],
            bump: 255,
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

    fn a_payout_output(sats: u64, script: u8) -> TxOut {
        TxOut {
            value: Amount::from_sat(sats),
            script_pubkey: ScriptBuf::from_bytes(vec![0x00, 0x14, script]),
        }
    }

    #[test]
    fn a_bundle_adds_every_fee() {
        let records = [a_record(0, 50_000, 1), a_record(1, 60_000, 2)];
        let bundle = bundle_of(&records).unwrap();
        let fee: [u8; 8] = bundle.output[0].script_pubkey.as_bytes()[2..]
            .try_into()
            .unwrap();
        assert_eq!(u64::from_be_bytes(fee), 200);
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
    fn an_empty_record_set_gives_no_bundle() {
        assert!(bundle_of(&[]).is_err());
    }

    #[test]
    fn a_paid_bundle_names_each_record() {
        let records = [a_record(0, 50_000, 1), a_record(1, 60_000, 2)];
        let paid = a_paid_m6(vec![a_payout_output(50_000, 1), a_payout_output(60_000, 2)]);
        assert_eq!(records_paid_by(&records, &paid), vec![0, 1]);
    }

    #[test]
    fn the_treasury_output_matches_no_record() {
        let records = [a_record(0, 1_000_000, 0xb5)];
        let paid = a_paid_m6(Vec::new());
        assert!(records_paid_by(&records, &paid).is_empty());
    }

    #[test]
    fn a_partial_bundle_names_only_the_records_it_pays() {
        let records = [a_record(0, 50_000, 1), a_record(1, 60_000, 2)];
        let paid = a_paid_m6(vec![a_payout_output(60_000, 2)]);
        assert_eq!(records_paid_by(&records, &paid), vec![1]);
    }

    #[test]
    fn two_equal_records_each_match_one_output() {
        let records = [a_record(0, 50_000, 1), a_record(1, 50_000, 1)];
        let paid = a_paid_m6(vec![a_payout_output(50_000, 1), a_payout_output(50_000, 1)]);
        assert_eq!(records_paid_by(&records, &paid), vec![0, 1]);
    }

    #[test]
    fn one_output_does_not_close_two_equal_records() {
        let records = [a_record(0, 50_000, 1), a_record(1, 50_000, 1)];
        let paid = a_paid_m6(vec![a_payout_output(50_000, 1)]);
        assert_eq!(records_paid_by(&records, &paid).len(), 1);
    }

    #[test]
    fn a_wrong_value_matches_no_record() {
        let records = [a_record(0, 50_000, 1)];
        let paid = a_paid_m6(vec![a_payout_output(49_999, 1)]);
        assert!(records_paid_by(&records, &paid).is_empty());
    }

    #[test]
    fn a_wrong_script_matches_no_record() {
        let records = [a_record(0, 50_000, 1)];
        let paid = a_paid_m6(vec![a_payout_output(50_000, 9)]);
        assert!(records_paid_by(&records, &paid).is_empty());
    }
}
