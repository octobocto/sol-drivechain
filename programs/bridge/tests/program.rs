//! Drives the bridge program inside a local SVM.
//!
//! The test loads the SBF artifact, so build it first:
//! `cargo-build-sbf --manifest-path programs/bridge/Cargo.toml --sbf-out-dir target/deploy`

use std::path::PathBuf;

use anchor_lang::{prelude::Pubkey, Discriminator, InstructionData, ToAccountMetas};
use litesvm::LiteSVM;
use sol_drivechain_bridge::{
    accounts, instruction, BmmAnswer, Config, WithdrawalRecord, BMM_ANSWER_ID, BMM_ANSWER_LEN,
    LAMPORTS_PER_SAT, MAX_SCRIPT_PUBKEY_LEN,
};
use solana_account::Account;
use solana_instruction::Instruction;
use solana_keypair::Keypair;
use solana_signer::Signer as _;
use solana_transaction::Transaction;

const VAULT_LAMPORTS: u64 = 21_000_000 * 1_000_000_000;
const A_DEPOSIT_SATS: u64 = 1_000_000;
const A_P2WPKH: [u8; 22] = [
    0x00, 0x14, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20,
];

const SYSTEM_PROGRAM: Pubkey = Pubkey::new_from_array([0u8; 32]);
const SYSVAR_PROGRAM: Pubkey =
    Pubkey::from_str_const("Sysvar1111111111111111111111111111111111111");

/// The eCash height at which the chain starts to settle BMM.
const BMM_START: u64 = 100;

/// The rent reserve of an empty account under the default LiteSVM rent.
const TREASURY_RESERVE: u64 = 890_880;

struct Chain {
    svm: LiteSVM,
    program_id: Pubkey,
    config: Pubkey,
    vault: Pubkey,
    treasury: Pubkey,
    oracle: Keypair,
    user: Keypair,
}

