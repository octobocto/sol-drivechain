//! The credit loop. It sends `credit_deposits` for the next eCash height that
//! the bridge config of the followed branch has not credited. Anyone can run
//! it, and the peg runs it for liveness.

use std::{collections::HashMap, time::Duration};

use bitcoin::hashes::Hash as _;
use solana_commitment_config::CommitmentConfig;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    instruction::Instruction,
    pubkey::Pubkey,
    signature::{Keypair, Signer as _},
    transaction::Transaction,
};

use crate::{
    address,
    bridge::{self, Config},
    enforcer::{DepositEvent, Enforcer, EnforcerError},
    loaded,
};

/// The most deposits in one credit. Each deposit with a target adds one
/// account to the tx, so a part with large targets holds fewer.
pub const MAX_PART: usize = 16;

/// The failed sends of the same part in a row that stop the loop. A leader
/// can drop a credit for some rounds, so one failure is no stop.
pub const MAX_PART_FAILURES: u32 = 100;

/// The compute limit of one credit. A full part takes far less, so a height
/// never stops at the default limit of 200,000 units.
pub const CREDIT_COMPUTE_UNITS: u32 = 1_400_000;

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
    #[error(
        "the credit of eCash height {height} from deposit {first}, {count} deposits, failed \
         {failures} times in a row"
    )]
    PartFails {
        height: u64,
        first: u32,
        count: u8,
        failures: u32,
        #[source]
        source: solana_rpc_client_api::client_error::Error,
    },
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
/// config, at most `most` deposits. A deposit whose OP_RETURN names no
/// pubkey adds no target.
pub fn next_part(
    height: u64,
    credit_index: u64,
    deposits: &[DepositEvent],
    slot: u8,
    most: usize,
) -> Part {
    let first = usize::try_from(credit_index)
        .unwrap_or(usize::MAX)
        .min(deposits.len());
    let part = &deposits[first..deposits.len().min(first.saturating_add(most.min(MAX_PART)))];
    Part {
        height,
        first: u32::try_from(first).unwrap_or(u32::MAX),
        count: u8::try_from(part.len()).unwrap_or(u8::MAX),
        total: deposits.len(),
        targets: part
            .iter()
            .filter_map(|deposit| target(deposit, slot))
            .collect(),
    }
}

/// The count of deposits of `part`, from its first deposit, that one credit
/// takes. The tx loads `fixed`, and each target from `lens`, and the load
/// stays below the loaded data budget. It is at least 1 when the part holds
/// a deposit, because one target of at most 10 MiB always fits.
pub fn part_count(
    part: &[DepositEvent],
    slot: u8,
    fixed: &[Pubkey],
    lens: &HashMap<Pubkey, u64>,
) -> usize {
    let items: Vec<Vec<Pubkey>> = part
        .iter()
        .map(|deposit| target(deposit, slot).into_iter().collect())
        .collect();
    loaded::fitting(fixed, &items, lens, loaded::LOADED_DATA_BUDGET, MAX_PART)
}

/// The instructions of the credit of `part` of eCash block `block_hash`.
pub fn credit_instructions(
    program_id: &Pubkey,
    block_hash: &[u8; 32],
    part: &Part,
) -> [Instruction; 2] {
    [
        bridge::compute_unit_limit_ix(CREDIT_COMPUTE_UNITS),
        bridge::credit_deposits_ix(
            program_id,
            part.height,
            block_hash,
            part.first,
            part.count,
            &part.targets,
        ),
    ]
}

/// The failed sends of one part in a row.
#[derive(Debug, Default)]
pub struct Failures {
    part: Option<([u8; 32], Part)>,
    count: u32,
}

impl Failures {
    /// Counts a failed send of `part` of eCash block `block_hash`, and gives
    /// the failures of that part in a row.
    pub fn add(&mut self, block_hash: [u8; 32], part: &Part) -> u32 {
        let key = (block_hash, part.clone());
        if self.part.as_ref() == Some(&key) {
            self.count = self.count.saturating_add(1);
        } else {
            self.part = Some(key);
            self.count = 1;
        }
        self.count
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }
}

/// The pubkey that a deposit OP_RETURN names. The validator reads it the same
/// way.
pub fn target(deposit: &DepositEvent, slot: u8) -> Option<Pubkey> {
    let text = std::str::from_utf8(&deposit.address).ok()?;
    address::deposit_target(text, slot)
}

