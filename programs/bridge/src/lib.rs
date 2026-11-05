use anchor_lang::prelude::*;
use anchor_lang::system_program::{transfer, Transfer};

declare_id!("ARA8mQfWk85ULDLAujy3b8gLknFF2QzrsAKHYbc8beu");

/// The peg is 1 SOL to 1 BTC, so one satoshi is ten lamports.
pub const LAMPORTS_PER_SAT: u64 = 10;

/// The longest standard Bitcoin script pubkey is a 34-byte P2WSH or P2TR.
pub const MAX_SCRIPT_PUBKEY_LEN: usize = 34;

/// A payout below this value is dust, and a wallet cannot spend it.
pub const MIN_PAYOUT_SATS: u64 = 546;

/// The lowest mainchain fee of one withdrawal. It pays for the 43 bytes of
/// its output at more than 20 sat/vB, so a flood of records costs real money.
pub const MIN_FEE_SATS: u64 = 1_000;

/// The lowest mainchain fee while `waiting` records wait for a seal:
/// `MIN_FEE_SATS × (1 + waiting / 100)`. A flood of records then costs more
/// with each record.
pub fn min_fee_sats(waiting: u64) -> u64 {
    MIN_FEE_SATS.saturating_add(MIN_FEE_SATS.saturating_mul(waiting) / 100)
}

/// The most records in one bundle. Each payout output is at most 43 bytes,
/// so a bundle stays near 260,000 weight units, below the standard 400,000.
pub const MAX_BUNDLE_RECORDS: u64 = 1_500;

/// A validator drops the BMM pairs this many eCash blocks below its tip
/// (`PAIR_WINDOW` in the Agave patch).
pub const PAIR_WINDOW: u64 = 4_032;

/// True when no validator still holds a pair for eCash height `height`. The
/// eCash tip is at least `credited_height`.
pub fn pair_too_old(height: u64, credited_height: u64) -> bool {
    height.saturating_add(PAIR_WINDOW) < credited_height
}

pub const CONFIG_SEED: &[u8] = b"config";
pub const VAULT_SEED: &[u8] = b"vault";
pub const WITHDRAWAL_SEED: &[u8] = b"withdrawal";
pub const TREASURY_SEED: &[u8] = b"treasury";

/// The account that the patched validator builds for each tx with a
/// top-level `settle_bmm`. It holds the answer of the local enforcer.
pub const BMM_ANSWER_ID: Pubkey = pubkey!("BmmAnswer1111111111111111111111111111111111");

/// The `BmmAnswer` data: height, block hash, found, commitment, Solana block,
/// recorded, credited height, credits active.
pub const BMM_ANSWER_LEN: usize = 8 + 32 + 1 + 32 + 32 + 1 + 8 + 1;

/// The block record that the patched validator writes.
pub const BMM_BLOCKS_ID: Pubkey = pubkey!("BmmB1ocks1111111111111111111111111111111111");

/// The BMM values of the cluster, which only the genesis writes.
pub const BMM_PARAMS_ID: Pubkey = pubkey!("BmmParams1111111111111111111111111111111111");

/// The account that the patched validator builds for each tx with a
/// top-level `bundle_paid`.
pub const BMM_PAID_ID: Pubkey = pubkey!("BmmPaid111111111111111111111111111111111111");

/// The `BmmPaid` data: height, block hash, M6ID, status, paid height.
pub const BMM_PAID_LEN: usize = 8 + 32 + 32 + 1 + 8;

/// The account that the patched validator builds for each tx with a
/// top-level `credit_deposits`. It holds the deposits of one eCash block from
/// the local enforcer.
pub const DEPOSIT_ANSWER_ID: Pubkey = pubkey!("DepositAnswer111111111111111111111111111111");

/// The `DepositAnswer` head: height, block hash, parent hash, deposit count of
/// the block, first index, entry count.
pub const DEPOSIT_ANSWER_HEAD_LEN: usize = 8 + 32 + 32 + 4 + 4 + 1;

/// One `DepositAnswer` entry: sequence number, value in sats, a flag, and the
/// target pubkey.
pub const DEPOSIT_ANSWER_ENTRY_LEN: usize = 8 + 8 + 1 + 32;

/// The owner of every sysvar. The runtime sends a sysvar read-only.
const SYSVAR_OWNER: Pubkey = pubkey!("Sysvar1111111111111111111111111111111111111");

const NATIVE_LOADER: Pubkey = pubkey!("NativeLoader1111111111111111111111111111111");

