//! The primordial accounts of a new chain: the vault, the treasury, the
//! bridge config, the block record, the oracle, and the DEX accounts. A
//! migration adds the balances, the carried accounts, and the bridge cursor of
//! an old chain.

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
    /// The key that seals a bundle and marks it paid.
    pub oracle: Pubkey,
    /// The lamports that the oracle of a new chain gets to pay its tx fees.
    /// A migration gives it nothing new.
    pub oracle_lamports: u64,
    /// The spendable lamports that `solana-genesis` adds beside these
    /// accounts and that no deposit backs: the identity, the vote and stake
    /// accounts, and the faucet.
    pub float_lamports: u64,
    /// The fewest slots between a bundle seal and its `bundle_paid`.
    pub bundle_min_slots: u64,
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
    #[error("the account `{0}` in the state file is not base64")]
    BadRecordData(String),
    #[error("line {0} of the DEX accounts file is not in the form that the genesis reads")]
    BadAccountsFile(usize),
    #[error("the float overflows a u64")]
    FloatOverflow,
    #[error(
        "the DEX fee account `{pubkey}` holds {amount} tokens. Its owner moves them out before the \
         migration"
    )]
    DexFeesHeld { pubkey: Pubkey, amount: u64 },
    #[error("the DEX fee account `{0}` in the state file is not a native token account")]
    BadDexAccount(Pubkey),
    #[error(
        "the DEX fee account `{pubkey}` holds {lamports} lamports above its reserve. Its owner \
         moves them out before the migration"
    )]
    DexLamportsHeld { pubkey: Pubkey, lamports: u64 },
    #[error(
        "the CP-Swap pool `{0}` holds protocol or fund fees. Collect them on the old chain before \
         the migration"
    )]
    PoolFeesHeld(Pubkey),
    #[error(
        "the CP-Swap pool `{0}` holds creator fees, and its old config keeps a part of them. \
         Collect the creator fees on the old chain before the migration"
    )]
    PoolCreatorFeesHeld(Pubkey),
    #[error(
        "the CP-Swap account `{0}` of the old chain is no pool. It can hold an admin right, so the \
         migration stops"
    )]
    DexAuthority(Pubkey),
}

/// The token program. A token account of 165 bytes can close, so its
/// lamports are spendable.
const TOKEN_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
const TOKEN_ACCOUNT_LEN: usize = 165;
/// The token amount, the native flag, and the native reserve of a token
/// account.
const TOKEN_MINT_AND_OWNER: std::ops::Range<usize> = 0..64;
const TOKEN_AMOUNT: std::ops::Range<usize> = 64..72;
const TOKEN_NATIVE_FLAG: std::ops::Range<usize> = 109..113;
const TOKEN_NATIVE_RESERVE: std::ops::Range<usize> = 113..121;
/// The protocol and the fund fees of token 0 and token 1 in a CP-Swap pool.
const POOL_FEES: std::ops::Range<usize> = 341..373;
/// The config of a CP-Swap pool, and its creator fees of token 0 and token 1.
const POOL_CONFIG: std::ops::Range<usize> = 8..40;
const POOL_CREATOR_FEES: std::ops::Range<usize> = 397..413;
/// The part of the creator fees that a CP-Swap config keeps for the protocol.
const CONFIG_CREATOR_SHARE: std::ops::Range<usize> = 116..124;

const CPMM_ID: Pubkey = Pubkey::from_str_const("CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C");

fn u64_at(data: &[u8], at: std::ops::Range<usize>) -> Option<u64> {
    Some(u64::from_le_bytes(data.get(at)?.try_into().ok()?))
}

/// True when a CP-Swap pool holds protocol or fund fees. The fresh config
/// gives their collection to a key that nobody holds.
fn pool_holds_fees(data: &[u8]) -> bool {
    data.get(..8) == Some(&bridge::discriminator("account", "PoolState")[..])
        && data
            .get(POOL_FEES)
            .is_none_or(|fees| fees.iter().any(|byte| *byte != 0))
}

