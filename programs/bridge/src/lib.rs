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

const OP_RETURN: u8 = 0x6a;

#[program]
pub mod bridge {
    use super::*;

    pub fn initialize(ctx: Context<Initialize>, oracle: Pubkey) -> Result<()> {
        let config = &mut ctx.accounts.config;
        config.oracle = oracle;
        config.deposit_high_water = 0;
        config.pegged_lamports = 0;
        config.withdrawal_count = 0;
        config.bump = ctx.bumps.config;
        config.vault_bump = ctx.bumps.vault;
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
    #[account(mut)]
    pub payer: Signer<'info>,
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
    pub bump: u8,
    pub vault_bump: u8,
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
}