/// The keys that the runtime sends read-only in every tx, whatever the sender
/// asks: the reserved account keys of the patched Agave.
pub const RESERVED_KEYS: &[Pubkey] = &[
    pubkey!("AddressLookupTab1e1111111111111111111111111"),
    pubkey!("BPFLoader2111111111111111111111111111111111"),
    pubkey!("BPFLoader1111111111111111111111111111111111"),
    pubkey!("BPFLoaderUpgradeab1e11111111111111111111111"),
    pubkey!("ComputeBudget111111111111111111111111111111"),
    pubkey!("Config1111111111111111111111111111111111111"),
    pubkey!("Ed25519SigVerify111111111111111111111111111"),
    pubkey!("Feature111111111111111111111111111111111111"),
    pubkey!("LoaderV411111111111111111111111111111111111"),
    pubkey!("KeccakSecp256k11111111111111111111111111111"),
    pubkey!("Secp256r1SigVerify1111111111111111111111111"),
    pubkey!("StakeConfig11111111111111111111111111111111"),
    pubkey!("Stake11111111111111111111111111111111111111"),
    pubkey!("11111111111111111111111111111111"),
    pubkey!("Vote111111111111111111111111111111111111111"),
    pubkey!("ZkE1Gama1Proof11111111111111111111111111111"),
    pubkey!("ZkTokenProof1111111111111111111111111111111"),
    pubkey!("SysvarC1ock11111111111111111111111111111111"),
    pubkey!("SysvarEpochRewards1111111111111111111111111"),
    pubkey!("SysvarEpochSchedu1e111111111111111111111111"),
    pubkey!("SysvarFees111111111111111111111111111111111"),
    pubkey!("Sysvar1nstructions1111111111111111111111111"),
    pubkey!("SysvarLastRestartS1ot1111111111111111111111"),
    pubkey!("SysvarRecentB1ockHashes11111111111111111111"),
    pubkey!("SysvarRent111111111111111111111111111111111"),
    pubkey!("SysvarRewards111111111111111111111111111111"),
    pubkey!("SysvarS1otHashes111111111111111111111111111"),
    pubkey!("SysvarS1otHistory11111111111111111111111111"),
    pubkey!("SysvarStakeHistory1111111111111111111111111"),
    NATIVE_LOADER,
    SYSVAR_OWNER,
    BMM_ANSWER_ID,
    BMM_BLOCKS_ID,
    BMM_PAID_ID,
    BMM_PARAMS_ID,
    DEPOSIT_ANSWER_ID,
];

/// The keys that the patch adds to the reserved account keys of Agave.
pub const CHAIN_RESERVED_KEYS: &[Pubkey] = &[
    BMM_ANSWER_ID,
    BMM_BLOCKS_ID,
    BMM_PAID_ID,
    BMM_PARAMS_ID,
    DEPOSIT_ANSWER_ID,
];

#[program]
pub mod bridge {
    use super::*;

    /// Settles eCash height `height`, which block `block_hash` holds on the
    /// active chain. Heights settle one at a time, in order. The payee of a
    /// valid checkpoint gets the whole treasury. A height with no
    /// commitment, or a checkpoint that its own pair proves invalid, moves
    /// the cursor with no payout. A commitment that no pair matches moves it
    /// only when the credits reach `bmm_skip_blocks` above its height, or at
    /// once when its height is below the pair window.
    pub fn settle_bmm(
        ctx: Context<SettleBmm>,
        height: u64,
        block_hash: [u8; 32],
        solana_block: [u8; 32],
    ) -> Result<()> {
        let next = ctx.accounts.config.bmm_next_height;
        require_gte!(height, next, BridgeError::HeightAlreadyPassed);
        require_eq!(height, next, BridgeError::HeightNotNext);
        let answer = BmmAnswer::read(&ctx.accounts.bmm_answer)?;
        require!(
            answer.height == height
                && answer.block_hash == block_hash
                && answer.solana_block == solana_block,
            BridgeError::AnswerForAnotherQuestion
        );
        let payee = &ctx.accounts.payee;
        let slot = Clock::get()?.slot;
        let pays = match answer.commitment {
            None => {
                msg!("eCash height {} holds no commitment", height);
                false
            }
            Some(commitment) if commitment_of(&solana_block, &payee.key()) == commitment => {
                let valid = answer.recorded
                    && answer
                        .credited_height
                        .saturating_add(ctx.accounts.config.deposit_lag)
                        >= height
                    && answer.credits_active;
                if !valid {
                    msg!(
                        "the pair of eCash height {} names an invalid checkpoint",
                        height
                    );
                }
                valid
            }
            Some(_) => {
                let config = &ctx.accounts.config;
                if pair_too_old(height, config.credited_height) {
                    msg!("eCash height {} is below the pair window", height);
                } else {
                    require_gte!(
                        config.credited_height,
                        height.saturating_add(config.bmm_skip_blocks),
                        BridgeError::PairDoesNotMatch
                    );
                    msg!("eCash height {} waited too long for its pair", height);
                }
                false
            }
        };
        if pays {
            pay_treasury(ctx.accounts, height)?;
        }
        let config = &mut ctx.accounts.config;
        config.bmm_next_height = height.checked_add(1).ok_or(BridgeError::AmountOverflow)?;
        config.bmm_next_since_slot = slot;
        Ok(())
    }

