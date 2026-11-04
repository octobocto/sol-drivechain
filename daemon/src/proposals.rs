//! The bundles that this daemon handed to its enforcer. The enforcer keeps
//! each one as its own until eCash settles it, and the KNOWN policy votes for
//! it, so the peg never seals or proposes a range that overlaps another one.

use std::{ops::Range, path::PathBuf};

use serde::{Deserialize, Serialize};

use crate::peg::BundleStatus;

/// The eCash blocks on top of a failure before it frees a range. A reorg can
/// remove a shallower failure, and the bundle is then pending again.
pub const FAILURE_CONFIRMATIONS: u32 = 6;

/// One bundle that the daemon handed to its enforcer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proposal {
    pub start: u64,
    pub end: u64,
    /// The M6ID in the hex that the explorers show.
    pub m6id: String,
    /// The eCash height of the seal. Its history starts there.
    pub sealed_height: u64,
    /// The eCash tip at the newest hand-off. A failure at or below it
    /// belongs to an older hand-off.
    pub proposed_at: u32,
}

impl Proposal {
    pub fn overlaps(&self, range: &Range<u64>) -> bool {
        self.start < range.end && range.start < self.end
    }
}

/// The content of the proposal file.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Kept {
    /// The file dropped each proposal below this record index, after the
    /// BMM root marked it paid.
    dropped_through: u64,
    list: Vec<Proposal>,
}

#[derive(Debug, thiserror::Error)]
pub enum ProposalsError {
    #[error("the daemon cannot read the proposal file `{path}`")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("the proposal file `{path}` is not valid")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error(
        "the bridge holds {withdrawal_count} withdrawals, and the proposal file `{path}` is \
         missing. Restore it, or pass --new-bundle-log when no other copy exists"
    )]
    Missing {
        path: PathBuf,
        withdrawal_count: u64,
    },
    #[error("the daemon cannot write the proposal file `{path}`")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// The proposal file. Each write goes to a new file that then replaces the
/// old one, and the memory changes only after the file does.
pub struct Proposals {
    path: PathBuf,
    kept: Kept,
    existed: bool,
}

impl Proposals {
    /// Reads the file. A file that does not exist holds no proposal.
    pub fn load(path: PathBuf) -> Result<Self, ProposalsError> {
        let (kept, existed) = match std::fs::read_to_string(&path) {
            Ok(text) => (
                serde_json::from_str(&text).map_err(|source| ProposalsError::Parse {
                    path: path.clone(),
                    source,
                })?,
                true,
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (Kept::default(), false),
            Err(source) => return Err(ProposalsError::Read { path, source }),
        };
        Ok(Self {
            path,
            kept,
            existed,
        })
    }

    /// True when the file was on disk at the load.
    pub fn existed(&self) -> bool {
        self.existed
    }

    pub fn list(&self) -> &[Proposal] {
        &self.kept.list
    }

    /// The record index below which the file holds no proposal. A range that
    /// starts below it can repeat a dropped one.
    pub fn dropped_through(&self) -> u64 {
        self.kept.dropped_through
    }

    /// Writes the file when it is not on disk, so a restart after the first
    /// withdrawal finds it.
    pub fn persist(&mut self) -> Result<(), ProposalsError> {
        if self.existed {
            return Ok(());
        }
        let kept = self.kept.clone();
        self.write(&kept)?;
        self.existed = true;
        Ok(())
    }

    /// Keeps `proposal` before the daemon hands it to the enforcer. A new
    /// hand-off of the same M6ID moves its `proposed_at`.
    pub fn add(&mut self, proposal: Proposal) -> Result<(), ProposalsError> {
        let mut kept = self.kept.clone();
        kept.list.retain(|old| old.m6id != proposal.m6id);
        kept.list.push(proposal);
        self.store(kept)
    }

    /// Drops each proposal that ends at or below `paid_through` of the BMM
    /// root. A range below that point can no longer go to eCash.
    pub fn drop_paid(&mut self, paid_through: u64) -> Result<(), ProposalsError> {
        if paid_through <= self.kept.dropped_through
            && self.kept.list.iter().all(|kept| kept.end > paid_through)
        {
            return Ok(());
        }
        let mut kept = self.kept.clone();
        kept.list.retain(|old| old.end > paid_through);
        kept.dropped_through = kept.dropped_through.max(paid_through);
        self.store(kept)
    }

    fn store(&mut self, kept: Kept) -> Result<(), ProposalsError> {
        if kept == self.kept {
            return Ok(());
        }
        self.write(&kept)?;
        self.kept = kept;
        self.existed = true;
        Ok(())
    }

    fn write(&self, kept: &Kept) -> Result<(), ProposalsError> {
        let body = serde_json::to_string_pretty(kept).map_err(|source| ProposalsError::Parse {
            path: self.path.clone(),
            source,
        })?;
        write_durably(&self.path, body.as_bytes()).map_err(|source| ProposalsError::Write {
            path: self.path.clone(),
            source,
        })
    }

    /// The proposals of another M6ID that overlap `range`.
    pub fn overlapping<'a>(
        &'a self,
        range: &'a Range<u64>,
        m6id: Option<&'a str>,
    ) -> impl Iterator<Item = &'a Proposal> + 'a {
        self.kept
            .list
            .iter()
            .filter(move |kept| kept.overlaps(range) && Some(kept.m6id.as_str()) != m6id)
    }
}

