//! Reads the state of a halted chain for the genesis of its successor: the
//! system balances, every account that carries over byte for byte, and the
//! cursor of the bridge.
//!
//! No lamport of a user stays behind. A stake or vote account turns into
//! plain lamports for its withdrawer, and an account that no rule carries
//! stops the export.

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
}

/// One account that carries over byte for byte.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CarriedAccount {
    pub pubkey: String,
    pub lamports: u64,
    pub owner: String,
    /// The account data in base64.
    pub data: String,
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
    /// The export cannot carry these accounts, so one with lamports stops it.
    Refuse,
}

const OWNER_RULES: &[(&str, Rule)] = &[
    (
        "Stake11111111111111111111111111111111111111",
        Rule::StakeToWithdrawer,
    ),
    (
        "Vote111111111111111111111111111111111111111",
        Rule::VoteToWithdrawer,
    ),
    ("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA", Rule::Carry),
    ("CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C", Rule::Carry),
    ("Sysvar1111111111111111111111111111111111111", Rule::Fixed),
    ("NativeLoader1111111111111111111111111111111", Rule::Fixed),
    ("BPFLoader2111111111111111111111111111111111", Rule::Fixed),
    ("BPFLoader1111111111111111111111111111111111", Rule::Fixed),
    ("Feature111111111111111111111111111111111111", Rule::Fixed),
    ("Config1111111111111111111111111111111111111", Rule::Fixed),
    ("BPFLoaderUpgradeab1e11111111111111111111111", Rule::Refuse),
    ("LoaderV411111111111111111111111111111111111", Rule::Refuse),
    ("AddressLookupTab1e1111111111111111111111111", Rule::Refuse),
    ("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb", Rule::Refuse),
];

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
        "account {pubkey} of {owner} holds {lamports} lamports that the migration cannot carry"
    )]
    CannotCarry {
        pubkey: Pubkey,
        owner: Pubkey,
        lamports: u64,
    },
    #[error("the {kind} account {pubkey} names no withdrawer that the migration can read")]
    NoWithdrawer { kind: &'static str, pubkey: Pubkey },
    #[error(
        "withdrawer {0} holds data, so it cannot take the lamports of a stake or vote account"
    )]
    WithdrawerHoldsData(Pubkey),
    #[error(
        "the supply is {supply} lamports, and the accounts that the migration reads hold \
         {found}. Accounts of another program hold the rest, so the migration stops"
    )]
    Uncarried { supply: u64, found: u64 },
    #[error("the lamports overflow a u64")]
    Overflow,
    #[error("the bridge holds account {0}, which is no withdrawal record")]
    NotARecord(Pubkey),
}

/// One account of a `getProgramAccounts` answer.
struct RawAccount {
    pubkey: Pubkey,
    lamports: u64,
    data: Vec<u8>,
}

/// Reads the state of a halted chain. Every read must see the same slot.
pub async fn export(rpc: &RpcClient, program_id: &Pubkey) -> Result<State, MigrateError> {
    let first = slot(rpc).await?;
    let mut slots = Vec::new();
    let (seen, bridge_accounts) = program_accounts(rpc, program_id).await?;
    slots.push(seen);
    let (seen, system_accounts) = program_accounts(rpc, &bridge::SYSTEM_PROGRAM_ID).await?;
    slots.push(seen);
    let mut owned = Vec::new();
    for (owner, rule) in OWNER_RULES {
        let owner: Pubkey = owner
            .parse()
            .map_err(|_| MigrateError::BadAnswer("owner"))?;
        let (seen, accounts) = program_accounts(rpc, &owner).await?;
        slots.push(seen);
        owned.push((owner, *rule, accounts));
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
    let config = Config::decode(&config.data)?;
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
    };

    let mut carried = Carried {
        fixed: config_lamports,
        read: config_lamports,
        ..Carried::default()
    };
    for account in bridge_accounts {
        if account.pubkey == config_key {
            continue;
        }
        let record = WithdrawalRecord::decode(&account.data)
            .map_err(|_| MigrateError::NotARecord(account.pubkey))?;
        tracing::info!(
            index = record.index,
            burned = record.burned_lamports,
            "the old chain holds a withdrawal record"
        );
        carried.carry(account, *program_id);
    }
    for account in system_accounts {
        carried.system(account);
    }
    for (owner, rule, accounts) in owned {
        for account in accounts {
            carried.apply(owner, rule, account)?;
        }
    }
    let state = carried.finish(first, cursor, supply_lamports)?;
    Ok(state)
}

/// The accounts that the export collects, sorted by what happens to them.
#[derive(Default)]
struct Carried {
    balances: BTreeMap<Pubkey, u64>,
    accounts: Vec<CarriedAccount>,
    /// The lamports of stake and vote accounts, by withdrawer.
    converted: BTreeMap<Pubkey, u64>,
    fixed: u64,
    /// Every lamport that the export read.
    read: u64,
}

