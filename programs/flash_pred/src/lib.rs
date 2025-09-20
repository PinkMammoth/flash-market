use anchor_lang::prelude::*;
use anchor_spl::token::{self, Token, TokenAccount, Transfer};
use pyth_client;
use std::convert::TryInto;

declare_id!("Fg6PaFpoGXkYsidMpWTK6W2BeZ7FEfcYkg3q2aW7v4Yk");

const MARKET_SEED: &[u8] = b"market";
const USERPOS_SEED: &[u8] = b"userpos";

#[program]
pub mod flash_pred {
    use super::*;

    pub fn create_market(
        ctx: Context<CreateMarket>,
        asset_name: String,
        strike_price: u64,
        duration_secs: i64,
        cutoff_buffer_secs: i64,
        grace_secs: i64,
        max_delay_secs: i64,
    ) -> Result<()> {
        let market = &mut ctx.accounts.market;
        let clock = Clock::get()?;

        market.asset_name = asset_name;
        market.strike_price = strike_price;
        market.expiry_ts = clock.unix_timestamp.checked_add(duration_secs).ok_or(ErrorCode::Overflow)?;
        market.creator = ctx.accounts.creator.key();
        market.keeper = ctx.accounts.keeper.key();
        market.outcome = Outcome::Pending;
        market.yes_pool = 0;
        market.no_pool = 0;
        market.pyth_price_feed = ctx.accounts.pyth_price_feed.key();
        market.bump = ctx.bumps.market; // Updated bump access for Anchor 0.31.1

        Ok(())
    }

    pub fn place_bet(ctx: Context<PlaceBet>, amount: u64, side: Side) -> Result<()> {
        let clock = Clock::get()?;
        let market = &mut ctx.accounts.market;
        let cutoff_ts = market.expiry_ts.checked_sub(market.cutoff_buffer_secs).ok_or(ErrorCode::Overflow)?;
        require!(clock.unix_timestamp <= cutoff_ts, ErrorCode::BettingClosed);

        let (to_vault_info, market_pool_field) = match side {
            Side::Yes => (ctx.accounts.yes_vault.to_account_info(), &mut market.yes_pool),
            Side::No => (ctx.accounts.no_vault.to_account_info(), &mut market.no_pool),
        };

        let cpi_accounts = Transfer {
            from: ctx.accounts.user_token_account.to_account_info(),
            to: to_vault_info.clone(),
            authority: ctx.accounts.user.to_account_info(),
        };
        let cpi_ctx = CpiContext::new(ctx.accounts.token_program.to_account_info(), cpi_accounts);
        token::transfer(cpi_ctx, amount)?;

        *market_pool_field = market_pool_field.checked_add(amount).ok_or(ErrorCode::Overflow)?;

        let user_pos = &mut ctx.accounts.user_position;
        // init_if_needed handles initialization automatically in Anchor 0.31.1
        user_pos.user = ctx.accounts.user.key();
        user_pos.market = market.key();
        user_pos.side = if side == Side::Yes { 0u8 } else { 1u8 };
        user_pos.amount = user_pos.amount.checked_add(amount).ok_or(ErrorCode::Overflow)?;
        user_pos.claimed = false;
        
        Ok(())
    }

    pub fn resolve_market(ctx: Context<ResolveMarket>) -> Result<()> {
        let market = &mut ctx.accounts.market;
        let clock = Clock::get()?;
        let grace_expiry_ts = market.expiry_ts.checked_add(market.grace_secs).ok_or(ErrorCode::Overflow)?;
        require!(clock.unix_timestamp >= grace_expiry_ts, ErrorCode::MarketNotExpired);
        require!(ctx.accounts.keeper.key() == market.keeper, ErrorCode::InvalidKeeper);
        require!(market.outcome == Outcome::Pending, ErrorCode::MarketAlreadyResolved);

        let pyth_info = &ctx.accounts.pyth_price_feed;
        let data = &pyth_info.try_borrow_data()?;
        let price_feed = pyth_client::cast::<pyth_client::PriceFeed>(data);
        let price = price_feed.get_price_unchecked(); // Use get_price_unchecked for simplicity in this context

        let agg_price = price.price;
        let expo = price.expo;
        let conf = price.conf;

        if agg_price == 0 { return err!(ErrorCode::InvalidOraclePrice); }
        let abs_price = if agg_price < 0 { -agg_price } else { agg_price };
        let max_conf_bps: u64 = 500; // 5%
        require!((conf as u128) * 10000u128 <= (abs_price as u128) * (max_conf_bps as u128), ErrorCode::InvalidOracleConfidence);

        let mut normalized_price_i128: i128 = agg_price as i128;
        if expo < 0 {
            let mul = 10i128.pow((-expo) as u32);
            normalized_price_i128 = normalized_price_i128.checked_mul(mul).ok_or(ErrorCode::Overflow)?;
        } else if expo > 0 {
            let div = 10i128.pow(expo as u32);
            normalized_price_i128 = normalized_price_i128.checked_div(div).ok_or(ErrorCode::Overflow)?;
        }
        
        let normalized_price: u64 = normalized_price_i128.try_into().map_err(|_| ErrorCode::Overflow)?;

        if normalized_price > market.strike_price {
            market.outcome = Outcome::Yes;
        } else {
            market.outcome = Outcome::No;
        }

        market.settlement_price = normalized_price;

        Ok(())
    }