/// Writes `body` to a new file, syncs it, renames it over `path`, and syncs
/// the directory, so the file holds the body before the hand-off.
fn write_durably(path: &std::path::Path, body: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;

    let fresh = path.with_extension("new");
    let mut file = std::fs::File::create(&fresh)?;
    file.write_all(body)?;
    file.sync_all()?;
    std::fs::rename(&fresh, path)?;
    let dir = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    std::fs::File::open(dir)?.sync_all()
}

/// Fails when the file is missing while the bridge holds withdrawals: the
/// peg then cannot know which bundles its enforcer keeps.
pub fn check_journal(
    proposals: &Proposals,
    withdrawal_count: u64,
    new_log: bool,
) -> Result<(), ProposalsError> {
    if proposals.existed() || withdrawal_count == 0 || new_log {
        return Ok(());
    }
    Err(ProposalsError::Missing {
        path: proposals.path.clone(),
        withdrawal_count,
    })
}

/// The first overlapping proposal that eCash can still pay, or paid. Only a
/// failure after the newest hand-off, `FAILURE_CONFIRMATIONS` deep below the
/// eCash tip, ends one: the enforcer then settles it, and KNOWN never votes
/// for it again.
pub fn blocking<'a>(
    overlaps: &[(&'a Proposal, BundleStatus)],
    tip_height: u32,
) -> Option<&'a Proposal> {
    overlaps
        .iter()
        .find(|(proposal, status)| {
            !matches!(
                status,
                BundleStatus::Failed { height }
                    if *height > proposal.proposed_at
                        && tip_height.saturating_sub(*height) + 1 >= FAILURE_CONFIRMATIONS
            )
        })
        .map(|(proposal, _)| *proposal)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a_proposal(start: u64, end: u64, m6id: &str) -> Proposal {
        Proposal {
            start,
            end,
            m6id: m6id.to_owned(),
            sealed_height: 40,
            proposed_at: 60,
        }
    }

    fn a_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "sol-drivechain-proposals-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("bundles.json")
    }

    #[test]
    fn a_proposal_survives_a_restart() {
        let path = a_path("restart");
        let mut proposals = Proposals::load(path.clone()).unwrap();
        assert!(proposals.list().is_empty());
        proposals.add(a_proposal(0, 5, "aa")).unwrap();
        proposals.add(a_proposal(0, 5, "aa")).unwrap();
        proposals.add(a_proposal(5, 9, "bb")).unwrap();
        let again = Proposals::load(path).unwrap();
        assert_eq!(
            again.list(),
            &[a_proposal(0, 5, "aa"), a_proposal(5, 9, "bb")]
        );
    }

    #[test]
    fn a_failed_write_keeps_the_memory_as_it_was() {
        let path = a_path("failed-write");
        let mut proposals = Proposals::load(path.clone()).unwrap();
        // A directory at the temporary path makes the write fail.
        std::fs::create_dir_all(path.with_extension("new")).unwrap();
        assert!(proposals.add(a_proposal(0, 5, "aa")).is_err());
        assert!(proposals.list().is_empty());
        std::fs::remove_dir_all(path.with_extension("new")).unwrap();
        proposals.add(a_proposal(0, 5, "aa")).unwrap();
        assert_eq!(Proposals::load(path).unwrap().list().len(), 1);
    }

    #[test]
    fn no_count_drops_a_proposal() {
        let mut proposals = Proposals::load(a_path("count")).unwrap();
        for at in 0..600u64 {
            proposals
                .add(a_proposal(at, at + 1, &format!("{at:x}")))
                .unwrap();
        }
        assert_eq!(proposals.list().len(), 600);
        assert_eq!(proposals.list()[0].start, 0);
    }

    #[test]
    fn only_a_final_payment_drops_a_proposal() {
        let path = a_path("drop");
        let mut proposals = Proposals::load(path.clone()).unwrap();
        proposals.add(a_proposal(0, 5, "aa")).unwrap();
        proposals.add(a_proposal(5, 9, "bb")).unwrap();
        proposals.drop_paid(4).unwrap();
        assert_eq!(proposals.list().len(), 2);
        proposals.drop_paid(5).unwrap();
        assert_eq!(proposals.list(), &[a_proposal(5, 9, "bb")]);
        assert_eq!(proposals.dropped_through(), 5);
        proposals.drop_paid(3).unwrap();
        let again = Proposals::load(path).unwrap();
        assert_eq!(again.dropped_through(), 5);
        assert_eq!(again.list().len(), 1);
    }

    #[test]
    fn a_rollback_range_overlaps_the_old_proposal() {
        let mut proposals = Proposals::load(a_path("overlap")).unwrap();
        proposals.add(a_proposal(3, 8, "old")).unwrap();
        let same = 3..8;
        assert_eq!(proposals.overlapping(&same, Some("old")).count(), 0);
        let wider = 3..12;
        let found: Vec<_> = proposals.overlapping(&wider, Some("new")).collect();
        assert_eq!(found, vec![&a_proposal(3, 8, "old")]);
        let after = 8..12;
        assert_eq!(proposals.overlapping(&after, None).count(), 0);
        let before = 0..3;
        assert_eq!(proposals.overlapping(&before, None).count(), 0);
    }

    #[test]
    fn only_a_deep_failure_after_the_newest_hand_off_frees_a_range() {
        let old = a_proposal(3, 8, "old");
        for status in [
            BundleStatus::Unknown,
            BundleStatus::Submitted { height: 50 },
            BundleStatus::Succeeded { height: 70 },
            BundleStatus::Failed { height: 55 },
            BundleStatus::Failed { height: 60 },
        ] {
            assert_eq!(blocking(&[(&old, status)], 200), Some(&old), "{status:?}");
        }
        let failed = BundleStatus::Failed { height: 61 };
        assert_eq!(blocking(&[(&old, failed)], 65), Some(&old));
        assert_eq!(blocking(&[(&old, failed)], 66), None);
        assert_eq!(blocking(&[], 66), None);
    }

    #[test]
    fn a_reorg_that_drops_the_failure_blocks_the_range_again() {
        let old = a_proposal(3, 8, "old");
        let failed = BundleStatus::Failed { height: 61 };
        assert_eq!(blocking(&[(&old, failed)], 70), None);
        // The reorg drops the failure block, so the bundle is pending again.
        let pending = BundleStatus::Submitted { height: 50 };
        assert_eq!(blocking(&[(&old, pending)], 70), Some(&old));
        // A new failure on the new chain counts only once it is deep.
        let again = BundleStatus::Failed { height: 68 };
        assert_eq!(blocking(&[(&old, again)], 70), Some(&old));
    }

    #[test]
    fn a_missing_file_with_withdrawals_stops_the_peg() {
        let proposals = Proposals::load(a_path("missing")).unwrap();
        assert!(!proposals.existed());
        assert!(check_journal(&proposals, 0, false).is_ok());
        assert!(matches!(
            check_journal(&proposals, 3, false),
            Err(ProposalsError::Missing {
                withdrawal_count: 3,
                ..
            })
        ));
        assert!(check_journal(&proposals, 3, true).is_ok());
        let path = a_path("present");
        let mut written = Proposals::load(path.clone()).unwrap();
        written.add(a_proposal(0, 1, "aa")).unwrap();
        let again = Proposals::load(path).unwrap();
        assert!(again.existed());
        assert!(check_journal(&again, 3, false).is_ok());
    }

    #[test]
    fn a_fresh_start_writes_the_empty_file_at_once() {
        let path = a_path("fresh");
        let mut proposals = Proposals::load(path.clone()).unwrap();
        assert!(check_journal(&proposals, 0, false).is_ok());
        proposals.persist().unwrap();
        // The first withdrawal lands, and the daemon restarts before any
        // proposal.
        let again = Proposals::load(path).unwrap();
        assert!(again.existed());
        assert!(again.list().is_empty());
        assert!(check_journal(&again, 1, false).is_ok());
    }
}