/// True when the bridge config can take a part of the active eCash block
/// `block_hash` with parent `prev_hash`: the config credited the parent, and
/// its open parts are of the same block.
pub fn can_credit(config: &Config, block_hash: &[u8; 32], prev_hash: &[u8; 32]) -> bool {
    (config.credited_block == [0u8; 32] || config.credited_block == *prev_hash)
        && (config.credit_index == 0 || config.credit_part_block == *block_hash)
}

/// True when a leader takes a credit of `height` at eCash tip `tip_height`:
/// the tip is at least D + 1 blocks above the block.
pub fn credit_is_ready(height: u64, tip_height: u64, deposit_confirmations: u64) -> bool {
    height
        .checked_add(deposit_confirmations)
        .and_then(|needed| needed.checked_add(1))
        .is_some_and(|needed| tip_height >= needed)
}

pub async fn run(
    enforcer: &mut Enforcer,
    solana_rpc_url: String,
    program_id: Pubkey,
    payer: &Keypair,
    slot: u8,
) -> Result<(), CreditError> {
    // The followed branch can be one that the stake does not confirm yet.
    let rpc = RpcClient::new_with_commitment(solana_rpc_url, CommitmentConfig::processed());
    tracing::info!(payer = %payer.pubkey(), "the credit loop starts");
    let mut failures = Failures::default();
    loop {
        if !credit_next(enforcer, &rpc, &program_id, payer, slot, &mut failures).await? {
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
    slot: u8,
    failures: &mut Failures,
) -> Result<bool, CreditError> {
    let config = read_config(rpc, program_id).await?;
    let height = config.credited_height.saturating_add(1);
    let tip = enforcer.tip().await?;
    if !credit_is_ready(height, u64::from(tip.1), config.deposit_confirmations) {
        return Ok(false);
    }
    let at = u32::try_from(height).map_err(|_| CreditError::HeightTooLarge(height))?;
    let (block_hash, prev_hash) = enforcer.block_at(tip, at).await?;
    if !can_credit(
        &config,
        &block_hash.to_byte_array(),
        &prev_hash.to_byte_array(),
    ) {
        tracing::warn!(
            height,
            "the followed branch credited an eCash block that the active chain dropped, so \
             nobody can credit on it"
        );
        return Ok(false);
    }
    let deposits = enforcer.block_deposits(block_hash, prev_hash).await?;
    let block = block_hash.to_byte_array();
    let most = next_part(height, config.credit_index, &deposits, slot, MAX_PART);
    let fixed = loaded::tx_keys(
        &payer.pubkey(),
        &credit_instructions(
            program_id,
            &block,
            &Part {
                targets: Vec::new(),
                ..most.clone()
            },
        ),
    );
    let lens = loaded::loaded_lens(
        rpc,
        &[fixed.as_slice(), &most.targets].concat(),
        CommitmentConfig::processed(),
    )
    .await
    .map_err(|source| CreditError::Rpc {
        call: "getMultipleAccounts(credit accounts)",
        source,
    })?;
    let start = most.first as usize;
    let count = part_count(
        deposits
            .get(start..start.saturating_add(usize::from(most.count)))
            .unwrap_or_default(),
        slot,
        &fixed,
        &lens,
    );
    let part = next_part(height, config.credit_index, &deposits, slot, count);
    let blockhash = rpc
        .get_latest_blockhash()
        .await
        .map_err(|source| CreditError::Rpc {
            call: "getLatestBlockhash",
            source,
        })?;
    let transaction = Transaction::new_signed_with_payer(
        &credit_instructions(program_id, &block, &part),
        Some(&payer.pubkey()),
        &[payer],
        blockhash,
    );
    match rpc.send_and_confirm_transaction(&transaction).await {
        Ok(_) => {
            failures.clear();
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
        Err(source) => {
            // Another sender can credit the same height first, and a fork
            // switch can drop the tx. The next round reads the config again.
            let count = failures.add(block, &part);
            if count >= MAX_PART_FAILURES {
                return Err(CreditError::PartFails {
                    height,
                    first: part.first,
                    count: part.count,
                    failures: count,
                    source,
                });
            }
            tracing::warn!(
                height,
                first = part.first,
                count = part.count,
                failures = count,
                error = %source,
                "the credit did not land, and the next round tries again"
            );
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

    fn a_config(credit_index: u64, credit_part_block: [u8; 32]) -> Config {
        Config {
            oracle: Pubkey::new_from_array([7u8; 32]),
            deposit_high_water: 0,
            pegged_lamports: 0,
            withdrawal_count: 0,
            bmm_next_height: 0,
            bmm_paid_total: 0,
            deposit_confirmations: 6,
            deposit_lag: 100,
            credited_height: 10,
            credited_block: [1u8; 32],
            credit_index,
            credit_part_block,
            stranded_lamports: 0,
            bump: 0,
            vault_bump: 0,
            treasury_bump: 0,
            vault_supply: 0,
            float_lamports: 0,
            paid_through: 0,
            bundled_through: 0,
            bundle_sealed_height: 0,
            bmm_skip_blocks: 0,
            bmm_next_since_slot: 0,
            bundle_m6id: [0u8; 32],
        }
    }

    #[test]
    fn open_parts_of_a_dropped_block_stop_the_credits() {
        assert!(can_credit(&a_config(0, [0u8; 32]), &[2u8; 32], &[1u8; 32]));
        assert!(!can_credit(&a_config(0, [0u8; 32]), &[2u8; 32], &[9u8; 32]));
        assert!(can_credit(&a_config(3, [2u8; 32]), &[2u8; 32], &[1u8; 32]));
        assert!(!can_credit(&a_config(3, [2u8; 32]), &[8u8; 32], &[1u8; 32]));
    }

    #[test]
    fn a_credit_waits_for_d_plus_one_blocks_on_top() {
        assert!(!credit_is_ready(100, 106, 6));
        assert!(credit_is_ready(100, 107, 6));
        assert!(credit_is_ready(100, 102, 1));
        assert!(!credit_is_ready(100, 101, 1));
        assert!(!credit_is_ready(u64::MAX - 6, u64::MAX, 6));
    }

    #[test]
    fn a_part_starts_at_the_credit_index_and_holds_at_most_the_limit() {
        let key = Pubkey::new_from_array([7u8; 32]);
        let deposits: Vec<DepositEvent> = (0..20)
            .map(|index| a_deposit(index, key.to_string().as_bytes()))
            .collect();
        let first = next_part(50, 0, &deposits, 8, MAX_PART);
        assert_eq!((first.first, first.count, first.total), (0, 16, 20));
        assert_eq!(first.targets.len(), 16);
        let last = next_part(50, 16, &deposits, 8, MAX_PART);
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
        let part = next_part(50, 0, &deposits, 8, MAX_PART);
        assert_eq!(part.count, 3);
        assert_eq!(part.targets, vec![key]);
    }

    #[test]
    fn a_full_address_of_this_slot_adds_its_target() {
        let key = Pubkey::new_from_array([7u8; 32]);
        let full = address::format_deposit_address(8, &key);
        let deposits = [a_deposit(0, full.as_bytes())];
        assert_eq!(next_part(50, 0, &deposits, 8, MAX_PART).targets, vec![key]);
        assert!(next_part(50, 0, &deposits, 9, MAX_PART).targets.is_empty());
    }

    const MIB: u64 = 1024 * 1024;

    /// The keys and loaded lengths of a credit tx with no target: the payer,
    /// the programs, and the bridge accounts, with a bridge program of 1 MiB.
    fn fixed_accounts() -> (Vec<Pubkey>, HashMap<Pubkey, u64>) {
        let program_id = Pubkey::new_from_array([3u8; 32]);
        let payer = Pubkey::new_from_array([4u8; 32]);
        let empty = next_part(50, 0, &[], 8, MAX_PART);
        let fixed = loaded::tx_keys(
            &payer,
            &credit_instructions(&program_id, &[2u8; 32], &empty),
        );
        let mut lens: HashMap<Pubkey, u64> = fixed
            .iter()
            .map(|key| (*key, loaded::ACCOUNT_BASE_LEN + 1_000))
            .collect();
        lens.insert(program_id, loaded::ACCOUNT_BASE_LEN + MIB);
        (fixed, lens)
    }

    /// Deposits to `sizes.len()` targets, and the loaded length of each.
    fn deposits_to(sizes: &[u64], lens: &mut HashMap<Pubkey, u64>) -> Vec<DepositEvent> {
        sizes
            .iter()
            .enumerate()
            .map(|(index, size)| {
                let key = Pubkey::new_from_array([100 + index as u8; 32]);
                lens.insert(key, loaded::ACCOUNT_BASE_LEN + size);
                a_deposit(index as u64, key.to_string().as_bytes())
            })
            .collect()
    }

    fn loaded_len(fixed: &[Pubkey], part: &Part, lens: &HashMap<Pubkey, u64>) -> u64 {
        let mut keys = fixed.to_vec();
        keys.extend(&part.targets);
        keys.sort();
        keys.dedup();
        keys.iter()
            .map(|key| lens.get(key).copied().unwrap_or(0))
            .sum()
    }

    #[test]
    fn a_part_stays_under_the_loaded_data_limit() {
        let (fixed, mut lens) = fixed_accounts();
        let deposits = deposits_to(&[10 * MIB; 7], &mut lens);
        let mut credit_index = 0;
        let mut counts = Vec::new();
        while credit_index < deposits.len() {
            let rest = &deposits[credit_index..];
            let count = part_count(rest, 8, &fixed, &lens);
            let part = next_part(50, credit_index as u64, &deposits, 8, count);
            assert!(loaded_len(&fixed, &part, &lens) <= loaded::LOADED_DATA_BUDGET);
            counts.push(part.count);
            credit_index += usize::from(part.count);
        }
        assert_eq!(counts, [5, 2]);
    }

    #[test]
    fn a_single_target_of_ten_mib_gets_a_part_of_one() {
        let (fixed, mut lens) = fixed_accounts();
        let deposits = deposits_to(&[10 * MIB], &mut lens);
        assert_eq!(part_count(&deposits, 8, &fixed, &lens), 1);
        let part = next_part(50, 0, &deposits, 8, 1);
        assert_eq!((part.first, part.count, part.targets.len()), (0, 1, 1));
        assert!(loaded_len(&fixed, &part, &lens) <= loaded::MAX_LOADED_DATA);
    }

    #[test]
    fn small_targets_fill_a_whole_part() {
        let (fixed, mut lens) = fixed_accounts();
        let deposits = deposits_to(&[0; 20], &mut lens);
        assert_eq!(part_count(&deposits, 8, &fixed, &lens), MAX_PART);
    }

    #[test]
    fn deposits_to_one_large_target_load_it_one_time() {
        let (fixed, mut lens) = fixed_accounts();
        let key = Pubkey::new_from_array([7u8; 32]);
        lens.insert(key, loaded::ACCOUNT_BASE_LEN + 10 * MIB);
        let deposits: Vec<DepositEvent> = (0..16)
            .map(|index| a_deposit(index, key.to_string().as_bytes()))
            .collect();
        assert_eq!(part_count(&deposits, 8, &fixed, &lens), MAX_PART);
    }

    #[test]
    fn a_deposit_without_a_target_loads_nothing() {
        let (fixed, mut lens) = fixed_accounts();
        let mut deposits = deposits_to(&[10 * MIB; 5], &mut lens);
        deposits.push(a_deposit(5, b"no pubkey"));
        deposits.extend(deposits_to(&[10 * MIB; 6], &mut lens).into_iter().skip(5));
        assert_eq!(part_count(&deposits, 8, &fixed, &lens), 6);
    }

    #[test]
    fn the_same_part_counts_its_failures_in_a_row() {
        let key = Pubkey::new_from_array([7u8; 32]);
        let deposits = [a_deposit(0, key.to_string().as_bytes())];
        let part = next_part(50, 0, &deposits, 8, MAX_PART);
        let mut failures = Failures::default();
        assert_eq!(failures.add([2u8; 32], &part), 1);
        assert_eq!(failures.add([2u8; 32], &part), 2);
        assert_eq!(failures.add([3u8; 32], &part), 1, "another block");
        let other = next_part(51, 0, &deposits, 8, MAX_PART);
        assert_eq!(failures.add([3u8; 32], &other), 1, "another height");
        failures.clear();
        assert_eq!(failures.add([3u8; 32], &other), 1);
    }

    #[test]
    fn an_empty_block_gives_an_empty_part() {
        let part = next_part(50, 0, &[], 8, MAX_PART);
        assert_eq!((part.first, part.count, part.total), (0, 0, 0));
        assert!(part.targets.is_empty());
    }
}
