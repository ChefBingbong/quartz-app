use anchor_lang::{
    prelude::*, 
    solana_program::{
        instruction::Instruction, 
        sysvar::instructions::{
            self,
            load_current_index_checked, 
            load_instruction_at_checked
        }
    }, 
    Discriminator
};
use anchor_spl::{
    associated_token::AssociatedToken, 
    token::{self, Mint, Token, TokenAccount}
};
use drift::{
    cpi::{
        accounts::Deposit as DriftDeposit,
        deposit as drift_deposit
    }, 
    state::{
        state::State as DriftState, 
        user::User as DriftUser
    },
    program::Drift
};
use crate::{
    check, 
    constants::{DRIFT_MARKET_INDEX_USDC, JUPITER_EXACT_OUT_ROUTE_DISCRIMINATOR, JUPITER_ID, USDC_MINT}, 
    errors::QuartzError, 
    helpers::get_jup_exact_out_route_out_amount, 
    load_mut,
    math::{get_drift_margin_calculation, get_quartz_account_health}, 
    state::Vault
};

#[derive(Accounts)]
pub struct AutoRepayDeposit<'info> {
    #[account(
        mut,
        seeds = [b"vault".as_ref(), owner.key().as_ref()],
        bump = vault.bump,
        has_one = owner
    )]
    pub vault: Box<Account<'info, Vault>>,

    #[account(
        init,
        seeds = [vault.key().as_ref(), spl_mint.key().as_ref()],
        bump,
        payer = caller,
        token::mint = spl_mint,
        token::authority = vault
    )]
    pub vault_spl: Box<Account<'info, TokenAccount>>,

    /// CHECK: Can be any account, once it has a Vault
    pub owner: UncheckedAccount<'info>,

    #[account(mut)]
    pub caller: Signer<'info>,

    #[account(
        mut,
        associated_token::mint = spl_mint,
        associated_token::authority = caller
    )]
    pub caller_spl: Box<Account<'info, TokenAccount>>,

    #[account(
        constraint = spl_mint.key().eq(&USDC_MINT) @ QuartzError::InvalidRepayMint
    )]
    pub spl_mint: Box<Account<'info, Mint>>,

    #[account(
        mut,
        seeds = [b"user".as_ref(), vault.key().as_ref(), (0u16).to_le_bytes().as_ref()],
        seeds::program = drift_program.key(),
        bump
    )]
    pub drift_user: AccountLoader<'info, DriftUser>,
    
    /// CHECK: This account is passed through to the Drift CPI, which performs the security checks
    #[account(mut)]
    pub drift_user_stats: UncheckedAccount<'info>,

    #[account(
        mut,
        seeds = [b"drift_state".as_ref()],
        seeds::program = drift_program.key(),
        bump
    )]
    pub drift_state: Box<Account<'info, DriftState>>,
    
    /// CHECK: This account is passed through to the Drift CPI, which performs the security checks
    #[account(mut)]
    pub spot_market_vault: UncheckedAccount<'info>,

    pub token_program: Program<'info, Token>,

    pub associated_token_program: Program<'info, AssociatedToken>,

    pub drift_program: Program<'info, Drift>,

    pub system_program: Program<'info, System>,

    /// CHECK: Account is safe once address is correct
    #[account(address = instructions::ID)]
    instructions: UncheckedAccount<'info>,
}

#[inline(never)]
fn validate_instruction_order<'info>(
    start_instruction: &Instruction,
    swap_instruction: &Instruction,
    withdraw_instruction: &Instruction
) -> Result<()> {
    // Check the 1st instruction is auto_repay_start
    check!(
        start_instruction.program_id.eq(&crate::id()),
        QuartzError::IllegalAutoRepayInstructions
    );

    check!(
        start_instruction.data[..8]
            .eq(&crate::instruction::AutoRepayStart::DISCRIMINATOR),
        QuartzError::IllegalAutoRepayInstructions
    );

    // Check the 2nd instruction is Jupiter's exact_out_route
    check!(
        swap_instruction.program_id.eq(&JUPITER_ID),
        QuartzError::IllegalAutoRepayInstructions
    );

    check!(
        swap_instruction.data[..8].eq(&JUPITER_EXACT_OUT_ROUTE_DISCRIMINATOR),
        QuartzError::IllegalAutoRepayInstructions
    );

    // This instruction is the 3rd instruction

    // Check the 4th instruction is auto_repay_withdraw
    check!(
        withdraw_instruction.program_id.eq(&crate::id()),
        QuartzError::IllegalAutoRepayInstructions
    );

    check!(
        withdraw_instruction.data[..8]
            .eq(&crate::instruction::AutoRepayWithdraw::DISCRIMINATOR),
        QuartzError::IllegalAutoRepayInstructions
    );

    Ok(())
}

