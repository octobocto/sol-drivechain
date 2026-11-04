//! Drives the bridge program inside a local SVM.
//!
//! The test loads the SBF artifact, so build it first:
//! `cargo-build-sbf --manifest-path programs/bridge/Cargo.toml --sbf-out-dir target/deploy`

use std::path::PathBuf;

use anchor_lang::{
    prelude::{AccountMeta, Pubkey},
    AccountSerialize, Discriminator, InstructionData, Space, ToAccountMetas,
};
use litesvm::LiteSVM;
use sol_drivechain_bridge::{
    accounts, commitment_of, instruction, min_fee_sats, AnsweredDeposit, BmmAnswer, Config,
    DepositAnswer, WithdrawalRecord, BMM_ANSWER_ID, BMM_ANSWER_LEN, DEPOSIT_ANSWER_ENTRY_LEN,
    DEPOSIT_ANSWER_HEAD_LEN, DEPOSIT_ANSWER_ID, LAMPORTS_PER_SAT, MAX_BUNDLE_RECORDS,
    MAX_SCRIPT_PUBKEY_LEN, MIN_FEE_SATS, PAIR_WINDOW,
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

/// The slots that a commitment without a pair blocks the cursor.
const BMM_SKIP_SLOTS: u64 = 1_000;

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
        Self::start_with(0, 0)
    }

    /// A chain whose genesis holds `float_lamports` of float and asks for
    /// `bundle_min_slots` between a seal and its payment.
    fn start_with(float_lamports: u64, bundle_min_slots: u64) -> Self {
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

        let mut config_data = genesis_config(oracle.pubkey(), float_lamports, bundle_min_slots);
        // The cursor of the genesis moved at the first slot of the chain.
        let start_slot = svm.get_sysvar::<anchor_lang::prelude::Clock>().slot;
        config_data[251..259].copy_from_slice(&start_slot.to_le_bytes());
        let config_lamports = svm.minimum_balance_for_rent_exemption(config_data.len());
        svm.set_account(
            config,
            Account {
                lamports: config_lamports,
                data: config_data,
                owner: program_id,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();

        Self {
            svm,
            program_id,
            config,
            vault,
            treasury,
            oracle,
            user,
        }
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
        let block_hash = ecash_block(height);
        self.block_answer(height, block_hash, prev_block_hash, deposits, first, count);
    }

    /// The same as `deposit_answer`, for the eCash block `block_hash`.
    fn block_answer(
        &mut self,
        height: u64,
        block_hash: [u8; 32],
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
        data.extend_from_slice(&block_hash);
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
        self.block_credit_ix(height, ecash_block(height), deposits, first, count)
    }

    /// The same as `credit_ix`, for the eCash block `block_hash`.
    fn block_credit_ix(
        &self,
        height: u64,
        block_hash: [u8; 32],
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
                block_hash,
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
        // A repeat of a failed withdrawal must run again, not read as a replay.
        self.svm.expire_blockhash();
        self.send(instruction, &user)
    }

    fn seal_as(&mut self, signer: &Keypair) -> Result<(), String> {
        let instruction = self.build(
            accounts::SealBundle {
                config: self.config,
                oracle: signer.pubkey(),
            }
            .to_account_metas(None),
            instruction::SealBundle {}.data(),
        );
        let signer = signer.insecure_clone();
        self.svm.expire_blockhash();
        self.send(instruction, &signer)
    }

    fn seal(&mut self) -> Result<(), String> {
        let oracle = self.oracle.insecure_clone();
        self.seal_as(&oracle)
    }

    fn bundle_paid_as(&mut self, signer: &Keypair, through: u64) -> Result<(), String> {
        let instruction = self.build(
            accounts::BundlePaid {
                config: self.config,
                oracle: signer.pubkey(),
            }
            .to_account_metas(None),
            instruction::BundlePaid { through }.data(),
        );
        let signer = signer.insecure_clone();
        self.svm.expire_blockhash();
        self.send(instruction, &signer)
    }

    fn bundle_paid(&mut self, through: u64) -> Result<(), String> {
        let oracle = self.oracle.insecure_clone();
        self.bundle_paid_as(&oracle, through)
    }

    /// Closes a record, sent by a stranger.
    fn close_record(&mut self, index: u64) -> Result<(), String> {
        let instruction = self.build(
            accounts::CloseRecord {
                config: self.config,
                record: self.record_key(index),
                owner: self.user.pubkey(),
            }
            .to_account_metas(None),
            instruction::CloseRecord { index }.data(),
        );
        self.send_all(&[instruction])
    }

    /// Seals the open records, marks the bundle paid, and closes each record.
    fn pay_out(&mut self) -> Result<(), String> {
        self.seal()?;
        let config = self.config();
        self.bundle_paid(config.bundled_through)?;
        for index in config.paid_through..config.bundled_through {
            self.close_record(index)?;
        }
        Ok(())
    }

    /// Sends lamports to the vault, the way any wallet can.
    fn gift_vault(&mut self, lamports: u64) {
        let mut account = self.svm.get_account(&self.vault).expect("vault");
        account.lamports += lamports;
        self.svm.set_account(self.vault, account).unwrap();
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
        data[106..114].copy_from_slice(&answer.credited_height.to_le_bytes());
        data[114] = u8::from(answer.credits_active);
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
        self.win_with_credits(height, payee, DEPOSIT_START, true);
    }

    /// The same as `win`, with the credits that the record entry of the
    /// Solana block holds: the credited eCash height, and whether the active
    /// eCash chain still holds the credited blocks.
    fn win_with_credits(
        &mut self,
        height: u64,
        payee: Pubkey,
        credited_height: u64,
        credits_active: bool,
    ) {
        self.answer(BmmAnswer {
            height,
            block_hash: BLOCK,
            commitment: Some(commitment_of(&SOLANA_BLOCK, &payee)),
            solana_block: SOLANA_BLOCK,
            recorded: true,
            credited_height,
            credits_active,
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

    /// Moves the BMM cursor to `height`, as if each height below it settled.
    fn set_next_height(&mut self, height: u64) {
        let mut account = self.svm.get_account(&self.config).unwrap();
        account.data[64..72].copy_from_slice(&height.to_le_bytes());
        self.svm.set_account(self.config, account).unwrap();
    }

    /// Sets `credited_height`, as if the credits reached it.
    fn set_credited_height(&mut self, height: u64) {
        let mut account = self.svm.get_account(&self.config).unwrap();
        account.data[96..104].copy_from_slice(&height.to_le_bytes());
        self.svm.set_account(self.config, account).unwrap();
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

/// The config account that the genesis writes, in the Anchor layout.
fn genesis_config(oracle: Pubkey, float_lamports: u64, bundle_min_slots: u64) -> Vec<u8> {
    let program_id = sol_drivechain_bridge::ID;
    let bump = |seed: &[u8]| Pubkey::find_program_address(&[seed], &program_id).1;
    let config = Config {
        oracle,
        deposit_high_water: 0,
        pegged_lamports: 0,
        withdrawal_count: 0,
        bmm_next_height: BMM_START,
        bmm_paid_total: 0,
        deposit_confirmations: DEPOSIT_CONFIRMATIONS,
        deposit_lag: DEPOSIT_LAG,
        credited_height: DEPOSIT_START,
        credited_block: [0; 32],
        credit_index: 0,
        credit_part_block: [0; 32],
        stranded_lamports: 0,
        bump: bump(b"config"),
        vault_bump: bump(b"vault"),
        treasury_bump: bump(b"treasury"),
        vault_supply: VAULT_LAMPORTS,
        float_lamports,
        paid_through: 0,
        bundled_through: 0,
        bundle_sealed_height: 0,
        bundle_sealed_slot: 0,
        bundle_min_slots,
        bmm_skip_slots: BMM_SKIP_SLOTS,
        bmm_next_since_slot: 0,
    };
    let mut data = Vec::new();
    config
        .try_serialize(&mut data)
        .expect("the config serializes");
    data.resize(8 + Config::INIT_SPACE, 0);
    data
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
        instruction::SealBundle::DISCRIMINATOR,
        &hash(b"global:seal_bundle").to_bytes()[..8]
    );
    assert_eq!(
        instruction::BundlePaid::DISCRIMINATOR,
        &hash(b"global:bundle_paid").to_bytes()[..8]
    );
    assert_eq!(
        instruction::CloseRecord::DISCRIMINATOR,
        &hash(b"global:close_record").to_bytes()[..8]
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
fn the_genesis_config_has_the_layout_that_the_daemon_writes() {
    // The daemon writes the genesis config by hand at these offsets.
    let oracle = Pubkey::new_from_array([5u8; 32]);
    let data = genesis_config(oracle, 7, 9);
    let at = |offset: usize| u64::from_le_bytes(data[offset..offset + 8].try_into().unwrap());
    assert_eq!(data.len(), 259);
    assert_eq!(&data[..8], Config::DISCRIMINATOR);
    assert_eq!(&data[8..40], oracle.as_ref());
    assert_eq!(at(64), BMM_START);
    assert_eq!(at(80), DEPOSIT_CONFIRMATIONS);
    assert_eq!(at(88), DEPOSIT_LAG);
    assert_eq!(at(96), DEPOSIT_START);
    let program_id = sol_drivechain_bridge::ID;
    let bump = |seed: &[u8]| Pubkey::find_program_address(&[seed], &program_id).1;
    assert_eq!(
        data[184..187],
        [bump(b"config"), bump(b"vault"), bump(b"treasury")]
    );
    assert_eq!(at(187), VAULT_LAMPORTS);
    assert_eq!(at(195), 7);
    assert_eq!(at(235), 9);
    assert_eq!(at(243), BMM_SKIP_SLOTS);
    assert_eq!(at(251), 0);
}

#[test]
fn the_program_takes_the_genesis_config() {
    let mut chain = Chain::start();
    let config = chain.config();
    assert_eq!(config.oracle, chain.oracle.pubkey());
    assert_eq!(config.bmm_next_height, BMM_START);
    assert_eq!(config.credited_height, DEPOSIT_START);
    let user = chain.user.pubkey();
    chain
        .credit(DEPOSIT_START + 1, &[to(user, 0, A_DEPOSIT_SATS)])
        .expect("a credit against the genesis config lands");
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
    let deposits = [to(user, 1, 100_000), to(user, 2, 100_000)];
    chain
        .credit_part(DEPOSIT_START + 2, &deposits, 0, 1)
        .expect("the first part lands");
    let data = chain.svm.get_account(&chain.config).unwrap().data;
    let at = |offset: usize| u64::from_le_bytes(data[offset..offset + 8].try_into().unwrap());
    assert_eq!(at(136), 1);
    assert_eq!(&data[144..176], &ecash_block(DEPOSIT_START + 2));
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
    assert_eq!(config.credit_part_block, ecash_block(height));
    assert!(chain.credit_part(height, &deposits, 0, 2).is_err());
    assert!(chain.credit_part(height, &deposits, 1, 2).is_err());
    assert!(chain.credit(height + 1, &[]).is_err());
    chain
        .credit_part(height, &deposits, 2, 1)
        .expect("the last part");
    let config = chain.config();
    assert_eq!(config.credited_height, height);
    assert_eq!(config.credit_index, 0);
    assert_eq!(config.credit_part_block, [0; 32]);
    assert_eq!(config.pegged_lamports, 2 * 100_000 * LAMPORTS_PER_SAT);
    assert_eq!(config.stranded_lamports, 100_000 * LAMPORTS_PER_SAT);
}

#[test]
fn a_part_of_another_block_at_the_same_height_fails() {
    let mut chain = Chain::start();
    let user = chain.user.pubkey();
    let height = DEPOSIT_START + 1;
    let parent = ecash_block(height - 1);
    let old_block = ecash_block(height);
    let new_block = [0xb7; 32];
    let old_deposits = [
        to(user, 0, 500_000),
        to(user, 1, 500_000),
        to(user, 2, 100_000),
    ];
    chain.block_answer(height, old_block, parent, &old_deposits, 0, 2);
    let first = chain.block_credit_ix(height, old_block, &old_deposits, 0, 2);
    chain.send_all(&[first]).expect("the first part");
    let pegged = chain.config().pegged_lamports;

    // An eCash reorg replaced the block at this height.
    let new_deposits = [
        to(user, 0, 100_000),
        to(user, 1, 100_000),
        to(user, 2, 100_000),
    ];
    chain.block_answer(height, new_block, parent, &new_deposits, 2, 1);
    let rest = chain.block_credit_ix(height, new_block, &new_deposits, 2, 1);
    assert!(chain.send_all(&[rest]).is_err());
    let config = chain.config();
    assert_eq!(config.pegged_lamports, pegged);
    assert_eq!(config.credited_height, DEPOSIT_START);
    assert_eq!(config.credit_index, 2);
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
fn a_target_that_the_runtime_makes_read_only_strands_and_the_next_height_credits() {
    let mut chain = Chain::start();
    let user = chain.user.pubkey();
    let vault = chain.vault;
    let vault_before = chain.balance(&vault);
    let reserved = [
        SYSVAR_PROGRAM,
        Pubkey::from_str_const("SysvarC1ock11111111111111111111111111111111"),
        Pubkey::from_str_const("NativeLoader1111111111111111111111111111111"),
        Pubkey::from_str_const("ZkTokenProof1111111111111111111111111111111"),
        Pubkey::from_str_const("BPFLoaderUpgradeab1e11111111111111111111111"),
    ];
    let mut deposits: Vec<AnsweredDeposit> = reserved
        .iter()
        .enumerate()
        .map(|(index, key)| to(*key, index as u64, 1_000))
        .collect();
    deposits.push(to(user, reserved.len() as u64, A_DEPOSIT_SATS));
    chain
        .credit(DEPOSIT_START + 1, &deposits)
        .expect("a deposit to a reserved key strands, and the height credits");
    let config = chain.config();
    let stranded = reserved.len() as u64 * 1_000 * LAMPORTS_PER_SAT;
    assert_eq!(config.stranded_lamports, stranded);
    assert_eq!(config.pegged_lamports, A_DEPOSIT_SATS * LAMPORTS_PER_SAT);
    assert_eq!(config.credited_height, DEPOSIT_START + 1);
    assert_eq!(
        vault_before - chain.balance(&vault),
        A_DEPOSIT_SATS * LAMPORTS_PER_SAT
    );
    chain
        .credit(DEPOSIT_START + 2, &[to(user, 10, A_DEPOSIT_SATS)])
        .expect("the next height credits");
}

#[test]
fn the_bridge_knows_every_reserved_account_key_of_agave() {
    use agave_reserved_account_keys::ReservedAccountKeys;
    for key in ReservedAccountKeys::all_keys_iter() {
        assert!(
            sol_drivechain_bridge::RESERVED_KEYS.contains(key),
            "the bridge does not know the reserved key {key}"
        );
    }
}

#[test]
fn a_creditable_target_sent_read_only_still_fails_the_tx() {
    let mut chain = Chain::start();
    let user = chain.user.pubkey();
    let height = DEPOSIT_START + 1;
    let deposits = [to(SYSVAR_PROGRAM, 0, 1_000), to(user, 1, A_DEPOSIT_SATS)];
    chain.deposit_answer(height, ecash_block(height - 1), &deposits, 0, 2);
    let mut instruction = chain.credit_ix(height, &deposits, 0, 2);
    for meta in instruction.accounts.iter_mut().rev().take(2) {
        meta.is_writable = false;
    }
    assert!(chain.send_all(&[instruction]).is_err());
    assert_eq!(chain.config().credited_height, DEPOSIT_START);
    assert_eq!(chain.config().stranded_lamports, 0);
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
        let pending: u64 = (config.paid_through..config.withdrawal_count)
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

    chain.pay_out().expect("the bundle pays out");
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

fn check_treasury_script(opcode: u8) {
    let mut chain = Chain::start();
    chain
        .deposit(0, A_DEPOSIT_SATS)
        .expect("the deposit passes");
    for slot in 0..=u8::MAX {
        let index = u64::from(slot);
        let fee_sats = min_fee_sats(index);
        let lamports = (fee_sats + 600) * LAMPORTS_PER_SAT;
        let config = chain.svm.get_account(&chain.config).unwrap();
        let vault = chain.svm.get_account(&chain.vault).unwrap();
        let user_balance = chain.balance(&chain.user.pubkey());
        let fee = 5_000;

        assert!(
            chain
                .withdraw(index, lamports, fee_sats, &[opcode, 0x01, slot, 0x51])
                .is_err(),
            "the bridge accepts treasury opcode {opcode:#x} for slot {slot}"
        );

        assert_eq!(chain.svm.get_account(&chain.config), Some(config));
        assert_eq!(chain.svm.get_account(&chain.vault), Some(vault));
        assert_eq!(chain.balance(&chain.user.pubkey()), user_balance - fee);
        assert!(chain.svm.get_account(&chain.record_key(index)).is_none());

        chain
            .withdraw(index, lamports, fee_sats, &A_P2WPKH)
            .expect("the valid withdrawal passes after the rejection");
        assert_eq!(chain.config().withdrawal_count, index + 1);
        assert_eq!(chain.record(index).unwrap().script_pubkey, A_P2WPKH);
    }
    chain.seal().expect("the valid bundle seals");
    assert_eq!(chain.config().bundled_through, 256);
}

#[test]
fn a_nop5_treasury_script_fails_for_each_slot() {
    check_treasury_script(0xb4);
}

#[test]
fn a_nop8_treasury_script_fails_for_each_slot() {
    check_treasury_script(0xb7);
}

fn a_hash(len: usize) -> Vec<u8> {
    (1..=len as u8).collect()
}

/// The five standard templates, byte for byte.
fn standard_scripts() -> Vec<Vec<u8>> {
    vec![
        [&[0x76, 0xa9, 0x14][..], &a_hash(20), &[0x88, 0xac]].concat(),
        [&[0xa9, 0x14][..], &a_hash(20), &[0x87]].concat(),
        [&[0x00, 0x14][..], &a_hash(20)].concat(),
        [&[0x00, 0x20][..], &a_hash(32)].concat(),
        [&[0x51, 0x20][..], &a_hash(32)].concat(),
    ]
}

#[test]
fn each_standard_template_passes() {
    let mut chain = Chain::start();
    chain
        .deposit(0, A_DEPOSIT_SATS)
        .expect("the deposit passes");
    for script in standard_scripts() {
        assert!(sol_drivechain_bridge::is_standard_script(&script));
        let index = chain.config().withdrawal_count;
        let fee_sats = min_fee_sats(index);
        let lamports = (fee_sats + 600) * LAMPORTS_PER_SAT;
        chain
            .withdraw(index, lamports, fee_sats, &script)
            .expect("the standard withdrawal passes");
        assert_eq!(chain.record(index).unwrap().script_pubkey, script);
    }
}

#[test]
fn every_other_script_fails() {
    let mut chain = Chain::start();
    chain
        .deposit(0, A_DEPOSIT_SATS)
        .expect("the deposit passes");
    let lamports = (MIN_FEE_SATS + 600) * LAMPORTS_PER_SAT;
    let mut bad: Vec<Vec<u8>> = Vec::new();
    for opcode in [0xb4u8, 0xb7] {
        bad.push(vec![opcode, 0x01, 9, 0x51]);
        bad.push(vec![opcode, 0x01, 9, 0x51, 0x51]);
        bad.push(vec![0x51, opcode, 0x01, 9, 0x51]);
        bad.push(vec![opcode, 0x4c, 0x01, 9, 0x51]);
        bad.push([&[0x00, 0x13][..], &a_hash(19)].concat());
    }
    for script in standard_scripts() {
        // One byte short, one byte long, and one wrong push length.
        bad.push(script[..script.len() - 1].to_vec());
        bad.push([&script[..], &[0x00]].concat());
        let mut wrong = script.clone();
        let push = if script[0] == 0x76 { 2 } else { 1 };
        wrong[push] ^= 0x01;
        bad.push(wrong);
    }
    bad.push([&[0x52, 0x20][..], &a_hash(32)].concat());
    bad.push([&[0x21][..], &a_hash(33), &[0xac]].concat());
    bad.push(vec![0x6a, 0x04, 1, 2, 3, 4]);
    bad.push(vec![0x51]);
    bad.push(Vec::new());
    for script in bad {
        assert!(
            !sol_drivechain_bridge::is_standard_script(&script),
            "{script:02x?} reads as standard"
        );
        let index = chain.config().withdrawal_count;
        assert!(
            chain
                .withdraw(index, lamports, MIN_FEE_SATS, &script)
                .is_err(),
            "the bridge takes {script:02x?}"
        );
    }
    assert_eq!(chain.config().withdrawal_count, 0);
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
        .withdraw(1, 2_000_000, 1_010, &A_P2WPKH)
        .expect("the second withdrawal lands");
    assert_eq!(chain.config().withdrawal_count, 2);
    assert!(chain.record(0).is_some());
    assert!(chain.record(1).is_some());
}

#[test]
fn a_paid_record_closes_and_returns_the_rent() {
    let mut chain = Chain::start();
    chain.deposit(0, A_DEPOSIT_SATS).expect("the deposit lands");
    chain
        .withdraw(0, 5_000_000, 1_000, &A_P2WPKH)
        .expect("the withdrawal lands");

    let rent = chain.balance(&chain.record_key(0));
    assert!(rent > 0);
    let before = chain.balance(&chain.user.pubkey());

    chain.pay_out().expect("the bundle pays out");

    assert!(chain.record(0).is_none());
    assert_eq!(chain.balance(&chain.user.pubkey()) - before, rent);
}

#[test]
fn only_the_oracle_seals_and_marks_a_bundle_paid() {
    let mut chain = Chain::start();
    chain.deposit(0, A_DEPOSIT_SATS).expect("the deposit lands");
    chain
        .withdraw(0, 5_000_000, 1_000, &A_P2WPKH)
        .expect("the withdrawal lands");

    let thief = chain.a_stranger();
    assert!(chain.seal_as(&thief).is_err());
    chain.seal().expect("the oracle seals");
    assert!(chain.bundle_paid_as(&thief, 1).is_err());
    assert_eq!(chain.config().paid_through, 0);
    assert!(chain.close_record(0).is_err());
    assert!(chain.record(0).is_some());
}

#[test]
fn a_record_stays_in_its_bundle_until_the_bundle_pays() {
    let mut chain = Chain::start();
    chain.deposit(0, A_DEPOSIT_SATS).expect("the deposit lands");
    chain
        .withdraw(0, 5_000_000, 1_000, &A_P2WPKH)
        .expect("the first withdrawal lands");
    chain.seal().expect("the first bundle seals");
    chain
        .withdraw(1, 2_000_000, 1_000, &A_P2WPKH)
        .expect("the second withdrawal lands");

    assert!(chain.seal().is_err(), "a second live bundle");
    let config = chain.config();
    assert_eq!((config.paid_through, config.bundled_through), (0, 1));

    chain.bundle_paid(1).expect("the first bundle pays");
    assert!(chain.bundle_paid(1).is_err(), "a repeat of the payment");
    chain.seal().expect("the second bundle seals");
    let config = chain.config();
    assert_eq!((config.paid_through, config.bundled_through), (1, 2));
    assert!(chain.bundle_paid(1).is_err(), "a stale payment");
}

#[test]
fn a_seal_without_an_open_record_fails() {
    let mut chain = Chain::start();
    assert!(chain.seal().is_err());
    assert!(chain.bundle_paid(0).is_err());
}

#[test]
fn a_bundle_holds_at_most_the_record_cap() {
    let mut chain = Chain::start();
    chain.deposit(0, 100_000_000).expect("the deposit lands");
    for index in 0..=MAX_BUNDLE_RECORDS {
        let fee_sats = min_fee_sats(index);
        let each = (fee_sats + 600) * LAMPORTS_PER_SAT;
        chain
            .withdraw(index, each, fee_sats, &A_P2WPKH)
            .expect("the withdrawal lands");
    }
    chain.seal().expect("the bundle seals");
    assert_eq!(chain.config().bundled_through, MAX_BUNDLE_RECORDS);
    chain
        .bundle_paid(MAX_BUNDLE_RECORDS)
        .expect("the bundle pays");
    chain.seal().expect("the rest seals");
    assert_eq!(chain.config().bundled_through, MAX_BUNDLE_RECORDS + 1);
}

#[test]
fn a_bundle_younger_than_the_minimum_cannot_be_marked_paid() {
    let mut chain = Chain::start_with(0, 1_000);
    chain.deposit(0, A_DEPOSIT_SATS).expect("the deposit lands");
    chain
        .withdraw(0, 5_000_000, 1_000, &A_P2WPKH)
        .expect("the withdrawal lands");
    chain.seal().expect("the bundle seals");
    let sealed = chain.config().bundle_sealed_slot;
    chain.svm.warp_to_slot(sealed + 999);
    assert!(chain.bundle_paid(1).is_err());
    chain.svm.warp_to_slot(sealed + 1_000);
    chain
        .bundle_paid(1)
        .expect("the bundle pays at the minimum age");
}

#[test]
fn the_seal_keeps_the_ecash_height_of_the_credits() {
    let mut chain = Chain::start();
    chain.deposit(0, A_DEPOSIT_SATS).expect("the deposit lands");
    chain
        .withdraw(0, 5_000_000, 1_000, &A_P2WPKH)
        .expect("the withdrawal lands");
    chain.seal().expect("the bundle seals");
    assert_eq!(chain.config().bundle_sealed_height, DEPOSIT_START + 1);
}

#[test]
fn a_fee_below_the_minimum_fails() {
    let mut chain = Chain::start();
    chain.deposit(0, A_DEPOSIT_SATS).expect("the deposit lands");
    assert!(chain
        .withdraw(0, 5_000_000, MIN_FEE_SATS - 1, &A_P2WPKH)
        .is_err());
    chain
        .withdraw(0, 5_000_000, MIN_FEE_SATS, &A_P2WPKH)
        .expect("the minimum fee lands");
}

#[test]
fn the_float_cannot_peg_out_before_gifts_back_it() {
    let float = 3_000_000;
    let mut chain = Chain::start_with(float, 0);
    chain.deposit(0, A_DEPOSIT_SATS).expect("the deposit lands");
    assert!(chain.withdraw(0, 5_000_000, 1_000, &A_P2WPKH).is_err());
    chain.gift_vault(float - 1);
    assert!(chain.withdraw(0, 5_000_000, 1_000, &A_P2WPKH).is_err());
    chain.gift_vault(1);
    chain
        .withdraw(0, 5_000_000, 1_000, &A_P2WPKH)
        .expect("the backed float lets the withdrawal land");
}

#[test]
fn a_gift_to_the_vault_backs_nothing_beyond_the_pegged_total() {
    let mut chain = Chain::start();
    chain.deposit(0, 100_000).expect("the deposit lands");
    chain.gift_vault(100_000_000);
    assert!(chain.withdraw(0, 2_000_000, 1_000, &A_P2WPKH).is_err());
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
fn the_answer_decodes_found_none_recorded_and_the_credits() {
    let mut data = vec![0u8; BMM_ANSWER_LEN];
    data[..8].copy_from_slice(&5u64.to_le_bytes());
    data[8..40].copy_from_slice(&BLOCK);
    data[73..105].copy_from_slice(&SOLANA_BLOCK);
    data[106..114].copy_from_slice(&DEPOSIT_START.to_le_bytes());
    data[114] = 1;
    assert_eq!(
        BmmAnswer::decode(&data),
        Some(BmmAnswer {
            height: 5,
            block_hash: BLOCK,
            commitment: None,
            solana_block: SOLANA_BLOCK,
            recorded: false,
            credited_height: DEPOSIT_START,
            credits_active: true,
        })
    );
    data[114] = 2;
    assert_eq!(BmmAnswer::decode(&data), None);
    data[114] = 1;
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
    assert_eq!(BmmAnswer::decode(&data[..114]), None);
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
fn a_checkpoint_with_too_few_credits_moves_the_cursor_and_pays_nothing() {
    let mut chain = Chain::start();
    let winner = chain.a_stranger().pubkey();
    let height = DEPOSIT_START + DEPOSIT_LAG;
    chain.set_next_height(height);
    chain.pay_fees(3_000_000);
    chain.win_with_credits(height, winner, DEPOSIT_START, true);
    chain
        .settle(height, winner)
        .expect("a checkpoint at exactly the credited height + lag pays");
    assert_eq!(chain.config().bmm_paid_total, 3_000_000);

    chain.pay_fees(1_000_000);
    let before = chain.balance(&winner);
    chain.win_with_credits(height + 1, winner, DEPOSIT_START, true);
    chain
        .settle(height + 1, winner)
        .expect("an invalid checkpoint moves the cursor");
    assert_eq!(chain.balance(&winner), before);
    assert_eq!(chain.balance(&chain.treasury), TREASURY_RESERVE + 1_000_000);
    assert_eq!(chain.config().bmm_next_height, height + 2);
}

#[test]
fn a_checkpoint_whose_credits_left_the_chain_moves_the_cursor_and_pays_nothing() {
    let mut chain = Chain::start();
    let winner = chain.a_stranger().pubkey();
    chain.pay_fees(3_000_000);
    chain.win_with_credits(BMM_START, winner, DEPOSIT_START, false);

    chain
        .settle(BMM_START, winner)
        .expect("an invalid checkpoint moves the cursor");
    assert_eq!(chain.balance(&chain.treasury), TREASURY_RESERVE + 3_000_000);
    assert_eq!(chain.config().bmm_next_height, BMM_START + 1);
}

#[test]
fn a_height_with_no_commitment_moves_the_cursor_and_pays_nothing() {
    let mut chain = Chain::start();
    let anyone = chain.a_stranger().pubkey();
    let before = chain.balance(&anyone);
    chain.pay_fees(1_000_000);
    chain.answer(BmmAnswer {
        height: BMM_START,
        block_hash: BLOCK,
        commitment: None,
        solana_block: SOLANA_BLOCK,
        recorded: true,
        credited_height: DEPOSIT_START,
        credits_active: true,
    });

    chain
        .settle(BMM_START, anyone)
        .expect("an empty height settles");
    assert_eq!(chain.balance(&anyone), before);
    assert_eq!(chain.balance(&chain.treasury), TREASURY_RESERVE + 1_000_000);
    assert_eq!(chain.config().bmm_next_height, BMM_START + 1);
}

#[test]
fn a_payee_that_is_not_in_the_pair_fails_until_the_skip_time() {
    let mut chain = Chain::start();
    let winner = chain.a_stranger().pubkey();
    let thief = chain.a_stranger().pubkey();
    let before = chain.balance(&thief);
    chain.pay_fees(1_000_000);
    chain.win(BMM_START, winner);

    let since = chain.config().bmm_next_since_slot;
    chain.svm.warp_to_slot(since + BMM_SKIP_SLOTS - 1);
    assert!(chain.settle(BMM_START, thief).is_err());
    assert_eq!(chain.config().bmm_next_height, BMM_START);

    chain.svm.warp_to_slot(since + BMM_SKIP_SLOTS);
    chain
        .settle(BMM_START, thief)
        .expect("after the skip time the height moves on");
    assert_eq!(chain.balance(&thief), before);
    assert_eq!(chain.balance(&chain.treasury), TREASURY_RESERVE + 1_000_000);
    assert_eq!(chain.config().bmm_next_height, BMM_START + 1);
    assert_eq!(chain.config().bmm_next_since_slot, since + BMM_SKIP_SLOTS);
}

#[test]
fn a_cursor_below_the_pair_window_skips_each_height_at_once() {
    let mut chain = Chain::start();
    let withheld = chain.a_stranger().pubkey();
    let anyone = chain.a_stranger().pubkey();
    chain.pay_fees(1_000_000);
    chain.set_credited_height(BMM_START + 2 + PAIR_WINDOW);
    let since = chain.config().bmm_next_since_slot;
    chain.svm.warp_to_slot(since + 1);
    for height in [BMM_START, BMM_START + 1] {
        chain.win(height, withheld);
        chain
            .settle(height, anyone)
            .expect("a height below the pair window moves on at once");
        assert_eq!(chain.config().bmm_next_height, height + 1);
    }
    chain.win(BMM_START + 2, withheld);
    assert!(chain.settle(BMM_START + 2, anyone).is_err());
    assert_eq!(chain.config().bmm_next_height, BMM_START + 2);
    assert_eq!(chain.balance(&chain.treasury), TREASURY_RESERVE + 1_000_000);
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
        credited_height: DEPOSIT_START,
        credits_active: true,
    });

    let instruction = chain.settle_ix(BMM_START, OTHER_SOLANA_BLOCK, winner);
    assert!(chain.send_all(&[instruction]).is_err());
    assert_eq!(chain.config().bmm_next_height, BMM_START);
}

#[test]
fn a_block_that_the_record_does_not_hold_moves_the_cursor_and_pays_nothing() {
    let mut chain = Chain::start();
    let winner = chain.a_stranger().pubkey();
    chain.pay_fees(1_000_000);
    chain.answer(BmmAnswer {
        height: BMM_START,
        block_hash: BLOCK,
        commitment: Some(commitment_of(&SOLANA_BLOCK, &winner)),
        solana_block: SOLANA_BLOCK,
        recorded: false,
        credited_height: DEPOSIT_START,
        credits_active: true,
    });

    chain
        .settle(BMM_START, winner)
        .expect("an invalid checkpoint moves the cursor");
    assert_eq!(chain.balance(&chain.treasury), TREASURY_RESERVE + 1_000_000);
    assert_eq!(chain.config().bmm_next_height, BMM_START + 1);
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
fn a_later_height_cannot_settle_before_the_next_one() {
    let mut chain = Chain::start();
    let first = chain.a_stranger().pubkey();
    let later = chain.a_stranger().pubkey();
    chain.pay_fees(1_000_000);
    chain.win(BMM_START + 1, later);
    assert!(chain.settle(BMM_START + 1, later).is_err());
    assert_eq!(chain.config().bmm_next_height, BMM_START);

    let before = chain.balance(&first);
    chain.win(BMM_START, first);
    chain
        .settle(BMM_START, first)
        .expect("the next height settles");
    assert_eq!(chain.balance(&first) - before, 1_000_000);
    chain.pay_fees(500_000);
    let before = chain.balance(&later);
    chain.win(BMM_START + 1, later);
    chain
        .settle(BMM_START + 1, later)
        .expect("the height after it settles in order");
    assert_eq!(chain.balance(&later) - before, 500_000);
}

#[test]
fn a_leader_cannot_put_the_later_settle_first_in_one_block() {
    let mut chain = Chain::start();
    let first = chain.a_stranger().pubkey();
    let later = chain.a_stranger().pubkey();
    chain.pay_fees(1_000_000);
    chain.win(BMM_START + 1, later);
    let early = chain.settle_ix(BMM_START + 1, SOLANA_BLOCK, later);
    assert!(chain.send_all(&[early]).is_err());
    chain.win(BMM_START, first);
    let before = chain.balance(&first);
    chain
        .settle(BMM_START, first)
        .expect("the next height settles");
    assert_eq!(chain.balance(&first) - before, 1_000_000);
}

#[test]
fn the_fees_of_a_height_without_a_pair_roll_over_after_the_skip_time() {
    let mut chain = Chain::start();
    let first = chain.a_stranger().pubkey();
    let withheld = chain.a_stranger().pubkey();
    let later = chain.a_stranger().pubkey();
    let later_before = chain.balance(&later);

    chain.pay_fees(1_000_000);
    chain.win(BMM_START, first);
    chain.settle(BMM_START, first).expect("height one");
    let since = chain.config().bmm_next_since_slot;
    chain.pay_fees(2_000_000);
    // Height two has a commitment, and nobody publishes its pair.
    chain.win(BMM_START + 1, withheld);
    let anyone = chain.a_stranger().pubkey();
    chain.svm.warp_to_slot(since + BMM_SKIP_SLOTS - 1);
    assert!(chain.settle(BMM_START + 1, anyone).is_err());
    chain.svm.warp_to_slot(since + BMM_SKIP_SLOTS);
    chain
        .settle(BMM_START + 1, anyone)
        .expect("the skip moves the cursor");
    chain.pay_fees(500_000);
    chain.win(BMM_START + 2, later);
    chain.settle(BMM_START + 2, later).expect("height three");

    assert_eq!(chain.balance(&later) - later_before, 2_500_000);
    assert_eq!(chain.config().bmm_paid_total, 3_500_000);
    chain.win(BMM_START + 1, withheld);
    assert!(chain.settle(BMM_START + 1, withheld).is_err());
}

#[test]
fn the_cursor_takes_heights_in_order() {
    let mut chain = Chain::start();
    let payee = chain.a_stranger().pubkey();
    for height in [BMM_START, BMM_START + 1, BMM_START + 2] {
        chain.win(height, payee);
        chain
            .settle(height, payee)
            .expect("each next height settles");
        assert_eq!(chain.config().bmm_next_height, height + 1);
    }
    for height in [BMM_START + 1, BMM_START + 4] {
        chain.win(height, payee);
        assert!(chain.settle(height, payee).is_err());
    }
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
        credited_height: DEPOSIT_START,
        credits_active: true,
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

#[test]
fn a_read_only_payee_fails_the_tx_and_the_height_stays() {
    let mut chain = Chain::start();
    let winner = chain.a_stranger().pubkey();
    chain.pay_fees(2_000_000);
    chain.win(BMM_START, winner);

    let mut instruction = chain.settle_ix(BMM_START, SOLANA_BLOCK, winner);
    for meta in instruction.accounts.iter_mut() {
        if meta.pubkey == winner {
            meta.is_writable = false;
        }
    }
    assert!(chain.send_all(&[instruction]).is_err());
    assert_eq!(chain.config().bmm_next_height, BMM_START);
    assert_eq!(chain.balance(&chain.treasury), TREASURY_RESERVE + 2_000_000);

    let before = chain.balance(&winner);
    chain
        .settle(BMM_START, winner)
        .expect("the writable settle lands");
    assert_eq!(chain.balance(&winner) - before, 2_000_000);
}

#[test]
fn the_vault_as_the_payee_gets_nothing_and_the_height_settles() {
    let mut chain = Chain::start();
    let vault = chain.vault;
    chain.pay_fees(2_000_000);
    chain.win(BMM_START, vault);

    chain.settle(BMM_START, vault).expect("the height settles");
    assert_eq!(chain.balance(&vault), VAULT_LAMPORTS);
    assert_eq!(chain.balance(&chain.treasury), TREASURY_RESERVE + 2_000_000);
    assert_eq!(chain.config().bmm_next_height, BMM_START + 1);
}

/// The compute limit of one credit in the daemon.
const CREDIT_COMPUTE_UNITS: u32 = 1_400_000;

#[test]
fn a_full_part_of_new_targets_fits_the_credit_compute_limit() {
    let mut chain = Chain::start();
    let height = DEPOSIT_START + 1;
    let deposits: Vec<AnsweredDeposit> = (0..16)
        .map(|index| to(Keypair::new().pubkey(), index, 1_000_000))
        .collect();
    chain.deposit_answer(height, ecash_block(height - 1), &deposits, 0, 16);
    let mut limit = CREDIT_COMPUTE_UNITS.to_le_bytes().to_vec();
    limit.insert(0, 2);
    let budget = Instruction {
        program_id: Pubkey::from_str_const("ComputeBudget111111111111111111111111111111"),
        accounts: Vec::new(),
        data: limit,
    };
    let credit = chain.credit_ix(height, &deposits, 0, 16);
    let sender = chain.a_stranger();
    chain.svm.expire_blockhash();
    let transaction = Transaction::new_signed_with_payer(
        &[budget, credit],
        Some(&sender.pubkey()),
        &[&sender],
        chain.svm.latest_blockhash(),
    );
    let units = chain
        .svm
        .send_transaction(transaction)
        .expect("the full part lands")
        .compute_units_consumed;
    println!("a part of 16 new targets takes {units} compute units");
    assert!(units < u64::from(CREDIT_COMPUTE_UNITS) / 4);
    assert_eq!(chain.config().credited_height, height);
    assert_eq!(
        chain.config().pegged_lamports,
        16 * 1_000_000 * LAMPORTS_PER_SAT
    );
}

#[test]
fn the_fee_rises_with_the_records_that_wait_for_a_seal() {
    assert_eq!(min_fee_sats(0), 1_000);
    assert_eq!(min_fee_sats(1), 1_010);
    assert_eq!(min_fee_sats(100), 2_000);
    assert_eq!(min_fee_sats(1_499), 15_990);
    assert_eq!(min_fee_sats(u64::MAX), 1_000 + u64::MAX / 100);
    let mut chain = Chain::start();
    chain.deposit(0, 10_000_000).expect("the deposit lands");
    for index in 0..100 {
        let fee_sats = min_fee_sats(index);
        chain
            .withdraw(
                index,
                (fee_sats + 600) * LAMPORTS_PER_SAT,
                fee_sats,
                &A_P2WPKH,
            )
            .expect("a withdrawal at the quoted fee lands");
    }
    let lamports = (2_000 + 600) * LAMPORTS_PER_SAT;
    assert!(chain.withdraw(100, lamports, 1_999, &A_P2WPKH).is_err());
    chain
        .withdraw(100, lamports, 2_000, &A_P2WPKH)
        .expect("the fee of 100 waiting records lands");
    chain.seal().expect("the bundle seals");
    let lamports = (MIN_FEE_SATS + 600) * LAMPORTS_PER_SAT;
    chain
        .withdraw(101, lamports, MIN_FEE_SATS, &A_P2WPKH)
        .expect("a seal takes the price back down");
}
