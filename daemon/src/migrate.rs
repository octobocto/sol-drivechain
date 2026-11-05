//! Reads the state of a halted chain for the genesis of its successor: the
//! system balances, every account that carries over byte for byte, and the
//! cursor of the bridge.
//!
//! No lamport of a user stays behind, and no user can stop the export. Each
//! account carries over byte for byte, except the stake and vote accounts,
//! which turn into lamports for their withdrawer, and the accounts that the
//! new genesis makes again.

use std::collections::BTreeMap;

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use solana_commitment_config::CommitmentConfig;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_rpc_client_api::request::RpcRequest;
use solana_sdk::pubkey::Pubkey;

use crate::bridge::{self, Config, WithdrawalRecord};

/// The state of a halted chain at one slot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    pub slot: u64,
    pub cursor: Cursor,
    /// The lamports of each system account without data, the vault and the
    /// treasury too, plus the lamports of each stake and vote account that
    /// names it as withdrawer.
    pub balances: BTreeMap<String, u64>,
    /// The accounts that carry over byte for byte: the withdrawal records,
    /// the nonce accounts, and the token and DEX accounts.
    pub accounts: Vec<CarriedAccount>,
    /// The total supply of the old chain.
    pub supply_lamports: u64,
    /// The lamports of the programs, the sysvars, and the other accounts that
    /// the new genesis makes again. Nobody can spend them.
    pub fixed_lamports: u64,
    /// The lamports of accounts whose owner is no program. No program can
    /// take lamports out of them, so nobody can spend them.
    #[serde(default)]
    pub ownerless_lamports: u64,
    /// The epoch of the old chain at the halt.
    #[serde(default)]
    pub epoch: u64,
}

/// The counters of the bridge config that carry over.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    pub pegged_lamports: u64,
    pub deposit_high_water: u64,
    pub withdrawal_count: u64,
    pub bmm_next_height: u64,
    pub bmm_paid_total: u64,
    pub credited_height: u64,
    /// The eCash block at `credited_height`, in the internal byte order.
    pub credited_block: [u8; 32],
    pub credit_index: u64,
    /// The eCash block of the parts so far at `credited_height + 1`.
    #[serde(default)]
    pub credit_part_block: [u8; 32],
    pub stranded_lamports: u64,
    pub vault_supply: u64,
    pub float_lamports: u64,
    pub paid_through: u64,
    pub bundled_through: u64,
    pub bundle_sealed_height: u64,
    /// The M6ID of the live bundle, in the internal byte order.
    /// Zero without a live bundle.
    #[serde(default)]
    pub bundle_m6id: [u8; 32],
}

/// One account that carries over byte for byte.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CarriedAccount {
    pub pubkey: String,
    pub lamports: u64,
    pub owner: String,
    /// The account data in base64.
    pub data: String,
    /// A program account of a loader that the new genesis does not make.
    #[serde(default)]
    pub executable: bool,
}

/// What the export does with the accounts of one owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Rule {
    /// Carry each account byte for byte.
    Carry,
    /// Give the lamports to the withdrawer of a stake account.
    StakeToWithdrawer,
    /// Give the lamports to the withdrawer of a vote account.
    VoteToWithdrawer,
    /// The new genesis makes these accounts again, and nobody can spend them.
    Fixed,
}

/// The owners with a rule other than `Carry`. The export also reads the
/// accounts of each program that it finds, and it carries them.
const OWNER_RULES: &[(&str, Rule)] = &[
    (
        "Stake11111111111111111111111111111111111111",
        Rule::StakeToWithdrawer,
    ),
    (
        "Vote111111111111111111111111111111111111111",
        Rule::VoteToWithdrawer,
    ),
    ("Sysvar1111111111111111111111111111111111111", Rule::Fixed),
    ("NativeLoader1111111111111111111111111111111", Rule::Fixed),
    ("Feature111111111111111111111111111111111111", Rule::Fixed),
];

/// The keys whose account the new genesis makes again, whatever the owner:
/// the stake config that `solana-genesis` writes, and the programs that
/// `build-genesis.sh` loads with `--bpf-program`. The bridge program joins
/// them in the export.
const GENESIS_KEYS: &[&str] = &[
    "StakeConfig11111111111111111111111111111111",
    "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
    "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL",
    "MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr",
    "CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C",
];

/// The owners that the export reads first. The programs that it finds there
/// lead to the rest.
/// The builtins come first by name, because a loader in Agave 4 can hold a
/// program account without the executable flag.
const ROOT_OWNERS: &[&str] = &[
    "Stake11111111111111111111111111111111111111",
    "Vote111111111111111111111111111111111111111",
    "Config1111111111111111111111111111111111111",
    "AddressLookupTab1e1111111111111111111111111",
    "ZkE1Gama1Proof11111111111111111111111111111",
    "NativeLoader1111111111111111111111111111111",
    "Sysvar1111111111111111111111111111111111111",
    "Feature111111111111111111111111111111111111",
    "BPFLoader1111111111111111111111111111111111",
    "BPFLoader2111111111111111111111111111111111",
    "BPFLoaderUpgradeab1e11111111111111111111111",
    "LoaderV411111111111111111111111111111111111",
];