/// The pool of a CP-Swap account that holds creator fees whose split the
/// fresh config changes: the carried config of the pool keeps a part of them,
/// or the old chain holds no such config.
fn pool_with_shared_creator_fees(
    pubkey: &Pubkey,
    pool: &Account,
    accounts: &BTreeMap<Pubkey, Account>,
) -> Option<Pubkey> {
    let pool_kind = bridge::discriminator("account", "PoolState");
    if pool.owner != CPMM_ID || pool.data.get(..8) != Some(&pool_kind[..]) {
        return None;
    }
    let fees = pool.data.get(POOL_CREATOR_FEES);
    if fees.is_some_and(|fees| fees.iter().all(|byte| *byte == 0)) {
        return None;
    }
    let shared = pool
        .data
        .get(POOL_CONFIG)
        .and_then(|key| Pubkey::try_from(key).ok())
        .and_then(|key| accounts.get(&key))
        .filter(|config| {
            config.owner == CPMM_ID
                && config.data.get(..8) == Some(&bridge::discriminator("account", "AmmConfig")[..])
        })
        .and_then(|config| u64_at(&config.data, CONFIG_CREATOR_SHARE))
        .is_none_or(|share| share != 0);
    shared.then_some(*pubkey)
}

/// True when a CP-Swap account holds only pool state. Any other CP-Swap
/// account (a config, a permission, a fee share) can give a key a right.
fn cpmm_pool_data(data: &[u8]) -> bool {
    ["PoolState", "ObservationState"]
        .iter()
        .any(|name| data.get(..8) == Some(&bridge::discriminator("account", name)[..]))
}

/// The fresh DEX account with the lamports of the carried one at its key.
/// The fresh data drops each key and each fee rate of the old chain. A token
/// account of another owner must hold no tokens and no surplus lamports,
/// because its new owner is a key that nobody holds.
fn fresh_over(fresh: &Account, carried: &Account) -> Result<Account, GenesisError> {
    let mut data = fresh.data.clone();
    if fresh.owner == TOKEN_PROGRAM_ID && fresh.data.len() == TOKEN_ACCOUNT_LEN {
        if carried.owner != TOKEN_PROGRAM_ID || carried.data.len() != TOKEN_ACCOUNT_LEN {
            return Err(GenesisError::BadDexAccount(carried.pubkey));
        }
        // Nobody can sign for the fresh owner, so anyone can leave tokens or
        // lamports here that nobody takes out. They carry as they are.
        if carried.data[TOKEN_MINT_AND_OWNER] == fresh.data[TOKEN_MINT_AND_OWNER] {
            return Ok(carried.clone());
        }
        let amount = u64_at(&carried.data, TOKEN_AMOUNT).unwrap_or_default();
        if amount != 0 {
            return Err(GenesisError::DexFeesHeld {
                pubkey: carried.pubkey,
                amount,
            });
        }
        let reserve = u64_at(&carried.data, TOKEN_NATIVE_RESERVE)
            .filter(|_| carried.data[TOKEN_NATIVE_FLAG] == [1, 0, 0, 0])
            .ok_or(GenesisError::BadDexAccount(carried.pubkey))?;
        if carried.lamports != reserve {
            return Err(GenesisError::DexLamportsHeld {
                pubkey: carried.pubkey,
                lamports: carried.lamports.saturating_sub(reserve),
            });
        }
        data[TOKEN_NATIVE_RESERVE].copy_from_slice(&carried.lamports.to_le_bytes());
    }
    Ok(Account {
        pubkey: fresh.pubkey,
        lamports: carried.lamports,
        owner: fresh.owner,
        data,
    })
}

