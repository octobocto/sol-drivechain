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
    /// The eCash blocks that the credits must pass above a commitment without
    /// a pair before the BMM cursor skips it.
    pub bmm_skip_blocks: u64,
    pub lamports_per_byte_year: u64,
    /// The eCash tip when the chain starts. BMM settles from this height,
    /// and a new chain counts the deposits up to it as credited.
    pub ecash_start_height: u64,
    /// D, the eCash confirmations before a credit.
    pub deposit_confirmations: u64,
    /// A BMM checkpoint at eCash height H counts only when its bank credited
    /// every deposit through H minus this lag.
    pub deposit_lag: u64,
    /// The bootstrap vote and stake keys. `solana-genesis` writes fresh
    /// accounts there, so no carried account may hold one.
    pub bootstrap_keys: [Option<Pubkey>; 2],
}

/// One account of the primordial accounts file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Account {
    pub pubkey: Pubkey,
    pub lamports: u64,
    pub owner: Pubkey,
    pub data: Vec<u8>,
    pub executable: bool,
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
    #[error("`{0}` is in the balances and in the accounts of the state file")]
    KeyTwice(Pubkey),
    #[error(
        "the state carries `{0}`, a key where `solana-genesis` writes a fresh bootstrap account"
    )]
    BootstrapKeyCarried(Pubkey),
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
fn u64_at(data: &[u8], at: std::ops::Range<usize>) -> Option<u64> {
    Some(u64::from_le_bytes(data.get(at)?.try_into().ok()?))
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
        executable: false,
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
        bmm_skip_blocks: params.bmm_skip_blocks,
        bmm_next_since_slot: 0,
        bundle_m6id: [0u8; 32],
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
                executable: false,
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
        config.bundle_m6id = cursor.bundle_m6id;

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
            if state.balances.contains_key(&account.pubkey) {
                return Err(GenesisError::KeyTwice(pubkey));
            }
            let owner: Pubkey = account
                .owner
                .parse()
                .map_err(|_| GenesisError::BadPubkey(account.owner.clone()))?;
            let mut data = base64::engine::general_purpose::STANDARD
                .decode(&account.data)
                .map_err(|_| GenesisError::BadRecordData(account.pubkey.clone()))?;
            rebase_to_slot_zero(&owner, &mut data, state.epoch);
            accounts.insert(
                pubkey,
                Account {
                    pubkey,
                    lamports: account.lamports,
                    owner,
                    data,
                    executable: account.executable,
                },
            );
        }
    }

    if let Some(state) = state {
        for key in params.bootstrap_keys.iter().flatten() {
            if state.balances.contains_key(&key.to_string())
                || state
                    .accounts
                    .iter()
                    .any(|account| account.pubkey == key.to_string())
            {
                return Err(GenesisError::BootstrapKeyCarried(*key));
            }
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
            executable: false,
        },
    );
    Ok(accounts.into_values().collect())
}

const UPGRADEABLE_LOADER_ID: Pubkey =
    Pubkey::from_str_const("BPFLoaderUpgradeab1e11111111111111111111111");
const LOADER_V4_ID: Pubkey = Pubkey::from_str_const("LoaderV411111111111111111111111111111111111");
const LOOKUP_TABLE_ID: Pubkey =
    Pubkey::from_str_const("AddressLookupTab1e1111111111111111111111111");
const STAKE_ID: Pubkey = Pubkey::from_str_const("Stake11111111111111111111111111111111111111");

const VOTE_ID: Pubkey = Pubkey::from_str_const("Vote111111111111111111111111111111111111111");
const TOKEN_2022_ID: Pubkey = Pubkey::from_str_const("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");

