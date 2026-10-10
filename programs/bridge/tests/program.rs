//! Drives the bridge program inside a local SVM.
//!
//! The test loads the SBF artifact, so build it first:
//! `cargo-build-sbf --manifest-path programs/bridge/Cargo.toml --sbf-out-dir target/deploy`

use std::path::PathBuf;

use anchor_lang::{
    prelude::{AccountMeta, Pubkey},
    Discriminator, InstructionData, ToAccountMetas,
};
use litesvm::LiteSVM;
use sol_drivechain_bridge::{
    accounts, commitment_of, instruction, AnsweredDeposit, BmmAnswer, Config, DepositAnswer,
    WithdrawalRecord, BMM_ANSWER_ID, BMM_ANSWER_LEN, DEPOSIT_ANSWER_ENTRY_LEN,
    DEPOSIT_ANSWER_HEAD_LEN, DEPOSIT_ANSWER_ID, LAMPORTS_PER_SAT, MAX_SCRIPT_PUBKEY_LEN,
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

/// The deposits up to this eCash height count as credited at the start.
const DEPOSIT_START: u64 = 200;
const DEPOSIT_CONFIRMATIONS: u64 = 6;
const DEPOSIT_LAG: u64 = 100;

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
                deposit_start_height: DEPOSIT_START,
                deposit_confirmations: DEPOSIT_CONFIRMATIONS,
                deposit_lag: DEPOSIT_LAG,
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

    /// Writes the `DepositAnswer` account, the way the patched validator does
    /// for a tx with a top-level `credit_deposits`: the part of `deposits`
    /// from `first`, at most `count` of them.
    fn deposit_answer(
        &mut self,
        height: u64,
        prev_block_hash: [u8; 32],
        deposits: &[AnsweredDeposit],
        first: u32,
        count: u8,
    ) {
        let part: Vec<&AnsweredDeposit> = deposits
            .iter()
            .skip(first as usize)
            .take(usize::from(count))
            .collect();
        let mut data = height.to_le_bytes().to_vec();
        data.extend_from_slice(&ecash_block(height));
        data.extend_from_slice(&prev_block_hash);
        data.extend_from_slice(&(deposits.len() as u32).to_le_bytes());
        data.extend_from_slice(&first.to_le_bytes());
        data.push(part.len() as u8);
        for deposit in part {
            data.extend_from_slice(&deposit.sequence_number.to_le_bytes());
            data.extend_from_slice(&deposit.value_sats.to_le_bytes());
            match deposit.target {
                Some(target) => {
                    data.push(1);
                    data.extend_from_slice(target.as_ref());
                }
                None => {
                    data.push(0);
                    data.extend_from_slice(&[0u8; 32]);
                }
            }
        }
        self.svm
            .set_account(
                DEPOSIT_ANSWER_ID,
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

    /// A `credit_deposits` with one writable account for each target in the
    /// part, the way the daemon builds it.
    fn credit_ix(
        &self,
        height: u64,
        deposits: &[AnsweredDeposit],
        first: u32,
        count: u8,
    ) -> Instruction {
        let mut metas = accounts::CreditDeposits {
            config: self.config,
            vault: self.vault,
            deposit_answer: DEPOSIT_ANSWER_ID,
            system_program: SYSTEM_PROGRAM,
        }
        .to_account_metas(None);
        metas.extend(
            deposits
                .iter()
                .skip(first as usize)
                .take(usize::from(count))
                .filter_map(|deposit| deposit.target)
                .map(|target| AccountMeta::new(target, false)),
        );
        self.build(
            metas,
            instruction::CreditDeposits {
                height,
                block_hash: ecash_block(height),
                first,
                count,
            }
            .data(),
        )
    }

    /// Credits a part of the deposits of eCash block `ecash_block(height)`,
    /// whose parent is `ecash_block(height - 1)`. A stranger sends it.
    fn credit_part(
        &mut self,
        height: u64,
        deposits: &[AnsweredDeposit],
        first: u32,
        count: u8,
    ) -> Result<(), String> {
        self.deposit_answer(height, ecash_block(height - 1), deposits, first, count);
        let instruction = self.credit_ix(height, deposits, first, count);
        self.send_all(&[instruction])
    }

    fn credit(&mut self, height: u64, deposits: &[AnsweredDeposit]) -> Result<(), String> {
        self.credit_part(height, deposits, 0, deposits.len() as u8)
    }

    /// Credits one deposit to the user at the next eCash height.
    fn deposit(&mut self, sequence_number: u64, value_sats: u64) -> Result<(), String> {
        let height = self.config().credited_height + 1;
        let user = self.user.pubkey();
        self.credit(height, &[to(user, sequence_number, value_sats)])
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
    fn answer(&mut self, answer: BmmAnswer) {
        let mut data = vec![0u8; BMM_ANSWER_LEN];
        data[..8].copy_from_slice(&answer.height.to_le_bytes());
        data[8..40].copy_from_slice(&answer.block_hash);
        if let Some(commitment) = answer.commitment {
            data[40] = 1;
            data[41..73].copy_from_slice(&commitment);
        }
        data[73..105].copy_from_slice(&answer.solana_block);
        data[105] = u8::from(answer.recorded);
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

    /// The answer for a commitment at `height` in eCash block `BLOCK` to
    /// `SOLANA_BLOCK` and `payee`, with the Solana block in the record.
    fn win(&mut self, height: u64, payee: Pubkey) {
        self.answer(BmmAnswer {
            height,
            block_hash: BLOCK,
            commitment: Some(commitment_of(&SOLANA_BLOCK, &payee)),
            solana_block: SOLANA_BLOCK,
            recorded: true,
        });
    }

    fn settle_ix(&self, height: u64, solana_block: [u8; 32], payee: Pubkey) -> Instruction {
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
        self.build(
            metas,
            instruction::SettleBmm {
                height,
                block_hash: BLOCK,
                solana_block,
            }
            .data(),
        )
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

    fn settle(&mut self, height: u64, payee: Pubkey) -> Result<(), String> {
        let instruction = self.settle_ix(height, SOLANA_BLOCK, payee);
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

/// The eCash block at a height in these tests.
fn ecash_block(height: u64) -> [u8; 32] {
    let mut block = [0xe0u8; 32];
    block[..8].copy_from_slice(&height.to_le_bytes());
    block
}

fn to(target: Pubkey, sequence_number: u64, value_sats: u64) -> AnsweredDeposit {
    AnsweredDeposit {
        sequence_number,
        value_sats,
        target: Some(target),
    }
}

/// A deposit whose OP_RETURN names no pubkey.
fn nowhere(sequence_number: u64, value_sats: u64) -> AnsweredDeposit {
    AnsweredDeposit {
        sequence_number,
        value_sats,
        target: None,
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
        instruction::CreditDeposits::DISCRIMINATOR,
        &hash(b"global:credit_deposits").to_bytes()[..8]
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
fn settle_bmm_carries_the_height_the_block_hash_then_the_solana_block() {
    // The patched validator reads the question at bytes 8 to 80.
    let data = instruction::SettleBmm {
        height: 0x0102_0304_0506_0708,
        block_hash: [9u8; 32],
        solana_block: [7u8; 32],
    }
    .data();
    assert_eq!(data.len(), 80);
    assert_eq!(&data[8..16], &0x0102_0304_0506_0708u64.to_le_bytes());
    assert_eq!(&data[16..48], &[9u8; 32]);
    assert_eq!(&data[48..80], &[7u8; 32]);
}

#[test]
fn the_commitment_is_sha256_of_the_solana_block_then_the_payee() {
    let payee = Pubkey::new_from_array([4u8; 32]);
    let mut bytes = SOLANA_BLOCK.to_vec();
    bytes.extend_from_slice(payee.as_ref());
    assert_eq!(
        commitment_of(&SOLANA_BLOCK, &payee),
        solana_sha256_hasher::hash(&bytes).to_bytes()
    );
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
    assert_eq!(config.deposit_confirmations, DEPOSIT_CONFIRMATIONS);
    assert_eq!(config.deposit_lag, DEPOSIT_LAG);
    assert_eq!(config.credited_height, DEPOSIT_START);
    assert_eq!(config.credited_block, [0u8; 32]);
    assert_eq!(config.credit_index, 0);
    assert_eq!(config.stranded_lamports, 0);
}

#[test]
fn the_validator_reads_the_config_at_fixed_offsets() {
    // The patched validator reads D, the lag, and the credits by offset.
    let mut chain = Chain::start();
    let user = chain.user.pubkey();
    chain
        .credit(DEPOSIT_START + 1, &[to(user, 0, A_DEPOSIT_SATS)])
        .expect("the credit lands");
    let data = chain.svm.get_account(&chain.config).unwrap().data;
    let at = |offset: usize| u64::from_le_bytes(data[offset..offset + 8].try_into().unwrap());
    assert_eq!(&data[..8], Config::DISCRIMINATOR);
    assert_eq!(at(80), DEPOSIT_CONFIRMATIONS);
    assert_eq!(at(88), DEPOSIT_LAG);
    assert_eq!(at(96), DEPOSIT_START + 1);
    assert_eq!(&data[104..136], &ecash_block(DEPOSIT_START + 1));
}

#[test]
fn credit_deposits_carries_the_height_the_block_the_first_index_and_the_count() {
    // The patched validator reads the question at bytes 8 to 53.
    let data = instruction::CreditDeposits {
        height: 0x0102_0304_0506_0708,
        block_hash: [9u8; 32],
        first: 0x0a0b_0c0d,
        count: 7,
    }
    .data();
    assert_eq!(data.len(), 53);
    assert_eq!(&data[8..16], &0x0102_0304_0506_0708u64.to_le_bytes());
    assert_eq!(&data[16..48], &[9u8; 32]);
    assert_eq!(&data[48..52], &0x0a0b_0c0du32.to_le_bytes());
    assert_eq!(data[52], 7);
}

#[test]
fn the_deposit_answer_decodes_its_head_and_entries() {
    let target = Pubkey::new_from_array([3u8; 32]);
    let mut data = 9u64.to_le_bytes().to_vec();
    data.extend_from_slice(&[1u8; 32]);
    data.extend_from_slice(&[2u8; 32]);
    data.extend_from_slice(&5u32.to_le_bytes());
    data.extend_from_slice(&3u32.to_le_bytes());
    data.push(2);
    for (sequence_number, flag) in [(7u64, 1u8), (8, 0)] {
        data.extend_from_slice(&sequence_number.to_le_bytes());
        data.extend_from_slice(&100u64.to_le_bytes());
        data.push(flag);
        data.extend_from_slice(target.as_ref());
    }
    assert_eq!(
        data.len(),
        DEPOSIT_ANSWER_HEAD_LEN + 2 * DEPOSIT_ANSWER_ENTRY_LEN
    );
    let answer = DepositAnswer::decode(&data).unwrap();
    assert_eq!(answer.height, 9);
    assert_eq!(answer.block_hash, [1u8; 32]);
    assert_eq!(answer.prev_block_hash, [2u8; 32]);
    assert_eq!(answer.total, 5);
    assert_eq!(answer.first, 3);
    assert_eq!(answer.deposits, vec![to(target, 7, 100), nowhere(8, 100)]);
    assert_eq!(DepositAnswer::decode(&data[..data.len() - 1]), None);
    let flag = DEPOSIT_ANSWER_HEAD_LEN + 16;
    data[flag] = 2;
    assert_eq!(DepositAnswer::decode(&data), None);
}

#[test]
fn a_credit_pays_ten_lamports_for_each_satoshi_out_of_the_vault() {
    let mut chain = Chain::start();
    let user = chain.user.pubkey();
    let user_before = chain.balance(&user);
    let vault_before = chain.balance(&chain.vault);
    chain
        .credit(DEPOSIT_START + 1, &[to(user, 0, A_DEPOSIT_SATS)])
        .expect("the credit lands");
    let lamports = A_DEPOSIT_SATS * LAMPORTS_PER_SAT;
    assert_eq!(chain.balance(&user) - user_before, lamports);
    assert_eq!(vault_before - chain.balance(&chain.vault), lamports);
    let config = chain.config();
    assert_eq!(config.pegged_lamports, lamports);
    assert_eq!(config.deposit_high_water, 1);
    assert_eq!(config.credited_height, DEPOSIT_START + 1);
    assert_eq!(config.credited_block, ecash_block(DEPOSIT_START + 1));
}

#[test]
fn anyone_can_credit_and_nobody_needs_the_oracle() {
    let mut chain = Chain::start();
    let stranger = chain.a_stranger().pubkey();
    chain
        .credit(DEPOSIT_START + 1, &[to(stranger, 0, A_DEPOSIT_SATS)])
        .expect("a stranger sends the credit");
    assert_eq!(
        chain.config().pegged_lamports,
        A_DEPOSIT_SATS * LAMPORTS_PER_SAT
    );
}

#[test]
fn credits_go_in_height_order_and_an_empty_height_counts() {
    let mut chain = Chain::start();
    let user = chain.user.pubkey();
    chain
        .credit(DEPOSIT_START + 1, &[])
        .expect("an empty height");
    assert_eq!(chain.config().credited_height, DEPOSIT_START + 1);
    chain
        .credit(DEPOSIT_START + 2, &[to(user, 3, 100_000)])
        .expect("the next height");
    chain
        .credit(
            DEPOSIT_START + 3,
            &[to(user, 4, 100_000), to(user, 9, 100_000)],
        )
        .expect("the height after it");
    let config = chain.config();
    assert_eq!(config.credited_height, DEPOSIT_START + 3);
    assert_eq!(config.deposit_high_water, 10);
    assert_eq!(config.pegged_lamports, 3 * 100_000 * LAMPORTS_PER_SAT);
}

#[test]
fn a_credit_out_of_height_order_fails() {
    let mut chain = Chain::start();
    let user = chain.user.pubkey();
    assert!(chain
        .credit(DEPOSIT_START + 2, &[to(user, 0, A_DEPOSIT_SATS)])
        .is_err());
    assert!(chain
        .credit(DEPOSIT_START, &[to(user, 0, A_DEPOSIT_SATS)])
        .is_err());
    assert_eq!(chain.config().credited_height, DEPOSIT_START);
    assert_eq!(chain.config().pegged_lamports, 0);
}

#[test]
fn a_block_takes_several_parts_in_order() {
    let mut chain = Chain::start();
    let user = chain.user.pubkey();
    let height = DEPOSIT_START + 1;
    let deposits = [
        to(user, 0, 100_000),
        nowhere(1, 100_000),
        to(user, 2, 100_000),
    ];
    assert!(chain.credit_part(height, &deposits, 1, 2).is_err());
    chain
        .credit_part(height, &deposits, 0, 2)
        .expect("the first part");
    let config = chain.config();
    assert_eq!(config.credited_height, DEPOSIT_START);
    assert_eq!(config.credit_index, 2);
    assert!(chain.credit_part(height, &deposits, 0, 2).is_err());
    assert!(chain.credit_part(height, &deposits, 1, 2).is_err());
    assert!(chain.credit(height + 1, &[]).is_err());
    chain
        .credit_part(height, &deposits, 2, 1)
        .expect("the last part");
    let config = chain.config();
    assert_eq!(config.credited_height, height);
    assert_eq!(config.credit_index, 0);
    assert_eq!(config.pegged_lamports, 2 * 100_000 * LAMPORTS_PER_SAT);
    assert_eq!(config.stranded_lamports, 100_000 * LAMPORTS_PER_SAT);
}

#[test]
fn an_empty_part_of_a_block_with_deposits_fails() {
    let mut chain = Chain::start();
    let user = chain.user.pubkey();
    let deposits = [to(user, 0, 100_000)];
    assert!(chain
        .credit_part(DEPOSIT_START + 1, &deposits, 0, 0)
        .is_err());
}

#[test]
fn a_double_credit_fails() {
    let mut chain = Chain::start();
    let user = chain.user.pubkey();
    let deposits = [to(user, 4, A_DEPOSIT_SATS)];
    chain
        .credit(DEPOSIT_START + 1, &deposits)
        .expect("the first credit");
    let before = chain.balance(&user);
    assert!(chain.credit(DEPOSIT_START + 1, &deposits).is_err());
    assert!(chain.credit(DEPOSIT_START + 2, &deposits).is_err());
    assert!(chain
        .credit(DEPOSIT_START + 2, &[to(user, 3, A_DEPOSIT_SATS)])
        .is_err());
    assert_eq!(chain.balance(&user), before);
    assert_eq!(chain.config().deposit_high_water, 5);
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
fn a_block_that_does_not_follow_the_credited_block_fails() {
    let mut chain = Chain::start();
    let user = chain.user.pubkey();
    chain
        .credit(DEPOSIT_START + 1, &[])
        .expect("the first height");
    let deposits = [to(user, 0, A_DEPOSIT_SATS)];
    chain.deposit_answer(DEPOSIT_START + 2, [0xaa; 32], &deposits, 0, 1);
    let instruction = chain.credit_ix(DEPOSIT_START + 2, &deposits, 0, 1);
    assert!(chain.send_all(&[instruction]).is_err());
    assert_eq!(chain.config().credited_height, DEPOSIT_START + 1);
}

#[test]
fn a_credit_without_an_answer_or_with_another_answer_fails() {
    let mut chain = Chain::start();
    let user = chain.user.pubkey();
    let deposits = [to(user, 0, A_DEPOSIT_SATS)];
    let instruction = chain.credit_ix(DEPOSIT_START + 1, &deposits, 0, 1);
    assert!(chain.send_all(std::slice::from_ref(&instruction)).is_err());
    chain.deposit_answer(
        DEPOSIT_START + 2,
        ecash_block(DEPOSIT_START),
        &deposits,
        0,
        1,
    );
    assert!(chain.send_all(std::slice::from_ref(&instruction)).is_err());
    chain.deposit_answer(
        DEPOSIT_START + 1,
        ecash_block(DEPOSIT_START),
        &deposits,
        0,
        0,
    );
    assert!(chain.send_all(&[instruction]).is_err());
    assert_eq!(chain.config().pegged_lamports, 0);
}

#[test]
fn a_wrong_or_missing_target_fails() {
    let mut chain = Chain::start();
    let user = chain.user.pubkey();
    let thief = chain.a_stranger().pubkey();
    let height = DEPOSIT_START + 1;
    let deposits = [to(user, 0, A_DEPOSIT_SATS)];
    chain.deposit_answer(height, ecash_block(height - 1), &deposits, 0, 1);
    let mut wrong = chain.credit_ix(height, &deposits, 0, 1);
    wrong.accounts.last_mut().unwrap().pubkey = thief;
    assert!(chain.send_all(&[wrong]).is_err());
    let mut missing = chain.credit_ix(height, &deposits, 0, 1);
    missing.accounts.pop();
    assert!(chain.send_all(&[missing]).is_err());
    assert_eq!(chain.config().pegged_lamports, 0);
}

#[test]
fn a_read_only_target_fails_the_tx_and_strands_nothing() {
    let mut chain = Chain::start();
    let user = chain.user.pubkey();
    let height = DEPOSIT_START + 1;
    let deposits = [to(user, 0, A_DEPOSIT_SATS)];
    chain.deposit_answer(height, ecash_block(height - 1), &deposits, 0, 1);
    let mut instruction = chain.credit_ix(height, &deposits, 0, 1);
    instruction.accounts.last_mut().unwrap().is_writable = false;
    assert!(chain.send_all(&[instruction]).is_err());
    assert_eq!(chain.config().stranded_lamports, 0);
    assert_eq!(chain.config().credited_height, DEPOSIT_START);
}

#[test]
fn a_target_that_cannot_take_the_lamports_strands_them_in_the_vault() {
    let mut chain = Chain::start();
    let below_rent = Keypair::new().pubkey();
    let program = chain.program_id;
    let vault = chain.vault;
    let vault_before = chain.balance(&vault);
    let deposits = [
        nowhere(0, 1_000),
        to(below_rent, 1, 1_000),
        to(program, 2, 1_000),
        to(vault, 3, 1_000),
    ];
    chain
        .credit(DEPOSIT_START + 1, &deposits)
        .expect("the height credits");
    let config = chain.config();
    assert_eq!(config.stranded_lamports, 4 * 1_000 * LAMPORTS_PER_SAT);
    assert_eq!(config.pegged_lamports, 0);
    assert_eq!(config.deposit_high_water, 4);
    assert_eq!(config.credited_height, DEPOSIT_START + 1);
    assert_eq!(chain.balance(&vault), vault_before);
    assert_eq!(chain.balance(&below_rent), 0);
}

#[test]
fn both_balance_equations_hold_from_deposit_to_paid_withdrawal() {
    let mut chain = Chain::start();
    let user = chain.user.pubkey();
    // The eCash side, which the test keeps: the treasury and the deposits
    // that no credit took yet.
    let mut treasury_sats: u64 = 0;
    let mut awaiting_sats: u64 = 0;
    let check = |chain: &Chain, treasury_sats: u64, awaiting_sats: u64| {
        let config = chain.config();
        assert_eq!(
            chain.balance(&chain.vault) + config.pegged_lamports,
            VAULT_LAMPORTS
        );
        let pending: u64 = (0..config.withdrawal_count)
            .filter_map(|index| chain.record(index))
            .map(|record| record.burned_lamports)
            .sum();
        assert_eq!(
            treasury_sats * LAMPORTS_PER_SAT,
            config.pegged_lamports
                + config.stranded_lamports
                + awaiting_sats * LAMPORTS_PER_SAT
                + pending
        );
    };
    check(&chain, treasury_sats, awaiting_sats);

    let deposits = [to(user, 0, A_DEPOSIT_SATS), nowhere(1, 30_000)];
    treasury_sats += A_DEPOSIT_SATS + 30_000;
    awaiting_sats += A_DEPOSIT_SATS + 30_000;
    check(&chain, treasury_sats, awaiting_sats);

    chain
        .credit(DEPOSIT_START + 1, &deposits)
        .expect("the credit lands");
    awaiting_sats -= A_DEPOSIT_SATS + 30_000;
    check(&chain, treasury_sats, awaiting_sats);

    chain
        .withdraw(0, 5_000_000, 1_000, &A_P2WPKH)
        .expect("the withdrawal lands");
    check(&chain, treasury_sats, awaiting_sats);

    let oracle = chain.oracle.insecure_clone();
    chain.mark_paid_as(&oracle, 0).expect("mark_paid succeeds");
    treasury_sats -= 5_000_000 / LAMPORTS_PER_SAT;
    check(&chain, treasury_sats, awaiting_sats);
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
const SOLANA_BLOCK: [u8; 32] = [0x5b; 32];
const OTHER_SOLANA_BLOCK: [u8; 32] = [0x5c; 32];

#[test]
fn the_answer_decodes_found_none_and_recorded() {
    let mut data = vec![0u8; BMM_ANSWER_LEN];
    data[..8].copy_from_slice(&5u64.to_le_bytes());
    data[8..40].copy_from_slice(&BLOCK);
    data[73..105].copy_from_slice(&SOLANA_BLOCK);
    assert_eq!(
        BmmAnswer::decode(&data),
        Some(BmmAnswer {
            height: 5,
            block_hash: BLOCK,
            commitment: None,
            solana_block: SOLANA_BLOCK,
            recorded: false,
        })
    );
    data[40] = 1;
    data[41..73].copy_from_slice(&[4u8; 32]);
    data[105] = 1;
    let answer = BmmAnswer::decode(&data).unwrap();
    assert_eq!(answer.commitment, Some([4u8; 32]));
    assert!(answer.recorded);
    data[105] = 2;
    assert_eq!(BmmAnswer::decode(&data), None);
    data[105] = 1;
    data[40] = 2;
    assert_eq!(BmmAnswer::decode(&data), None);
    assert_eq!(BmmAnswer::decode(&data[..105]), None);
}

#[test]
fn a_valid_pair_pays_the_whole_treasury_above_the_reserve_to_the_payee() {
    let mut chain = Chain::start();
    let winner = chain.a_stranger().pubkey();
    let before = chain.balance(&winner);
    chain.pay_fees(3_000_000);
    chain.win(BMM_START, winner);

    chain.settle(BMM_START, winner).expect("the height settles");

    assert_eq!(chain.balance(&winner) - before, 3_000_000);
    assert_eq!(chain.balance(&chain.treasury), TREASURY_RESERVE);
    assert_eq!(chain.config().bmm_paid_total, 3_000_000);
    assert_eq!(chain.config().bmm_next_height, BMM_START + 1);
}

#[test]
fn a_height_with_no_commitment_fails() {
    let mut chain = Chain::start();
    let anyone = chain.a_stranger().pubkey();
    chain.pay_fees(1_000_000);
    chain.answer(BmmAnswer {
        height: BMM_START,
        block_hash: BLOCK,
        commitment: None,
        solana_block: SOLANA_BLOCK,
        recorded: true,
    });

    assert!(chain.settle(BMM_START, anyone).is_err());
    assert_eq!(chain.balance(&chain.treasury), TREASURY_RESERVE + 1_000_000);
    assert_eq!(chain.config().bmm_next_height, BMM_START);
}

#[test]
fn a_payee_that_is_not_in_the_pair_fails() {
    let mut chain = Chain::start();
    let winner = chain.a_stranger().pubkey();
    let thief = chain.a_stranger().pubkey();
    let before = chain.balance(&thief);
    chain.pay_fees(1_000_000);
    chain.win(BMM_START, winner);

    assert!(chain.settle(BMM_START, thief).is_err());
    assert_eq!(chain.balance(&thief), before);
    assert_eq!(chain.config().bmm_next_height, BMM_START);
}

#[test]
fn a_solana_block_that_is_not_in_the_pair_fails() {
    let mut chain = Chain::start();
    let winner = chain.a_stranger().pubkey();
    chain.pay_fees(1_000_000);
    chain.answer(BmmAnswer {
        height: BMM_START,
        block_hash: BLOCK,
        commitment: Some(commitment_of(&SOLANA_BLOCK, &winner)),
        solana_block: OTHER_SOLANA_BLOCK,
        recorded: true,
    });

    let instruction = chain.settle_ix(BMM_START, OTHER_SOLANA_BLOCK, winner);
    assert!(chain.send_all(&[instruction]).is_err());
    assert_eq!(chain.config().bmm_next_height, BMM_START);
}

#[test]
fn a_block_that_the_record_does_not_hold_fails() {
    let mut chain = Chain::start();
    let winner = chain.a_stranger().pubkey();
    chain.pay_fees(1_000_000);
    chain.answer(BmmAnswer {
        height: BMM_START,
        block_hash: BLOCK,
        commitment: Some(commitment_of(&SOLANA_BLOCK, &winner)),
        solana_block: SOLANA_BLOCK,
        recorded: false,
    });

    assert!(chain.settle(BMM_START, winner).is_err());
    assert_eq!(chain.balance(&chain.treasury), TREASURY_RESERVE + 1_000_000);
    assert_eq!(chain.config().bmm_next_height, BMM_START);
}

#[test]
fn a_payee_that_cannot_hold_the_payout_gets_nothing_and_the_height_settles() {
    let mut chain = Chain::start();
    // An account that does not exist cannot take less than its rent reserve.
    let empty = Keypair::new().pubkey();
    chain.pay_fees(1_000);
    chain.win(BMM_START, empty);

    chain.settle(BMM_START, empty).expect("the height settles");

    assert_eq!(chain.balance(&empty), 0);
    assert_eq!(chain.balance(&chain.treasury), TREASURY_RESERVE + 1_000);
    assert_eq!(chain.config().bmm_next_height, BMM_START + 1);
}

#[test]
fn a_new_payee_account_takes_a_payout_above_its_rent_reserve() {
    let mut chain = Chain::start();
    let fresh = Keypair::new().pubkey();
    chain.pay_fees(2_000_000);
    chain.win(BMM_START, fresh);

    chain.settle(BMM_START, fresh).expect("the height settles");
    assert_eq!(chain.balance(&fresh), 2_000_000);
}

#[test]
fn the_treasury_as_the_payee_gets_nothing_and_the_height_settles() {
    let mut chain = Chain::start();
    let treasury = chain.treasury;
    chain.pay_fees(2_000_000);
    chain.win(BMM_START, treasury);

    chain
        .settle(BMM_START, treasury)
        .expect("the height settles");
    assert_eq!(chain.balance(&treasury), TREASURY_RESERVE + 2_000_000);
    assert_eq!(chain.config().bmm_next_height, BMM_START + 1);
}

#[test]
fn an_executable_payee_gets_nothing_and_the_height_settles() {
    let mut chain = Chain::start();
    // The runtime refuses a lamport change on an executable account.
    let program = chain.program_id;
    chain.pay_fees(2_000_000);
    chain.win(BMM_START, program);

    chain
        .settle(BMM_START, program)
        .expect("the height settles");

    assert_eq!(chain.balance(&chain.treasury), TREASURY_RESERVE + 2_000_000);
    assert_eq!(chain.config().bmm_paid_total, 0);
    assert_eq!(chain.config().bmm_next_height, BMM_START + 1);
}

#[test]
fn a_later_settle_skips_the_earlier_heights_for_ever() {
    let mut chain = Chain::start();
    let skipped = chain.a_stranger().pubkey();
    let later = chain.a_stranger().pubkey();
    chain.win(BMM_START + 3, later);
    chain
        .settle(BMM_START + 3, later)
        .expect("a later height settles");
    assert_eq!(chain.config().bmm_next_height, BMM_START + 4);

    chain.win(BMM_START, skipped);
    assert!(chain.settle(BMM_START, skipped).is_err());
    chain.win(BMM_START + 3, later);
    assert!(chain.settle(BMM_START + 3, later).is_err());
    assert_eq!(chain.config().bmm_next_height, BMM_START + 4);
}

#[test]
fn the_fees_of_skipped_heights_roll_over_to_the_next_payee() {
    let mut chain = Chain::start();
    let first = chain.a_stranger().pubkey();
    let skipped = chain.a_stranger().pubkey();
    let later = chain.a_stranger().pubkey();
    let first_before = chain.balance(&first);
    let later_before = chain.balance(&later);

    chain.pay_fees(1_000_000);
    chain.win(BMM_START, first);
    chain.settle(BMM_START, first).expect("height one");
    chain.pay_fees(2_000_000);
    // Height two has a commitment, but nobody settles it.
    chain.pay_fees(500_000);
    chain.win(BMM_START + 2, later);
    chain.settle(BMM_START + 2, later).expect("height three");

    assert_eq!(chain.balance(&first) - first_before, 1_000_000);
    assert_eq!(chain.balance(&later) - later_before, 2_500_000);
    assert_eq!(chain.config().bmm_paid_total, 3_500_000);
    chain.win(BMM_START + 1, skipped);
    assert!(chain.settle(BMM_START + 1, skipped).is_err());
}

#[test]
fn the_cursor_takes_heights_in_order() {
    let mut chain = Chain::start();
    let payee = chain.a_stranger().pubkey();
    for height in [BMM_START, BMM_START + 1, BMM_START + 5] {
        chain.win(height, payee);
        chain
            .settle(height, payee)
            .expect("each higher height settles");
        assert_eq!(chain.config().bmm_next_height, height + 1);
    }
    chain.win(BMM_START + 4, payee);
    assert!(chain.settle(BMM_START + 4, payee).is_err());
}

#[test]
fn a_tx_without_an_answer_fails() {
    let mut chain = Chain::start();
    let anyone = chain.a_stranger().pubkey();
    assert!(chain.settle(BMM_START, anyone).is_err());
    assert_eq!(chain.config().bmm_next_height, BMM_START);
}

#[test]
fn an_answer_for_another_height_fails() {
    let mut chain = Chain::start();
    let winner = chain.a_stranger().pubkey();
    chain.win(BMM_START + 1, winner);
    assert!(chain.settle(BMM_START, winner).is_err());
}

#[test]
fn an_answer_for_another_block_fails() {
    let mut chain = Chain::start();
    let winner = chain.a_stranger().pubkey();
    chain.answer(BmmAnswer {
        height: BMM_START,
        block_hash: [0xb2; 32],
        commitment: Some(commitment_of(&SOLANA_BLOCK, &winner)),
        solana_block: SOLANA_BLOCK,
        recorded: true,
    });
    assert!(chain.settle(BMM_START, winner).is_err());
}

#[test]
fn a_second_settle_in_the_same_tx_fails_the_tx() {
    let mut chain = Chain::start();
    let winner = chain.a_stranger().pubkey();
    chain.pay_fees(1_000_000);
    chain.win(BMM_START, winner);

    let first = chain.settle_ix(BMM_START, SOLANA_BLOCK, winner);
    let second = chain.settle_ix(BMM_START + 1, SOLANA_BLOCK, winner);
    assert!(chain.send_all(&[first.clone(), second]).is_err());
    let repeat = chain.settle_ix(BMM_START, SOLANA_BLOCK, winner);
    assert!(chain.send_all(&[first, repeat]).is_err());
    assert_eq!(chain.config().bmm_next_height, BMM_START);
}
