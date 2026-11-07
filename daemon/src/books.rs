//! The balance equations of the peg.
//!
//! - vault + pegged = supply + gifts. A gift to the vault only adds backing.
//! - gifts >= float: the gifts back the genesis float, so every spendable
//!   lamport has Bitcoin behind it.
//! - eCash treasury × 10 = pegged + stranded + awaiting credit + pending
//!   withdrawals.
//!
//! A deposit raises the eCash treasury before its credit, and a withdrawal
//! lowers `pegged` before eCash pays it.

use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;

use solana_commitment_config::CommitmentConfig;

use crate::{
    bridge::{self, Config, LAMPORTS_PER_SAT},
    enforcer::{Enforcer, PegEvent},
    peg::BundleStatus,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Books {
    pub supply: u64,
    pub vault: u64,
    pub float: u64,
    pub pegged: u64,
    pub stranded: u64,
    pub ecash_treasury_sats: u64,
    pub awaiting: u64,
    pub awaiting_deposits: usize,
    pub pending: u64,
    pub pending_withdrawals: usize,
}

impl Books {
    /// The lamports that gifts put in the vault, or `None` when the vault
    /// holds less than the supply less the pegged lamports.
    pub fn gifts(&self) -> Option<u64> {
        self.vault
            .checked_add(self.pegged)?
            .checked_sub(self.supply)
    }

    pub fn vault_holds(&self) -> bool {
        self.gifts().is_some()
    }

    pub fn float_backed(&self) -> bool {
        self.gifts().is_some_and(|gifts| gifts >= self.float)
    }

    pub fn hold(&self) -> bool {
        self.vault_holds() && self.float_backed() && self.treasury_holds()
    }

    pub fn treasury_holds(&self) -> bool {
        let left = self.ecash_treasury_sats.checked_mul(LAMPORTS_PER_SAT);
        let right = self
            .pegged
            .checked_add(self.stranded)
            .and_then(|sum| sum.checked_add(self.awaiting))
            .and_then(|sum| sum.checked_add(self.pending));
        left.is_some() && left == right
    }

    pub fn report(&self) -> String {
        let verdict = |holds: bool| if holds { "holds" } else { "DOES NOT HOLD" };
        format!(
            "vault + pegged = supply + gifts: {}\n\
             \x20 vault                {}\n\
             \x20 pegged               {}\n\
             \x20 supply               {}\n\
             \x20 gifts                {}\n\
             gifts >= float, so withdrawals are open: {}\n\
             \x20 float                {}\n\
             eCash treasury x 10 = pegged + stranded + awaiting credit + pending withdrawals: {}\n\
             \x20 eCash treasury x 10  {} ({} sats)\n\
             \x20 pegged               {}\n\
             \x20 stranded             {}\n\
             \x20 awaiting credit      {} ({} deposits)\n\
             \x20 pending withdrawals  {} ({} records)\n",
            verdict(self.vault_holds()),
            self.vault,
            self.pegged,
            self.supply,
            self.gifts()
                .map_or_else(|| "none".to_owned(), |gifts| gifts.to_string()),
            verdict(self.float_backed()),
            self.float,
            verdict(self.treasury_holds()),
            self.ecash_treasury_sats.saturating_mul(LAMPORTS_PER_SAT),
            self.ecash_treasury_sats,
            self.pegged,
            self.stranded,
            self.awaiting,
            self.awaiting_deposits,
            self.pending,
            self.pending_withdrawals,
        )
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BooksError {
    #[error(transparent)]
    Enforcer(#[from] crate::enforcer::EnforcerError),
    #[error(transparent)]
    Bridge(#[from] bridge::BridgeError),
    #[error(transparent)]
    Peg(#[from] crate::peg::PegError),
    #[error(transparent)]
    M6(#[from] crate::m6::M6Error),
    #[error("the Solana rpc call `{call}` failed")]
    Rpc {
        call: &'static str,
        #[source]
        source: solana_rpc_client_api::client_error::Error,
    },
    #[error("eCash height {0} does not fit a u32")]
    HeightTooLarge(u64),
    #[error("the eCash tip moved during every read")]
    TipMoves,
}

/// The burned lamports of the records that eCash did not pay yet. The records
/// below `paid_through` are paid. The live bundle counts as paid as soon as
/// eCash pays it, before its `bundle_paid`.
pub fn pending(
    records: &[bridge::WithdrawalRecord],
    config: &Config,
    live_status: BundleStatus,
) -> (usize, u64) {
    let live = config.live_bundle();
    let paid_live = matches!(live_status, BundleStatus::Succeeded { .. });
    records
        .iter()
        .filter(|record| record.index >= config.paid_through)
        .filter(|record| {
            !(paid_live
                && live
                    .as_ref()
                    .is_some_and(|live| live.contains(&record.index)))
        })
        .fold((0, 0), |(count, lamports), record| {
            (
                count + 1,
                u64::saturating_add(lamports, record.burned_lamports),
            )
        })
}

/// The deposits of the active eCash chain that the bank did not credit yet:
/// every deposit after `credited_height`, less the first `credit_index` of
/// the next block. `events` holds the peg events after the credited block.
pub fn awaiting(events: &[PegEvent], credited_height: u64, credit_index: u64) -> (usize, u64) {
    let mut count = 0;
    let mut lamports: u64 = 0;
    for event in events {
        let PegEvent::Connect {
            height, deposits, ..
        } = event
        else {
            continue;
        };
        let height = u64::from(*height);
        if height <= credited_height {
            continue;
        }
        let skip = if height == credited_height.saturating_add(1) {
            usize::try_from(credit_index).unwrap_or(usize::MAX)
        } else {
            0
        };
        for deposit in deposits.iter().skip(skip) {
            count += 1;
            lamports = lamports.saturating_add(deposit.value_sats.saturating_mul(LAMPORTS_PER_SAT));
        }
    }
    (count, lamports)
}

/// Reads both sides of the books at `commitment`.
pub async fn read(
    rpc: &RpcClient,
    enforcer: &mut Enforcer,
    program_id: &Pubkey,
    commitment: CommitmentConfig,
) -> Result<Books, BooksError> {
    let (config, vault) = crate::peg::config_and_vault(rpc, program_id, commitment).await?;
    let records = crate::peg::read_records(
        rpc,
        program_id,
        config.paid_through..config.withdrawal_count,
        commitment,
    )
    .await?;
    let live_m6id = match config.live_bundle() {
        Some(live) => {
            let live_records = crate::peg::live_records(rpc, program_id, live, commitment).await?;
            Some(crate::m6::m6id(&crate::peg::bundle_of(&live_records)?))
        }
        None => None,
    };
    let credited_height = u32::try_from(config.credited_height)
        .map_err(|_| BooksError::HeightTooLarge(config.credited_height))?;

    // A new eCash block between the reads would count its deposit on one
    // side only, so the reads repeat until the tip stays.
    for _ in 0..5 {
        let tip = enforcer.tip().await?;
        let ctip = enforcer.ctip().await?;
        let events = enforcer
            .peg_data_from(tip, credited_height.min(tip.1))
            .await?;
        let live_status = match live_m6id {
            Some(m6id) => {
                crate::peg::bundle_status_at(enforcer, tip, config.bundle_sealed_height, m6id)
                    .await?
            }
            None => BundleStatus::Unknown,
        };
        if enforcer.tip().await? != tip {
            continue;
        }
        let (pending_withdrawals, pending) = pending(&records, &config, live_status);
        let (awaiting_deposits, awaiting) =
            awaiting(&events, config.credited_height, config.credit_index);
        return Ok(Books {
            supply: config.vault_supply,
            vault,
            float: config.float_lamports,
            pegged: config.pegged_lamports,
            stranded: config.stranded_lamports,
            ecash_treasury_sats: ctip.map_or(0, |ctip| ctip.value_sats),
            awaiting,
            awaiting_deposits,
            pending,
            pending_withdrawals,
        });
    }
    Err(BooksError::TipMoves)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::enforcer::DepositEvent;
    use bitcoin::{hashes::Hash as _, BlockHash};

    fn a_block(height: u32, values: &[u64]) -> PegEvent {
        PegEvent::Connect {
            block_hash: BlockHash::from_byte_array([height as u8; 32]),
            height,
            deposits: values
                .iter()
                .enumerate()
                .map(|(index, value)| DepositEvent {
                    sequence_number: index as u64,
                    address: Vec::new(),
                    value_sats: *value,
                })
                .collect(),
            bundles: Vec::new(),
        }
    }

    fn balanced() -> Books {
        Books {
            supply: 1_000_000,
            vault: 600_000,
            float: 0,
            pegged: 400_000,
            stranded: 5_000,
            ecash_treasury_sats: 46_000,
            awaiting: 30_000,
            awaiting_deposits: 1,
            pending: 25_000,
            pending_withdrawals: 1,
        }
    }

    #[test]
    fn balanced_books_hold_both_equations() {
        let books = balanced();
        assert!(books.vault_holds());
        assert!(books.treasury_holds());
        assert!(books.hold());
        assert!(!books.report().contains("DOES NOT HOLD"));
    }

    #[test]
    fn a_gift_to_the_vault_keeps_the_books_closed() {
        let mut books = balanced();
        books.vault += 1;
        assert_eq!(books.gifts(), Some(1));
        assert!(books.hold());
    }

    #[test]
    fn an_unbacked_float_opens_the_books() {
        let mut books = balanced();
        books.float = 5_000;
        assert!(!books.float_backed());
        assert!(!books.hold());
        assert!(books.report().contains("DOES NOT HOLD"));
        books.vault += 4_999;
        assert!(!books.float_backed());
        books.vault += 1;
        assert!(books.float_backed());
        assert!(books.hold());
    }

    #[test]
    fn a_lamport_off_on_either_side_breaks_an_equation() {
        let mut books = balanced();
        books.vault -= 1;
        assert!(!books.vault_holds());
        assert!(books.treasury_holds());
        let mut books = balanced();
        books.pending += 1;
        assert!(!books.treasury_holds());
        assert!(books.report().contains("DOES NOT HOLD"));
    }

    #[test]
    fn awaiting_counts_the_deposits_after_the_credited_block() {
        let events = [
            a_block(10, &[100]),
            a_block(11, &[200, 300, 400]),
            a_block(13, &[500]),
        ];
        assert_eq!(awaiting(&events, 10, 0), (4, 14_000));
        assert_eq!(awaiting(&events, 10, 2), (2, 9_000));
        assert_eq!(awaiting(&events, 13, 0), (0, 0));
    }

    fn a_record(index: u64, burned_lamports: u64) -> bridge::WithdrawalRecord {
        bridge::WithdrawalRecord {
            index,
            owner: solana_sdk::pubkey::Pubkey::new_from_array([1u8; 32]),
            burned_lamports,
            payout_sats: 1_000,
            fee_sats: 1_000,
            script_pubkey: vec![0x51],
            bump: 255,
        }
    }

    fn a_config(paid_through: u64, bundled_through: u64) -> Config {
        let mut data = Config {
            oracle: solana_sdk::pubkey::Pubkey::default(),
            deposit_high_water: 0,
            pegged_lamports: 0,
            withdrawal_count: 6,
            bmm_next_height: 0,
            bmm_paid_total: 0,
            deposit_confirmations: 6,
            deposit_lag: 100,
            credited_height: 0,
            credited_block: [0u8; 32],
            credit_index: 0,
            credit_part_block: [0u8; 32],
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
        };
        data.paid_through = paid_through;
        data.bundled_through = bundled_through;
        data
    }

    #[test]
    fn pending_counts_the_records_that_ecash_did_not_pay() {
        let records: Vec<_> = (1..6).map(|index| a_record(index, 100 * index)).collect();
        let config = a_config(2, 4);
        assert_eq!(
            pending(&records, &config, BundleStatus::Unknown),
            (4, 1_400)
        );
        assert_eq!(
            pending(&records, &config, BundleStatus::Submitted { height: 9 }),
            (4, 1_400)
        );
        assert_eq!(
            pending(&records, &config, BundleStatus::Succeeded { height: 9 }),
            (2, 900)
        );
        assert_eq!(
            pending(&records, &a_config(4, 4), BundleStatus::Unknown),
            (2, 900)
        );
    }
}