    pub fn claim_winnings(ctx: Context<ClaimWinnings>) -> Result<()> {
        let market = &mut ctx.accounts.market;
        require!(market.outcome == Outcome::Yes || market.outcome == Outcome::No, ErrorCode::MarketNotResolved);

        let user_pos = &mut ctx.accounts.user_position;
        require!(user_pos.user == ctx.accounts.user.key(), ErrorCode::Unauthorized);
        require!(!user_pos.claimed, ErrorCode::AlreadyClaimed);

        let user_side_enum = if user_pos.side == 0u8 { Outcome::Yes } else { Outcome::No };
        require!(user_side_enum == market.outcome, ErrorCode::InvalidSideForPayout);

        let total_yes = market.yes_pool as u128;
        let total_no = market.no_pool as u128;
        let winner_pool = match market.outcome {
            Outcome::Yes => total_yes,
            Outcome::No => total_no,
            _ => unreachable!(),
        };
        let total_pool = total_yes.checked_add(total_no).ok_or(ErrorCode::Overflow)?;

        if winner_pool == 0 { return err!(ErrorCode::DivideByZero); }

        let user_amount = user_pos.amount as u128;
        let payout_u128 = user_amount.checked_mul(total_pool).ok_or(ErrorCode::Overflow)?.checked_div(winner_pool).ok_or(ErrorCode::DivideByZero)?;
        let payout: u64 = payout_u128.try_into().map_err(|_| ErrorCode::Overflow)?;

        let vault_account = match market.outcome {
            Outcome::Yes => ctx.accounts.yes_vault.to_account_info(),
            Outcome::No => ctx.accounts.no_vault.to_account_info(),
            _ => unreachable!(),
        };

        let seeds = &[MARKET_SEED, market.creator.as_ref(), &[market.bump]];
        let signer = &[&seeds[..]];

        let cpi_accounts = Transfer {
            from: vault_account.clone(),
            to: ctx.accounts.user_token_account.to_account_info(),
            authority: ctx.accounts.market.to_account_info(),
        };
        let cpi_ctx = CpiContext::new_with_signer(ctx.accounts.token_program.to_account_info(), cpi_accounts, signer);
        token::transfer(cpi_ctx, payout)?;

        user_pos.claimed = true;

        Ok(())
    }

    pub fn refund_unsettlable(ctx: Context<RefundUnsettlable>) -> Result<()> {
        let market = &mut ctx.accounts.market;
        let user_pos = &mut ctx.accounts.user_position;
        let clock = Clock::get()?;

        let max_delay_expiry_ts = market.expiry_ts.checked_add(market.max_delay_secs).ok_or(ErrorCode::Overflow)?;
        let can_refund = (clock.unix_timestamp >= max_delay_expiry_ts && market.outcome == Outcome::Pending) || market.outcome == Outcome::Refunded;
        require!(can_refund, ErrorCode::RefundNotAllowed);
        require!(user_pos.user == ctx.accounts.user.key(), ErrorCode::Unauthorized);
        require!(!user_pos.claimed, ErrorCode::AlreadyClaimed);

        let amount = user_pos.amount;
        let from_vault = if user_pos.side == 0u8 { ctx.accounts.yes_vault.to_account_info() } else { ctx.accounts.no_vault.to_account_info() };

        let seeds = &[MARKET_SEED, market.creator.as_ref(), &[market.bump]];
        let signer = &[&seeds[..]];

        let cpi_accounts = Transfer {
            from: from_vault,
            to: ctx.accounts.user_token_account.to_account_info(),
            authority: ctx.accounts.market.to_account_info(),
        };
        let cpi_ctx = CpiContext::new_with_signer(ctx.accounts.token_program.to_account_info(), cpi_accounts, signer);
        token::transfer(cpi_ctx, amount)?;

        user_pos.claimed = true;

        Ok(())
    }
}

#[derive(Accounts)]
#[instruction(asset_name: String)]
pub struct CreateMarket<'info> {
    #[account(init, payer = creator, seeds = [MARKET_SEED, creator.key().as_ref()], bump, space = 8 + 512)]
    pub market: Account<'info, Market>,
    #[account(mut)]
    pub creator: Signer<'info>,
    /// CHECK: permissioned keeper pubkey
    pub keeper: UncheckedAccount<'info>,
    /// CHECK: pyth price feed pubkey
    pub pyth_price_feed: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
