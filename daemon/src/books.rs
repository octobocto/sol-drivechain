//! The two balance equations of the peg.
//!
//! - vault + pegged = supply.
//! - eCash treasury × 10 = pegged + stranded + awaiting credit + pending
//!   withdrawals.
//!
//! A deposit raises the eCash treasury before its credit, and a withdrawal
//! lowers `pegged` before eCash pays it.

use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;

use crate::{
    bridge::{self, Config, LAMPORTS_PER_SAT},
    enforcer::{Enforcer, PegEvent},
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Books {
    pub supply: u64,
    pub vault: u64,
    pub pegged: u64,
    pub stranded: u64,
    pub ecash_treasury_sats: u64,
    pub awaiting: u64,
    pub awaiting_deposits: usize,
    pub pending: u64,
    pub pending_withdrawals: usize,
}

impl Books {
    pub fn vault_holds(&self) -> bool {
        self.vault.checked_add(self.pegged) == Some(self.supply)
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
            "vault + pegged = supply: {}\n\
             \x20 vault                {}\n\
             \x20 pegged               {}\n\
             \x20 supply               {}\n\
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

/// Reads both sides of the books. `rpc` sets the Solana commitment.
pub async fn read(
    rpc: &RpcClient,
    enforcer: &mut Enforcer,
    program_id: &Pubkey,
    supply: u64,
) -> Result<Books, BooksError> {
    let data = rpc
        .get_account_data(&bridge::config_pda(program_id).0)
        .await
        .map_err(|source| BooksError::Rpc {
            call: "getAccountInfo(config)",
            source,
        })?;
    let config = Config::decode(&data)?;
    let vault = rpc
        .get_balance(&bridge::vault_pda(program_id).0)
        .await
        .map_err(|source| BooksError::Rpc {
            call: "getBalance(vault)",
            source,
        })?;
    let records = crate::peg::open_records(rpc, program_id).await?;
    let credited_height = u32::try_from(config.credited_height)
        .map_err(|_| BooksError::HeightTooLarge(config.credited_height))?;

    // A new eCash block between the reads would count its deposit on one
    // side only, so the reads repeat until the tip stays.
    for _ in 0..5 {
        let tip = enforcer.tip().await?;
        let ctip = enforcer.ctip().await?;
        let (start, _) = enforcer.block_at(tip, credited_height).await?;
        let events = enforcer.two_way_peg_data(Some(start), tip.0).await?;
        if enforcer.tip().await? != tip {
            continue;
        }
        let (awaiting_deposits, awaiting) =
            awaiting(&events, config.credited_height, config.credit_index);
        return Ok(Books {
            supply,
            vault,
            pegged: config.pegged_lamports,
            stranded: config.stranded_lamports,
            ecash_treasury_sats: ctip.map_or(0, |ctip| ctip.value_sats),
            awaiting,
            awaiting_deposits,
            pending: records
                .iter()
                .map(|record| record.burned_lamports)
                .fold(0, u64::saturating_add),
            pending_withdrawals: records.len(),
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
        assert!(!books.report().contains("DOES NOT HOLD"));
    }

    #[test]
    fn a_lamport_off_on_either_side_breaks_an_equation() {
        let mut books = balanced();
        books.vault += 1;
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
}
