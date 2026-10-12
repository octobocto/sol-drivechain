//! The primordial accounts of a new chain: the vault, the treasury, the
//! bridge config, the block record, and the oracle. A migration adds the
//! balances, the withdrawal records, and the deposit cursor of an old chain.

use std::collections::BTreeMap;

use base64::Engine as _;
use solana_sdk::pubkey::Pubkey;

use crate::{
    bridge::{self, Config},
    migrate::State,
};

/// What the genesis of a new chain sets.
#[derive(Clone, Copy, Debug)]
pub struct Params {
    pub program_id: Pubkey,
    /// The whole supply of the chain. The vault holds every lamport of it
    /// that no deposit credited.
    pub supply_lamports: u64,
    /// The key that marks a withdrawal paid.
    pub oracle: Pubkey,
    /// The lamports that the oracle gets to pay its tx fees.
    pub oracle_lamports: u64,
    pub lamports_per_byte_year: u64,
    /// The eCash tip when the chain starts. BMM settles from this height,
    /// and a new chain counts the deposits up to it as credited.
    pub ecash_start_height: u64,
    /// D, the eCash confirmations before a credit.
    pub deposit_confirmations: u64,
    /// A BMM checkpoint at eCash height H counts only when its bank credited
    /// every deposit through H minus this lag.
    pub deposit_lag: u64,
}

/// One account of the primordial accounts file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Account {
    pub pubkey: Pubkey,
    pub lamports: u64,
    pub owner: Pubkey,
    pub data: Vec<u8>,
}

#[derive(Debug, thiserror::Error)]
pub enum GenesisError {
    #[error("D must be at least one eCash confirmation")]
    NoConfirmations,
    #[error("the rent of a {0} byte account overflows a u64")]
    RentOverflow(usize),
    #[error("the old chain holds no vault account")]
    NoVault,
    #[error(
        "the old vault holds {vault} lamports and {pegged} are pegged, which is not the supply of \
         {supply}"
    )]
    VaultOpen {
        vault: u64,
        pegged: u64,
        supply: u64,
    },
    #[error("`{0}` in the state file is not a Solana pubkey")]
    BadPubkey(String),
    #[error("the withdrawal record `{0}` in the state file is not base64")]
    BadRecordData(String),
}

/// The accounts that the genesis writes, one per pubkey.
pub fn accounts(params: &Params, state: Option<&State>) -> Result<Vec<Account>, GenesisError> {
    if params.deposit_confirmations == 0 {
        return Err(GenesisError::NoConfirmations);
    }
    let rent = |len: usize| {
        bridge::rent_exempt_lamports(len, params.lamports_per_byte_year)
            .ok_or(GenesisError::RentOverflow(len))
    };
    let program_id = &params.program_id;
    let (config_key, config_bump) = bridge::config_pda(program_id);
    let (vault_key, vault_bump) = bridge::vault_pda(program_id);
    let (treasury_key, treasury_bump) = bridge::treasury_pda(program_id);

    let mut config = Config {
        oracle: params.oracle,
        deposit_high_water: 0,
        pegged_lamports: 0,
        withdrawal_count: 0,
        bmm_next_height: params.ecash_start_height,
        bmm_paid_total: 0,
        deposit_confirmations: params.deposit_confirmations,
        deposit_lag: params.deposit_lag,
        credited_height: params.ecash_start_height,
        credited_block: [0u8; 32],
        credit_index: 0,
        credit_part_block: [0u8; 32],
        stranded_lamports: 0,
        bump: config_bump,
        vault_bump,
        treasury_bump,
    };
    let mut accounts: BTreeMap<Pubkey, Account> = BTreeMap::new();
    let mut put = |pubkey: Pubkey, lamports: u64, owner: Pubkey, data: Vec<u8>| {
        accounts.insert(
            pubkey,
            Account {
                pubkey,
                lamports,
                owner,
                data,
            },
        );
    };
    put(
        params.oracle,
        params.oracle_lamports,
        bridge::SYSTEM_PROGRAM_ID,
        Vec::new(),
    );
    put(
        vault_key,
        params.supply_lamports,
        bridge::SYSTEM_PROGRAM_ID,
        Vec::new(),
    );
    // Every fee goes to the treasury, and the runtime burns a fee that would
    // leave it below the rent reserve. So it starts at exactly the reserve.
    put(
        treasury_key,
        rent(0)?,
        bridge::SYSTEM_PROGRAM_ID,
        Vec::new(),
    );
    put(
        bridge::BMM_BLOCKS_ID,
        rent(bridge::BLOCK_RECORD_LEN)?,
        bridge::SYSVAR_OWNER_ID,
        vec![0u8; bridge::BLOCK_RECORD_LEN],
    );

    if let Some(state) = state {
        let cursor = &state.cursor;
        config.deposit_high_water = cursor.deposit_high_water;
        config.pegged_lamports = cursor.pegged_lamports;
        config.withdrawal_count = cursor.withdrawal_count;
        config.bmm_next_height = cursor.bmm_next_height.max(params.ecash_start_height);
        config.bmm_paid_total = cursor.bmm_paid_total;
        config.credited_height = cursor.credited_height;
        config.credited_block = cursor.credited_block;
        config.credit_index = cursor.credit_index;
        config.credit_part_block = cursor.credit_part_block;
        config.stranded_lamports = cursor.stranded_lamports;

        let vault = *state
            .balances
            .get(&vault_key.to_string())
            .ok_or(GenesisError::NoVault)?;
        if vault.checked_add(cursor.pegged_lamports) != Some(params.supply_lamports) {
            return Err(GenesisError::VaultOpen {
                vault,
                pegged: cursor.pegged_lamports,
                supply: params.supply_lamports,
            });
        }
        for (pubkey, lamports) in &state.balances {
            let pubkey: Pubkey = pubkey
                .parse()
                .map_err(|_| GenesisError::BadPubkey(pubkey.clone()))?;
            put(pubkey, *lamports, bridge::SYSTEM_PROGRAM_ID, Vec::new());
        }
        for record in &state.records {
            let pubkey: Pubkey = record
                .pubkey
                .parse()
                .map_err(|_| GenesisError::BadPubkey(record.pubkey.clone()))?;
            let data = base64::engine::general_purpose::STANDARD
                .decode(&record.data)
                .map_err(|_| GenesisError::BadRecordData(record.pubkey.clone()))?;
            put(pubkey, record.lamports, *program_id, data);
        }
    }

    let data = config.encode();
    put(config_key, rent(data.len())?, *program_id, data);
    Ok(accounts.into_values().collect())
}