impl Carried {
    fn system(&mut self, account: RawAccount) {
        if account.data.is_empty() {
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
        });
    }

    fn apply(
        &mut self,
        owner: Pubkey,
        rule: Rule,
        account: RawAccount,
    ) -> Result<(), MigrateError> {
        match rule {
            Rule::Carry => self.carry(account, owner),
            Rule::Fixed => {
                self.read = self.read.saturating_add(account.lamports);
                self.fixed = self.fixed.saturating_add(account.lamports);
            }
            Rule::Refuse if account.lamports == 0 => {}
            Rule::Refuse => {
                return Err(MigrateError::CannotCarry {
                    pubkey: account.pubkey,
                    owner,
                    lamports: account.lamports,
                })
            }
            Rule::StakeToWithdrawer | Rule::VoteToWithdrawer => {
                let (kind, withdrawer) = if rule == Rule::StakeToWithdrawer {
                    ("stake", stake_withdrawer(&account.data))
                } else {
                    ("vote", vote_withdrawer(&account.data))
                };
                let withdrawer = withdrawer.ok_or(MigrateError::NoWithdrawer {
                    kind,
                    pubkey: account.pubkey,
                })?;
                tracing::info!(
                    account = %account.pubkey,
                    %withdrawer,
                    lamports = account.lamports,
                    "the {kind} account turns into lamports for its withdrawer"
                );
                self.read = self.read.saturating_add(account.lamports);
                let entry = self.converted.entry(withdrawer).or_default();
                *entry = entry
                    .checked_add(account.lamports)
                    .ok_or(MigrateError::Overflow)?;
            }
        }
        Ok(())
    }

    fn finish(
        mut self,
        slot: u64,
        cursor: Cursor,
        supply_lamports: u64,
    ) -> Result<State, MigrateError> {
        for (withdrawer, lamports) in std::mem::take(&mut self.converted) {
            let text = withdrawer.to_string();
            if self.accounts.iter().any(|account| account.pubkey == text) {
                return Err(MigrateError::WithdrawerHoldsData(withdrawer));
            }
            let entry = self.balances.entry(withdrawer).or_default();
            *entry = entry.checked_add(lamports).ok_or(MigrateError::Overflow)?;
        }
        if self.read != supply_lamports {
            return Err(MigrateError::Uncarried {
                supply: supply_lamports,
                found: self.read,
            });
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
        })
    }
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
        let data = account["data"][0]
            .as_str()
            .and_then(|text| base64::engine::general_purpose::STANDARD.decode(text).ok())
            .ok_or_else(bad)?;
        accounts.push(RawAccount {
            pubkey,
            lamports,
            data,
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
        carried
            .apply(
                owner("Stake11111111111111111111111111111111111111"),
                Rule::StakeToWithdrawer,
                raw(5, 300, a_stake_account(withdrawer)),
            )
            .unwrap();
        carried
            .apply(
                owner("Vote111111111111111111111111111111111111111"),
                Rule::VoteToWithdrawer,
                raw(6, 20, a_vote_account(withdrawer)),
            )
            .unwrap();
        let state = carried.finish(1, a_cursor(), 1_320).unwrap();
        assert_eq!(state.balances[&withdrawer.to_string()], 1_320);
        assert!(state.accounts.is_empty());
    }

    #[test]
    fn token_and_nonce_accounts_carry_byte_for_byte() {
        let mut carried = Carried::default();
        carried.system(raw(1, 1_500, vec![1u8; 80]));
        let token = owner("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
        carried
            .apply(token, Rule::Carry, raw(2, 2_344, vec![2u8; 165]))
            .unwrap();
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
    fn an_account_that_no_rule_carries_stops_the_export() {
        let mut carried = Carried::default();
        let loader = owner("BPFLoaderUpgradeab1e11111111111111111111111");
        carried
            .apply(loader, Rule::Refuse, raw(3, 0, Vec::new()))
            .unwrap();
        assert!(matches!(
            carried.apply(loader, Rule::Refuse, raw(3, 10, Vec::new())),
            Err(MigrateError::CannotCarry { lamports: 10, .. })
        ));
    }

    #[test]
    fn lamports_outside_every_read_stop_the_export() {
        let mut carried = Carried::default();
        carried.system(raw(1, 1_000, Vec::new()));
        carried
            .apply(
                owner("Sysvar1111111111111111111111111111111111111"),
                Rule::Fixed,
                raw(2, 50, Vec::new()),
            )
            .unwrap();
        assert!(matches!(
            carried.finish(1, a_cursor(), 1_051),
            Err(MigrateError::Uncarried {
                supply: 1_051,
                found: 1_050
            })
        ));
    }

    #[test]
    fn a_stake_account_without_a_withdrawer_stops_the_export() {
        let mut carried = Carried::default();
        let stake = owner("Stake11111111111111111111111111111111111111");
        assert!(matches!(
            carried.apply(stake, Rule::StakeToWithdrawer, raw(5, 300, vec![0u8; 200])),
            Err(MigrateError::NoWithdrawer { kind: "stake", .. })
        ));
    }

    #[test]
    fn a_withdrawer_with_data_stops_the_export() {
        let withdrawer = Pubkey::new_from_array([1u8; 32]);
        let mut carried = Carried::default();
        carried.system(raw(1, 1_500, vec![1u8; 80]));
        carried
            .apply(
                owner("Stake11111111111111111111111111111111111111"),
                Rule::StakeToWithdrawer,
                raw(5, 300, a_stake_account(withdrawer)),
            )
            .unwrap();
        assert!(matches!(
            carried.finish(1, a_cursor(), 1_800),
            Err(MigrateError::WithdrawerHoldsData(_))
        ));
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
            }],
            supply_lamports: 100,
            fixed_lamports: 85,
        };
        let text = serde_json::to_string(&state).unwrap();
        assert_eq!(serde_json::from_str::<State>(&text).unwrap(), state);
    }
}