    /// Credits the deposits of eCash block `block_hash` at height `height`,
    /// from index `first`, `count` of them. Anyone can send it. The heights go
    /// in order, and a block with many deposits takes several parts in order.
    /// The remaining accounts are the targets of the deposits that name a
    /// pubkey, in deposit order. A target that cannot take the lamports leaves
    /// them in the vault as stranded. The parts of one height must all name
    /// the same eCash block.
    pub fn credit_deposits<'info>(
        ctx: Context<'info, CreditDeposits<'info>>,
        height: u64,
        block_hash: [u8; 32],
        first: u32,
        count: u8,
    ) -> Result<()> {
        let config = &ctx.accounts.config;
        require_eq!(
            height,
            config
                .credited_height
                .checked_add(1)
                .ok_or(BridgeError::AmountOverflow)?,
            BridgeError::CreditOutOfOrder
        );
        require_eq!(
            u64::from(first),
            config.credit_index,
            BridgeError::PartOutOfOrder
        );
        let answer = DepositAnswer::read(&ctx.accounts.deposit_answer)?;
        require!(
            answer.height == height
                && answer.block_hash == block_hash
                && answer.first == first
                && answer.deposits.len() == usize::from(count),
            BridgeError::AnswerForAnotherQuestion
        );
        require!(
            config.credited_block == [0; 32] || answer.prev_block_hash == config.credited_block,
            BridgeError::NotOnCreditedChain
        );
        require!(
            config.credit_index == 0 || block_hash == config.credit_part_block,
            BridgeError::PartOfAnotherBlock
        );
        let end = u64::from(first)
            .checked_add(u64::from(count))
            .ok_or(BridgeError::AmountOverflow)?;
        require!(count > 0 || answer.total == 0, BridgeError::EmptyPart);

        let vault = ctx.accounts.vault.to_account_info();
        let rent = Rent::get()?;
        let vault_bump = config.vault_bump;
        let signer_seeds: &[&[&[u8]]] = &[&[VAULT_SEED, &[vault_bump]]];
        let mut targets = ctx.remaining_accounts.iter();
        let mut high_water = config.deposit_high_water;
        let mut credited: u64 = 0;
        let mut stranded: u64 = 0;
        for deposit in &answer.deposits {
            require_gte!(
                deposit.sequence_number,
                high_water,
                BridgeError::SequenceNumberAlreadyApplied
            );
            high_water = deposit
                .sequence_number
                .checked_add(1)
                .ok_or(BridgeError::AmountOverflow)?;
            let lamports = deposit
                .value_sats
                .checked_mul(LAMPORTS_PER_SAT)
                .ok_or(BridgeError::AmountOverflow)?;
            let target = match deposit.target {
                Some(key) => {
                    let target = targets.next().ok_or(BridgeError::MissingTarget)?;
                    require_keys_eq!(target.key(), key, BridgeError::WrongTarget);
                    Some(target)
                }
                None => None,
            };
            let takes = match target {
                Some(target) => can_take(target, &vault.key(), lamports, &rent)?,
                None => false,
            };
            match target {
                Some(target) if takes => {
                    if lamports > 0 {
                        transfer(
                            CpiContext::new_with_signer(
                                ctx.accounts.system_program.key(),
                                Transfer {
                                    from: vault.clone(),
                                    to: target.clone(),
                                },
                                signer_seeds,
                            ),
                            lamports,
                        )?;
                    }
                    credited = credited
                        .checked_add(lamports)
                        .ok_or(BridgeError::AmountOverflow)?;
                    msg!(
                        "deposit {} credits {} lamports to {}",
                        deposit.sequence_number,
                        lamports,
                        target.key()
                    );
                }
                _ => {
                    stranded = stranded
                        .checked_add(lamports)
                        .ok_or(BridgeError::AmountOverflow)?;
                    msg!(
                        "deposit {} strands {} lamports in the vault",
                        deposit.sequence_number,
                        lamports
                    );
                }
            }
        }

        let config = &mut ctx.accounts.config;
        config.deposit_high_water = high_water;
        config.pegged_lamports = config
            .pegged_lamports
            .checked_add(credited)
            .ok_or(BridgeError::AmountOverflow)?;
        config.stranded_lamports = config
            .stranded_lamports
            .checked_add(stranded)
            .ok_or(BridgeError::AmountOverflow)?;
        if end >= u64::from(answer.total) {
            config.credited_height = height;
            config.credited_block = block_hash;
            config.credit_index = 0;
            config.credit_part_block = [0; 32];
        } else {
            config.credit_index = end;
            config.credit_part_block = block_hash;
        }
        Ok(())
    }

    pub fn withdraw(
        ctx: Context<Withdraw>,
        lamports: u64,
        fee_sats: u64,
        script_pubkey: Vec<u8>,
    ) -> Result<()> {
        require!(
            lamports.is_multiple_of(LAMPORTS_PER_SAT),
            BridgeError::AmountNotAWholeSat
        );
        require!(
            is_standard_script(&script_pubkey),
            BridgeError::ScriptPubkeyNotStandard
        );

        let total_sats = lamports / LAMPORTS_PER_SAT;
        let payout_sats = total_sats
            .checked_sub(fee_sats)
            .ok_or(BridgeError::FeeAboveAmount)?;
        require!(payout_sats >= MIN_PAYOUT_SATS, BridgeError::PayoutIsDust);
        let config = &mut ctx.accounts.config;
        let waiting = config
            .withdrawal_count
            .saturating_sub(config.bundled_through);
        require_gte!(fee_sats, min_fee_sats(waiting), BridgeError::FeeTooLow);
        // The genesis float circulates beside the pegged lamports, so no
        // lamport pegs out until gifts to the vault back the whole float.
        let backed = ctx
            .accounts
            .vault
            .lamports()
            .checked_add(config.pegged_lamports)
            .ok_or(BridgeError::AmountOverflow)?;
        let needed = config
            .vault_supply
            .checked_add(config.float_lamports)
            .ok_or(BridgeError::AmountOverflow)?;
        require_gte!(backed, needed, BridgeError::FloatNotBacked);
        config.pegged_lamports = config
            .pegged_lamports
            .checked_sub(lamports)
            .ok_or(BridgeError::AmountAbovePeggedTotal)?;
        let index = config.withdrawal_count;
        config.withdrawal_count = index.checked_add(1).ok_or(BridgeError::AmountOverflow)?;

        let record = &mut ctx.accounts.record;
        record.index = index;
        record.owner = ctx.accounts.user.key();
        record.burned_lamports = lamports;
        record.payout_sats = payout_sats;
        record.fee_sats = fee_sats;
        record.script_pubkey = script_pubkey;
        record.bump = ctx.bumps.record;

        transfer(
            CpiContext::new(
                ctx.accounts.system_program.key(),
                Transfer {
                    from: ctx.accounts.user.to_account_info(),
                    to: ctx.accounts.vault.to_account_info(),
                },
            ),
            lamports,
        )
    }

    /// Puts the open records from `paid_through` up to `through` into the
    /// live bundle, whose blinded M6 has the ID `m6id`. Only one bundle is
    /// live at a time, and a record never leaves its bundle, so no two
    /// bundles that eCash can pay hold the same record.
    pub fn seal_bundle(ctx: Context<SealBundle>, through: u64, m6id: [u8; 32]) -> Result<()> {
        let config = &mut ctx.accounts.config;
        require_eq!(
            config.bundled_through,
            config.paid_through,
            BridgeError::BundleLive
        );
        require_gt!(
            config.withdrawal_count,
            config.bundled_through,
            BridgeError::NothingToSeal
        );
        let most = config
            .paid_through
            .checked_add(MAX_BUNDLE_RECORDS)
            .ok_or(BridgeError::AmountOverflow)?
            .min(config.withdrawal_count);
        require!(
            through > config.paid_through && through <= most,
            BridgeError::BadBundleEnd
        );
        config.bundled_through = through;
        config.bundle_m6id = m6id;
        config.bundle_sealed_height = config.credited_height;
        config.bundle_sealed_slot = Clock::get()?.slot;
        msg!(
            "the bundle holds records {} to {}",
            config.paid_through,
            through
        );
        Ok(())
    }

    /// Marks the live bundle paid. `through` must name its end, so a repeat
    /// fails. The bundle must be at least `bundle_min_slots` old. eCash block
    /// `block_hash` at `height` must hold a Succeeded event for `m6id`, the
    /// M6ID of the seal.
    pub fn bundle_paid(
        ctx: Context<BundlePaid>,
        through: u64,
        height: u64,
        block_hash: [u8; 32],
        m6id: [u8; 32],
    ) -> Result<()> {
        let config = &mut ctx.accounts.config;
        require_gt!(
            config.bundled_through,
            config.paid_through,
            BridgeError::NoLiveBundle
        );
        require_eq!(
            through,
            config.bundled_through,
            BridgeError::NotTheLiveBundle
        );
        let ripe = config
            .bundle_sealed_slot
            .checked_add(config.bundle_min_slots)
            .ok_or(BridgeError::AmountOverflow)?;
        require_gte!(Clock::get()?.slot, ripe, BridgeError::BundleTooYoung);
        let paid = BmmPaid::read(&ctx.accounts.bmm_paid)?;
        require!(
            paid.height == height && paid.block_hash == block_hash && paid.m6id == m6id,
            BridgeError::PaidAnswerForAnotherQuestion
        );
        require!(m6id == config.bundle_m6id, BridgeError::NotTheSealedM6id);
        require!(paid.paid, BridgeError::BundleNotPaid);
        msg!(
            "the mainchain paid records {} to {}",
            config.paid_through,
            through
        );
        config.paid_through = through;
        Ok(())
    }

    /// Closes the record of a paid withdrawal and returns its rent to the
    /// owner. Anyone can send it.
    pub fn close_record(ctx: Context<CloseRecord>, index: u64) -> Result<()> {
        require_gt!(
            ctx.accounts.config.paid_through,
            index,
            BridgeError::RecordNotPaid
        );
        Ok(())
    }
}

