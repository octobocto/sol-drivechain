//! Reads the state of a halted chain for the genesis of its successor: the
//! system balances, the withdrawal records, and the deposit cursor.

use std::collections::BTreeMap;

use base64::Engine as _;
use bitcoin::hashes::Hash as _;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use solana_commitment_config::CommitmentConfig;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_rpc_client_api::request::RpcRequest;
use solana_sdk::pubkey::Pubkey;

use crate::{
    bridge::{self, Config, OracleEraConfig, WithdrawalRecord},
    enforcer::{Enforcer, PegEvent},
};

/// The state of a halted chain at one slot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    pub slot: u64,
    pub cursor: Cursor,
    /// The lamports of each system account without data, the vault and the
    /// treasury too.
    pub balances: BTreeMap<String, u64>,
    pub records: Vec<RecordAccount>,
    /// The total supply of the old chain. The gap to the copied lamports is
    /// in vote, stake, token, and program accounts, which stay behind.
    pub supply_lamports: u64,
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
}

/// One open withdrawal record, as the old chain holds it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordAccount {
    pub pubkey: String,
    pub lamports: u64,
    /// The account data in base64.
    pub data: String,
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
    #[error(transparent)]
    Enforcer(#[from] crate::enforcer::EnforcerError),
    #[error(
        "the config is from the oracle era, so the daemon needs the enforcer to find the cursor"
    )]
    NoEnforcer,
    #[error("the oracle credited no deposit, so no eCash height marks the cursor")]
    NoDepositCredited,
    #[error("no eCash block on the active chain holds deposit {0}")]
    NoBlockForDeposit(u64),
    #[error(
        "eCash block {height} holds deposit {credited} that the oracle credited and deposit \
         {open} that it did not"
    )]
    SplitBlock {
        height: u32,
        credited: u64,
        open: u64,
    },
}

/// One account of a `getProgramAccounts` answer.
struct RawAccount {
    pubkey: Pubkey,
    lamports: u64,
    data: Vec<u8>,
}

/// Reads the state of a halted chain. Every read must see the same slot.
pub async fn export(
    rpc: &RpcClient,
    program_id: &Pubkey,
    enforcer: Option<&mut Enforcer>,
) -> Result<State, MigrateError> {
    let first = slot(rpc).await?;
    let (bridge_slot, bridge_accounts) = program_accounts(rpc, program_id, None).await?;
    let (system_slot, system_accounts) =
        program_accounts(rpc, &bridge::SYSTEM_PROGRAM_ID, Some(0)).await?;
    let (supply_slot, supply_lamports) = supply(rpc).await?;
    let last = slot(rpc).await?;
    if [bridge_slot, system_slot, supply_slot, last]
        .iter()
        .any(|seen| *seen != first)
    {
        return Err(MigrateError::NotHalted {
            first,
            last: bridge_slot.max(system_slot).max(supply_slot).max(last),
        });
    }

    let config_key = bridge::config_pda(program_id).0;
    let config = bridge_accounts
        .iter()
        .find(|account| account.pubkey == config_key)
        .ok_or(MigrateError::NoConfig)?;
    let cursor = match Config::decode(&config.data) {
        Ok(config) => Cursor {
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
        },
        Err(_) => {
            let old = OracleEraConfig::decode(&config.data)?;
            let enforcer = enforcer.ok_or(MigrateError::NoEnforcer)?;
            let tip = enforcer.tip().await?;
            let events = enforcer.two_way_peg_data(None, tip.0).await?;
            let (credited_height, credited_block) =
                oracle_era_cursor(&events, old.deposit_high_water)?;
            Cursor {
                pegged_lamports: old.pegged_lamports,
                deposit_high_water: old.deposit_high_water,
                withdrawal_count: old.withdrawal_count,
                bmm_next_height: old.bmm_next_height,
                bmm_paid_total: old.bmm_paid_total,
                credited_height,
                credited_block,
                credit_index: 0,
                credit_part_block: [0u8; 32],
                stranded_lamports: 0,
            }
        }
    };

    let mut records = Vec::new();
    for account in &bridge_accounts {
        if account.pubkey == config_key {
            continue;
        }
        let record = WithdrawalRecord::decode(&account.data)?;
        tracing::info!(
            index = record.index,
            burned = record.burned_lamports,
            "the old chain holds an open withdrawal"
        );
        records.push(RecordAccount {
            pubkey: account.pubkey.to_string(),
            lamports: account.lamports,
            data: base64::engine::general_purpose::STANDARD.encode(&account.data),
        });
    }
    let balances = system_accounts
        .into_iter()
        .filter(|account| account.lamports > 0)
        .map(|account| (account.pubkey.to_string(), account.lamports))
        .collect();
    Ok(State {
        slot: first,
        cursor,
        balances,
        records,
        supply_lamports,
    })
}