/// The builtins that the native loader holds.
const BUILTINS: &[&str] = &[
    "11111111111111111111111111111111",
    "Stake11111111111111111111111111111111111111",
    "Vote111111111111111111111111111111111111111",
    "Config1111111111111111111111111111111111111",
    "BPFLoader1111111111111111111111111111111111",
    "BPFLoader2111111111111111111111111111111111",
    "BPFLoaderUpgradeab1e11111111111111111111111",
    "LoaderV411111111111111111111111111111111111",
    "AddressLookupTab1e1111111111111111111111111",
    "ComputeBudget111111111111111111111111111111",
    "ZkE1Gama1Proof11111111111111111111111111111",
    "Ed25519SigVerify111111111111111111111111111",
    "KeccakSecp256k11111111111111111111111111111",
    "Secp256r1SigVerify1111111111111111111111111",
    "Feature111111111111111111111111111111111111",
];

const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];

/// The programs among the accounts of `owner`. Anyone can give an empty
/// account to a loader, so a loader account counts only when it holds a real
/// program: each one costs a full scan of the accounts.
fn program_ids(owner: &Pubkey, accounts: &[RawAccount]) -> Vec<Pubkey> {
    let name = owner.to_string();
    let program_data: std::collections::BTreeSet<Pubkey> = accounts
        .iter()
        .filter(|account| account.data.get(..4) == Some(&3u32.to_le_bytes()[..]))
        .map(|account| account.pubkey)
        .collect();
    accounts
        .iter()
        .filter(|account| match name.as_str() {
            "NativeLoader1111111111111111111111111111111" => {
                account.executable || BUILTINS.contains(&account.pubkey.to_string().as_str())
            }
            "BPFLoader1111111111111111111111111111111111"
            | "BPFLoader2111111111111111111111111111111111" => {
                account.data.get(..4) == Some(&ELF_MAGIC[..])
            }
            // The slot, the authority, and a status of retracted (0),
            // deployed (1), or finalized (2). Only the loader writes this
            // state, and a retracted program can deploy again.
            "LoaderV411111111111111111111111111111111111" => {
                let status = account
                    .data
                    .get(40..48)
                    .and_then(|status| status.try_into().ok())
                    .map(u64::from_le_bytes);
                let authority = account.data.get(8..40);
                match status {
                    Some(2) => true,
                    Some(0 | 1) => authority.is_some_and(|key| key.iter().any(|byte| *byte != 0)),
                    _ => false,
                }
            }
            // A `Program` state whose ProgramData account exists.
            "BPFLoaderUpgradeab1e11111111111111111111111" => {
                account.data.get(..4) == Some(&2u32.to_le_bytes()[..])
                    && account
                        .data
                        .get(4..36)
                        .and_then(|key| Pubkey::try_from(key).ok())
                        .is_some_and(|key| program_data.contains(&key))
            }
            _ => account.executable,
        })
        .map(|account| account.pubkey)
        .collect()
}

fn rule_of(owner: &Pubkey) -> Rule {
    OWNER_RULES
        .iter()
        .find(|(name, _)| owner.to_string() == *name)
        .map_or(Rule::Carry, |(_, rule)| *rule)
}

#[derive(Debug, thiserror::Error)]
pub enum MigrateError {
    #[error("the Solana rpc call `{call}` failed")]
    Rpc {
        call: &'static str,
        #[source]
        source: solana_rpc_client_api::client_error::Error,
    },
    #[error("the rpc answer to `{0}` has an unknown form")]
    BadAnswer(&'static str),
    #[error(
        "the chain moved from slot {first} to slot {last} during the read. Halt it first: start \
         the validator with HALT=1"
    )]
    NotHalted { first: u64, last: u64 },
    #[error("the bridge holds no config account")]
    NoConfig,
    #[error(transparent)]
    Bridge(#[from] bridge::BridgeError),
    #[error(
        "the supply is {supply} lamports, and the accounts that the migration reads hold \
         {found}, which is more"
    )]
    Overcounted { supply: u64, found: u64 },
    #[error("the lamports overflow a u64")]
    Overflow,
    #[error("the live bundle holds record {0}, and the chain holds no such record")]
    MissingRecord(u64),
    #[error(transparent)]
    M6(#[from] crate::m6::M6Error),
    #[error(
        "accounts whose owner is no program hold {found} lamports, and the export cannot carry \
         them. Pass --accept-ownerless-lamports {found} to leave them behind"
    )]
    OwnerlessLamports { found: u64, accepted: Option<u64> },
}

/// One account of a `getProgramAccounts` answer.
struct RawAccount {
    pubkey: Pubkey,
    lamports: u64,
    data: Vec<u8>,
    executable: bool,
}