/// Pays the whole treasury above its reserve to the payee of a valid
/// checkpoint at `height`, if the payee can hold it.
fn pay_treasury(accounts: &mut SettleBmm, height: u64) -> Result<()> {
    let payee = &accounts.payee;
    let treasury = &accounts.treasury;
    let config = &accounts.config;
    let payout = treasury
        .lamports()
        .saturating_sub(Rent::get()?.minimum_balance(0));
    let excluded = [
        treasury.key(),
        accounts.config.key(),
        vault_pda(config.vault_bump)?,
    ];
    if payout > 0 && can_hold(payee, &excluded, payout)? {
        let treasury_bump = accounts.config.treasury_bump;
        let signer_seeds: &[&[&[u8]]] = &[&[TREASURY_SEED, &[treasury_bump]]];
        transfer(
            CpiContext::new_with_signer(
                accounts.system_program.key(),
                Transfer {
                    from: treasury.to_account_info(),
                    to: payee.to_account_info(),
                },
                signer_seeds,
            ),
            payout,
        )?;
        let config = &mut accounts.config;
        config.bmm_paid_total = config
            .bmm_paid_total
            .checked_add(payout)
            .ok_or(BridgeError::AmountOverflow)?;
        msg!(
            "eCash height {} pays {} lamports to {}",
            height,
            payout,
            payee.key()
        );
    } else {
        msg!("eCash height {} pays nothing to {}", height, payee.key());
    }

    Ok(())
}

