//! The account data that one tx loads. Agave refuses a tx whose accounts
//! hold more than `MAX_LOADED_DATA` bytes, so the daemon sizes each credit
//! part and each close batch to stay below `LOADED_DATA_BUDGET`.

use std::collections::{HashMap, HashSet};

use solana_commitment_config::CommitmentConfig;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_rpc_client_api::{
    client_error::{Error as ClientError, ErrorKind},
    config::{RpcAccountInfoConfig, UiAccountEncoding, UiDataSliceConfig},
};
use solana_sdk::{instruction::Instruction, pubkey::Pubkey};

/// The most account data that Agave loads for one tx,
/// `MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES`. It is also the default limit.
pub const MAX_LOADED_DATA: u64 = 64 * 1024 * 1024;

/// The bytes that Agave adds for each loaded account on top of its data.
pub const ACCOUNT_BASE_LEN: u64 = 64;

/// The load that one tx of the daemon may hold. The margin covers accounts
/// that grow between the read and the tx.
pub const LOADED_DATA_BUDGET: u64 = MAX_LOADED_DATA - 8 * 1024 * 1024;

/// The keys that one instruction loads: its program and its accounts.
pub fn instruction_keys(instruction: &Instruction) -> Vec<Pubkey> {
    std::iter::once(instruction.program_id)
        .chain(instruction.accounts.iter().map(|meta| meta.pubkey))
        .collect()
}

/// The keys that a tx of `instructions` with fee payer `payer` loads.
pub fn tx_keys(payer: &Pubkey, instructions: &[Instruction]) -> Vec<Pubkey> {
    std::iter::once(*payer)
        .chain(instructions.iter().flat_map(instruction_keys))
        .collect()
}

/// The count of leading `items`, at most `most`, that one tx takes. The tx
/// loads each key of `fixed` and of the items it takes one time, and the
/// load stays at or below `budget`. The first item always counts, so a tx
/// of one item goes out.
pub fn fitting(
    fixed: &[Pubkey],
    items: &[Vec<Pubkey>],
    lens: &HashMap<Pubkey, u64>,
    budget: u64,
    most: usize,
) -> usize {
    let len = |key: &Pubkey| lens.get(key).copied().unwrap_or(0);
    let mut loaded: HashSet<Pubkey> = HashSet::new();
    let mut total: u64 = 0;
    for key in fixed {
        if loaded.insert(*key) {
            total = total.saturating_add(len(key));
        }
    }
    let mut taken = 0;
    for item in items.iter().take(most) {
        let new: HashSet<Pubkey> = item
            .iter()
            .filter(|key| !loaded.contains(*key))
            .copied()
            .collect();
        let added = new.iter().map(len).fold(0u64, u64::saturating_add);
        if taken > 0 && total.saturating_add(added) > budget {
            break;
        }
        total = total.saturating_add(added);
        loaded.extend(new);
        taken += 1;
    }
    taken
}

/// Reads the loaded length of each key: the data length and the base
/// length, or 0 for a missing account. It reads no account data.
pub async fn loaded_lens(
    rpc: &RpcClient,
    keys: &[Pubkey],
    commitment: CommitmentConfig,
) -> Result<HashMap<Pubkey, u64>, ClientError> {
    let mut unique: Vec<Pubkey> = Vec::with_capacity(keys.len());
    let mut seen: HashSet<Pubkey> = HashSet::new();
    for key in keys {
        if seen.insert(*key) {
            unique.push(*key);
        }
    }
    let config = RpcAccountInfoConfig {
        encoding: Some(UiAccountEncoding::Base64),
        data_slice: Some(UiDataSliceConfig {
            offset: 0,
            length: 0,
        }),
        commitment: Some(commitment),
        min_context_slot: None,
    };
    let mut lens = HashMap::with_capacity(unique.len());
    for chunk in unique.chunks(100) {
        let accounts = rpc
            .get_multiple_ui_accounts_with_config(chunk, config.clone())
            .await?
            .value;
        for (key, account) in chunk.iter().zip(accounts) {
            let len = match account {
                None => 0,
                Some(account) => {
                    let space = account.space.ok_or_else(|| {
                        ClientError::from(ErrorKind::Custom(format!(
                            "the rpc gave no data length for account {key}"
                        )))
                    })?;
                    ACCOUNT_BASE_LEN.saturating_add(space)
                }
            };
            lens.insert(*key, len);
        }
    }
    Ok(lens)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a_key(byte: u8) -> Pubkey {
        Pubkey::new_from_array([byte; 32])
    }

    #[test]
    fn a_key_that_comes_again_loads_one_time() {
        let lens = HashMap::from([(a_key(1), 30), (a_key(2), 30)]);
        let items = vec![vec![a_key(2)], vec![a_key(2), a_key(2)], vec![a_key(1)]];
        assert_eq!(fitting(&[a_key(1)], &items, &lens, 60, 10), 3);
        assert_eq!(fitting(&[], &items, &lens, 59, 10), 2);
    }

    #[test]
    fn the_first_item_counts_above_the_budget() {
        let lens = HashMap::from([(a_key(1), 100), (a_key(2), 1)]);
        let items = vec![vec![a_key(1)], vec![a_key(2)]];
        assert_eq!(fitting(&[], &items, &lens, 50, 10), 1);
        assert_eq!(fitting(&[], &[], &lens, 50, 10), 0);
    }

    #[test]
    fn an_item_without_keys_adds_nothing() {
        let lens = HashMap::from([(a_key(1), 50)]);
        let items = vec![vec![a_key(1)], Vec::new(), Vec::new()];
        assert_eq!(fitting(&[], &items, &lens, 50, 10), 3);
    }

    #[test]
    fn the_count_stays_at_most_the_limit() {
        let items = vec![Vec::new(); 20];
        assert_eq!(fitting(&[], &items, &HashMap::new(), 50, 16), 16);
    }

    #[test]
    fn a_tx_loads_the_payer_each_program_and_each_account() {
        let ix = Instruction {
            program_id: a_key(5),
            accounts: vec![solana_sdk::instruction::AccountMeta::new(a_key(6), false)],
            data: Vec::new(),
        };
        assert_eq!(tx_keys(&a_key(1), &[ix]), [a_key(1), a_key(5), a_key(6)]);
    }
}