/// Reads the state of a halted chain. Every read must see the same slot.
/// `accept_ownerless` must name the exact ownerless lamports, or the export
/// stops when there are any.
pub async fn export(
    rpc: &RpcClient,
    program_id: &Pubkey,
    accept_ownerless: Option<u64>,
) -> Result<State, MigrateError> {
    let first = slot(rpc).await?;
    let epoch = rpc
        .get_epoch_info_with_commitment(CommitmentConfig::processed())
        .await
        .map_err(|source| MigrateError::Rpc {
            call: "getEpochInfo",
            source,
        })?
        .epoch;
    let mut slots = Vec::new();
    let (seen, bridge_accounts) = program_accounts(rpc, program_id).await?;
    slots.push(seen);
    let (seen, system_accounts) = program_accounts(rpc, &bridge::SYSTEM_PROGRAM_ID).await?;
    slots.push(seen);
    // Each program owns its accounts, and a loader or the native loader owns
    // each program, so a walk from the loaders reaches every program.
    let mut owned = Vec::new();
    let mut seen_owners: std::collections::BTreeSet<Pubkey> =
        [*program_id, bridge::SYSTEM_PROGRAM_ID]
            .into_iter()
            .collect();
    let mut queue: Vec<Pubkey> = ROOT_OWNERS
        .iter()
        .map(|name| name.parse().map_err(|_| MigrateError::BadAnswer("owner")))
        .collect::<Result<_, _>>()?;
    for account in system_accounts.iter().chain(&bridge_accounts) {
        if account.executable {
            queue.push(account.pubkey);
        }
    }
    while let Some(owner) = queue.pop() {
        if !seen_owners.insert(owner) {
            continue;
        }
        let (seen, accounts) = program_accounts(rpc, &owner).await?;
        slots.push(seen);
        queue.extend(program_ids(&owner, &accounts));
        owned.push((owner, rule_of(&owner), accounts));
    }
    let (seen, supply_lamports) = supply(rpc).await?;
    slots.push(seen);
    slots.push(slot(rpc).await?);
    if slots.iter().any(|seen| *seen != first) {
        return Err(MigrateError::NotHalted {
            first,
            last: slots.into_iter().max().unwrap_or(first),
        });
    }

    let config_key = bridge::config_pda(program_id).0;
    let config = bridge_accounts
        .iter()
        .find(|account| account.pubkey == config_key)
        .ok_or(MigrateError::NoConfig)?;
    let config_lamports = config.lamports;
    let config = Config::decode_any(&config.data)?;
    let cursor = Cursor {
        pegged_lamports: config.pegged_lamports,
        deposit_high_water: config.deposit_high_water,
        withdrawal_count: config.withdrawal_count,
        bmm_next_height: config.bmm_next_height,
        bmm_paid_total: config.bmm_paid_total,
        credited_height: config.credited_height,
        credited_block: config.credited_block,
        credit_index: config.credit_index,
        credit_part_block: config.credit_part_block,
        stranded_lamports: config.stranded_lamports,
        vault_supply: config.vault_supply,
        float_lamports: config.float_lamports,
        paid_through: config.paid_through,
        bundled_through: config.bundled_through,
        bundle_sealed_height: config.bundle_sealed_height,
        bundle_m6id: live_m6id(program_id, config.live_bundle(), &bridge_accounts)?,
    };

    let mut carried = Carried {
        fixed: config_lamports,
        read: config_lamports,
        bridge_program: Some(*program_id),
        ..Carried::default()
    };
    for account in bridge_accounts {
        if account.pubkey == config_key {
            continue;
        }
        // A record carries, and so does any other account of the bridge: the
        // program never reads it, so it stays inert.
        if let Ok(record) = WithdrawalRecord::decode(&account.data) {
            tracing::info!(
                index = record.index,
                burned = record.burned_lamports,
                "the old chain holds a withdrawal record"
            );
        }
        carried.carry(account, *program_id);
    }
    for account in system_accounts {
        carried.system(account);
    }
    for (owner, rule, accounts) in owned {
        for account in accounts {
            carried.apply(owner, rule, account);
        }
    }
    let mut state = carried.finish(first, cursor, supply_lamports)?;
    accept_ownerless_lamports(state.ownerless_lamports, accept_ownerless)?;
    state.epoch = epoch;
    Ok(state)
}