fn vault_pda(bump: u8) -> Result<Pubkey> {
    Pubkey::create_program_address(&[VAULT_SEED, &[bump]], &crate::ID)
        .map_err(|_| error!(BridgeError::BadBump))
}

/// True for the byte-exact P2PKH, P2SH, P2WPKH, P2WSH, and P2TR templates.
/// No other output can go in an M6: the enforcer reads an OP_DRIVECHAIN or
/// odd output as a treasury or a deposit, and then refuses the M6.
pub fn is_standard_script(script: &[u8]) -> bool {
    matches!(
        script,
        [0x76, 0xa9, 0x14, hash @ .., 0x88, 0xac] if hash.len() == 20
    ) || matches!(script, [0xa9, 0x14, hash @ .., 0x87] if hash.len() == 20)
        || matches!(script, [0x00, 0x14, hash @ ..] if hash.len() == 20)
        || matches!(script, [0x00 | 0x51, 0x20, key @ ..] if key.len() == 32)
}

/// h* of a BMM bid: SHA-256(Solana block ‖ payee).
pub fn commitment_of(solana_block: &[u8; 32], payee: &Pubkey) -> [u8; 32] {
    solana_sha256_hasher::hashv(&[solana_block, payee.as_ref()]).to_bytes()
}

/// The answer of the local enforcer and of the bank to the first top-level
/// `settle_bmm` of the tx.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BmmAnswer {
    pub height: u64,
    pub block_hash: [u8; 32],
    pub commitment: Option<[u8; 32]>,
    pub solana_block: [u8; 32],
    /// The block record of the bank holds `solana_block`.
    pub recorded: bool,
    /// The highest eCash height that `solana_block` credited in full, from
    /// its record entry.
    pub credited_height: u64,
    /// The active eCash chain holds every block that `solana_block` credited.
    pub credits_active: bool,
}

impl BmmAnswer {
    pub fn read(account: &UncheckedAccount) -> Result<Self> {
        let data = account.try_borrow_data()?;
        Self::decode(&data).ok_or_else(|| error!(BridgeError::NoAnswer))
    }

    pub fn decode(data: &[u8]) -> Option<Self> {
        if data.len() != BMM_ANSWER_LEN {
            return None;
        }
        let height = u64::from_le_bytes(data[..8].try_into().ok()?);
        let block_hash: [u8; 32] = data[8..40].try_into().ok()?;
        let commitment: [u8; 32] = data[41..73].try_into().ok()?;
        let commitment = match data[40] {
            0 => None,
            1 => Some(commitment),
            _ => return None,
        };
        let solana_block: [u8; 32] = data[73..105].try_into().ok()?;
        let flag = |byte: u8| match byte {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        };
        Some(Self {
            height,
            block_hash,
            commitment,
            solana_block,
            recorded: flag(data[105])?,
            credited_height: u64::from_le_bytes(data[106..114].try_into().ok()?),
            credits_active: flag(data[114])?,
        })
    }
}