impl Chain {
    fn start() -> Self {
        let program_id = sol_drivechain_bridge::ID;
        let config = Pubkey::find_program_address(&[b"config"], &program_id).0;
        let vault = Pubkey::find_program_address(&[b"vault"], &program_id).0;
        let treasury = Pubkey::find_program_address(&[b"treasury"], &program_id).0;

        let mut svm = LiteSVM::new();
        svm.add_program(program_id, &program_bytes())
            .expect("the svm loads the bridge program");

        let oracle = Keypair::new();
        let user = Keypair::new();
        svm.airdrop(&oracle.pubkey(), 100_000_000_000).unwrap();
        svm.airdrop(&user.pubkey(), 100_000_000_000).unwrap();
        svm.set_account(
            vault,
            Account {
                lamports: VAULT_LAMPORTS,
                data: Vec::new(),
                owner: SYSTEM_PROGRAM,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
        svm.set_account(
            treasury,
            Account {
                lamports: TREASURY_RESERVE,
                data: Vec::new(),
                owner: SYSTEM_PROGRAM,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();

        let mut chain = Self {
            svm,
            program_id,
            config,
            vault,
            treasury,
            oracle,
            user,
        };
        let payer = chain.oracle.insecure_clone();
        let oracle_key = payer.pubkey();
        let instruction = chain.build(
            accounts::Initialize {
                config,
                vault,
                treasury,
                payer: oracle_key,
                system_program: SYSTEM_PROGRAM,
            }
            .to_account_metas(None),
            instruction::Initialize {
                oracle: oracle_key,
                bmm_start_height: BMM_START,
            }
            .data(),
        );
        chain
            .send(instruction, &payer)
            .expect("initialize succeeds");
        chain
    }

    fn build(
        &self,
        accounts: Vec<anchor_lang::prelude::AccountMeta>,
        data: Vec<u8>,
    ) -> Instruction {
        Instruction {
            program_id: self.program_id,
            accounts,
            data,
        }
    }

    fn send(&mut self, instruction: Instruction, payer: &Keypair) -> Result<(), String> {
        let transaction = Transaction::new_signed_with_payer(
            &[instruction],
            Some(&payer.pubkey()),
            &[payer],
            self.svm.latest_blockhash(),
        );
        self.svm
            .send_transaction(transaction)
            .map(|_| ())
            .map_err(|failed| format!("{:?}", failed.err))
    }

    fn deposit_as(
        &mut self,
        signer: &Keypair,
        recipient: Pubkey,
        sequence_number: u64,
        value_sats: u64,
    ) -> Result<(), String> {
        let instruction = self.build(
            accounts::Deposit {
                config: self.config,
                vault: self.vault,
                recipient,
                oracle: signer.pubkey(),
                system_program: SYSTEM_PROGRAM,
            }
            .to_account_metas(None),
            instruction::Deposit {
                sequence_number,
                value_sats,
            }
            .data(),
        );
        let signer = signer.insecure_clone();
        self.send(instruction, &signer)
    }

    fn deposit(&mut self, sequence_number: u64, value_sats: u64) -> Result<(), String> {
        let oracle = self.oracle.insecure_clone();
        let recipient = self.user.pubkey();
        self.deposit_as(&oracle, recipient, sequence_number, value_sats)
    }

    fn withdraw(
        &mut self,
        index: u64,
        lamports: u64,
        fee_sats: u64,
        script_pubkey: &[u8],
    ) -> Result<(), String> {
        let user = self.user.insecure_clone();
        let instruction = self.build(
            accounts::Withdraw {
                config: self.config,
                vault: self.vault,
                record: self.record_key(index),
                user: user.pubkey(),
                system_program: SYSTEM_PROGRAM,
            }
            .to_account_metas(None),
            instruction::Withdraw {
                lamports,
                fee_sats,
                script_pubkey: script_pubkey.to_vec(),
            }
            .data(),
        );
        self.send(instruction, &user)
    }

    fn mark_paid_as(&mut self, signer: &Keypair, index: u64) -> Result<(), String> {
        let owner = self.user.pubkey();
        let instruction = self.build(
            accounts::MarkPaid {
                config: self.config,
                record: self.record_key(index),
                owner,
                oracle: signer.pubkey(),
            }
            .to_account_metas(None),
            instruction::MarkPaid { index }.data(),
        );
        let signer = signer.insecure_clone();
        self.send(instruction, &signer)
    }

    /// Writes the `BmmAnswer` account, the way the patched validator does
    /// for a tx with a top-level `settle_bmm`.
    fn answer(&mut self, height: u64, block_hash: [u8; 32], payee: Option<Pubkey>) {
        let mut data = vec![0u8; BMM_ANSWER_LEN];
        data[..8].copy_from_slice(&height.to_le_bytes());
        data[8..40].copy_from_slice(&block_hash);
        if let Some(payee) = payee {
            data[40] = 1;
            data[41..].copy_from_slice(payee.as_ref());
        }
        self.svm
            .set_account(
                BMM_ANSWER_ID,
                Account {
                    lamports: 1,
                    data,
                    owner: SYSVAR_PROGRAM,
                    executable: false,
                    rent_epoch: 0,
                },
            )
            .unwrap();
    }

    fn settle_ix(&self, height: u64, block_hash: [u8; 32], payee: Pubkey) -> Instruction {
        let mut metas = accounts::SettleBmm {
            config: self.config,
            treasury: self.treasury,
            payee,
            bmm_answer: BMM_ANSWER_ID,
            system_program: SYSTEM_PROGRAM,
        }
        .to_account_metas(None);
        // The payee carries no `mut` constraint, so the sender marks it
        // writable, the same way the daemon does.
        for meta in metas.iter_mut() {
            if meta.pubkey == payee {
                meta.is_writable = true;
            }
        }
        self.build(metas, instruction::SettleBmm { height, block_hash }.data())
    }

    /// Sends the instructions in one tx, signed by a new stranger.
    fn send_all(&mut self, instructions: &[Instruction]) -> Result<(), String> {
        let sender = self.a_stranger();
        // Each call needs a new blockhash, or a repeat looks like a replay.
        self.svm.expire_blockhash();
        let transaction = Transaction::new_signed_with_payer(
            instructions,
            Some(&sender.pubkey()),
            &[&sender],
            self.svm.latest_blockhash(),
        );
        self.svm
            .send_transaction(transaction)
            .map(|_| ())
            .map_err(|failed| format!("{:?}", failed.err))
    }

    fn settle(&mut self, height: u64, block_hash: [u8; 32], payee: Pubkey) -> Result<(), String> {
        let instruction = self.settle_ix(height, block_hash, payee);
        self.send_all(&[instruction])
    }

    /// Adds fees to the treasury, the way the patched validator does.
    fn pay_fees(&mut self, lamports: u64) {
        let mut account = self.svm.get_account(&self.treasury).expect("treasury");
        account.lamports += lamports;
        self.svm.set_account(self.treasury, account).unwrap();
    }

    fn record_key(&self, index: u64) -> Pubkey {
        Pubkey::find_program_address(&[b"withdrawal", &index.to_le_bytes()], &self.program_id).0
    }

    fn config(&self) -> Config {
        let account = self.svm.get_account(&self.config).expect("config exists");
        decode::<Config>(&account.data)
    }

    fn record(&self, index: u64) -> Option<WithdrawalRecord> {
        let account = self.svm.get_account(&self.record_key(index))?;
        if account.data.is_empty() {
            return None;
        }
        Some(decode::<WithdrawalRecord>(&account.data))
    }

    fn balance(&self, key: &Pubkey) -> u64 {
        self.svm.get_account(key).map(|a| a.lamports).unwrap_or(0)
    }

    fn a_stranger(&mut self) -> Keypair {
        let stranger = Keypair::new();
        self.svm
            .airdrop(&stranger.pubkey(), 100_000_000_000)
            .unwrap();
        stranger
    }
}

fn decode<T: anchor_lang::AccountDeserialize>(data: &[u8]) -> T {
    let mut slice = data;
    T::try_deserialize(&mut slice).expect("the account decodes")
}

fn program_bytes() -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/deploy/sol_drivechain_bridge.so");
    std::fs::read(&path).unwrap_or_else(|error| {
        panic!(
            "cannot read {}: {error}. Run cargo-build-sbf first.",
            path.display()
        )
    })
}

#[test]
fn the_discriminator_is_eight_bytes_of_sha256() {
    // The daemon builds instruction data by hand, so it must agree with Anchor.
    use solana_sha256_hasher::hash;
    assert_eq!(
        Config::DISCRIMINATOR,
        &hash(b"account:Config").to_bytes()[..8]
    );
    assert_eq!(
        instruction::Deposit::DISCRIMINATOR,
        &hash(b"global:deposit").to_bytes()[..8]
    );
    assert_eq!(
        instruction::Withdraw::DISCRIMINATOR,
        &hash(b"global:withdraw").to_bytes()[..8]
    );
    assert_eq!(
        instruction::MarkPaid::DISCRIMINATOR,
        &hash(b"global:mark_paid").to_bytes()[..8]
    );
    assert_eq!(
        WithdrawalRecord::DISCRIMINATOR,
        &hash(b"account:WithdrawalRecord").to_bytes()[..8]
    );
    assert_eq!(
        instruction::SettleBmm::DISCRIMINATOR,
        &hash(b"global:settle_bmm").to_bytes()[..8]
    );
}

#[test]
fn settle_bmm_carries_the_height_then_the_block_hash() {
    // The patched validator reads the question at bytes 8 to 48.
    let data = instruction::SettleBmm {
        height: 0x0102_0304_0506_0708,
        block_hash: [9u8; 32],
    }
    .data();
    assert_eq!(data.len(), 48);
    assert_eq!(&data[8..16], &0x0102_0304_0506_0708u64.to_le_bytes());
    assert_eq!(&data[16..48], &[9u8; 32]);
}

#[test]
fn initialize_names_the_oracle_and_starts_the_counters() {
    let chain = Chain::start();
    let config = chain.config();
    assert_eq!(config.oracle, chain.oracle.pubkey());
    assert_eq!(config.deposit_high_water, 0);
    assert_eq!(config.pegged_lamports, 0);
    assert_eq!(config.withdrawal_count, 0);
    assert_eq!(config.bmm_next_height, BMM_START);
}

#[test]
fn a_deposit_credits_ten_lamports_for_each_satoshi() {
    let mut chain = Chain::start();
    let before = chain.balance(&chain.user.pubkey());
    chain.deposit(0, A_DEPOSIT_SATS).expect("the deposit lands");
    assert_eq!(
        chain.balance(&chain.user.pubkey()) - before,
        A_DEPOSIT_SATS * LAMPORTS_PER_SAT
    );
    assert_eq!(
        chain.config().pegged_lamports,
        A_DEPOSIT_SATS * LAMPORTS_PER_SAT
    );
    assert_eq!(chain.config().deposit_high_water, 1);
}

#[test]
fn a_deposit_takes_the_lamports_out_of_the_vault() {
    let mut chain = Chain::start();
    let before = chain.balance(&chain.vault);
    chain.deposit(0, A_DEPOSIT_SATS).expect("the deposit lands");
    assert_eq!(
        before - chain.balance(&chain.vault),
        A_DEPOSIT_SATS * LAMPORTS_PER_SAT
    );
}

#[test]
fn a_repeat_sequence_number_fails() {
    let mut chain = Chain::start();
    chain.deposit(4, A_DEPOSIT_SATS).expect("the deposit lands");
    assert!(chain.deposit(4, A_DEPOSIT_SATS).is_err());
}

#[test]
fn a_lower_sequence_number_fails() {
    let mut chain = Chain::start();
    chain.deposit(9, A_DEPOSIT_SATS).expect("the deposit lands");
    assert!(chain.deposit(8, A_DEPOSIT_SATS).is_err());
}

#[test]
fn a_sequence_number_gap_is_fine() {
    let mut chain = Chain::start();
    chain.deposit(0, A_DEPOSIT_SATS).expect("the deposit lands");
    chain
        .deposit(7, A_DEPOSIT_SATS)
        .expect("a withdrawal also raises the mainchain counter, so a gap is normal");
    assert_eq!(chain.config().deposit_high_water, 8);
}

#[test]
fn a_zero_deposit_fails() {
    let mut chain = Chain::start();
    assert!(chain.deposit(0, 0).is_err());
}

#[test]
fn only_the_oracle_credits_a_deposit() {
    let mut chain = Chain::start();
    let thief = chain.a_stranger();
    let recipient = thief.pubkey();
    assert!(chain
        .deposit_as(&thief, recipient, 0, A_DEPOSIT_SATS)
        .is_err());
}

#[test]
fn a_withdrawal_writes_a_record_and_moves_the_lamports_back() {
    let mut chain = Chain::start();
    chain.deposit(0, A_DEPOSIT_SATS).expect("the deposit lands");
    let before = chain.balance(&chain.vault);

    chain
        .withdraw(0, 5_000_000, 1_000, &A_P2WPKH)
        .expect("the withdrawal lands");

    assert_eq!(chain.balance(&chain.vault) - before, 5_000_000);
    let record = chain.record(0).expect("the record exists");
    assert_eq!(record.index, 0);
    assert_eq!(record.owner, chain.user.pubkey());
    assert_eq!(record.burned_lamports, 5_000_000);
    assert_eq!(record.payout_sats, 500_000 - 1_000);
    assert_eq!(record.fee_sats, 1_000);
    assert_eq!(record.script_pubkey, A_P2WPKH);
    assert_eq!(chain.config().withdrawal_count, 1);
    assert_eq!(
        chain.config().pegged_lamports,
        A_DEPOSIT_SATS * LAMPORTS_PER_SAT - 5_000_000
    );
}

#[test]
fn an_amount_that_is_not_a_whole_satoshi_fails() {
    let mut chain = Chain::start();
    chain.deposit(0, A_DEPOSIT_SATS).expect("the deposit lands");
    assert!(chain.withdraw(0, 5_000_001, 1_000, &A_P2WPKH).is_err());
}

#[test]
fn an_amount_above_the_pegged_total_fails() {
    let mut chain = Chain::start();
    chain.deposit(0, 100_000).expect("the deposit lands");
    assert!(chain.withdraw(0, 2_000_000, 1_000, &A_P2WPKH).is_err());
}

#[test]
fn the_genesis_lamports_cannot_peg_out() {
    let mut chain = Chain::start();
    assert_eq!(chain.config().pegged_lamports, 0);
    assert!(chain.withdraw(0, 10_000_000, 1_000, &A_P2WPKH).is_err());
}

#[test]
fn a_fee_above_the_amount_fails() {
    let mut chain = Chain::start();
    chain.deposit(0, A_DEPOSIT_SATS).expect("the deposit lands");
    assert!(chain.withdraw(0, 5_000_000, 600_000, &A_P2WPKH).is_err());
}

#[test]
fn a_dust_payout_fails() {
    let mut chain = Chain::start();
    chain.deposit(0, A_DEPOSIT_SATS).expect("the deposit lands");
    assert!(chain.withdraw(0, 5_000, 100, &A_P2WPKH).is_err());
}

#[test]
fn an_op_return_script_fails() {
    let mut chain = Chain::start();
    chain.deposit(0, A_DEPOSIT_SATS).expect("the deposit lands");
    assert!(chain
        .withdraw(0, 5_000_000, 1_000, &[0x6a, 0x04, 1, 2, 3, 4])
        .is_err());
}

#[test]
fn an_empty_script_fails() {
    let mut chain = Chain::start();
    chain.deposit(0, A_DEPOSIT_SATS).expect("the deposit lands");
    assert!(chain.withdraw(0, 5_000_000, 1_000, &[]).is_err());
}

#[test]
fn a_script_above_the_limit_fails() {
    let mut chain = Chain::start();
    chain.deposit(0, A_DEPOSIT_SATS).expect("the deposit lands");
    let too_long = vec![0x00u8; MAX_SCRIPT_PUBKEY_LEN + 1];
    assert!(chain.withdraw(0, 5_000_000, 1_000, &too_long).is_err());
}

#[test]
fn two_withdrawals_take_two_record_addresses() {
    let mut chain = Chain::start();
    chain.deposit(0, A_DEPOSIT_SATS).expect("the deposit lands");
    chain
        .withdraw(0, 5_000_000, 1_000, &A_P2WPKH)
        .expect("the first withdrawal lands");
    chain
        .withdraw(1, 2_000_000, 1_000, &A_P2WPKH)
        .expect("the second withdrawal lands");
    assert_eq!(chain.config().withdrawal_count, 2);
    assert!(chain.record(0).is_some());
    assert!(chain.record(1).is_some());
}

#[test]
fn mark_paid_closes_the_record_and_returns_the_rent() {
    let mut chain = Chain::start();
    chain.deposit(0, A_DEPOSIT_SATS).expect("the deposit lands");
    chain
        .withdraw(0, 5_000_000, 1_000, &A_P2WPKH)
        .expect("the withdrawal lands");

    let rent = chain.balance(&chain.record_key(0));
    assert!(rent > 0);
    let before = chain.balance(&chain.user.pubkey());

    let oracle = chain.oracle.insecure_clone();
    chain.mark_paid_as(&oracle, 0).expect("mark_paid succeeds");

    assert!(chain.record(0).is_none());
    assert_eq!(chain.balance(&chain.user.pubkey()) - before, rent);
}

#[test]
fn only_the_oracle_marks_a_record_paid() {
    let mut chain = Chain::start();
    chain.deposit(0, A_DEPOSIT_SATS).expect("the deposit lands");
    chain
        .withdraw(0, 5_000_000, 1_000, &A_P2WPKH)
        .expect("the withdrawal lands");

    let thief = chain.a_stranger();
    assert!(chain.mark_paid_as(&thief, 0).is_err());
    assert!(chain.record(0).is_some());
}

#[test]
fn a_full_round_trip_returns_the_vault_to_its_genesis_balance() {
    let mut chain = Chain::start();
    assert_eq!(chain.balance(&chain.vault), VAULT_LAMPORTS);

    chain.deposit(0, A_DEPOSIT_SATS).expect("the deposit lands");
    let credited = A_DEPOSIT_SATS * LAMPORTS_PER_SAT;
    chain
        .withdraw(0, credited, 1_000, &A_P2WPKH)
        .expect("the withdrawal lands");

    assert_eq!(chain.balance(&chain.vault), VAULT_LAMPORTS);
    assert_eq!(chain.config().pegged_lamports, 0);
}

const BLOCK: [u8; 32] = [0xb1; 32];
const OTHER_BLOCK: [u8; 32] = [0xb2; 32];

#[test]
fn the_answer_decodes_found_and_none() {
    let mut data = vec![0u8; BMM_ANSWER_LEN];
    data[..8].copy_from_slice(&5u64.to_le_bytes());
    data[8..40].copy_from_slice(&BLOCK);
    assert_eq!(
        BmmAnswer::decode(&data),
        Some(BmmAnswer {
            height: 5,
            block_hash: BLOCK,
            commitment: None,
        })
    );
    data[40] = 1;
    data[41..].copy_from_slice(&[4u8; 32]);
    assert_eq!(
        BmmAnswer::decode(&data).unwrap().commitment,
        Some([4u8; 32])
    );
    data[40] = 2;
    assert_eq!(BmmAnswer::decode(&data), None);
    assert_eq!(BmmAnswer::decode(&data[..40]), None);
}

#[test]
fn a_win_pays_the_whole_treasury_above_the_reserve_to_the_payee() {
    let mut chain = Chain::start();
    let winner = chain.a_stranger().pubkey();
    let before = chain.balance(&winner);
    chain.pay_fees(3_000_000);
    chain.answer(BMM_START, BLOCK, Some(winner));

    chain
        .settle(BMM_START, BLOCK, winner)
        .expect("the height settles");

    assert_eq!(chain.balance(&winner) - before, 3_000_000);
    assert_eq!(chain.balance(&chain.treasury), TREASURY_RESERVE);
    assert_eq!(chain.config().bmm_paid_total, 3_000_000);
    assert_eq!(chain.config().bmm_next_height, BMM_START + 1);
}

#[test]
fn a_height_with_no_commitment_pays_nobody() {
    let mut chain = Chain::start();
    let anyone = chain.a_stranger().pubkey();
    chain.pay_fees(1_000_000);
    chain.answer(BMM_START, BLOCK, None);

    chain
        .settle(BMM_START, BLOCK, anyone)
        .expect("an empty height settles");

    assert_eq!(chain.balance(&chain.treasury), TREASURY_RESERVE + 1_000_000);
    assert_eq!(chain.config().bmm_paid_total, 0);
    assert_eq!(chain.config().bmm_next_height, BMM_START + 1);
}

#[test]
fn a_payee_account_that_is_not_the_commitment_fails() {
    let mut chain = Chain::start();
    let winner = chain.a_stranger().pubkey();
    let thief = chain.a_stranger().pubkey();
    let before = chain.balance(&thief);
    chain.pay_fees(1_000_000);
    chain.answer(BMM_START, BLOCK, Some(winner));

    assert!(chain.settle(BMM_START, BLOCK, thief).is_err());
    assert_eq!(chain.balance(&thief), before);
    assert_eq!(chain.config().bmm_next_height, BMM_START);
}

#[test]
fn a_payee_that_cannot_hold_the_payout_gets_nothing_and_the_height_settles() {
    let mut chain = Chain::start();
    // An account that does not exist cannot take less than its rent reserve.
    let empty = Keypair::new().pubkey();
    chain.pay_fees(1_000);
    chain.answer(BMM_START, BLOCK, Some(empty));

    chain
        .settle(BMM_START, BLOCK, empty)
        .expect("the height settles");

    assert_eq!(chain.balance(&empty), 0);
    assert_eq!(chain.balance(&chain.treasury), TREASURY_RESERVE + 1_000);
    assert_eq!(chain.config().bmm_next_height, BMM_START + 1);
}

#[test]
fn a_new_payee_account_takes_a_payout_above_its_rent_reserve() {
    let mut chain = Chain::start();
    let fresh = Keypair::new().pubkey();
    chain.pay_fees(2_000_000);
    chain.answer(BMM_START, BLOCK, Some(fresh));

    chain
        .settle(BMM_START, BLOCK, fresh)
        .expect("the height settles");
    assert_eq!(chain.balance(&fresh), 2_000_000);
}

#[test]
fn the_treasury_as_the_payee_gets_nothing_and_the_height_settles() {
    let mut chain = Chain::start();
    let treasury = chain.treasury;
    chain.pay_fees(2_000_000);
    chain.answer(BMM_START, BLOCK, Some(treasury));

    chain
        .settle(BMM_START, BLOCK, treasury)
        .expect("the height settles");
    assert_eq!(chain.balance(&treasury), TREASURY_RESERVE + 2_000_000);
    assert_eq!(chain.config().bmm_next_height, BMM_START + 1);
}

#[test]
fn an_executable_payee_gets_nothing_and_the_height_settles() {
    let mut chain = Chain::start();
    // The runtime refuses a lamport change on an executable account, so a
    // settle that paid it would fail and stop every later height.
    let program = chain.program_id;
    chain.pay_fees(2_000_000);
    chain.answer(BMM_START, BLOCK, Some(program));

    chain
        .settle(BMM_START, BLOCK, program)
        .expect("the height settles");

    assert_eq!(chain.balance(&chain.treasury), TREASURY_RESERVE + 2_000_000);
    assert_eq!(chain.config().bmm_paid_total, 0);
    assert_eq!(chain.config().bmm_next_height, BMM_START + 1);
}

#[test]
fn a_height_out_of_order_fails() {
    let mut chain = Chain::start();
    let anyone = chain.a_stranger().pubkey();
    chain.answer(BMM_START + 1, BLOCK, None);
    assert!(chain.settle(BMM_START + 1, BLOCK, anyone).is_err());
}

#[test]
fn a_height_settles_only_once() {
    let mut chain = Chain::start();
    let anyone = chain.a_stranger().pubkey();
    chain.answer(BMM_START, BLOCK, None);
    chain
        .settle(BMM_START, BLOCK, anyone)
        .expect("the height settles");
    assert!(chain.settle(BMM_START, BLOCK, anyone).is_err());
}

#[test]
fn a_tx_without_an_answer_fails() {
    let mut chain = Chain::start();
    let anyone = chain.a_stranger().pubkey();
    assert!(chain.settle(BMM_START, BLOCK, anyone).is_err());
    assert_eq!(chain.config().bmm_next_height, BMM_START);
}

#[test]
fn an_answer_for_another_height_fails() {
    let mut chain = Chain::start();
    let winner = chain.a_stranger().pubkey();
    chain.answer(BMM_START + 1, BLOCK, Some(winner));
    assert!(chain.settle(BMM_START, BLOCK, winner).is_err());
}

#[test]
fn an_answer_for_another_block_fails() {
    let mut chain = Chain::start();
    let winner = chain.a_stranger().pubkey();
    chain.answer(BMM_START, OTHER_BLOCK, Some(winner));
    assert!(chain.settle(BMM_START, BLOCK, winner).is_err());
}

#[test]
fn a_second_settle_in_the_same_tx_fails_the_tx() {
    let mut chain = Chain::start();
    let winner = chain.a_stranger().pubkey();
    chain.pay_fees(1_000_000);
    chain.answer(BMM_START, BLOCK, Some(winner));

    let first = chain.settle_ix(BMM_START, BLOCK, winner);
    let second = chain.settle_ix(BMM_START + 1, OTHER_BLOCK, winner);
    assert!(chain.send_all(&[first.clone(), second]).is_err());
    let repeat = chain.settle_ix(BMM_START, BLOCK, winner);
    assert!(chain.send_all(&[first, repeat]).is_err());
    assert_eq!(chain.config().bmm_next_height, BMM_START);
}

#[test]
fn each_winner_gets_the_fees_between_two_settles() {
    let mut chain = Chain::start();
    let first = chain.a_stranger().pubkey();
    let second = chain.a_stranger().pubkey();
    let first_before = chain.balance(&first);
    let second_before = chain.balance(&second);

    chain.pay_fees(1_000_000);
    chain.answer(BMM_START, BLOCK, Some(first));
    chain.settle(BMM_START, BLOCK, first).expect("height one");
    chain.pay_fees(2_000_000);
    chain.answer(BMM_START + 1, OTHER_BLOCK, None);
    chain
        .settle(BMM_START + 1, OTHER_BLOCK, first)
        .expect("an empty height");
    chain.pay_fees(500_000);
    chain.answer(BMM_START + 2, BLOCK, Some(second));
    chain
        .settle(BMM_START + 2, BLOCK, second)
        .expect("height three");

    assert_eq!(chain.balance(&first) - first_before, 1_000_000);
    assert_eq!(chain.balance(&second) - second_before, 2_500_000);
    assert_eq!(chain.config().bmm_paid_total, 3_500_000);
}