/// Moves the epochs of the `TransferFeeConfig` extension of a Token-2022 mint
/// back by `source_epoch`. An old fee stays in force, and a pending one keeps
/// the epochs that it has left. No other extension holds an epoch or a slot.
/// Only a mint whose base and extension list decode to the end changes.
fn rebase_transfer_fees(data: &mut [u8], source_epoch: u64) {
    const MINT_LEN: usize = 82;
    const MULTISIG_LEN: usize = 355;
    const ACCOUNT_TYPE: usize = 165;
    const MINT: u8 = 1;
    const TRANSFER_FEE_CONFIG: u16 = 1;
    const TRANSFER_FEE_CONFIG_LEN: usize = 108;
    // The epoch of the older fee, then of the newer fee, in the extension.
    const EPOCHS: [usize; 2] = [72, 90];
    if data.len() == MULTISIG_LEN
        || data.get(ACCOUNT_TYPE) != Some(&MINT)
        || data[MINT_LEN..ACCOUNT_TYPE].iter().any(|byte| *byte != 0)
    {
        return;
    }
    let mut fee_config = None;
    let mut at = ACCOUNT_TYPE + 1;
    while at < data.len() {
        // A mint of the multisig length ends with a two-byte zero tag.
        if data.len() - at == 2 {
            if data[at..] != [0, 0] {
                return;
            }
            break;
        }
        let Some(head) = data.get(at..at + 4) else {
            return;
        };
        let kind = u16::from_le_bytes([head[0], head[1]]);
        let len = usize::from(u16::from_le_bytes([head[2], head[3]]));
        let value = at + 4;
        if kind == 0 {
            // Unused space ends the list, and it holds only zero bytes.
            if data[at..].iter().any(|byte| *byte != 0) {
                return;
            }
            break;
        }
        if value + len > data.len() {
            return;
        }
        if kind == TRANSFER_FEE_CONFIG {
            if len != TRANSFER_FEE_CONFIG_LEN || fee_config.is_some() {
                return;
            }
            fee_config = Some(value);
        }
        at = value + len;
    }
    let Some(value) = fee_config else {
        return;
    };
    for offset in EPOCHS {
        let field = &mut data[value + offset..value + offset + 8];
        let epoch = u64::from_le_bytes(field.try_into().unwrap_or_default());
        field.copy_from_slice(&epoch.saturating_sub(source_epoch).to_le_bytes());
    }
}

/// Moves each slot and epoch field of a carried account to the start of the
/// new chain, which begins at slot 0 and epoch 0. `source_epoch` is the epoch
/// of the old chain at the halt. Without this, a program counts as not
/// deployed, and a lookup table, a stake, or a vote waits for the old slot.
pub fn rebase_to_slot_zero(owner: &Pubkey, data: &mut [u8], source_epoch: u64) {
    fn set(data: &mut [u8], at: std::ops::Range<usize>, keep_max: bool) {
        if let Some(field) = data.get_mut(at) {
            if !(keep_max && field.iter().all(|byte| *byte == 0xff)) {
                field.copy_from_slice(&0u64.to_le_bytes());
            }
        }
    }
    let tag = data
        .get(..4)
        .and_then(|tag| tag.try_into().ok())
        .map(u32::from_le_bytes);
    match (*owner, tag) {
        // ProgramData: the slot of its deployment.
        (UPGRADEABLE_LOADER_ID, Some(3)) => set(data, 4..12, false),
        // The slot of the deployment.
        (LOADER_V4_ID, _) => set(data, 0..8, false),
        // The deactivation slot, and the slot of the last extension.
        (LOOKUP_TABLE_ID, Some(1)) => {
            set(data, 4..12, true);
            set(data, 12..20, false);
        }
        (STAKE_ID, Some(tag @ (1 | 2))) => {
            // The lockup epoch keeps the epochs that it has left.
            if let Some(field) = data.get_mut(84..92) {
                let epoch = u64::from_le_bytes(field.try_into().unwrap_or_default());
                field.copy_from_slice(&epoch.saturating_sub(source_epoch).to_le_bytes());
            }
            // The activation and the deactivation epoch of a delegation.
            if tag == 2 {
                set(data, 164..172, true);
                set(data, 172..180, true);
            }
        }
        (TOKEN_2022_ID, _) => rebase_transfer_fees(data, source_epoch),
        (VOTE_ID, Some(_)) => {
            if let Some(fresh) = reset_vote(data, source_epoch) {
                data.copy_from_slice(&fresh);
            }
        }
        _ => {}
    }
}