/// The accounts that the genesis writes, one per pubkey. `extra` holds the
/// DEX accounts. At the key of a carried account, the fresh data replaces the
/// old data, and the old lamports stay.
pub fn accounts(
    params: &Params,
    state: Option<&State>,
    extra: &[Account],
) -> Result<Vec<Account>, GenesisError> {
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
        vault_supply: params.supply_lamports,
        float_lamports: params
            .float_lamports
            .checked_add(params.oracle_lamports)
            .ok_or(GenesisError::FloatOverflow)?,
        paid_through: 0,
        bundled_through: 0,
        bundle_sealed_height: 0,
        bundle_sealed_slot: 0,
        bundle_min_slots: params.bundle_min_slots,
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
    if state.is_none() {
        put(
            params.oracle,
            params.oracle_lamports,
            bridge::SYSTEM_PROGRAM_ID,
            Vec::new(),
        );
    }
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
        config.vault_supply = cursor.vault_supply;
        config.float_lamports = cursor
            .float_lamports
            .checked_add(params.float_lamports)
            .ok_or(GenesisError::FloatOverflow)?;
        config.paid_through = cursor.paid_through;
        config.bundled_through = cursor.bundled_through;
        config.bundle_sealed_height = cursor.bundle_sealed_height;

        let vault = *state
            .balances
            .get(&vault_key.to_string())
            .ok_or(GenesisError::NoVault)?;
        // A gift leaves a surplus in the vault, and a surplus is harmless.
        if vault
            .checked_add(cursor.pegged_lamports)
            .is_none_or(|backed| backed < cursor.vault_supply)
        {
            return Err(GenesisError::VaultOpen {
                vault,
                pegged: cursor.pegged_lamports,
                supply: cursor.vault_supply,
            });
        }
        for (pubkey, lamports) in &state.balances {
            let pubkey: Pubkey = pubkey
                .parse()
                .map_err(|_| GenesisError::BadPubkey(pubkey.clone()))?;
            put(pubkey, *lamports, bridge::SYSTEM_PROGRAM_ID, Vec::new());
        }
        for account in &state.accounts {
            let pubkey: Pubkey = account
                .pubkey
                .parse()
                .map_err(|_| GenesisError::BadPubkey(account.pubkey.clone()))?;
            let owner: Pubkey = account
                .owner
                .parse()
                .map_err(|_| GenesisError::BadPubkey(account.owner.clone()))?;
            let data = base64::engine::general_purpose::STANDARD
                .decode(&account.data)
                .map_err(|_| GenesisError::BadRecordData(account.pubkey.clone()))?;
            if owner == CPMM_ID && pool_holds_fees(&data) {
                return Err(GenesisError::PoolFeesHeld(pubkey));
            }
            if owner == CPMM_ID
                && !cpmm_pool_data(&data)
                && !extra.iter().any(|fresh| fresh.pubkey == pubkey)
            {
                return Err(GenesisError::DexAuthority(pubkey));
            }
            put(pubkey, account.lamports, owner, data);
        }
        if let Some(pool) = accounts
            .iter()
            .find_map(|(pubkey, account)| pool_with_shared_creator_fees(pubkey, account, &accounts))
        {
            return Err(GenesisError::PoolCreatorFeesHeld(pool));
        }
    }

    for account in extra {
        if let Some(carried) = accounts.get(&account.pubkey) {
            let fresh = fresh_over(account, carried)?;
            accounts.insert(account.pubkey, fresh);
            continue;
        }
        if account.owner == TOKEN_PROGRAM_ID && account.data.len() == TOKEN_ACCOUNT_LEN {
            config.float_lamports = config
                .float_lamports
                .checked_add(account.lamports)
                .ok_or(GenesisError::FloatOverflow)?;
        }
        accounts.insert(account.pubkey, account.clone());
    }

    let data = config.encode();
    accounts.insert(
        config_key,
        Account {
            pubkey: config_key,
            lamports: rent(data.len())?,
            owner: *program_id,
            data,
        },
    );
    Ok(accounts.into_values().collect())
}

