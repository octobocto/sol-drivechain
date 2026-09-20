use std::collections::VecDeque;

use crate::enforcer::DepositEvent;

/// Holds each deposit until the mainchain buries it under enough blocks.
///
/// A `DisconnectBlock` event carries only the disconnected block hash, not its
/// height, so a reorg drops every deposit that still waits. The enforcer emits
/// those deposits again on the new chain, and the on-chain high-water mark
/// stops a second credit.
pub struct PendingDeposits {
    confirmations: u32,
    tip_height: u32,
    queue: VecDeque<(u32, DepositEvent)>,
}

impl PendingDeposits {
    pub fn new(confirmations: u32) -> Self {
        Self {
            confirmations: confirmations.max(1),
            tip_height: 0,
            queue: VecDeque::new(),
        }
    }

    /// Adds the deposits of one connected block, and returns those that the
    /// chain now buries deep enough.
    pub fn connect(&mut self, height: u32, deposits: Vec<DepositEvent>) -> Vec<DepositEvent> {
        self.tip_height = self.tip_height.max(height);
        for deposit in deposits {
            self.queue.push_back((height, deposit));
        }
        self.take_confirmed()
    }

    /// Drops every deposit that still waits.
    pub fn disconnect(&mut self) {
        self.queue.clear();
    }

    pub fn waiting(&self) -> usize {
        self.queue.len()
    }

    /// The queue holds the deposits in block order, so the first one that
    /// still waits stops the walk.
    fn take_confirmed(&mut self) -> Vec<DepositEvent> {
        let mut ready = Vec::new();
        while let Some((height, deposit)) = self.queue.pop_front() {
            if self.depth(height) < self.confirmations {
                self.queue.push_front((height, deposit));
                break;
            }
            ready.push(deposit);
        }
        ready
    }

    fn depth(&self, height: u32) -> u32 {
        self.tip_height.saturating_sub(height).saturating_add(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a_deposit(sequence_number: u64) -> DepositEvent {
        DepositEvent {
            sequence_number,
            address: b"s8_abc_000000".to_vec(),
            value_sats: 100_000,
        }
    }

    #[test]
    fn one_confirmation_releases_at_once() {
        let mut pending = PendingDeposits::new(1);
        let ready = pending.connect(10, vec![a_deposit(0)]);
        assert_eq!(ready.len(), 1);
        assert_eq!(pending.waiting(), 0);
    }

    #[test]
    fn a_deposit_waits_for_the_chain_to_grow() {
        let mut pending = PendingDeposits::new(3);
        assert!(pending.connect(10, vec![a_deposit(0)]).is_empty());
        assert!(pending.connect(11, vec![]).is_empty());
        let ready = pending.connect(12, vec![]);
        assert_eq!(ready, vec![a_deposit(0)]);
    }

    #[test]
    fn the_deposits_leave_in_the_order_they_arrive() {
        let mut pending = PendingDeposits::new(2);
        pending.connect(10, vec![a_deposit(0), a_deposit(1)]);
        let ready = pending.connect(11, vec![a_deposit(2)]);
        assert_eq!(
            ready.iter().map(|d| d.sequence_number).collect::<Vec<_>>(),
            vec![0, 1]
        );
        assert_eq!(pending.waiting(), 1);
    }

    #[test]
    fn a_reorg_drops_each_deposit_that_waits() {
        let mut pending = PendingDeposits::new(3);
        pending.connect(10, vec![a_deposit(0)]);
        assert_eq!(pending.waiting(), 1);
        pending.disconnect();
        assert_eq!(pending.waiting(), 0);
    }

    #[test]
    fn a_reorg_keeps_a_deposit_that_already_left() {
        let mut pending = PendingDeposits::new(1);
        let ready = pending.connect(10, vec![a_deposit(0)]);
        pending.disconnect();
        assert_eq!(ready, vec![a_deposit(0)]);
    }

    #[test]
    fn zero_confirmations_becomes_one() {
        let mut pending = PendingDeposits::new(0);
        assert_eq!(pending.connect(1, vec![a_deposit(0)]).len(), 1);
    }

    #[test]
    fn a_lower_height_does_not_lower_the_tip() {
        let mut pending = PendingDeposits::new(3);
        pending.connect(10, vec![]);
        assert!(pending.connect(9, vec![a_deposit(0)]).is_empty());
        assert_eq!(pending.waiting(), 1);
    }

    #[test]
    fn a_deposit_that_still_waits_stays_at_the_front() {
        let mut pending = PendingDeposits::new(3);
        assert!(pending.connect(10, vec![a_deposit(1)]).is_empty());
        assert!(pending.connect(11, vec![a_deposit(2)]).is_empty());
        assert_eq!(pending.waiting(), 2);
        // Block 12 buries block 10 three deep, and block 11 only two.
        let ready = pending.connect(12, vec![]);
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].sequence_number, 1);
        assert_eq!(pending.waiting(), 1);
    }

    #[test]
    fn the_queue_keeps_the_block_order() {
        let mut pending = PendingDeposits::new(1);
        let ready = pending.connect(10, vec![a_deposit(5), a_deposit(6), a_deposit(7)]);
        let order: Vec<u64> = ready.iter().map(|d| d.sequence_number).collect();
        assert_eq!(order, vec![5, 6, 7]);
    }
}