/// The vote account with no vote, no root, no credits, and no timestamp. The
/// voter of `source_epoch` holds epoch 0, and each later voter keeps its
/// distance in epochs. The node, the withdrawer, the collectors, and the
/// commission stay. `None` for a layout that the function does not read.
pub fn reset_vote(data: &[u8], source_epoch: u64) -> Option<Vec<u8>> {
    struct Reader<'a> {
        data: &'a [u8],
        at: usize,
    }
    impl Reader<'_> {
        fn take(&mut self, len: usize) -> Option<&[u8]> {
            let end = self.at.checked_add(len)?;
            let out = self.data.get(self.at..end)?;
            self.at = end;
            Some(out)
        }
        fn u64(&mut self) -> Option<u64> {
            Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
        }
        fn u8(&mut self) -> Option<u8> {
            Some(*self.take(1)?.first()?)
        }
    }
    let tag = u32::from_le_bytes(data.get(..4)?.try_into().ok()?);
    let mut reader = Reader { data, at: 4 };
    let (vote_len, prior) = match tag {
        // 1.14.11: the node, the withdrawer, the commission, plain lockouts.
        1 => {
            reader.take(65)?;
            (12, true)
        }
        // 3: the same fields, landed votes.
        2 => {
            reader.take(65)?;
            (13, true)
        }
        // 4: the node, the withdrawer, two collectors, two commissions, the
        // pending rewards, and the BLS key.
        3 => {
            reader.take(32 * 4 + 2 + 2 + 8)?;
            if reader.u8()? == 1 {
                reader.take(48)?;
            }
            (13, false)
        }
        _ => return None,
    };
    let kept = reader.at;
    let votes = usize::try_from(reader.u64()?).ok()?;
    reader.take(votes.checked_mul(vote_len)?)?;
    if reader.u8()? == 1 {
        reader.u64()?;
    }
    let count = reader.u64()?;
    let mut voters = Vec::new();
    for _ in 0..count {
        let epoch = reader.u64()?;
        voters.push((epoch, reader.take(32)?.to_vec()));
    }
    let voters = rebase_voters(&voters, source_epoch)?;

    let mut fresh = data.get(..kept)?.to_vec();
    fresh.extend_from_slice(&0u64.to_le_bytes());
    fresh.push(0);
    fresh.extend_from_slice(&(voters.len() as u64).to_le_bytes());
    for (epoch, voter) in &voters {
        fresh.extend_from_slice(&epoch.to_le_bytes());
        fresh.extend_from_slice(voter);
    }
    if prior {
        fresh.extend_from_slice(&[0u8; 32 * 48]);
        fresh.extend_from_slice(&31u64.to_le_bytes());
        fresh.push(1);
    }
    fresh.extend_from_slice(&0u64.to_le_bytes());
    fresh.extend_from_slice(&[0u8; 16]);
    if fresh.len() > data.len() {
        return None;
    }
    fresh.resize(data.len(), 0);
    Some(fresh)
}