/// Reads a `--primordial-accounts-file` body of the form that [`yaml`] and
/// `fetch-dex-programs.sh` write.
pub fn parse_yaml(text: &str) -> Result<Vec<Account>, GenesisError> {
    let mut accounts = Vec::new();
    let mut lines = text
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty());
    while let Some((at, line)) = lines.next() {
        let bad = || GenesisError::BadAccountsFile(at + 1);
        let pubkey: Pubkey = line
            .strip_suffix(':')
            .filter(|key| !key.starts_with(' '))
            .and_then(|key| key.parse().ok())
            .ok_or_else(bad)?;
        let mut field = |name: &str| -> Result<String, GenesisError> {
            let (at, line) = lines.next().ok_or_else(bad)?;
            line.strip_prefix(&format!("  {name}: "))
                .map(str::to_owned)
                .ok_or(GenesisError::BadAccountsFile(at + 1))
        };
        let lamports = field("balance")?.parse().map_err(|_| bad())?;
        let owner = field("owner")?.parse().map_err(|_| bad())?;
        let data = field("data")?;
        let data = data
            .strip_prefix('"')
            .and_then(|data| data.strip_suffix('"'))
            .and_then(|data| base64::engine::general_purpose::STANDARD.decode(data).ok())
            .ok_or_else(bad)?;
        if field("executable")? != "false" {
            return Err(bad());
        }
        accounts.push(Account {
            pubkey,
            lamports,
            owner,
            data,
        });
    }
    Ok(accounts)
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
    use crate::migrate::{CarriedAccount, Cursor};

    const SUPPLY: u64 = 21_000_000 * 1_000_000_000;

    fn params() -> Params {
        Params {
            program_id: Pubkey::new_from_array([3u8; 32]),
            supply_lamports: SUPPLY,
            oracle: Pubkey::new_from_array([4u8; 32]),
            oracle_lamports: 1_000_000,
            float_lamports: 120_000,
            bundle_min_slots: 4_000,
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
        let accounts = accounts(&params(), None, &[]).unwrap();
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
        let accounts = accounts(&params(), None, &[]).unwrap();
        let config = find(&accounts, &bridge::config_pda(&params().program_id).0);
        assert_eq!(config.data.len(), Config::LEN);
        assert_eq!(config.lamports, (128 + 243) * 4);
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
            accounts(&zero, None, &[]),
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
                vault_supply: SUPPLY,
                float_lamports: 90_000,
                paid_through: 1,
                bundled_through: 2,
                bundle_sealed_height: 470,
            },
            balances,
            accounts: vec![CarriedAccount {
                pubkey: bridge::withdrawal_pda(&program_id, 1).0.to_string(),
                lamports: 1_400,
                owner: program_id.to_string(),
                data: base64::engine::general_purpose::STANDARD.encode([1u8, 2, 3]),
            }],
            supply_lamports: SUPPLY + 1_000,
            fixed_lamports: 500,
        }
    }

    #[test]
    fn a_migration_carries_the_balances_the_records_and_the_cursor() {
        let accounts = accounts(&params(), Some(&a_state()), &[]).unwrap();
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
        assert_eq!(config.vault_supply, SUPPLY);
        assert_eq!(config.float_lamports, 90_000 + 120_000);
        assert_eq!((config.paid_through, config.bundled_through), (1, 2));
        assert_eq!(config.bundle_sealed_height, 470);
        assert_eq!(config.bundle_sealed_slot, 0);
        assert_eq!(config.bundle_min_slots, 4_000);
    }

    #[test]
    fn a_migration_gives_the_oracle_nothing_new() {
        let mut state = a_state();
        state.balances.remove(&params().oracle.to_string());
        let accounts = accounts(&params(), Some(&state), &[]).unwrap();
        assert!(accounts
            .iter()
            .all(|account| account.pubkey != params().oracle));
    }

    #[test]
    fn a_surplus_in_the_vault_migrates() {
        let mut state = a_state();
        let vault = bridge::vault_pda(&params().program_id).0.to_string();
        *state.balances.get_mut(&vault).unwrap() += 5_000;
        let accounts = accounts(&params(), Some(&state), &[]).unwrap();
        let program_id = params().program_id;
        assert_eq!(
            find(&accounts, &bridge::vault_pda(&program_id).0).lamports,
            SUPPLY - 70_000_000 + 5_000
        );
    }

    #[test]
    fn a_new_chain_counts_the_oracle_and_the_bootstrap_lamports_as_float() {
        let config = config_of(&accounts(&params(), None, &[]).unwrap());
        assert_eq!(config.vault_supply, SUPPLY);
        assert_eq!(config.float_lamports, 1_000_000 + 120_000);
        assert_eq!(config.bundle_min_slots, 4_000);
    }

    fn a_token_account(byte: u8, lamports: u64, len: usize) -> Account {
        Account {
            pubkey: Pubkey::new_from_array([byte; 32]),
            lamports,
            owner: TOKEN_PROGRAM_ID,
            data: vec![0u8; len],
        }
    }

    #[test]
    fn a_dex_token_account_adds_float_and_a_mint_does_not() {
        let extra = [
            a_token_account(20, 2_344, 165),
            a_token_account(21, 10_000, 82),
        ];
        let accounts = accounts(&params(), None, &extra).unwrap();
        assert_eq!(find(&accounts, &extra[0].pubkey).lamports, 2_344);
        assert_eq!(find(&accounts, &extra[1].pubkey).lamports, 10_000);
        assert_eq!(
            config_of(&accounts).float_lamports,
            1_000_000 + 120_000 + 2_344
        );
    }

    /// The `AmmConfig` at index 0 on Solana mainnet: Raydium owners and fees.
    const MAINNET_AMM_CONFIG: &str = "2vQhaMvLK2//AAAAxAkAAAAAAADA1AEAAAAAAECcAAAAAAAAgNHwCAAAAAAF2xU6D/SH9pzeiQAf7ExGlbbUIV6iRLh71Otyh/hXqNcGSjj20XsRPl9m3PtjuHN//OJkSr8KALmLNDVtY4WD9AEAAAAAAABQwwAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";

    fn the_dex_accounts() -> Vec<Account> {
        let text = std::process::Command::new("python3")
            .args([
                "-I",
                "../genesis/dex_accounts.py",
                "accounts",
                "4",
                "/dev/stdout",
            ])
            .output()
            .expect("python3 runs dex_accounts.py")
            .stdout;
        parse_yaml(std::str::from_utf8(&text).unwrap()).unwrap()
    }

    fn carry(state: &mut State, account: &Account) {
        state.accounts.push(CarriedAccount {
            pubkey: account.pubkey.to_string(),
            lamports: account.lamports,
            owner: account.owner.to_string(),
            data: base64::engine::general_purpose::STANDARD.encode(&account.data),
        });
    }

    #[test]
    fn a_migration_ends_with_the_fresh_amm_config() {
        let dex = the_dex_accounts();
        let fresh = dex.iter().find(|account| account.owner == CPMM_ID).unwrap();
        let mut state = a_state();
        let old = Account {
            lamports: 2_584_522,
            data: base64::engine::general_purpose::STANDARD
                .decode(MAINNET_AMM_CONFIG)
                .unwrap(),
            ..fresh.clone()
        };
        carry(&mut state, &old);
        let accounts = accounts(&params(), Some(&state), &dex).unwrap();
        let config = find(&accounts, &fresh.pubkey);
        assert_eq!(config.data, fresh.data, "the no-admin config wins");
        assert_eq!(config.lamports, 2_584_522, "the old lamports stay");
        assert_ne!(config.data, old.data);
    }

    #[test]
    fn an_empty_fee_account_takes_the_fresh_owner_and_keeps_its_lamports() {
        let fee = a_token_account(20, 2_344, 165);
        let mut state = a_state();
        let old = Account {
            lamports: 900_000,
            data: native_account(0, 900_000),
            ..fee.clone()
        };
        carry(&mut state, &old);
        let accounts = accounts(&params(), Some(&state), std::slice::from_ref(&fee)).unwrap();
        let carried = find(&accounts, &fee.pubkey);
        assert_eq!(carried.lamports, 900_000);
        assert_eq!(carried.data[..113], fee.data[..113]);
        assert_eq!(carried.data[113..121], 900_000u64.to_le_bytes());
        assert_eq!(carried.data[121..], fee.data[121..]);
        assert_eq!(config_of(&accounts).float_lamports, 90_000 + 120_000);
    }

    fn native_account(amount: u64, reserve: u64) -> Vec<u8> {
        let mut data = vec![7u8; 165];
        data[TOKEN_AMOUNT].copy_from_slice(&amount.to_le_bytes());
        data[TOKEN_NATIVE_FLAG].copy_from_slice(&[1, 0, 0, 0]);
        data[TOKEN_NATIVE_RESERVE].copy_from_slice(&reserve.to_le_bytes());
        data
    }

    #[test]
    fn an_adminless_fee_account_carries_as_it_is() {
        let dex = the_dex_accounts();
        let fee = dex
            .iter()
            .find(|account| account.data.len() == TOKEN_ACCOUNT_LEN)
            .unwrap();
        let mut data = fee.data.clone();
        data[TOKEN_AMOUNT].copy_from_slice(&3u64.to_le_bytes());
        let old = Account {
            lamports: fee.lamports + 1,
            data,
            ..fee.clone()
        };
        let mut state = a_state();
        carry(&mut state, &old);
        let accounts = accounts(&params(), Some(&state), &dex).unwrap();
        assert_eq!(find(&accounts, &fee.pubkey), old);
    }

    #[test]
    fn lamports_above_the_reserve_stop_the_migration() {
        let fee = a_token_account(20, 2_344, 165);
        let mut state = a_state();
        carry(
            &mut state,
            &Account {
                lamports: 900_000,
                data: native_account(0, 2_344),
                ..fee.clone()
            },
        );
        assert!(matches!(
            accounts(&params(), Some(&state), std::slice::from_ref(&fee)),
            Err(GenesisError::DexLamportsHeld {
                lamports: 897_656,
                ..
            })
        ));
        let mut state = a_state();
        let mut not_native = native_account(0, 900_000);
        not_native[TOKEN_NATIVE_FLAG].copy_from_slice(&[0, 0, 0, 0]);
        carry(
            &mut state,
            &Account {
                lamports: 900_000,
                data: not_native,
                ..fee.clone()
            },
        );
        assert!(matches!(
            accounts(&params(), Some(&state), std::slice::from_ref(&fee)),
            Err(GenesisError::BadDexAccount(_))
        ));
    }

    #[test]
    fn a_pool_with_protocol_or_fund_fees_stops_the_migration() {
        for at in [341, 349, 357, 365] {
            let mut data = vec![0u8; 637];
            data[..8].copy_from_slice(&bridge::discriminator("account", "PoolState"));
            data[at] = 1;
            let mut state = a_state();
            carry(
                &mut state,
                &Account {
                    pubkey: Pubkey::new_from_array([30u8; 32]),
                    lamports: 5_000,
                    owner: CPMM_ID,
                    data,
                },
            );
            assert!(matches!(
                accounts(&params(), Some(&state), &[]),
                Err(GenesisError::PoolFeesHeld(_))
            ));
        }
    }

    fn a_pool_with_creator_fees(config: Pubkey) -> Account {
        let mut data = vec![0u8; 637];
        data[..8].copy_from_slice(&bridge::discriminator("account", "PoolState"));
        data[POOL_CONFIG].copy_from_slice(config.as_ref());
        data[POOL_CREATOR_FEES.start] = 9;
        Account {
            pubkey: Pubkey::new_from_array([30u8; 32]),
            lamports: 5_000,
            owner: CPMM_ID,
            data,
        }
    }

    fn with_creator_share(share: u64) -> Result<Vec<Account>, GenesisError> {
        let dex = the_dex_accounts();
        let fresh = dex.iter().find(|account| account.owner == CPMM_ID).unwrap();
        let mut old = base64::engine::general_purpose::STANDARD
            .decode(MAINNET_AMM_CONFIG)
            .unwrap();
        old[CONFIG_CREATOR_SHARE].copy_from_slice(&share.to_le_bytes());
        let mut state = a_state();
        carry(
            &mut state,
            &Account {
                data: old,
                ..fresh.clone()
            },
        );
        carry(&mut state, &a_pool_with_creator_fees(fresh.pubkey));
        accounts(&params(), Some(&state), &dex)
    }

    #[test]
    fn creator_fees_that_the_old_config_shares_stop_the_migration() {
        let mainnet = base64::engine::general_purpose::STANDARD
            .decode(MAINNET_AMM_CONFIG)
            .unwrap();
        assert_eq!(u64_at(&mainnet, CONFIG_CREATOR_SHARE), Some(50_000));
        assert!(matches!(
            with_creator_share(50_000),
            Err(GenesisError::PoolCreatorFeesHeld(_))
        ));
        let accounts = with_creator_share(0).unwrap();
        let pool = a_pool_with_creator_fees(Pubkey::default());
        assert_eq!(
            find(&accounts, &pool.pubkey).data[POOL_CREATOR_FEES.start],
            9
        );
    }

    #[test]
    fn creator_fees_of_a_pool_without_its_config_stop_the_migration() {
        let mut state = a_state();
        carry(
            &mut state,
            &a_pool_with_creator_fees(Pubkey::new_from_array([31u8; 32])),
        );
        assert!(matches!(
            accounts(&params(), Some(&state), &[]),
            Err(GenesisError::PoolCreatorFeesHeld(_))
        ));
    }

    #[test]
    fn a_fee_account_that_holds_tokens_stops_the_migration() {
        let fee = a_token_account(20, 2_344, 165);
        let mut state = a_state();
        let data = native_account(5, 899_995);
        carry(
            &mut state,
            &Account {
                lamports: 900_000,
                data,
                ..fee.clone()
            },
        );
        assert!(matches!(
            accounts(&params(), Some(&state), std::slice::from_ref(&fee)),
            Err(GenesisError::DexFeesHeld { amount: 5, .. })
        ));
    }

    #[test]
    fn a_pool_carries_and_another_cpmm_account_stops_the_migration() {
        let pool = Account {
            pubkey: Pubkey::new_from_array([30u8; 32]),
            lamports: 5_000,
            owner: CPMM_ID,
            data: bridge::discriminator("account", "PoolState")
                .into_iter()
                .chain([0u8; 629])
                .collect(),
        };
        let mut state = a_state();
        carry(&mut state, &pool);
        let accounts = accounts(&params(), Some(&state), &[]).unwrap();
        assert_eq!(find(&accounts, &pool.pubkey), pool);

        for name in [
            "AmmConfig",
            "Permission",
            "CreatorFeeShare",
            "SupportMintAssociated",
        ] {
            let mut state = a_state();
            carry(
                &mut state,
                &Account {
                    data: bridge::discriminator("account", name).to_vec(),
                    ..pool.clone()
                },
            );
            assert!(matches!(
                super::accounts(&params(), Some(&state), &[]),
                Err(GenesisError::DexAuthority(_))
            ));
        }
    }

    #[test]
    fn the_yaml_reads_back() {
        let written = [
            a_token_account(20, 2_344, 165),
            Account {
                pubkey: Pubkey::new_from_array([2u8; 32]),
                lamports: 21,
                owner: bridge::SYSVAR_OWNER_ID,
                data: Vec::new(),
            },
        ];
        assert_eq!(parse_yaml(&yaml(&written)).unwrap(), written);
        assert!(matches!(
            parse_yaml("abc:\n  balance: 1\n"),
            Err(GenesisError::BadAccountsFile(1))
        ));
        let mut executable = yaml(&written[..1]);
        executable = executable.replace("executable: false", "executable: true");
        assert!(parse_yaml(&executable).is_err());
    }

    #[test]
    fn a_migration_with_an_open_vault_fails() {
        let mut state = a_state();
        let vault = bridge::vault_pda(&params().program_id).0.to_string();
        *state.balances.get_mut(&vault).unwrap() -= 1;
        assert!(matches!(
            accounts(&params(), Some(&state), &[]),
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