/// The answer of the local enforcer to the first top-level `bundle_paid` of
/// the tx.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BmmPaid {
    pub height: u64,
    pub block_hash: [u8; 32],
    pub m6id: [u8; 32],
    /// Block `block_hash` at `height` holds a Succeeded event for `m6id`.
    pub paid: bool,
    pub paid_height: u64,
}

impl BmmPaid {
    pub fn read(account: &UncheckedAccount) -> Result<Self> {
        let data = account.try_borrow_data()?;
        Self::decode(&data).ok_or_else(|| error!(BridgeError::NoAnswer))
    }

    pub fn decode(data: &[u8]) -> Option<Self> {
        if data.len() != BMM_PAID_LEN {
            return None;
        }
        let paid = match data[72] {
            0 => false,
            1 => true,
            _ => return None,
        };
        Some(Self {
            height: u64::from_le_bytes(data[..8].try_into().ok()?),
            block_hash: data[8..40].try_into().ok()?,
            m6id: data[40..72].try_into().ok()?,
            paid,
            paid_height: u64::from_le_bytes(data[73..81].try_into().ok()?),
        })
    }
}

/// One deposit of a `DepositAnswer`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AnsweredDeposit {
    pub sequence_number: u64,
    pub value_sats: u64,
    /// The pubkey that the OP_RETURN names, or `None` when it names none.
    pub target: Option<Pubkey>,
}

/// The answer of the local enforcer to the first top-level `credit_deposits`
/// of the tx: a part of the deposits of one eCash block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DepositAnswer {
    pub height: u64,
    pub block_hash: [u8; 32],
    pub prev_block_hash: [u8; 32],
    /// The deposit count of the whole block.
    pub total: u32,
    pub first: u32,
    pub deposits: Vec<AnsweredDeposit>,
}

impl DepositAnswer {
    pub fn read(account: &UncheckedAccount) -> Result<Self> {
        let data = account.try_borrow_data()?;
        Self::decode(&data).ok_or_else(|| error!(BridgeError::NoAnswer))
    }

    pub fn decode(data: &[u8]) -> Option<Self> {
        let head = data.get(..DEPOSIT_ANSWER_HEAD_LEN)?;
        let count = usize::from(head[80]);
        let entries = data.get(DEPOSIT_ANSWER_HEAD_LEN..)?;
        if entries.len() != count.checked_mul(DEPOSIT_ANSWER_ENTRY_LEN)? {
            return None;
        }
        let deposits = entries
            .chunks_exact(DEPOSIT_ANSWER_ENTRY_LEN)
            .map(|entry| {
                let target = match entry[16] {
                    0 => None,
                    1 => Some(Pubkey::new_from_array(entry[17..49].try_into().ok()?)),
                    _ => return None,
                };
                Some(AnsweredDeposit {
                    sequence_number: u64::from_le_bytes(entry[..8].try_into().ok()?),
                    value_sats: u64::from_le_bytes(entry[8..16].try_into().ok()?),
                    target,
                })
            })
            .collect::<Option<Vec<_>>>()?;
        Some(Self {
            height: u64::from_le_bytes(head[..8].try_into().ok()?),
            block_hash: head[8..40].try_into().ok()?,
            prev_block_hash: head[40..72].try_into().ok()?,
            total: u32::from_le_bytes(head[72..76].try_into().ok()?),
            first: u32::from_le_bytes(head[76..80].try_into().ok()?),
            deposits,
        })
    }
}

/// True when a deposit target can take `lamports`. A target that cannot
/// keeps them in the vault. The key and the account decide, never the
/// writable flag of the tx: a creditable target that the tx sends read-only
/// fails the tx, so no sender can strand a deposit on purpose.
fn can_take(target: &AccountInfo, vault: &Pubkey, lamports: u64, rent: &Rent) -> Result<bool> {
    if target.key() == *vault
        || target.executable
        || *target.owner == SYSVAR_OWNER
        || *target.owner == NATIVE_LOADER
        || RESERVED_KEYS.contains(target.key)
    {
        return Ok(false);
    }
    require!(target.is_writable, BridgeError::TargetNotWritable);
    let Some(after) = target.lamports().checked_add(lamports) else {
        return Ok(false);
    };
    Ok(rent.is_exempt(after, target.data_len()))
}