/// The authorized voters on the new chain: the voter that holds
/// `source_epoch` moves to epoch 0, and each later voter moves back by
/// `source_epoch`. `voters` holds the old map in epoch order.
fn rebase_voters(voters: &[(u64, Vec<u8>)], source_epoch: u64) -> Option<Vec<(u64, Vec<u8>)>> {
    let current = voters
        .iter()
        .rev()
        .find(|(epoch, _)| *epoch <= source_epoch)
        .or_else(|| voters.first())?;
    let mut rebased = vec![(0, current.1.clone())];
    rebased.extend(
        voters
            .iter()
            .filter(|(epoch, _)| *epoch > source_epoch)
            .map(|(epoch, voter)| (epoch - source_epoch, voter.clone())),
    );
    Some(rebased)
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
        let executable = match field("executable")?.as_str() {
            "false" => false,
            "true" => true,
            _ => return Err(bad()),
        };
        accounts.push(Account {
            pubkey,
            lamports,
            owner,
            data,
            executable,
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
                "{}:\n  balance: {}\n  owner: {}\n  data: \"{}\"\n  executable: {}\n",
                account.pubkey,
                account.lamports,
                account.owner,
                base64::engine::general_purpose::STANDARD.encode(&account.data),
                account.executable
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CPMM_ID: Pubkey = Pubkey::from_str_const("CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C");
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
            bmm_skip_blocks: 6,
            lamports_per_byte_year: 4,
            ecash_start_height: 500,
            deposit_confirmations: 6,
            deposit_lag: 100,
            bootstrap_keys: [Some(Pubkey::new_from_array([40u8; 32])), None],
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
        assert_eq!(config.lamports, (128 + 291) * 4);
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
                bundle_m6id: [6u8; 32],
            },
            balances,
            accounts: vec![CarriedAccount {
                pubkey: bridge::withdrawal_pda(&program_id, 1).0.to_string(),
                lamports: 1_400,
                owner: program_id.to_string(),
                data: base64::engine::general_purpose::STANDARD.encode([1u8, 2, 3]),
                executable: false,
            }],
            supply_lamports: SUPPLY + 1_000,
            fixed_lamports: 500,
            ownerless_lamports: 0,
            epoch: 0,
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
        assert_eq!(config.bundle_m6id, [6u8; 32]);
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
            executable: false,
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
            executable: false,
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
    fn a_pool_that_holds_fees_carries_byte_for_byte() {
        let mut data = vec![0u8; 637];
        data[..8].copy_from_slice(&bridge::discriminator("account", "PoolState"));
        // The protocol and fund fees of both tokens, then the creator fees.
        for at in [341, 349, 357, 365, 397, 405] {
            data[at] = 7;
        }
        let pool = Account {
            pubkey: Pubkey::new_from_array([30u8; 32]),
            lamports: 5_000,
            owner: CPMM_ID,
            data,
            executable: false,
        };
        let dex = the_dex_accounts();
        let mut state = a_state();
        carry(&mut state, &pool);
        let accounts = accounts(&params(), Some(&state), &dex).unwrap();
        assert_eq!(find(&accounts, &pool.pubkey), pool);
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
    fn each_cpmm_account_carries_byte_for_byte() {
        for name in [
            "PoolState",
            "ObservationState",
            "Permission",
            "CreatorFeeShare",
            "SupportMintAssociated",
        ] {
            let account = Account {
                pubkey: Pubkey::new_from_array([30u8; 32]),
                lamports: 5_000,
                owner: CPMM_ID,
                data: bridge::discriminator("account", name).to_vec(),
                executable: false,
            };
            let mut state = a_state();
            carry(&mut state, &account);
            let accounts = accounts(&params(), Some(&state), &[]).unwrap();
            assert_eq!(find(&accounts, &account.pubkey), account);
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
                executable: false,
            },
        ];
        assert_eq!(parse_yaml(&yaml(&written)).unwrap(), written);
        assert!(matches!(
            parse_yaml("abc:\n  balance: 1\n"),
            Err(GenesisError::BadAccountsFile(1))
        ));
        let mut program = written[..1].to_vec();
        program[0].executable = true;
        assert_eq!(parse_yaml(&yaml(&program)).unwrap(), program);
        let odd = yaml(&written[..1]).replace("executable: false", "executable: maybe");
        assert!(parse_yaml(&odd).is_err());
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
            executable: false,
        }]);
        assert!(yaml.starts_with(&format!("{}:\n", Pubkey::new_from_array([2u8; 32]))));
        assert!(yaml.contains("  balance: 21\n"));
        assert!(yaml.contains("  owner: Sysvar1111111111111111111111111111111111111\n"));
        assert!(yaml.contains("  data: \"AAAA\"\n"));
        assert!(yaml.contains("  executable: false\n"));
    }

    #[test]
    fn a_key_in_the_balances_and_the_accounts_stops_the_genesis() {
        let mut state = a_state();
        let key = Pubkey::new_from_array([9u8; 32]);
        carry(
            &mut state,
            &Account {
                pubkey: key,
                lamports: 1,
                owner: bridge::SYSTEM_PROGRAM_ID,
                data: vec![1],
                executable: false,
            },
        );
        assert!(matches!(
            accounts(&params(), Some(&state), &[]),
            Err(GenesisError::KeyTwice(found)) if found == key
        ));
    }

    #[test]
    fn a_carried_bootstrap_key_stops_the_genesis() {
        let bootstrap = Pubkey::new_from_array([40u8; 32]);
        let mut state = a_state();
        carry(
            &mut state,
            &Account {
                pubkey: bootstrap,
                lamports: 5_000,
                owner: STAKE_ID,
                data: vec![0u8; 200],
                executable: false,
            },
        );
        assert!(matches!(
            accounts(&params(), Some(&state), &[]),
            Err(GenesisError::BootstrapKeyCarried(found)) if found == bootstrap
        ));
        let mut state = a_state();
        state.balances.insert(bootstrap.to_string(), 7);
        assert!(matches!(
            accounts(&params(), Some(&state), &[]),
            Err(GenesisError::BootstrapKeyCarried(_))
        ));
    }

    #[test]
    fn slot_fields_move_to_the_start_of_the_new_chain() {
        let mut program_data = vec![3, 0, 0, 0];
        program_data.extend_from_slice(&777u64.to_le_bytes());
        program_data.extend_from_slice(&[1; 33]);
        rebase_to_slot_zero(&UPGRADEABLE_LOADER_ID, &mut program_data, 0);
        assert_eq!(program_data[4..12], [0; 8]);
        assert_eq!(program_data[12..], [1; 33]);

        let mut buffer = vec![1, 0, 0, 0, 9, 9, 9, 9, 9, 9, 9, 9];
        rebase_to_slot_zero(&UPGRADEABLE_LOADER_ID, &mut buffer, 0);
        assert_eq!(buffer[4..12], [9; 8]);

        let mut v4 = 555u64.to_le_bytes().to_vec();
        v4.extend_from_slice(&[2; 40]);
        rebase_to_slot_zero(&LOADER_V4_ID, &mut v4, 0);
        assert_eq!(v4[..8], [0; 8]);

        let mut table = vec![1, 0, 0, 0];
        table.extend_from_slice(&u64::MAX.to_le_bytes());
        table.extend_from_slice(&900u64.to_le_bytes());
        table.extend_from_slice(&[3; 20]);
        rebase_to_slot_zero(&LOOKUP_TABLE_ID, &mut table, 0);
        assert_eq!(table[4..12], u64::MAX.to_le_bytes());
        assert_eq!(table[12..20], [0; 8]);
        table[4..12].copy_from_slice(&800u64.to_le_bytes());
        rebase_to_slot_zero(&LOOKUP_TABLE_ID, &mut table, 0);
        assert_eq!(table[4..12], [0; 8]);

        let mut stake = vec![0u8; 200];
        stake[..4].copy_from_slice(&2u32.to_le_bytes());
        stake[164..172].copy_from_slice(&40u64.to_le_bytes());
        stake[172..180].copy_from_slice(&u64::MAX.to_le_bytes());
        rebase_to_slot_zero(&STAKE_ID, &mut stake, 0);
        assert_eq!(stake[164..172], [0; 8]);
        assert_eq!(stake[172..180], u64::MAX.to_le_bytes());

        let mut token = vec![5u8; 165];
        rebase_to_slot_zero(&TOKEN_PROGRAM_ID, &mut token, 0);
        assert_eq!(token, vec![5u8; 165]);
    }

    #[test]
    fn a_carried_program_data_account_starts_at_slot_zero() {
        let mut data = vec![3, 0, 0, 0];
        data.extend_from_slice(&777u64.to_le_bytes());
        let program_data = Account {
            pubkey: Pubkey::new_from_array([41u8; 32]),
            lamports: 9_000,
            owner: UPGRADEABLE_LOADER_ID,
            data,
            executable: false,
        };
        let mut state = a_state();
        carry(&mut state, &program_data);
        let accounts = accounts(&params(), Some(&state), &[]).unwrap();
        assert_eq!(find(&accounts, &program_data.pubkey).data[4..12], [0; 8]);
    }

    fn a_vote_v4(voters: &[(u64, [u8; 32])], votes: usize) -> Vec<u8> {
        let mut data = 3u32.to_le_bytes().to_vec();
        for byte in [1u8, 2, 3, 4] {
            data.extend_from_slice(&[byte; 32]);
        }
        data.extend_from_slice(&500u16.to_le_bytes());
        data.extend_from_slice(&700u16.to_le_bytes());
        data.extend_from_slice(&9u64.to_le_bytes());
        data.push(0);
        data.extend_from_slice(&(votes as u64).to_le_bytes());
        for slot in 0..votes {
            data.push(1);
            data.extend_from_slice(&(1_000 + slot as u64).to_le_bytes());
            data.extend_from_slice(&3u32.to_le_bytes());
        }
        data.push(1);
        data.extend_from_slice(&990u64.to_le_bytes());
        data.extend_from_slice(&(voters.len() as u64).to_le_bytes());
        for (epoch, voter) in voters {
            data.extend_from_slice(&epoch.to_le_bytes());
            data.extend_from_slice(voter);
        }
        data.extend_from_slice(&1u64.to_le_bytes());
        data.extend_from_slice(&[7u8; 24]);
        data.extend_from_slice(&[8u8; 16]);
        data.resize(3762, 0);
        data
    }

    #[test]
    fn a_carried_vote_keeps_its_keys_and_drops_its_history() {
        let old = a_vote_v4(&[(40, [5u8; 32]), (41, [6u8; 32])], 31);
        let mut data = old.clone();
        rebase_to_slot_zero(&VOTE_ID, &mut data, 41);
        assert_eq!(data.len(), 3762);
        let prefix = 4 + 32 * 4 + 2 + 2 + 8 + 1;
        assert_eq!(data[..prefix], old[..prefix]);
        let mut want = old[..prefix].to_vec();
        want.extend_from_slice(&0u64.to_le_bytes());
        want.push(0);
        want.extend_from_slice(&1u64.to_le_bytes());
        want.extend_from_slice(&0u64.to_le_bytes());
        want.extend_from_slice(&[6u8; 32]);
        want.extend_from_slice(&0u64.to_le_bytes());
        want.extend_from_slice(&[0u8; 16]);
        want.resize(3762, 0);
        assert_eq!(data, want);
        assert_eq!(reset_vote(&[0, 0, 0, 0], 0), None);
    }

    #[test]
    fn a_v3_vote_gets_empty_prior_voters() {
        let mut data = 2u32.to_le_bytes().to_vec();
        data.extend_from_slice(&[1u8; 32]);
        data.extend_from_slice(&[2u8; 32]);
        data.push(10);
        data.extend_from_slice(&0u64.to_le_bytes());
        data.push(0);
        data.extend_from_slice(&1u64.to_le_bytes());
        data.extend_from_slice(&7u64.to_le_bytes());
        data.extend_from_slice(&[6u8; 32]);
        data.resize(3762, 0xaa);
        let fresh = reset_vote(&data, 7).unwrap();
        assert_eq!(fresh[..69], data[..69]);
        let prior = 69 + 8 + 1 + 8 + 8 + 32;
        assert_eq!(fresh[77], 0);
        assert_eq!(fresh[78..86], 1u64.to_le_bytes());
        assert_eq!(fresh[94..126], [6u8; 32]);
        assert_eq!(
            fresh[prior + 32 * 48..prior + 32 * 48 + 9],
            [31, 0, 0, 0, 0, 0, 0, 0, 1]
        );
    }

    #[test]
    fn the_lockup_keeps_the_epochs_that_it_has_left() {
        for tag in [1u32, 2] {
            let mut stake = vec![0u8; 200];
            stake[..4].copy_from_slice(&tag.to_le_bytes());
            stake[76..84].copy_from_slice(&1_700_000_000i64.to_le_bytes());
            stake[84..92].copy_from_slice(&50u64.to_le_bytes());
            rebase_to_slot_zero(&STAKE_ID, &mut stake, 40);
            assert_eq!(stake[84..92], 10u64.to_le_bytes());
            assert_eq!(stake[76..84], 1_700_000_000i64.to_le_bytes());
            stake[84..92].copy_from_slice(&30u64.to_le_bytes());
            rebase_to_slot_zero(&STAKE_ID, &mut stake, 40);
            assert_eq!(stake[84..92], [0; 8]);
        }
    }

    #[test]
    fn the_voter_of_the_halt_epoch_holds_epoch_zero() {
        let current = [5u8; 32];
        let future = [6u8; 32];
        let old = a_vote_v4(&[(40, current), (42, future)], 3);
        let mut data = old.clone();
        rebase_to_slot_zero(&VOTE_ID, &mut data, 41);
        let at = 4 + 32 * 4 + 2 + 2 + 8 + 1 + 8 + 1;
        assert_eq!(data[at..at + 8], 2u64.to_le_bytes());
        assert_eq!(data[at + 8..at + 16], 0u64.to_le_bytes());
        assert_eq!(data[at + 16..at + 48], current);
        assert_eq!(data[at + 48..at + 56], 1u64.to_le_bytes());
        assert_eq!(data[at + 56..at + 88], future);
        let before = rebase_voters(&[(43, current.to_vec())], 41).unwrap();
        assert_eq!(before, vec![(0, current.to_vec()), (2, current.to_vec())]);
    }

    fn a_fee_mint(older: u64, newer: u64) -> Vec<u8> {
        let mut data = vec![0u8; 165];
        data.push(1);
        // A mint close authority before the fee config.
        data.extend_from_slice(&3u16.to_le_bytes());
        data.extend_from_slice(&32u16.to_le_bytes());
        data.extend_from_slice(&[9u8; 32]);
        data.extend_from_slice(&1u16.to_le_bytes());
        data.extend_from_slice(&108u16.to_le_bytes());
        data.extend_from_slice(&[1u8; 64]);
        data.extend_from_slice(&77u64.to_le_bytes());
        for epoch in [older, newer] {
            data.extend_from_slice(&epoch.to_le_bytes());
            data.extend_from_slice(&500u64.to_le_bytes());
            data.extend_from_slice(&25u16.to_le_bytes());
        }
        data
    }

    #[test]
    fn transfer_fee_epochs_keep_their_distance_to_the_halt() {
        let mut data = a_fee_mint(40, 50);
        let before = data.clone();
        rebase_to_slot_zero(&TOKEN_2022_ID, &mut data, 45);
        let value = 166 + 4 + 32 + 4;
        assert_eq!(data[value + 72..value + 80], 0u64.to_le_bytes());
        assert_eq!(data[value + 90..value + 98], 5u64.to_le_bytes());
        assert_eq!(data[..value + 72], before[..value + 72]);
        assert_eq!(data[value + 80..value + 90], before[value + 80..value + 90]);
        // A list that does not decode to the end leaves the mint as it is.
        let mut torn = a_fee_mint(40, 50);
        torn.extend_from_slice(&[7u8, 0, 9]);
        let kept = torn.clone();
        rebase_to_slot_zero(&TOKEN_2022_ID, &mut torn, 45);
        assert_eq!(torn, kept);
        // A multisig holds signer keys at byte 165, and it keeps them.
        let mut multisig = vec![3u8; 355];
        multisig[165..170].copy_from_slice(&[1, 1, 0, 108, 0]);
        let kept = multisig.clone();
        rebase_to_slot_zero(&TOKEN_2022_ID, &mut multisig, 45);
        assert_eq!(multisig, kept);
        // A token account of Token-2022 keeps its bytes.
        let mut account = a_fee_mint(40, 50);
        account[165] = 2;
        let kept = account.clone();
        rebase_to_slot_zero(&TOKEN_2022_ID, &mut account, 45);
        assert_eq!(account, kept);
    }

    #[test]
    fn a_mint_with_the_two_byte_end_tag_gets_its_epochs_rebased() {
        let mut data = vec![0u8; 165];
        data.push(1);
        // TransferFeeConfig.
        data.extend_from_slice(&1u16.to_le_bytes());
        data.extend_from_slice(&108u16.to_le_bytes());
        let fee = data.len();
        data.extend_from_slice(&[1u8; 64]);
        data.extend_from_slice(&77u64.to_le_bytes());
        for epoch in [40u64, 50] {
            data.extend_from_slice(&epoch.to_le_bytes());
            data.extend_from_slice(&500u64.to_le_bytes());
            data.extend_from_slice(&25u16.to_le_bytes());
        }
        // MintCloseAuthority, PermanentDelegate, and DefaultAccountState.
        for (kind, len) in [(3u16, 32u16), (12, 32), (6, 1)] {
            data.extend_from_slice(&kind.to_le_bytes());
            data.extend_from_slice(&len.to_le_bytes());
            data.extend(std::iter::repeat_n(9u8, usize::from(len)));
        }
        assert_eq!(data.len(), 355);
        data.extend_from_slice(&[0, 0]);
        rebase_to_slot_zero(&TOKEN_2022_ID, &mut data, 40);
        assert_eq!(data[fee + 72..fee + 80], 0u64.to_le_bytes());
        assert_eq!(data[fee + 90..fee + 98], 10u64.to_le_bytes());
        // Two nonzero bytes at the end are no end tag.
        let mut torn = data.clone();
        torn[355..].copy_from_slice(&[1, 0]);
        torn[fee + 90..fee + 98].copy_from_slice(&50u64.to_le_bytes());
        rebase_to_slot_zero(&TOKEN_2022_ID, &mut torn, 40);
        assert_eq!(torn[fee + 90..fee + 98], 50u64.to_le_bytes());
    }
}