/// The cursor of a chain whose oracle credited each deposit by sequence
/// number: the eCash height and block that hold the last credited deposit.
/// `events` holds the peg events of the active chain, oldest first.
pub fn oracle_era_cursor(
    events: &[PegEvent],
    deposit_high_water: u64,
) -> Result<(u64, [u8; 32]), MigrateError> {
    let last = deposit_high_water
        .checked_sub(1)
        .ok_or(MigrateError::NoDepositCredited)?;
    for event in events {
        let PegEvent::Connect {
            block_hash,
            height,
            deposits,
            ..
        } = event
        else {
            continue;
        };
        if !deposits
            .iter()
            .any(|deposit| deposit.sequence_number == last)
        {
            continue;
        }
        if let Some(open) = deposits
            .iter()
            .find(|deposit| deposit.sequence_number >= deposit_high_water)
        {
            return Err(MigrateError::SplitBlock {
                height: *height,
                credited: last,
                open: open.sequence_number,
            });
        }
        return Ok((u64::from(*height), block_hash.to_byte_array()));
    }
    Err(MigrateError::NoBlockForDeposit(last))
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

/// The accounts of a program and the slot of the read. `data_size` keeps
/// only the accounts with that much data.
async fn program_accounts(
    rpc: &RpcClient,
    program_id: &Pubkey,
    data_size: Option<u64>,
) -> Result<(u64, Vec<RawAccount>), MigrateError> {
    let mut config = json!({
        "commitment": "processed",
        "encoding": "base64",
        "withContext": true,
    });
    if let Some(size) = data_size {
        config["filters"] = json!([{ "dataSize": size }]);
    }
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
    use crate::enforcer::DepositEvent;
    use bitcoin::BlockHash;

    fn a_block(height: u32, sequence_numbers: &[u64]) -> PegEvent {
        PegEvent::Connect {
            block_hash: BlockHash::from_byte_array([height as u8; 32]),
            height,
            deposits: sequence_numbers
                .iter()
                .map(|sequence_number| DepositEvent {
                    sequence_number: *sequence_number,
                    address: Vec::new(),
                    value_sats: 1_000,
                })
                .collect(),
            bundles: Vec::new(),
        }
    }

    #[test]
    fn the_cursor_is_the_block_of_the_last_credited_deposit() {
        let events = [a_block(10, &[0]), a_block(14, &[1, 2]), a_block(20, &[3])];
        assert_eq!(oracle_era_cursor(&events, 3).unwrap(), (14, [14u8; 32]));
        assert_eq!(oracle_era_cursor(&events, 4).unwrap(), (20, [20u8; 32]));
    }

    #[test]
    fn a_block_with_a_credited_and_an_open_deposit_fails() {
        let events = [a_block(14, &[1, 2])];
        assert!(matches!(
            oracle_era_cursor(&events, 2),
            Err(MigrateError::SplitBlock {
                height: 14,
                credited: 1,
                open: 2
            })
        ));
    }

    #[test]
    fn a_chain_without_a_credit_or_without_the_deposit_fails() {
        assert!(matches!(
            oracle_era_cursor(&[a_block(10, &[0])], 0),
            Err(MigrateError::NoDepositCredited)
        ));
        assert!(matches!(
            oracle_era_cursor(&[a_block(10, &[0])], 5),
            Err(MigrateError::NoBlockForDeposit(4))
        ));
    }

    #[test]
    fn the_state_file_round_trips() {
        let mut balances = BTreeMap::new();
        balances.insert(Pubkey::new_from_array([1u8; 32]).to_string(), 5);
        let state = State {
            slot: 9,
            cursor: Cursor {
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
            },
            balances,
            records: vec![RecordAccount {
                pubkey: Pubkey::new_from_array([2u8; 32]).to_string(),
                lamports: 10,
                data: "AQID".into(),
            }],
            supply_lamports: 100,
        };
        let text = serde_json::to_string(&state).unwrap();
        assert_eq!(serde_json::from_str::<State>(&text).unwrap(), state);
    }
}