/// True when the payee can take the payout. A payee that the key or the
/// account rules out gets nothing, and the fees go to the next winner. The
/// writable flag of the tx never decides: a payee that could take the payout
/// and comes read-only fails the tx, so no sender can cancel a payout.
fn can_hold(payee: &UncheckedAccount, excluded: &[Pubkey], payout: u64) -> Result<bool> {
    if excluded.contains(payee.key)
        || payee.executable
        || *payee.owner == crate::ID
        || *payee.owner == SYSVAR_OWNER
        || *payee.owner == NATIVE_LOADER
        || RESERVED_KEYS.contains(payee.key)
    {
        return Ok(false);
    }
    require!(payee.is_writable, BridgeError::PayeeNotWritable);
    let Some(after) = payee.lamports().checked_add(payout) else {
        return Ok(false);
    };
    Ok(Rent::get()?.is_exempt(after, payee.data_len()))
}

#[derive(Accounts)]
pub struct SettleBmm<'info> {
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Account<'info, Config>,
    #[account(mut, seeds = [TREASURY_SEED], bump = config.treasury_bump)]
    pub treasury: SystemAccount<'info>,
    /// CHECK: the program hashes this key into the commitment. A `mut`
    /// constraint would fail the tx for a payee that the runtime demotes, for
    /// example a program, so `can_hold` checks the flag after the key rules.
    pub payee: UncheckedAccount<'info>,
    /// CHECK: the address fixes the account, and the validator builds its data.
    #[account(address = BMM_ANSWER_ID)]
    pub bmm_answer: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct CreditDeposits<'info> {
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Account<'info, Config>,
    #[account(mut, seeds = [VAULT_SEED], bump = config.vault_bump)]
    pub vault: SystemAccount<'info>,
    /// CHECK: the address fixes the account, and the validator builds its data.
    #[account(address = DEPOSIT_ANSWER_ID)]
    pub deposit_answer: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Withdraw<'info> {
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Account<'info, Config>,
    #[account(mut, seeds = [VAULT_SEED], bump = config.vault_bump)]
    pub vault: SystemAccount<'info>,
    #[account(
        init,
        payer = user,
        space = 8 + WithdrawalRecord::INIT_SPACE,
        seeds = [WITHDRAWAL_SEED, &config.withdrawal_count.to_le_bytes()],
        bump,
    )]
    pub record: Account<'info, WithdrawalRecord>,
    #[account(mut)]
    pub user: Signer<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct SealBundle<'info> {
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump, has_one = oracle)]
    pub config: Account<'info, Config>,
    pub oracle: Signer<'info>,
}

#[derive(Accounts)]
pub struct BundlePaid<'info> {
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump, has_one = oracle)]
    pub config: Account<'info, Config>,
    pub oracle: Signer<'info>,
    /// CHECK: the address fixes the account, and the validator builds its data.
    #[account(address = BMM_PAID_ID)]
    pub bmm_paid: UncheckedAccount<'info>,
}

#[derive(Accounts)]
#[instruction(index: u64)]
pub struct CloseRecord<'info> {
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Account<'info, Config>,
    #[account(
        mut,
        close = owner,
        seeds = [WITHDRAWAL_SEED, &index.to_le_bytes()],
        bump = record.bump,
        has_one = owner,
    )]
    pub record: Account<'info, WithdrawalRecord>,
    /// CHECK: the record names this account, and it gets the rent back.
    #[account(mut)]
    pub owner: UncheckedAccount<'info>,
}

/// The bridge settings and counters. The genesis writes this account, so no
/// instruction creates it.
#[account]
#[derive(InitSpace)]
pub struct Config {
    /// The key that seals a bundle and marks it paid.
    pub oracle: Pubkey,
    /// The lowest mainchain sequence number that a deposit may still use.
    pub deposit_high_water: u64,
    pub pegged_lamports: u64,
    pub withdrawal_count: u64,
    /// The cursor: the lowest eCash height that `settle_bmm` may still settle.
    pub bmm_next_height: u64,
    /// Every lamport that `settle_bmm` paid to a winner.
    pub bmm_paid_total: u64,
    /// D: a block credits its deposits after this many eCash confirmations.
    pub deposit_confirmations: u64,
    /// A BMM checkpoint at eCash height H counts only when its bank credited
    /// every deposit through H - `deposit_lag`.
    pub deposit_lag: u64,
    /// The highest eCash height with every deposit credited.
    pub credited_height: u64,
    /// The eCash block at `credited_height`. Zero before the first credit.
    pub credited_block: [u8; 32],
    /// The index of the next deposit to credit at `credited_height + 1`.
    pub credit_index: u64,
    /// The eCash block whose deposits the parts so far credited at
    /// `credited_height + 1`. Zero when `credit_index` is zero.
    pub credit_part_block: [u8; 32],
    /// Lamports of deposits whose target could not take them. They stay in
    /// the vault.
    pub stranded_lamports: u64,
    pub bump: u8,
    pub vault_bump: u8,
    pub treasury_bump: u8,
    /// The lamports that the genesis put in the vault.
    pub vault_supply: u64,
    /// The spendable lamports outside the vault that no deposit backs.
    /// Withdrawals stay shut until gifts to the vault cover them.
    pub float_lamports: u64,
    /// Every record below this index is paid.
    pub paid_through: u64,
    /// The records from `paid_through` up to this index are the live bundle.
    pub bundled_through: u64,
    /// `credited_height` when the live bundle was sealed. Its M3 comes later.
    pub bundle_sealed_height: u64,
    /// The slot that sealed the live bundle.
    pub bundle_sealed_slot: u64,
    /// The fewest slots between a seal and its `bundle_paid`.
    pub bundle_min_slots: u64,
    /// A commitment at eCash height H that no pair matches can settle with no
    /// payout once `credited_height` reaches H + `bmm_skip_blocks`.
    pub bmm_skip_blocks: u64,
    /// The slot of the newest settle, or of the genesis.
    pub bmm_next_since_slot: u64,
    /// The M6ID of the newest sealed bundle, in the internal byte order.
    pub bundle_m6id: [u8; 32],
}