/// The M6ID of the bundle of the records in `live`, from `accounts`, or zero
/// without a live bundle.
fn live_m6id(
    program_id: &Pubkey,
    live: Option<std::ops::Range<u64>>,
    accounts: &[RawAccount],
) -> Result<[u8; 32], MigrateError> {
    use bitcoin::hashes::Hash as _;

    let Some(live) = live else {
        return Ok([0u8; 32]);
    };
    let by_key: BTreeMap<Pubkey, &RawAccount> = accounts
        .iter()
        .map(|account| (account.pubkey, account))
        .collect();
    let records = live
        .map(|index| {
            by_key
                .get(&bridge::withdrawal_pda(program_id, index).0)
                .and_then(|account| WithdrawalRecord::decode(&account.data).ok())
                .filter(|record| record.index == index)
                .ok_or(MigrateError::MissingRecord(index))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(crate::m6::m6id(&crate::peg::bundle_of(&records)?).to_byte_array())
}

/// The accounts that the export collects, sorted by what happens to them.
#[derive(Default)]
struct Carried {
    balances: BTreeMap<Pubkey, u64>,
    accounts: Vec<CarriedAccount>,
    /// The stake and vote accounts, by withdrawer, with their owner.
    converted: BTreeMap<Pubkey, Vec<(Pubkey, RawAccount)>>,
    fixed: u64,
    /// Every lamport that the export read.
    read: u64,
    /// The bridge program, which the new genesis loads again.
    bridge_program: Option<Pubkey>,
}

impl Carried {
    fn system(&mut self, account: RawAccount) {
        if account.data.is_empty() && !account.executable {
            self.read = self.read.saturating_add(account.lamports);
            if account.lamports > 0 {
                self.balances.insert(account.pubkey, account.lamports);
            }
        } else {
            self.carry(account, bridge::SYSTEM_PROGRAM_ID);
        }
    }

    fn carry(&mut self, account: RawAccount, owner: Pubkey) {
        self.read = self.read.saturating_add(account.lamports);
        self.accounts.push(CarriedAccount {
            pubkey: account.pubkey.to_string(),
            lamports: account.lamports,
            owner: owner.to_string(),
            data: base64::engine::general_purpose::STANDARD.encode(&account.data),
            executable: account.executable,
        });
    }

    fn apply(&mut self, owner: Pubkey, rule: Rule, account: RawAccount) {
        let rule = if GENESIS_KEYS.contains(&account.pubkey.to_string().as_str())
            || self.bridge_program == Some(account.pubkey)
        {
            Rule::Fixed
        } else {
            rule
        };
        match rule {
            Rule::Carry => self.carry(account, owner),
            Rule::Fixed => {
                self.read = self.read.saturating_add(account.lamports);
                self.fixed = self.fixed.saturating_add(account.lamports);
            }
            Rule::StakeToWithdrawer | Rule::VoteToWithdrawer => {
                let withdrawer = if rule == Rule::StakeToWithdrawer {
                    stake_withdrawer(&account.data)
                } else {
                    vote_withdrawer(&account.data)
                };
                match withdrawer {
                    Some(withdrawer) => self
                        .converted
                        .entry(withdrawer)
                        .or_default()
                        .push((owner, account)),
                    // No readable withdrawer, so the account stays as it is.
                    None => self.carry(account, owner),
                }
            }
        }
    }

    fn finish(
        mut self,
        slot: u64,
        cursor: Cursor,
        supply_lamports: u64,
    ) -> Result<State, MigrateError> {
        // Only a key that the new genesis writes as a plain system account
        // takes plain lamports. Every other key is known before any
        // conversion, so the result does not depend on the key order.
        let converted = std::mem::take(&mut self.converted);
        let not_plain: std::collections::BTreeSet<String> = self
            .accounts
            .iter()
            .map(|account| account.pubkey.clone())
            .chain(
                converted
                    .values()
                    .flatten()
                    .map(|(_, account)| account.pubkey.to_string()),
            )
            .collect();
        for (withdrawer, sources) in converted {
            if not_plain.contains(&withdrawer.to_string()) {
                for (owner, account) in sources {
                    self.carry(account, owner);
                }
                continue;
            }
            for (_, account) in sources {
                tracing::info!(
                    account = %account.pubkey,
                    %withdrawer,
                    lamports = account.lamports,
                    "a stake or vote account turns into lamports for its withdrawer"
                );
                self.read = self.read.saturating_add(account.lamports);
                let entry = self.balances.entry(withdrawer).or_default();
                *entry = entry
                    .checked_add(account.lamports)
                    .ok_or(MigrateError::Overflow)?;
            }
        }
        let ownerless_lamports =
            supply_lamports
                .checked_sub(self.read)
                .ok_or(MigrateError::Overcounted {
                    supply: supply_lamports,
                    found: self.read,
                })?;
        if ownerless_lamports > 0 {
            tracing::warn!(
                ownerless_lamports,
                "accounts whose owner is no program hold lamports that nobody can spend, and they \
                 stay behind"
            );
        }
        Ok(State {
            slot,
            cursor,
            balances: self
                .balances
                .into_iter()
                .map(|(pubkey, lamports)| (pubkey.to_string(), lamports))
                .collect(),
            accounts: self.accounts,
            supply_lamports,
            fixed_lamports: self.fixed,
            ownerless_lamports,
            epoch: 0,
        })
    }
}

/// Lets the export go on only when no lamport is ownerless, or when the
/// operator names their exact count.
pub fn accept_ownerless_lamports(found: u64, accepted: Option<u64>) -> Result<(), MigrateError> {
    if found == 0 || accepted == Some(found) {
        return Ok(());
    }
    Err(MigrateError::OwnerlessLamports { found, accepted })
}

/// The withdrawer of a stake account: `Initialized` or `Stake`, after the
/// tag, the rent reserve, and the staker.
pub fn stake_withdrawer(data: &[u8]) -> Option<Pubkey> {
    let tag = u32::from_le_bytes(data.get(..4)?.try_into().ok()?);
    if !(1..=2).contains(&tag) {
        return None;
    }
    Some(Pubkey::new_from_array(data.get(44..76)?.try_into().ok()?))
}

/// The withdrawer of a vote account. Each version since 1.14.11 keeps it
/// after the tag and the node pubkey.
pub fn vote_withdrawer(data: &[u8]) -> Option<Pubkey> {
    let tag = u32::from_le_bytes(data.get(..4)?.try_into().ok()?);
    if !(1..=3).contains(&tag) {
        return None;
    }
    Some(Pubkey::new_from_array(data.get(36..68)?.try_into().ok()?))
}

async fn slot(rpc: &RpcClient) -> Result<u64, MigrateError> {
    rpc.get_slot_with_commitment(CommitmentConfig::processed())
        .await
        .map_err(|source| MigrateError::Rpc {
            call: "getSlot",
            source,
        })
}

async fn supply(rpc: &RpcClient) -> Result<(u64, u64), MigrateError> {
    let answer: Value = rpc
        .send(
            RpcRequest::GetSupply,
            json!([{ "commitment": "processed", "excludeNonCirculatingAccountsList": true }]),
        )
        .await
        .map_err(|source| MigrateError::Rpc {
            call: "getSupply",
            source,
        })?;
    let slot = answer["context"]["slot"]
        .as_u64()
        .ok_or(MigrateError::BadAnswer("getSupply"))?;
    let total = answer["value"]["total"]
        .as_u64()
        .ok_or(MigrateError::BadAnswer("getSupply"))?;
    Ok((slot, total))
}

/// The accounts of a program and the slot of the read.
async fn program_accounts(
    rpc: &RpcClient,
    program_id: &Pubkey,
) -> Result<(u64, Vec<RawAccount>), MigrateError> {
    let config = json!({
        "commitment": "processed",
        "encoding": "base64",
        "withContext": true,
    });
    let answer: Value = rpc
        .send(
            RpcRequest::GetProgramAccounts,
            json!([program_id.to_string(), config]),
        )
        .await
        .map_err(|source| MigrateError::Rpc {
            call: "getProgramAccounts",
            source,
        })?;
    let bad = || MigrateError::BadAnswer("getProgramAccounts");
    let slot = answer["context"]["slot"].as_u64().ok_or_else(bad)?;
    let list = answer["value"].as_array().ok_or_else(bad)?;
    let mut accounts = Vec::with_capacity(list.len());
    for item in list {
        let pubkey: Pubkey = item["pubkey"]
            .as_str()
            .and_then(|text| text.parse().ok())
            .ok_or_else(bad)?;
        let account = &item["account"];
        let lamports = account["lamports"].as_u64().ok_or_else(bad)?;
        let executable = account["executable"].as_bool().ok_or_else(bad)?;
        let data = account["data"][0]
            .as_str()
            .and_then(|text| base64::engine::general_purpose::STANDARD.decode(text).ok())
            .ok_or_else(bad)?;
        accounts.push(RawAccount {
            pubkey,
            lamports,
            data,
            executable,
        });
    }
    Ok((slot, accounts))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(byte: u8, lamports: u64, data: Vec<u8>) -> RawAccount {
        RawAccount {
            pubkey: Pubkey::new_from_array([byte; 32]),
            lamports,
            data,
            executable: false,
        }
    }

    fn a_cursor() -> Cursor {
        Cursor {
            pegged_lamports: 1,
            deposit_high_water: 2,
            withdrawal_count: 3,
            bmm_next_height: 4,
            bmm_paid_total: 5,
            credited_height: 6,
            credited_block: [7u8; 32],
            credit_index: 0,
            credit_part_block: [0u8; 32],
            stranded_lamports: 8,
            vault_supply: 9,
            float_lamports: 10,
            paid_through: 1,
            bundled_through: 2,
            bundle_sealed_height: 3,
            bundle_m6id: [4u8; 32],
        }
    }

    fn a_stake_account(withdrawer: Pubkey) -> Vec<u8> {
        let mut data = vec![0u8; 200];
        data[..4].copy_from_slice(&2u32.to_le_bytes());
        data[12..44].copy_from_slice(&[0xaa; 32]);
        data[44..76].copy_from_slice(withdrawer.as_ref());
        data
    }

    fn a_vote_account(withdrawer: Pubkey) -> Vec<u8> {
        let mut data = vec![0u8; 3762];
        data[..4].copy_from_slice(&3u32.to_le_bytes());
        data[4..36].copy_from_slice(&[0xbb; 32]);
        data[36..68].copy_from_slice(withdrawer.as_ref());
        data
    }

    fn owner(name: &str) -> Pubkey {
        name.parse().unwrap()
    }

    #[test]
    fn stake_and_vote_lamports_go_to_the_withdrawer() {
        let withdrawer = Pubkey::new_from_array([4u8; 32]);
        let mut carried = Carried::default();
        carried.system(raw(4, 1_000, Vec::new()));
        carried.apply(
            owner("Stake11111111111111111111111111111111111111"),
            Rule::StakeToWithdrawer,
            raw(5, 300, a_stake_account(withdrawer)),
        );
        carried.apply(
            owner("Vote111111111111111111111111111111111111111"),
            Rule::VoteToWithdrawer,
            raw(6, 20, a_vote_account(withdrawer)),
        );
        let state = carried.finish(1, a_cursor(), 1_320).unwrap();
        assert_eq!(state.balances[&withdrawer.to_string()], 1_320);
        assert!(state.accounts.is_empty());
    }

    #[test]
    fn token_and_nonce_accounts_carry_byte_for_byte() {
        let mut carried = Carried::default();
        carried.system(raw(1, 1_500, vec![1u8; 80]));
        let token = owner("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
        carried.apply(token, Rule::Carry, raw(2, 2_344, vec![2u8; 165]));
        let state = carried.finish(1, a_cursor(), 3_844).unwrap();
        assert_eq!(state.accounts.len(), 2);
        assert_eq!(
            state.accounts[0].owner,
            bridge::SYSTEM_PROGRAM_ID.to_string()
        );
        assert_eq!(state.accounts[1].owner, token.to_string());
        assert_eq!(state.accounts[1].lamports, 2_344);
        assert!(state.balances.is_empty());
    }

    #[test]
    fn an_empty_account_of_the_bridge_carries_as_it_is() {
        let bridge_id = Pubkey::new_from_array([3u8; 32]);
        let mut carried = Carried::default();
        carried.carry(raw(7, 900, Vec::new()), bridge_id);
        let state = carried.finish(1, a_cursor(), 900).unwrap();
        assert_eq!(state.accounts[0].owner, bridge_id.to_string());
        assert_eq!(state.accounts[0].lamports, 900);
    }

    #[test]
    fn lookup_tables_and_token_2022_accounts_carry() {
        let mut carried = Carried::default();
        for (byte, name) in [
            (1, "AddressLookupTab1e1111111111111111111111111"),
            (2, "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb"),
            (3, "BPFLoaderUpgradeab1e11111111111111111111111"),
        ] {
            let owner = owner(name);
            assert_eq!(rule_of(&owner), Rule::Carry);
            carried.apply(owner, rule_of(&owner), raw(byte, 1_000, vec![byte; 56]));
        }
        let state = carried.finish(1, a_cursor(), 3_000).unwrap();
        assert_eq!(state.accounts.len(), 3);
    }

    #[test]
    fn a_program_account_keeps_its_executable_flag() {
        let mut carried = Carried::default();
        let mut program = raw(4, 1_000, vec![2u8; 36]);
        program.executable = true;
        carried.apply(
            owner("BPFLoaderUpgradeab1e11111111111111111111111"),
            Rule::Carry,
            program,
        );
        let state = carried.finish(1, a_cursor(), 1_000).unwrap();
        assert!(state.accounts[0].executable);
    }

    #[test]
    fn lamports_of_an_ownerless_account_stay_behind_and_nothing_stops() {
        let mut carried = Carried::default();
        carried.system(raw(1, 1_000, Vec::new()));
        carried.apply(
            owner("Sysvar1111111111111111111111111111111111111"),
            Rule::Fixed,
            raw(2, 50, Vec::new()),
        );
        let state = carried.finish(1, a_cursor(), 1_051).unwrap();
        assert_eq!(state.ownerless_lamports, 1);
        assert!(matches!(
            Carried::default()
                .finish(1, a_cursor(), 0)
                .map(|state| state.slot),
            Ok(1)
        ));
        let mut over = Carried::default();
        over.system(raw(1, 10, Vec::new()));
        assert!(matches!(
            over.finish(1, a_cursor(), 9),
            Err(MigrateError::Overcounted { .. })
        ));
    }

    #[test]
    fn ownerless_lamports_stop_the_export_unless_the_operator_names_them() {
        assert!(accept_ownerless_lamports(0, None).is_ok());
        assert!(accept_ownerless_lamports(0, Some(5)).is_ok());
        assert!(matches!(
            accept_ownerless_lamports(51, None),
            Err(MigrateError::OwnerlessLamports {
                found: 51,
                accepted: None
            })
        ));
        assert!(matches!(
            accept_ownerless_lamports(51, Some(50)),
            Err(MigrateError::OwnerlessLamports {
                found: 51,
                accepted: Some(50)
            })
        ));
        assert!(accept_ownerless_lamports(51, Some(51)).is_ok());
    }

    #[test]
    fn every_owner_with_a_rule_is_a_root_of_the_walk() {
        for (name, _) in OWNER_RULES {
            assert!(ROOT_OWNERS.contains(name), "{name}");
        }
        for name in [
            "Config1111111111111111111111111111111111111",
            "AddressLookupTab1e1111111111111111111111111",
            "ZkE1Gama1Proof11111111111111111111111111111",
        ] {
            assert!(ROOT_OWNERS.contains(&name), "{name}");
            assert_eq!(rule_of(&owner(name)), Rule::Carry);
        }
    }

    fn elf(byte: u8) -> RawAccount {
        raw(byte, 1, [&ELF_MAGIC[..], &[0u8; 60]].concat())
    }

    #[test]
    fn a_loader_names_only_its_real_programs() {
        let upgradeable = owner("BPFLoaderUpgradeab1e11111111111111111111111");
        let program_data = raw(2, 1, [&3u32.to_le_bytes()[..], &[0u8; 45]].concat());
        let program = raw(
            1,
            1,
            [
                &2u32.to_le_bytes()[..],
                &Pubkey::new_from_array([2u8; 32]).to_bytes(),
            ]
            .concat(),
        );
        let orphan = raw(
            4,
            1,
            [
                &2u32.to_le_bytes()[..],
                &Pubkey::new_from_array([9u8; 32]).to_bytes(),
            ]
            .concat(),
        );
        let buffer = raw(3, 1, 1u32.to_le_bytes().to_vec());
        assert_eq!(
            program_ids(&upgradeable, &[program, program_data, buffer, orphan]),
            vec![Pubkey::new_from_array([1u8; 32])]
        );

        for name in [
            "BPFLoader1111111111111111111111111111111111",
            "BPFLoader2111111111111111111111111111111111",
        ] {
            assert_eq!(
                program_ids(&owner(name), &[elf(5), raw(6, 1, vec![1, 2, 3, 4])]),
                vec![Pubkey::new_from_array([5u8; 32])]
            );
        }

        let v4 = owner("LoaderV411111111111111111111111111111111111");
        let state = |status: u64, authority: u8| {
            let mut data = vec![0u8; 8];
            data.extend_from_slice(&[authority; 32]);
            data.extend_from_slice(&status.to_le_bytes());
            data.extend_from_slice(&ELF_MAGIC);
            data
        };
        assert_eq!(
            program_ids(
                &v4,
                &[
                    raw(7, 1, state(1, 1)),
                    raw(8, 1, state(2, 0)),
                    raw(9, 1, state(0, 1)),
                    raw(10, 1, state(0, 0)),
                    raw(11, 1, state(3, 1)),
                ]
            ),
            vec![
                Pubkey::new_from_array([7u8; 32]),
                Pubkey::new_from_array([8u8; 32]),
                Pubkey::new_from_array([9u8; 32])
            ]
        );

        let native = owner("NativeLoader1111111111111111111111111111111");
        let mut stake = raw(10, 1, Vec::new());
        stake.pubkey = owner("Stake11111111111111111111111111111111111111");
        let mut flagged = raw(11, 1, Vec::new());
        flagged.executable = true;
        assert_eq!(
            program_ids(&native, &[stake, flagged, raw(12, 1, Vec::new())]),
            vec![
                owner("Stake11111111111111111111111111111111111111"),
                Pubkey::new_from_array([11u8; 32])
            ]
        );

        let token = owner("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
        let mut executable = raw(5, 1, Vec::new());
        executable.executable = true;
        assert_eq!(
            program_ids(&token, &[raw(6, 1, Vec::new()), executable]),
            vec![Pubkey::new_from_array([5u8; 32])]
        );
    }

    #[test]
    fn the_accounts_of_a_retracted_program_carry() {
        let mut retracted = vec![0u8; 8];
        retracted.extend_from_slice(&[4u8; 32]);
        retracted.extend_from_slice(&0u64.to_le_bytes());
        let v4 = owner("LoaderV411111111111111111111111111111111111");
        let program = raw(20, 1_000, retracted);
        let ids = program_ids(&v4, std::slice::from_ref(&program));
        assert_eq!(ids, vec![program.pubkey]);
        let mut carried = Carried::default();
        carried.apply(v4, rule_of(&v4), program);
        carried.apply(ids[0], rule_of(&ids[0]), raw(21, 2_000, vec![1u8; 16]));
        let state = carried.finish(1, a_cursor(), 3_000).unwrap();
        assert_eq!(state.accounts.len(), 2);
        assert_eq!(state.accounts[1].owner, ids[0].to_string());
    }

    #[test]
    fn empty_loader_accounts_send_no_owner_call() {
        for name in [
            "NativeLoader1111111111111111111111111111111",
            "BPFLoader1111111111111111111111111111111111",
            "BPFLoader2111111111111111111111111111111111",
            "BPFLoaderUpgradeab1e11111111111111111111111",
            "LoaderV411111111111111111111111111111111111",
        ] {
            let mut empty: Vec<RawAccount> = (0..1000u32)
                .map(|at| RawAccount {
                    pubkey: Pubkey::new_from_array({
                        let mut key = [0xeeu8; 32];
                        key[..4].copy_from_slice(&at.to_le_bytes());
                        key
                    }),
                    lamports: 1_000,
                    data: Vec::new(),
                    executable: false,
                })
                .collect();
            assert!(program_ids(&owner(name), &empty).is_empty(), "{name}");
            empty.push(elf(5));
            assert!(program_ids(&owner(name), &empty).len() <= 1, "{name}");
        }
    }

    #[test]
    fn a_foreign_loader_2_program_carries_and_the_bridge_does_not() {
        let loader = owner("BPFLoader2111111111111111111111111111111111");
        assert_eq!(rule_of(&loader), Rule::Carry);
        let bridge_id = Pubkey::new_from_array([3u8; 32]);
        let mut carried = Carried {
            bridge_program: Some(bridge_id),
            ..Carried::default()
        };
        let mut foreign = raw(7, 4_000, vec![0x7f, b'E', b'L', b'F']);
        foreign.executable = true;
        let mut bridge = raw(3, 5_000, vec![0x7f, b'E', b'L', b'F']);
        bridge.executable = true;
        let mut token = raw(8, 6_000, vec![0x7f, b'E', b'L', b'F']);
        token.pubkey = owner("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
        token.executable = true;
        for account in [foreign, bridge, token] {
            carried.apply(loader, rule_of(&loader), account);
        }
        let state = carried.finish(1, a_cursor(), 15_000).unwrap();
        assert_eq!(state.accounts.len(), 1);
        assert_eq!(
            state.accounts[0].pubkey,
            Pubkey::new_from_array([7u8; 32]).to_string()
        );
        assert!(state.accounts[0].executable);
        assert_eq!(state.fixed_lamports, 11_000);
    }

    #[test]
    fn the_stake_config_is_made_again_and_other_config_accounts_carry() {
        let config = owner("Config1111111111111111111111111111111111111");
        let mut carried = Carried::default();
        let mut stake_config = raw(1, 552, vec![0u8; 10]);
        stake_config.pubkey = owner("StakeConfig11111111111111111111111111111111");
        carried.apply(config, rule_of(&config), stake_config);
        carried.apply(config, rule_of(&config), raw(2, 900, vec![1u8; 40]));
        let state = carried.finish(1, a_cursor(), 1_452).unwrap();
        assert_eq!(state.fixed_lamports, 552);
        assert_eq!(state.accounts.len(), 1);
        assert_eq!(state.ownerless_lamports, 0);
    }

    #[test]
    fn the_bootstrap_stake_turns_into_lamports_for_the_identity() {
        let identity = Pubkey::new_from_array([4u8; 32]);
        let stake = owner("Stake11111111111111111111111111111111111111");
        let vote = owner("Vote111111111111111111111111111111111111111");
        let mut carried = Carried::default();
        carried.system(raw(4, 2_000_000, Vec::new()));
        carried.apply(
            stake,
            rule_of(&stake),
            raw(5, 100_000, a_stake_account(identity)),
        );
        carried.apply(
            vote,
            rule_of(&vote),
            raw(6, 15_560, a_vote_account(identity)),
        );
        let state = carried.finish(1, a_cursor(), 2_115_560).unwrap();
        assert_eq!(state.balances[&identity.to_string()], 2_115_560);
        assert_eq!(state.ownerless_lamports, 0);
    }

    #[test]
    fn a_stake_account_without_a_withdrawer_carries_as_it_is() {
        let mut carried = Carried::default();
        let stake = owner("Stake11111111111111111111111111111111111111");
        carried.apply(stake, Rule::StakeToWithdrawer, raw(5, 300, vec![0u8; 200]));
        let state = carried.finish(1, a_cursor(), 300).unwrap();
        assert_eq!(state.accounts[0].owner, stake.to_string());
        assert!(state.balances.is_empty());
    }

    #[test]
    fn a_stake_account_of_a_withdrawer_with_data_carries_as_it_is() {
        let withdrawer = Pubkey::new_from_array([1u8; 32]);
        let mut carried = Carried::default();
        carried.system(raw(1, 1_500, vec![1u8; 80]));
        let stake = owner("Stake11111111111111111111111111111111111111");
        carried.apply(
            stake,
            Rule::StakeToWithdrawer,
            raw(5, 300, a_stake_account(withdrawer)),
        );
        let state = carried.finish(1, a_cursor(), 1_800).unwrap();
        assert_eq!(state.accounts.len(), 2);
        assert_eq!(state.accounts[1].owner, stake.to_string());
        assert!(state.balances.is_empty());
    }

    #[test]
    fn a_chain_of_stake_withdrawers_keeps_every_lamport_in_any_key_order() {
        let stake = owner("Stake11111111111111111111111111111111111111");
        for (a, b, c) in [(9u8, 2u8, 5u8), (2, 9, 5), (5, 2, 9), (9, 5, 2)] {
            let key = |byte: u8| Pubkey::new_from_array([byte; 32]);
            let mut carried = Carried::default();
            // C holds data, B names C, and A names B.
            carried.system(raw(c, 1_500, vec![1u8; 80]));
            carried.apply(
                stake,
                Rule::StakeToWithdrawer,
                raw(b, 300, a_stake_account(key(c))),
            );
            carried.apply(
                stake,
                Rule::StakeToWithdrawer,
                raw(a, 200, a_stake_account(key(b))),
            );
            let state = carried.finish(1, a_cursor(), 2_000).unwrap();
            let carried_keys: Vec<String> = state
                .accounts
                .iter()
                .map(|account| account.pubkey.clone())
                .collect();
            for byte in [a, b, c] {
                assert!(carried_keys.contains(&key(byte).to_string()));
            }
            assert!(state.balances.is_empty());
            let total: u64 = state.accounts.iter().map(|account| account.lamports).sum();
            assert_eq!(total, 2_000);
        }
    }

    #[test]
    fn the_withdrawer_offsets_match_the_interface_layouts() {
        let withdrawer = Pubkey::new_from_array([9u8; 32]);
        assert_eq!(
            stake_withdrawer(&a_stake_account(withdrawer)),
            Some(withdrawer)
        );
        assert_eq!(
            vote_withdrawer(&a_vote_account(withdrawer)),
            Some(withdrawer)
        );
        let mut old_vote = a_vote_account(withdrawer);
        old_vote[..4].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(vote_withdrawer(&old_vote), None);
        assert_eq!(stake_withdrawer(&[1, 0, 0, 0]), None);
    }

    fn a_record_raw(program_id: &Pubkey, index: u64) -> RawAccount {
        let mut data = bridge::discriminator("account", "WithdrawalRecord").to_vec();
        data.extend_from_slice(&index.to_le_bytes());
        data.extend_from_slice(&[5u8; 32]);
        for value in [20_000u64, 1_000, 1_000] {
            data.extend_from_slice(&value.to_le_bytes());
        }
        data.extend_from_slice(&22u32.to_le_bytes());
        data.extend_from_slice(&[[0x00, 0x14].as_slice(), &[index as u8; 20]].concat());
        data.push(255);
        RawAccount {
            pubkey: bridge::withdrawal_pda(program_id, index).0,
            lamports: 1_000,
            data,
            executable: false,
        }
    }

    #[test]
    fn a_live_bundle_carries_the_m6id_of_its_records() {
        use bitcoin::hashes::Hash as _;

        let program_id = Pubkey::new_from_array([3u8; 32]);
        let accounts: Vec<RawAccount> = (0..4)
            .map(|index| a_record_raw(&program_id, index))
            .collect();
        let records: Vec<WithdrawalRecord> = accounts[1..3]
            .iter()
            .map(|account| WithdrawalRecord::decode(&account.data).unwrap())
            .collect();
        let want = crate::m6::m6id(&crate::peg::bundle_of(&records).unwrap()).to_byte_array();
        assert_eq!(live_m6id(&program_id, Some(1..3), &accounts).unwrap(), want);
        assert_eq!(live_m6id(&program_id, None, &accounts).unwrap(), [0u8; 32]);
        assert!(matches!(
            live_m6id(&program_id, Some(3..5), &accounts),
            Err(MigrateError::MissingRecord(4))
        ));
    }

    #[test]
    fn the_state_file_round_trips() {
        let mut balances = BTreeMap::new();
        balances.insert(Pubkey::new_from_array([1u8; 32]).to_string(), 5);
        let state = State {
            slot: 9,
            cursor: a_cursor(),
            balances,
            accounts: vec![CarriedAccount {
                pubkey: Pubkey::new_from_array([2u8; 32]).to_string(),
                lamports: 10,
                owner: Pubkey::new_from_array([3u8; 32]).to_string(),
                data: "AQID".into(),
                executable: false,
            }],
            supply_lamports: 100,
            fixed_lamports: 85,
            ownerless_lamports: 5,
            epoch: 12,
        };
        let text = serde_json::to_string(&state).unwrap();
        assert_eq!(serde_json::from_str::<State>(&text).unwrap(), state);
    }
}