#[inline(never)]
fn validate_account_health<'info>(
    ctx: &Context<'_, '_, 'info, 'info, AutoRepayDeposit<'info>>,
    drift_market_index: u16
) -> Result<()> {
    let user = &mut load_mut!(ctx.accounts.drift_user)?;
    let margin_calculation = get_drift_margin_calculation(
        user, 
        &ctx.accounts.drift_state, 
        drift_market_index, 
        &ctx.remaining_accounts
    )?;

    let quartz_account_health = get_quartz_account_health(margin_calculation)?;

    check!(
        quartz_account_health == 0,
        QuartzError::NotReachedAutoRepayThreshold
    );

    Ok(())
}

pub fn auto_repay_deposit_handler<'info>(
    ctx: Context<'_, '_, 'info, 'info, AutoRepayDeposit<'info>>,
    drift_market_index: u16
) -> Result<()> {
    check!(
        drift_market_index == DRIFT_MARKET_INDEX_USDC,
        QuartzError::UnsupportedDriftMarketIndex
    );

    let index: usize = load_current_index_checked(&ctx.accounts.instructions.to_account_info())?.into();
    let start_instruction = load_instruction_at_checked(index - 2, &ctx.accounts.instructions.to_account_info())?;
    let swap_instruction = load_instruction_at_checked(index - 1, &ctx.accounts.instructions.to_account_info())?;
    let withdraw_instruction = load_instruction_at_checked(index + 1, &ctx.accounts.instructions.to_account_info())?;

    validate_instruction_order(&start_instruction, &swap_instruction, &withdraw_instruction)?;

    // Validate mint
    let swap_destination_mint = swap_instruction.accounts[6].pubkey;
    check!(
        swap_destination_mint.eq(&ctx.accounts.spl_mint.key()),
        QuartzError::InvalidRepayMint
    );

    let swap_destination_token_account = swap_instruction.accounts[3].pubkey;
    check!(
        swap_destination_token_account.eq(&ctx.accounts.caller_spl.key()),
        QuartzError::InvalidDestinationTokenAccount
    );

    validate_account_health(&ctx, drift_market_index)?;

    let vault_bump = ctx.accounts.vault.bump;
    let owner = ctx.accounts.owner.key();
    let seeds = &[
        b"vault",
        owner.as_ref(),
        &[vault_bump]
    ];
    let signer_seeds = &[&seeds[..]];

    // Get deposit amount from swap instruction
    let deposit_amount = get_jup_exact_out_route_out_amount(&swap_instruction)?;

    // Transfer tokens from callers's ATA to vault's ATA
    token::transfer(
        CpiContext::new(
            ctx.accounts.token_program.to_account_info(), 
            token::Transfer { 
                from: ctx.accounts.caller_spl.to_account_info(), 
                to: ctx.accounts.vault_spl.to_account_info(), 
                authority: ctx.accounts.caller.to_account_info()
            }
        ),
        deposit_amount
    )?;

    // Drift Deposit CPI
    let mut cpi_ctx = CpiContext::new_with_signer(
        ctx.accounts.drift_program.to_account_info(),
        DriftDeposit {
            state: ctx.accounts.drift_state.to_account_info(),
            user: ctx.accounts.drift_user.to_account_info(),
            user_stats: ctx.accounts.drift_user_stats.to_account_info(),
            authority: ctx.accounts.vault.to_account_info(),
            spot_market_vault: ctx.accounts.spot_market_vault.to_account_info(),
            user_token_account: ctx.accounts.vault_spl.to_account_info(),
            token_program: ctx.accounts.token_program.to_account_info(),
        },
        signer_seeds
    );

    cpi_ctx.remaining_accounts = ctx.remaining_accounts.to_vec();

    // reduce_only = true to prevent depositing more than the borrowed position
    drift_deposit(cpi_ctx, drift_market_index, deposit_amount, true)?;

    // Close vault's ATA
    let cpi_ctx_close = CpiContext::new_with_signer(
        ctx.accounts.token_program.to_account_info(),
        token::CloseAccount {
            account: ctx.accounts.vault_spl.to_account_info(),
            destination: ctx.accounts.caller.to_account_info(),
            authority: ctx.accounts.vault.to_account_info(),
        },
        signer_seeds
    );
    token::close_account(cpi_ctx_close)?;

    Ok(())
}