#[account]
#[derive(InitSpace)]
pub struct WithdrawalRecord {
    pub index: u64,
    pub owner: Pubkey,
    pub burned_lamports: u64,
    pub payout_sats: u64,
    pub fee_sats: u64,
    #[max_len(MAX_SCRIPT_PUBKEY_LEN)]
    pub script_pubkey: Vec<u8>,
    pub bump: u8,
}

#[error_code]
pub enum BridgeError {
    #[msg("The mainchain gave this sequence number before.")]
    SequenceNumberAlreadyApplied,
    #[msg("The amount is not a whole number of satoshis.")]
    AmountNotAWholeSat,
    #[msg("The mainchain fee is above the amount.")]
    FeeAboveAmount,
    #[msg("The payout is dust.")]
    PayoutIsDust,
    #[msg("The script pubkey is not P2PKH, P2SH, P2WPKH, P2WSH, or P2TR.")]
    ScriptPubkeyNotStandard,
    #[msg("The amount is above the total that the mainchain treasury holds.")]
    AmountAbovePeggedTotal,
    #[msg("The amount overflows a u64.")]
    AmountOverflow,
    #[msg("The cursor is past this eCash height.")]
    HeightAlreadyPassed,
    #[msg("The eCash height is not the next height to settle.")]
    HeightNotNext,
    #[msg("The tx holds no BMM answer.")]
    NoAnswer,
    #[msg("The BMM answer is for another eCash height, eCash block, or Solana block.")]
    AnswerForAnotherQuestion,
    #[msg("The eCash block holds no BMM commitment for the sidechain.")]
    NoCommitment,
    #[msg("SHA-256 of the Solana block and the payee is not the commitment.")]
    PairDoesNotMatch,
    #[msg("The block record of this fork does not hold the Solana block.")]
    BlockNotRecorded,
    #[msg("The eCash height is not the next height to credit.")]
    CreditOutOfOrder,
    #[msg("The first deposit index is not the next deposit to credit.")]
    PartOutOfOrder,
    #[msg("The eCash block does not follow the last credited block.")]
    NotOnCreditedChain,
    #[msg("The part holds no deposit, but the block holds deposits.")]
    EmptyPart,
    #[msg("A deposit names a target that the remaining accounts do not hold.")]
    MissingTarget,
    #[msg("A remaining account is not the target of its deposit.")]
    WrongTarget,
    #[msg("The tx sends a deposit target read-only.")]
    TargetNotWritable,
    #[msg("The earlier parts of this eCash height credited another eCash block.")]
    PartOfAnotherBlock,
    #[msg("The Solana block credited deposits only through a height below H - lag.")]
    TooFewCredits,
    #[msg("The active eCash chain dropped a block that the Solana block credited.")]
    CreditsDropped,
    #[msg("The tx sends a BMM payee read-only.")]
    PayeeNotWritable,
    #[msg("The mainchain fee is below the minimum for the records that wait.")]
    FeeTooLow,
    #[msg("Gifts to the vault do not yet back the genesis float.")]
    FloatNotBacked,
    #[msg("A bundle is live.")]
    BundleLive,
    #[msg("No open record waits for a bundle.")]
    NothingToSeal,
    #[msg("No bundle is live.")]
    NoLiveBundle,
    #[msg("The bundle is not the live bundle.")]
    NotTheLiveBundle,
    #[msg("The live bundle is younger than the minimum.")]
    BundleTooYoung,
    #[msg("The record is not paid.")]
    RecordNotPaid,
    #[msg("The config holds a bump that gives no address.")]
    BadBump,
    #[msg("The bundle end is not above paid_through or is above the record cap or count.")]
    BadBundleEnd,
    #[msg("The paid answer is for another eCash height, eCash block, or M6ID.")]
    PaidAnswerForAnotherQuestion,
    #[msg("The M6ID is not the M6ID of the live bundle.")]
    NotTheSealedM6id,
    #[msg("The eCash block holds no Succeeded event for the M6ID.")]
    BundleNotPaid,
}