pub struct PlaceBet<'info> {
    #[account(mut, seeds = [MARKET_SEED, market.creator.as_ref()], bump = market.bump)]
    pub market: Account<'info, Market>,
    #[account(mut)]
    pub user: Signer<'info>,
    #[account(mut)]
    pub user_token_account: Account<'info, TokenAccount>,
    #[account(mut)]
    pub yes_vault: Account<'info, TokenAccount>,
    #[account(mut)]
    pub no_vault: Account<'info, TokenAccount>,
    #[account(init_if_needed, payer = user, seeds = [USERPOS_SEED, market.key().as_ref(), user.key().as_ref()], bump, space = 8 + 64)]
    pub user_position: Account<'info, UserPosition>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct ResolveMarket<'info> {
    #[account(mut, seeds = [MARKET_SEED, market.creator.as_ref()], bump = market.bump)]
    pub market: Account<'info, Market>,
    #[account(signer)]
    pub keeper: Signer<'info>,
    /// CHECK: Pyth feed account data passed for validation
    pub pyth_price_feed: UncheckedAccount<'info>,
}

#[derive(Accounts)]
pub struct ClaimWinnings<'info> {
    #[account(mut, seeds = [MARKET_SEED, market.creator.as_ref()], bump = market.bump)]
    pub market: Account<'info, Market>,
    #[account(mut, seeds = [USERPOS_SEED, market.key().as_ref(), user.key().as_ref()], bump)]
    pub user_position: Account<'info, UserPosition>,
    #[account(mut)]
    pub user: Signer<'info>,
    #[account(mut)]
    pub user_token_account: Account<'info, TokenAccount>,
    #[account(mut)]
    pub yes_vault: Account<'info, TokenAccount>,
    #[account(mut)]
    pub no_vault: Account<'info, TokenAccount>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct RefundUnsettlable<'info> {
    #[account(mut, seeds = [MARKET_SEED, market.creator.as_ref()], bump = market.bump)]
    pub market: Account<'info, Market>,
    #[account(mut, seeds = [USERPOS_SEED, market.key().as_ref(), user.key().as_ref()], bump)]
    pub user_position: Account<'info, UserPosition>,
    #[account(mut)]
    pub user: Signer<'info>,
    #[account(mut)]
    pub user_token_account: Account<'info, TokenAccount>,
    #[account(mut)]
    pub yes_vault: Account<'info, TokenAccount>,
    #[account(mut)]
    pub no_vault: Account<'info, TokenAccount>,
    pub token_program: Program<'info, Token>,
}

#[account]
pub struct Market {
    pub asset_name: String,
    pub strike_price: u64, // stored in 1e6 scale
    pub expiry_ts: i64,
    pub cutoff_buffer_secs: i64,
    pub grace_secs: i64,
    pub max_delay_secs: i64,
    pub creator: Pubkey,
    pub keeper: Pubkey,
    pub outcome: Outcome,
    pub yes_pool: u64,
    pub no_pool: u64,
    pub pyth_price_feed: Pubkey,
    pub settlement_price: u64,
    pub bump: u8,
}

#[account]
pub struct UserPosition {
    pub user: Pubkey,
    pub market: Pubkey,
    pub side: u8, // 0 = yes, 1 = no
    pub amount: u64,
    pub claimed: bool,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, PartialEq, Eq, Copy)]
pub enum Outcome {
    Pending,
    Yes,
    No,
    Refunded,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, PartialEq, Eq)]
pub enum Side {
    Yes,
    No,
}

#[error_code]
pub enum ErrorCode {
    #[msg("Betting window has closed.")]
    BettingClosed,
    #[msg("Market has not expired yet.")]
    MarketNotExpired,
    #[msg("Invalid keeper attempting to resolve.")]
    InvalidKeeper,
    #[msg("Market already resolved.")]
    MarketAlreadyResolved,
    #[msg("Market outcome not resolved yet.")]
    MarketNotResolved,
    #[msg("Math overflow detected.")]
    Overflow,
    #[msg("Divide by zero")]
    DivideByZero,
    #[msg("Missing PDA bump")]
    BumpMissing,
    #[msg("Unauthorized")]
    Unauthorized,
    #[msg("Invalid oracle price")]
    InvalidOraclePrice,
    #[msg("Invalid oracle confidence")]
    InvalidOracleConfidence,
    #[msg("User already claimed")]
    AlreadyClaimed,
    #[msg("Refund not allowed")]
    RefundNotAllowed,
    #[msg("User side mismatch")]
    SideMismatch,
    #[msg("Invalid side for payout")]
    InvalidSideForPayout,
}
