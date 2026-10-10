use anchor_lang::prelude::*;
use anchor_lang::system_program::{transfer, Transfer};

declare_id!("ARA8mQfWk85ULDLAujy3b8gLknFF2QzrsAKHYbc8beu");

/// The peg is 1 SOL to 1 BTC, so one satoshi is ten lamports.
pub const LAMPORTS_PER_SAT: u64 = 10;

/// The longest standard Bitcoin script pubkey is a 34-byte P2WSH or P2TR.
pub const MAX_SCRIPT_PUBKEY_LEN: usize = 34;

/// A payout below this value is dust, and a wallet cannot spend it.
pub const MIN_PAYOUT_SATS: u64 = 546;

pub const CONFIG_SEED: &[u8] = b"config";
pub const VAULT_SEED: &[u8] = b"vault";
pub const WITHDRAWAL_SEED: &[u8] = b"withdrawal";
pub const TREASURY_SEED: &[u8] = b"treasury";

/// The account that the patched validator builds for each tx with a
/// top-level `settle_bmm`. It holds the answer of the local enforcer.
pub const BMM_ANSWER_ID: Pubkey = pubkey!("BmmAnswer1111111111111111111111111111111111");

/// The `BmmAnswer` data: height, block hash, found, commitment, Solana block,
/// recorded.
pub const BMM_ANSWER_LEN: usize = 8 + 32 + 1 + 32 + 32 + 1;

const OP_RETURN: u8 = 0x6a;

#[program]
pub mod bridge {
    use super::*;

    pub fn initialize(
        ctx: Context<Initialize>,
        oracle: Pubkey,
        bmm_start_height: u64,
    ) -> Result<()> {
        let config = &mut ctx.accounts.config;
        config.oracle = oracle;
        config.deposit_high_water = 0;
        config.pegged_lamports = 0;
        config.withdrawal_count = 0;
        config.bmm_next_height = bmm_start_height;
        config.bmm_paid_total = 0;
        config.bump = ctx.bumps.config;
        config.vault_bump = ctx.bumps.vault;
        config.treasury_bump = ctx.bumps.treasury;
        Ok(())
    }