/// The `--primordial-accounts-file` body that `solana-genesis` reads.
pub fn yaml(accounts: &[Account]) -> String {
    accounts
        .iter()
        .map(|account| {
            format!(
                "{}:\n  balance: {}\n  owner: {}\n  data: \"{}\"\n  executable: false\n",
                account.pubkey,
                account.lamports,
                account.owner,
                base64::engine::general_purpose::STANDARD.encode(&account.data)
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrate::{Cursor, RecordAccount};

    const SUPPLY: u64 = 21_000_000 * 1_000_000_000;

    fn params() -> Params {
        Params {
            program_id: Pubkey::new_from_array([3u8; 32]),
            supply_lamports: SUPPLY,
            oracle: Pubkey::new_from_array([4u8; 32]),
            oracle_lamports: 1_000_000_000,
            lamports_per_byte_year: 4,
            ecash_start_height: 500,
            deposit_confirmations: 6,
            deposit_lag: 100,
        }
    }

    fn find(accounts: &[Account], pubkey: &Pubkey) -> Account {
        accounts
            .iter()
            .find(|account| &account.pubkey == pubkey)
            .cloned()
            .expect("the genesis holds the account")
    }

    fn config_of(accounts: &[Account]) -> Config {
        Config::decode(&find(accounts, &bridge::config_pda(&params().program_id).0).data).unwrap()
    }

    #[test]
    fn a_new_chain_holds_the_supply_in_the_vault_and_starts_at_the_tip() {
        let accounts = accounts(&params(), None).unwrap();
        let program_id = params().program_id;
        assert_eq!(
            find(&accounts, &bridge::vault_pda(&program_id).0).lamports,
            SUPPLY
        );
        assert_eq!(
            find(&accounts, &bridge::treasury_pda(&program_id).0).lamports,
            512
        );
        let config = config_of(&accounts);
        assert_eq!(config.oracle, params().oracle);
        assert_eq!(config.bmm_next_height, 500);
        assert_eq!(config.credited_height, 500);
        assert_eq!(config.credited_block, [0u8; 32]);
        assert_eq!(config.deposit_confirmations, 6);
        assert_eq!(config.deposit_lag, 100);
        assert_eq!(config.bump, bridge::config_pda(&program_id).1);
        assert_eq!(config.vault_bump, bridge::vault_pda(&program_id).1);
        assert_eq!(config.treasury_bump, bridge::treasury_pda(&program_id).1);
    }

    #[test]
    fn the_config_and_the_block_record_are_rent_exempt_at_their_size() {
        let accounts = accounts(&params(), None).unwrap();
        let config = find(&accounts, &bridge::config_pda(&params().program_id).0);
        assert_eq!(config.data.len(), Config::LEN);
        assert_eq!(config.lamports, (128 + 187) * 4);
        assert_eq!(config.owner, params().program_id);
        let record = find(&accounts, &bridge::BMM_BLOCKS_ID);
        assert_eq!(record.data, vec![0u8; bridge::BLOCK_RECORD_LEN]);
        assert_eq!(record.lamports, (128 + 229_376) * 4);
        assert_eq!(record.owner, bridge::SYSVAR_OWNER_ID);
    }

    #[test]
    fn d_of_zero_fails() {
        let mut zero = params();
        zero.deposit_confirmations = 0;
        assert!(matches!(
            accounts(&zero, None),
            Err(GenesisError::NoConfirmations)
        ));
    }

    fn a_state() -> State {
        let program_id = params().program_id;
        let mut balances = BTreeMap::new();
        balances.insert(
            bridge::vault_pda(&program_id).0.to_string(),
            SUPPLY - 70_000_000,
        );
        balances.insert(bridge::treasury_pda(&program_id).0.to_string(), 5_000);
        balances.insert(Pubkey::new_from_array([9u8; 32]).to_string(), 40_000_000);
        balances.insert(params().oracle.to_string(), 777);
        State {
            slot: 1_234,
            cursor: Cursor {
                pegged_lamports: 70_000_000,
                deposit_high_water: 3,
                withdrawal_count: 2,
                bmm_next_height: 450,
                bmm_paid_total: 9_000,
                credited_height: 480,
                credited_block: [8u8; 32],
                credit_index: 0,
                credit_part_block: [0u8; 32],
                stranded_lamports: 10,
            },
            balances,
            records: vec![RecordAccount {
                pubkey: bridge::withdrawal_pda(&program_id, 1).0.to_string(),
                lamports: 1_400,
                data: base64::engine::general_purpose::STANDARD.encode([1u8, 2, 3]),
            }],
            supply_lamports: SUPPLY + 1_000,
        }
    }

    #[test]
    fn a_migration_carries_the_balances_the_records_and_the_cursor() {
        let accounts = accounts(&params(), Some(&a_state())).unwrap();
        let program_id = params().program_id;
        assert_eq!(
            find(&accounts, &bridge::vault_pda(&program_id).0).lamports,
            SUPPLY - 70_000_000
        );
        assert_eq!(
            find(&accounts, &bridge::treasury_pda(&program_id).0).lamports,
            5_000
        );
        assert_eq!(
            find(&accounts, &Pubkey::new_from_array([9u8; 32])).lamports,
            40_000_000
        );
        assert_eq!(find(&accounts, &params().oracle).lamports, 777);
        let record = find(&accounts, &bridge::withdrawal_pda(&program_id, 1).0);
        assert_eq!((record.lamports, record.data), (1_400, vec![1, 2, 3]));
        assert_eq!(record.owner, program_id);
        let config = config_of(&accounts);
        assert_eq!(config.pegged_lamports, 70_000_000);
        assert_eq!(config.deposit_high_water, 3);
        assert_eq!(config.withdrawal_count, 2);
        assert_eq!(config.credited_height, 480);
        assert_eq!(config.credited_block, [8u8; 32]);
        assert_eq!(config.stranded_lamports, 10);
        assert_eq!(config.bmm_paid_total, 9_000);
        assert_eq!(config.bmm_next_height, 500);
        assert_eq!(config.oracle, params().oracle);
    }

    #[test]
    fn a_migration_with_an_open_vault_fails() {
        let mut state = a_state();
        state.cursor.pegged_lamports += 1;
        assert!(matches!(
            accounts(&params(), Some(&state)),
            Err(GenesisError::VaultOpen { .. })
        ));
    }

    #[test]
    fn the_yaml_names_each_account_with_base64_data() {
        let yaml = yaml(&[Account {
            pubkey: Pubkey::new_from_array([2u8; 32]),
            lamports: 21,
            owner: bridge::SYSVAR_OWNER_ID,
            data: vec![0, 0, 0],
        }]);
        assert!(yaml.starts_with(&format!("{}:\n", Pubkey::new_from_array([2u8; 32]))));
        assert!(yaml.contains("  balance: 21\n"));
        assert!(yaml.contains("  owner: Sysvar1111111111111111111111111111111111111\n"));
        assert!(yaml.contains("  data: \"AAAA\"\n"));
        assert!(yaml.contains("  executable: false\n"));
    }
}