    /// Settles the commitment at eCash height `height`, which block
    /// `block_hash` holds on the active chain. The commitment must be
    /// SHA-256(`solana_block` ‖ payee), and `solana_block` must be in the
    /// block record of this fork. The payee gets the whole treasury, and the
    /// cursor moves past `height`, so no lower height can settle later.
    pub fn settle_bmm(
        ctx: Context<SettleBmm>,
        height: u64,
        block_hash: [u8; 32],
        solana_block: [u8; 32],
    ) -> Result<()> {
        require_gte!(
            height,
            ctx.accounts.config.bmm_next_height,
            BridgeError::HeightAlreadyPassed
        );
        let answer = BmmAnswer::read(&ctx.accounts.bmm_answer)?;
        require!(
            answer.height == height
                && answer.block_hash == block_hash
                && answer.solana_block == solana_block,
            BridgeError::AnswerForAnotherQuestion
        );
        let commitment = answer.commitment.ok_or(BridgeError::NoCommitment)?;
        let payee = &ctx.accounts.payee;
        require!(
            commitment_of(&solana_block, &payee.key()) == commitment,
            BridgeError::PairDoesNotMatch
        );
        require!(answer.recorded, BridgeError::BlockNotRecorded);

        let treasury = &ctx.accounts.treasury;
        let payout = treasury
            .lamports()
            .saturating_sub(Rent::get()?.minimum_balance(0));
        if payout > 0 && can_hold(payee, treasury.key(), payout)? {
            let treasury_bump = ctx.accounts.config.treasury_bump;
            let signer_seeds: &[&[&[u8]]] = &[&[TREASURY_SEED, &[treasury_bump]]];
            transfer(
                CpiContext::new_with_signer(
                    ctx.accounts.system_program.key(),
                    Transfer {
                        from: treasury.to_account_info(),
                        to: payee.to_account_info(),
                    },
                    signer_seeds,
                ),
                payout,
            )?;
            let config = &mut ctx.accounts.config;
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

        let config = &mut ctx.accounts.config;
        config.bmm_next_height = height.checked_add(1).ok_or(BridgeError::AmountOverflow)?;
        Ok(())
    }

    pub fn deposit(ctx: Context<Deposit>, sequence_number: u64, value_sats: u64) -> Result<()> {
        require!(value_sats > 0, BridgeError::ZeroAmount);
        // The mainchain counter climbs on every treasury change, and a paid
        // withdrawal is one. So a gap is normal, and the test is `>=`, never
        // equality. This one check is the whole guard against a repeat credit.
        require!(
            sequence_number >= ctx.accounts.config.deposit_high_water,
            BridgeError::SequenceNumberAlreadyApplied
        );

        let lamports = value_sats
            .checked_mul(LAMPORTS_PER_SAT)
            .ok_or(BridgeError::AmountOverflow)?;

        let config = &mut ctx.accounts.config;
        config.deposit_high_water = sequence_number
            .checked_add(1)
            .ok_or(BridgeError::AmountOverflow)?;
        config.pegged_lamports = config
            .pegged_lamports
            .checked_add(lamports)
            .ok_or(BridgeError::AmountOverflow)?;

        let vault_bump = config.vault_bump;
        let signer_seeds: &[&[&[u8]]] = &[&[VAULT_SEED, &[vault_bump]]];
        transfer(
            CpiContext::new_with_signer(
                ctx.accounts.system_program.key(),
                Transfer {
                    from: ctx.accounts.vault.to_account_info(),
                    to: ctx.accounts.recipient.to_account_info(),
                },
                signer_seeds,
            ),
            lamports,
        )
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
        require!(!script_pubkey.is_empty(), BridgeError::ScriptPubkeyEmpty);
        require!(
            script_pubkey.len() <= MAX_SCRIPT_PUBKEY_LEN,
            BridgeError::ScriptPubkeyTooLong
        );
        require!(
            script_pubkey[0] != OP_RETURN,
            BridgeError::ScriptPubkeyUnspendable
        );

        let total_sats = lamports / LAMPORTS_PER_SAT;
        let payout_sats = total_sats
            .checked_sub(fee_sats)
            .ok_or(BridgeError::FeeAboveAmount)?;
        require!(payout_sats >= MIN_PAYOUT_SATS, BridgeError::PayoutIsDust);

        // The genesis gives the validator lamports that no Bitcoin backs. This
        // subtraction is what stops a peg-out of those lamports.
        let config = &mut ctx.accounts.config;
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

    pub fn mark_paid(_ctx: Context<MarkPaid>, index: u64) -> Result<()> {
        msg!("the mainchain paid withdrawal {}", index);
        Ok(())
    }
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
        let recorded = match data[105] {
            0 => false,
            1 => true,
            _ => return None,
        };
        Some(Self {
            height,
            block_hash,
            commitment,
            solana_block,
            recorded,
        })
    }
}

/// True when the payee can take the payout. A payee that cannot take it gets
/// nothing, and the fees go to the next winner.
fn can_hold(payee: &UncheckedAccount, treasury: Pubkey, payout: u64) -> Result<bool> {
    if !payee.is_writable || payee.executable || payee.key() == treasury {
        return Ok(false);
    }
    let Some(after) = payee.lamports().checked_add(payout) else {
        return Ok(false);
    };
    Ok(Rent::get()?.is_exempt(after, payee.data_len()))
}

#[derive(Accounts)]
pub struct Initialize<'info> {
    #[account(
        init,
        payer = payer,
        space = 8 + Config::INIT_SPACE,
        seeds = [CONFIG_SEED],
        bump,
    )]
    pub config: Account<'info, Config>,
    #[account(seeds = [VAULT_SEED], bump)]
    pub vault: SystemAccount<'info>,
    #[account(seeds = [TREASURY_SEED], bump)]
    pub treasury: SystemAccount<'info>,
    #[account(mut)]
    pub payer: Signer<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct SettleBmm<'info> {
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Account<'info, Config>,
    #[account(mut, seeds = [TREASURY_SEED], bump = config.treasury_bump)]
    pub treasury: SystemAccount<'info>,
    /// CHECK: the program hashes this key into the commitment, and it pays
    /// only an account that the runtime lets it write. A `mut` constraint would
    /// fail the tx for a payee that the runtime demotes, for example a program.
    pub payee: UncheckedAccount<'info>,
    /// CHECK: the address fixes the account, and the validator builds its data.
    #[account(address = BMM_ANSWER_ID)]
    pub bmm_answer: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Deposit<'info> {
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump, has_one = oracle)]
    pub config: Account<'info, Config>,
    #[account(mut, seeds = [VAULT_SEED], bump = config.vault_bump)]
    pub vault: SystemAccount<'info>,
    /// CHECK: the daemon reads this pubkey out of the mainchain deposit address.
    #[account(mut)]
    pub recipient: UncheckedAccount<'info>,
    pub oracle: Signer<'info>,
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
#[instruction(index: u64)]
pub struct MarkPaid<'info> {
    #[account(seeds = [CONFIG_SEED], bump = config.bump, has_one = oracle)]
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
    pub oracle: Signer<'info>,
}

#[account]
#[derive(InitSpace)]
pub struct Config {
    pub oracle: Pubkey,
    /// The lowest mainchain sequence number that a deposit may still use.
    pub deposit_high_water: u64,
    pub pegged_lamports: u64,
    pub withdrawal_count: u64,
    /// The cursor: the lowest eCash height that `settle_bmm` may still settle.
    pub bmm_next_height: u64,
    /// Every lamport that `settle_bmm` paid to a winner.
    pub bmm_paid_total: u64,
    pub bump: u8,
    pub vault_bump: u8,
    pub treasury_bump: u8,
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
    #[msg("The amount is zero.")]
    ZeroAmount,
    #[msg("The mainchain fee is above the amount.")]
    FeeAboveAmount,
    #[msg("The payout is dust.")]
    PayoutIsDust,
    #[msg("The script pubkey is empty.")]
    ScriptPubkeyEmpty,
    #[msg("The script pubkey is longer than 34 bytes.")]
    ScriptPubkeyTooLong,
    #[msg("The script pubkey starts with OP_RETURN, so no wallet can spend it.")]
    ScriptPubkeyUnspendable,
    #[msg("The amount is above the total that the mainchain treasury holds.")]
    AmountAbovePeggedTotal,
    #[msg("The amount overflows a u64.")]
    AmountOverflow,
    #[msg("The cursor is past this eCash height.")]
    HeightAlreadyPassed,
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
}
