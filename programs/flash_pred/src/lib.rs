use anchor_lang::prelude::*;
use anchor_spl::token::{self, Token, TokenAccount, Transfer};
use pyth_sdk_solana::load_price_feed_from_account_info;
use std::convert::TryInto;
use anchor_lang::solana_program::pubkey;

declare_id!("7SjyWVdpvmgvrojxmcd6KCC8atXC6h67sop8V5GYdUdz");

// --- ADMIN SETTINGS ---
// This wallet must be the OWNER of the Token Accounts used as treasuries.
pub const PLATFORM_ADMIN: Pubkey = pubkey!("8Ykfrgh4LKsecz48eAo7amXaNWJgmwX8sQRFuoktjdoC");

const MARKET_SEED: &[u8] = b"market";
const USERPOS_SEED: &[u8] = b"userpos";
const MAX_ASSET_NAME_LEN: usize = 32;

// TREASURY FEE CONSTANTS
// Updated to 3% as requested
const TREASURY_FEE_BPS: u64 = 300; 

#[program]
pub mod flash_pred {
    use super::*;

    pub fn create_market(
        ctx: Context<CreateMarket>,
        identifier: String,
        asset_name: String,
        strike_price: u64,
        duration_secs: i64,
        cutoff_buffer_secs: i64,
        grace_secs: i64,
        max_delay_secs: i64,
    ) -> Result<()> {
        // SECURITY CHECK: Platform Admin Only
        require!(
            ctx.accounts.creator.key() == PLATFORM_ADMIN,
            ErrorCode::UnauthorizedMarketCreation
        );

        require!(asset_name.len() <= MAX_ASSET_NAME_LEN, ErrorCode::AssetNameTooLong);
        require!(identifier.len() <= 32, ErrorCode::IdentifierTooLong);

        // --- SAFETY CHECKS: PREVENT BAD CONFIGURATION ---
        // 1. Duration must be positive
        require!(duration_secs > 0, ErrorCode::InvalidMarketConfig);
        // 2. Betting window must exist (Cutoff < Duration) and be non-negative
        // If Cutoff > Duration, betting is closed before market starts.
        require!(cutoff_buffer_secs >= 0 && cutoff_buffer_secs < duration_secs, ErrorCode::InvalidMarketConfig);
        // 3. Grace period must be non-negative and less than Max Delay 
        // If Grace >= Max Delay, the market allows Refund before Resolution is even possible.
        require!(grace_secs >= 0 && max_delay_secs > 0, ErrorCode::InvalidMarketConfig);
        require!(grace_secs < max_delay_secs, ErrorCode::InvalidMarketConfig);
        
        let market = &mut ctx.accounts.market;
        let clock = Clock::get()?;

        market.identifier = identifier;
        market.asset_name = asset_name;
        market.token_mint = ctx.accounts.token_mint.key(); // Store the Mint!
        market.strike_price = strike_price;
        market.expiry_ts = clock.unix_timestamp.checked_add(duration_secs).ok_or(ErrorCode::Overflow)?;
        market.cutoff_buffer_secs = cutoff_buffer_secs;
        market.grace_secs = grace_secs;
        market.max_delay_secs = max_delay_secs;
        market.creator = ctx.accounts.creator.key();
        market.keeper = ctx.accounts.keeper.key();
        market.outcome = Outcome::Pending;
        market.yes_pool = 0;
        market.no_pool = 0;
        market.pyth_price_feed = ctx.accounts.pyth_price_feed.key();
        market.settlement_price = 0;
        market.treasury_collected = 0;
        market.bump = ctx.bumps.market;

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
        user_pos.user = ctx.accounts.user.key();
        user_pos.market = market.key();
        user_pos.side = side as u8;
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

        // --- SAFETY CHECK: PREVENT RESOLUTION LAG ---
        // If the network was down or keeper offline for too long, 
        // the current price is no longer relevant to the expiry time.
        // We force a "Resolution Expired" state so users can refund.
        let max_resolution_ts = market.expiry_ts.checked_add(market.max_delay_secs).ok_or(ErrorCode::Overflow)?;
        require!(clock.unix_timestamp <= max_resolution_ts, ErrorCode::ResolutionTooLate);

        // NOTE: Treasury Wallet security is handled in the ResolveMarket struct constraints.

        require!(
            ctx.accounts.pyth_price_feed.key() == market.pyth_price_feed, 
            ErrorCode::InvalidOracleFeed
        );

        // 1. Load Price Feed (Pyth SDK v0.10.6)
        let price_feed = load_price_feed_from_account_info(
            &ctx.accounts.pyth_price_feed.to_account_info()
        ).map_err(|_| ErrorCode::InvalidOracleFeed)?;

        // 2. Get Current Price with Staleness Check
        let current_timestamp = clock.unix_timestamp;
        let max_age: u64 = 60;

        let current_price = price_feed
            .get_price_no_older_than(current_timestamp, max_age)
            .ok_or(ErrorCode::OraclePriceStale)?;

        let price_i64 = current_price.price;
        let conf_u64 = current_price.conf;
        let expo = current_price.expo;

        if price_i64 == 0 { return err!(ErrorCode::InvalidOraclePrice); }

        // 3. Confidence Check
        let abs_price = price_i64.abs();        
        let max_conf_bps: u64 = 500; // 5%
        require!(
            (conf_u64 as u128) * 10000u128 <= (abs_price as u128) * (max_conf_bps as u128),
            ErrorCode::InvalidOracleConfidence
        );

        // 4. Normalization
        let mut normalized_price_i128: i128 = price_i64 as i128;
        if expo < 0 {
            let mul = 10i128.pow((-expo) as u32);
            normalized_price_i128 = normalized_price_i128.checked_mul(mul).ok_or(ErrorCode::Overflow)?;
        } else if expo > 0 {
            let div = 10i128.pow(expo as u32);
            normalized_price_i128 = normalized_price_i128.checked_div(div).ok_or(ErrorCode::Overflow)?;
        }
        let normalized_price: u64 = normalized_price_i128.try_into().map_err(|_| ErrorCode::Overflow)?;

        // 5. Determine Outcome
        if normalized_price > market.strike_price {
            market.outcome = Outcome::Yes;
        } else if normalized_price < market.strike_price {
            market.outcome = Outcome::No;
        } else {
            // Price == Strike -> TIE
            market.outcome = Outcome::Tie;
        }
        market.settlement_price = normalized_price;

        // 6. LIQUIDITY CHECK (Void Rule)
        let is_one_sided = market.yes_pool == 0 || market.no_pool == 0;
        if is_one_sided {
             market.outcome = Outcome::Refunded;
        }

        // 7. Execute Settlement
        let seeds = &[MARKET_SEED, market.identifier.as_bytes(), market.creator.as_ref(), &[market.bump]];
        let signer = &[&seeds[..]];

        // CASE A: Normal Win
        if market.outcome == Outcome::Yes || market.outcome == Outcome::No {
             let (from_vault, to_vault, transfer_amount) = match market.outcome {
                Outcome::Yes => (
                    ctx.accounts.no_vault.to_account_info(), 
                    ctx.accounts.yes_vault.to_account_info(),
                    market.no_pool
                ),
                Outcome::No => (
                    ctx.accounts.yes_vault.to_account_info(), 
                    ctx.accounts.no_vault.to_account_info(),
                    market.yes_pool
                ),
                _ => unreachable!(),
            };
            
            if transfer_amount > 0 {
                let cpi_accounts = Transfer { from: from_vault, to: to_vault, authority: market.to_account_info() };
                let cpi_ctx = CpiContext::new_with_signer(ctx.accounts.token_program.to_account_info(), cpi_accounts, signer);
                token::transfer(cpi_ctx, transfer_amount)?;
            }

            let total_pool = market.yes_pool.checked_add(market.no_pool).ok_or(ErrorCode::Overflow)?;
            let treasury_fee = (total_pool as u128)
                .checked_mul(TREASURY_FEE_BPS as u128).ok_or(ErrorCode::Overflow)?
                .checked_div(10000u128).ok_or(ErrorCode::DivideByZero)?;
            
            market.treasury_collected = treasury_fee.try_into().map_err(|_| ErrorCode::Overflow)?;
        } 
        
        // CASE B: TIE (Fee Taken from Both Sides immediately)
        else if market.outcome == Outcome::Tie {
            if market.yes_pool > 0 {
                // Safe Math without Unwrap
                let fee_yes = (market.yes_pool as u128)
                    .checked_mul(TREASURY_FEE_BPS as u128).ok_or(ErrorCode::Overflow)?
                    .checked_div(10000).ok_or(ErrorCode::DivideByZero)? as u64;
                    
                if fee_yes > 0 {
                    let cpi_yes = CpiContext::new_with_signer(
                        ctx.accounts.token_program.to_account_info(),
                        Transfer { from: ctx.accounts.yes_vault.to_account_info(), to: ctx.accounts.treasury_wallet.to_account_info(), authority: market.to_account_info() },
                        signer
                    );
                    token::transfer(cpi_yes, fee_yes)?;
                }
            }

            if market.no_pool > 0 {
                // Safe Math without Unwrap
                let fee_no = (market.no_pool as u128)
                    .checked_mul(TREASURY_FEE_BPS as u128).ok_or(ErrorCode::Overflow)?
                    .checked_div(10000).ok_or(ErrorCode::DivideByZero)? as u64;
                    
                if fee_no > 0 {
                    let cpi_no = CpiContext::new_with_signer(
                        ctx.accounts.token_program.to_account_info(),
                        Transfer { from: ctx.accounts.no_vault.to_account_info(), to: ctx.accounts.treasury_wallet.to_account_info(), authority: market.to_account_info() },
                        signer
                    );
                    token::transfer(cpi_no, fee_no)?;
                }
            }
        }
        
        Ok(())
    }

    pub fn claim_winnings(ctx: Context<ClaimWinnings>) -> Result<()> {
        let market = &mut ctx.accounts.market;
        require!(
            market.outcome == Outcome::Yes || market.outcome == Outcome::No, 
            ErrorCode::MarketNotResolved
        );

        let user_pos = &mut ctx.accounts.user_position;
        require!(user_pos.user == ctx.accounts.user.key(), ErrorCode::Unauthorized);
        require!(user_pos.market == market.key(), ErrorCode::CrossMarketSpoofing);
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

        let treasury_amount = market.treasury_collected as u128;
        let payout_pool = total_pool.checked_sub(treasury_amount).ok_or(ErrorCode::Overflow)?;
        
        let user_amount = user_pos.amount as u128;
        let payout_u128 = user_amount
            .checked_mul(payout_pool).ok_or(ErrorCode::Overflow)?
            .checked_div(winner_pool).ok_or(ErrorCode::DivideByZero)?;
        
        let payout: u64 = payout_u128.try_into().map_err(|_| ErrorCode::Overflow)?;

        let vault_account = match market.outcome {
            Outcome::Yes => ctx.accounts.yes_vault.to_account_info(),
            Outcome::No => ctx.accounts.no_vault.to_account_info(),
            _ => unreachable!(),
        };

        let seeds = &[MARKET_SEED, market.identifier.as_bytes(), market.creator.as_ref(), &[market.bump]];
        let signer = &[&seeds[..]];

        user_pos.claimed = true;

        let cpi_accounts = Transfer {
            from: vault_account.clone(),
            to: ctx.accounts.user_token_account.to_account_info(),
            authority: market.to_account_info(),
        };
        let cpi_ctx = CpiContext::new_with_signer(ctx.accounts.token_program.to_account_info(), cpi_accounts, signer);
        token::transfer(cpi_ctx, payout)?;

        Ok(())
    }

    pub fn collect_treasury(ctx: Context<CollectTreasury>) -> Result<()> {
        let market = &mut ctx.accounts.market;
        require!(
            market.outcome == Outcome::Yes || market.outcome == Outcome::No, 
            ErrorCode::MarketNotResolved
        );
        require!(
            ctx.accounts.treasury_authority.key() == market.creator, 
            ErrorCode::Unauthorized
        );
        
        let treasury_amount = market.treasury_collected;
        require!(treasury_amount > 0, ErrorCode::NoTreasuryToCollect);

        let vault_account = match market.outcome {
            Outcome::Yes => ctx.accounts.yes_vault.to_account_info(),
            Outcome::No => ctx.accounts.no_vault.to_account_info(),
            _ => unreachable!(),
        };

        let identifier_bytes = market.identifier.clone();
        let creator_key = market.creator;
        let bump = market.bump;

        market.treasury_collected = 0;

        let seeds = &[MARKET_SEED, identifier_bytes.as_bytes(), creator_key.as_ref(), &[bump]];
        let signer = &[&seeds[..]];

        let cpi_accounts = Transfer {
            from: vault_account.clone(),
            to: ctx.accounts.treasury_wallet.to_account_info(),
            authority: market.to_account_info(),
        };
        let cpi_ctx = CpiContext::new_with_signer(ctx.accounts.token_program.to_account_info(), cpi_accounts, signer);
        token::transfer(cpi_ctx, treasury_amount)?;

        Ok(())
    }

    pub fn refund_unsettlable(ctx: Context<RefundUnsettlable>) -> Result<()> {
        let market = &mut ctx.accounts.market;
        let user_pos = &mut ctx.accounts.user_position;
        let clock = Clock::get()?;

        let can_refund = (clock.unix_timestamp >= market.expiry_ts + market.max_delay_secs && market.outcome == Outcome::Pending) 
            || market.outcome == Outcome::Refunded
            || market.outcome == Outcome::Tie;
        
        require!(can_refund, ErrorCode::RefundNotAllowed);
        require!(user_pos.user == ctx.accounts.user.key(), ErrorCode::Unauthorized);
        require!(!user_pos.claimed, ErrorCode::AlreadyClaimed);
        require!(user_pos.market == market.key(), ErrorCode::CrossMarketSpoofing);

        let mut refund_amount = user_pos.amount;

        // IF TIE: Apply Haircut (User pays fee)
        if market.outcome == Outcome::Tie {
            let keep_bps = 10000 - TREASURY_FEE_BPS; 
            let amount_u128 = refund_amount as u128;
            // Safe Math without Unwrap
            refund_amount = amount_u128
                .checked_mul(keep_bps as u128).ok_or(ErrorCode::Overflow)?
                .checked_div(10000).ok_or(ErrorCode::DivideByZero)? as u64;
        }

        let from_vault = if user_pos.side == 0u8 { 
            ctx.accounts.yes_vault.to_account_info() 
        } else { 
            ctx.accounts.no_vault.to_account_info() 
        };

        let seeds = &[MARKET_SEED, market.identifier.as_bytes(), market.creator.as_ref(), &[market.bump]];
        let signer = &[&seeds[..]];

        user_pos.claimed = true;

        let cpi_accounts = Transfer {
            from: from_vault,
            to: ctx.accounts.user_token_account.to_account_info(),
            authority: market.to_account_info(),
        };
        let cpi_ctx = CpiContext::new_with_signer(ctx.accounts.token_program.to_account_info(), cpi_accounts, signer);
        token::transfer(cpi_ctx, refund_amount)?;

        Ok(())
    }

    pub fn close_market(ctx: Context<CloseMarket>) -> Result<()> {
        let market = &ctx.accounts.market;
        let clock = Clock::get()?;
        
        require!(ctx.accounts.creator.key() == PLATFORM_ADMIN, ErrorCode::Unauthorized);
        
        // SAFE CLOSE CHECK:
        // 1. Market must be Resolved (Not Pending)
        // 2. AND (Vaults are Empty OR Emergency Timeout passed)
        // Emergency Timeout = Expiry + 90 Days (7776000 seconds)
        let is_empty = ctx.accounts.yes_vault.amount == 0 && ctx.accounts.no_vault.amount == 0;
        
        // Checked addition for safety against huge expiry_ts
        let emergency_ts = market.expiry_ts.checked_add(7776000).ok_or(ErrorCode::Overflow)?;
        let is_emergency = clock.unix_timestamp > emergency_ts;

        require!(market.outcome != Outcome::Pending, ErrorCode::MarketNotResolved);
        require!(is_empty || is_emergency, ErrorCode::SafeCloseViolation);
        
        let seeds = &[MARKET_SEED, market.identifier.as_bytes(), market.creator.as_ref(), &[market.bump]];
        let signer = &[&seeds[..]];

        if ctx.accounts.yes_vault.amount > 0 {
            let cpi_accounts = Transfer {
                from: ctx.accounts.yes_vault.to_account_info(),
                to: ctx.accounts.treasury_wallet.to_account_info(),
                authority: market.to_account_info(),
            };
            let cpi_ctx = CpiContext::new_with_signer(ctx.accounts.token_program.to_account_info(), cpi_accounts, signer);
            token::transfer(cpi_ctx, ctx.accounts.yes_vault.amount)?;
        }

        if ctx.accounts.no_vault.amount > 0 {
            let cpi_accounts = Transfer {
                from: ctx.accounts.no_vault.to_account_info(),
                to: ctx.accounts.treasury_wallet.to_account_info(),
                authority: market.to_account_info(),
            };
            let cpi_ctx = CpiContext::new_with_signer(ctx.accounts.token_program.to_account_info(), cpi_accounts, signer);
            token::transfer(cpi_ctx, ctx.accounts.no_vault.amount)?;
        }
        
        Ok(())
    }
}

// --- STRUCTS ---

#[derive(Accounts)]
#[instruction(identifier: String, asset_name: String)]
pub struct CreateMarket<'info> {
    #[account(
        init, 
        payer = creator, 
        seeds = [MARKET_SEED, identifier.as_bytes(), creator.key().as_ref()], 
        bump, 
        space = 8 + 600
    )]
    pub market: Account<'info, Market>,
    #[account(mut)]
    pub creator: Signer<'info>,
    /// CHECK: Permissioned keeper pubkey
    pub keeper: UncheckedAccount<'info>,
    /// CHECK: Pyth price feed pubkey
    pub pyth_price_feed: UncheckedAccount<'info>,
    // NEW: We store this to prevent fake token attacks
    pub token_mint: Account<'info, anchor_spl::token::Mint>,
    pub system_program: Program<'info, System>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
#[instruction(amount: u64, side: Side)]
pub struct PlaceBet<'info> {
    #[account(
        mut, 
        seeds = [MARKET_SEED, market.identifier.as_bytes(), market.creator.as_ref()], 
        bump = market.bump,
        has_one = token_mint
    )]
    pub market: Account<'info, Market>,
    #[account(mut)]
    pub user: Signer<'info>,
    #[account(
        mut,
        constraint = user_token_account.mint == token_mint.key()
    )]
    pub user_token_account: Account<'info, TokenAccount>,
    // NEW: Enforce Vault Ownership and Mint
    #[account(
        mut,
        constraint = yes_vault.owner == market.key() @ ErrorCode::InvalidVaultOwner,
        constraint = yes_vault.mint == token_mint.key()
    )]
    pub yes_vault: Account<'info, TokenAccount>,
    #[account(
        mut,
        constraint = no_vault.owner == market.key() @ ErrorCode::InvalidVaultOwner,
        constraint = no_vault.mint == token_mint.key()
    )]
    pub no_vault: Account<'info, TokenAccount>,
    #[account(
        init_if_needed, 
        payer = user, 
        seeds = [USERPOS_SEED, market.key().as_ref(), user.key().as_ref(), &[side as u8]], 
        bump, 
        space = 8 + 80
    )]
    pub user_position: Account<'info, UserPosition>,
    pub token_mint: Account<'info, anchor_spl::token::Mint>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct ResolveMarket<'info> {
    #[account(
        mut, 
        seeds = [MARKET_SEED, market.identifier.as_bytes(), market.creator.as_ref()], 
        bump = market.bump,
        has_one = token_mint
    )]
    pub market: Account<'info, Market>,
    #[account(signer)]
    pub keeper: Signer<'info>,
    /// CHECK: Pyth feed validated in instruction logic
    pub pyth_price_feed: UncheckedAccount<'info>,
    #[account(
        mut,
        constraint = yes_vault.owner == market.key() @ ErrorCode::InvalidVaultOwner,
        constraint = yes_vault.mint == token_mint.key()
    )]
    pub yes_vault: Account<'info, TokenAccount>,
    #[account(
        mut,
        constraint = no_vault.owner == market.key() @ ErrorCode::InvalidVaultOwner,
        constraint = no_vault.mint == token_mint.key()
    )]
    pub no_vault: Account<'info, TokenAccount>,
    #[account(
        mut,
        constraint = treasury_wallet.owner == PLATFORM_ADMIN @ ErrorCode::InvalidTreasuryWallet,
        constraint = treasury_wallet.mint == token_mint.key()
    )]
    pub treasury_wallet: Account<'info, TokenAccount>,
    pub token_mint: Account<'info, anchor_spl::token::Mint>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct ClaimWinnings<'info> {
    #[account(
        mut, 
        seeds = [MARKET_SEED, market.identifier.as_bytes(), market.creator.as_ref()], 
        bump = market.bump,
        has_one = token_mint
    )]
    pub market: Account<'info, Market>,
    #[account(mut)] 
    pub user_position: Account<'info, UserPosition>,
    #[account(mut)]
    pub user: Signer<'info>,
    #[account(mut)]
    pub user_token_account: Account<'info, TokenAccount>,
    #[account(
        mut,
        constraint = yes_vault.owner == market.key() @ ErrorCode::InvalidVaultOwner,
        constraint = yes_vault.mint == token_mint.key()
    )]
    pub yes_vault: Account<'info, TokenAccount>,
    #[account(
        mut,
        constraint = no_vault.owner == market.key() @ ErrorCode::InvalidVaultOwner,
        constraint = no_vault.mint == token_mint.key()
    )]
    pub no_vault: Account<'info, TokenAccount>,
    pub token_mint: Account<'info, anchor_spl::token::Mint>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct CollectTreasury<'info> {
    #[account(
        mut, 
        seeds = [MARKET_SEED, market.identifier.as_bytes(), market.creator.as_ref()], 
        bump = market.bump,
        has_one = token_mint
    )]
    pub market: Account<'info, Market>,
    #[account(signer)]
    pub treasury_authority: Signer<'info>,
    #[account(
        mut,
        constraint = treasury_wallet.owner == PLATFORM_ADMIN,
        constraint = treasury_wallet.mint == token_mint.key()
    )]
    pub treasury_wallet: Account<'info, TokenAccount>,
    #[account(
        mut,
        constraint = yes_vault.owner == market.key(),
        constraint = yes_vault.mint == token_mint.key()
    )]
    pub yes_vault: Account<'info, TokenAccount>,
    #[account(
        mut,
        constraint = no_vault.owner == market.key(),
        constraint = no_vault.mint == token_mint.key()
    )]
    pub no_vault: Account<'info, TokenAccount>,
    pub token_mint: Account<'info, anchor_spl::token::Mint>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct RefundUnsettlable<'info> {
    #[account(
        mut, 
        seeds = [MARKET_SEED, market.identifier.as_bytes(), market.creator.as_ref()], 
        bump = market.bump,
        has_one = token_mint
    )]
    pub market: Account<'info, Market>,
    #[account(mut)]
    pub user_position: Account<'info, UserPosition>,
    #[account(mut)]
    pub user: Signer<'info>,
    #[account(mut)]
    pub user_token_account: Account<'info, TokenAccount>,
    #[account(
        mut,
        constraint = yes_vault.owner == market.key(),
        constraint = yes_vault.mint == token_mint.key()
    )]
    pub yes_vault: Account<'info, TokenAccount>,
    #[account(
        mut,
        constraint = no_vault.owner == market.key(),
        constraint = no_vault.mint == token_mint.key()
    )]
    pub no_vault: Account<'info, TokenAccount>,
    pub token_mint: Account<'info, anchor_spl::token::Mint>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct CloseMarket<'info> {
    #[account(
        mut, 
        close = creator,
        seeds = [MARKET_SEED, market.identifier.as_bytes(), market.creator.as_ref()], 
        bump = market.bump,
        has_one = token_mint
    )]
    pub market: Account<'info, Market>,
    #[account(mut)]
    pub creator: Signer<'info>,
    #[account(mut)]
    pub yes_vault: Account<'info, TokenAccount>,
    #[account(mut)]
    pub no_vault: Account<'info, TokenAccount>,
    #[account(mut)]
    pub treasury_wallet: Account<'info, TokenAccount>,
    pub token_mint: Account<'info, anchor_spl::token::Mint>,
    pub token_program: Program<'info, Token>,
}

#[account]
pub struct Market {
    pub identifier: String,
    pub asset_name: String,
    pub token_mint: Pubkey, // ADDED: Critical for security
    pub strike_price: u64,
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
    pub treasury_collected: u64,
    pub bump: u8,
}

#[account]
pub struct UserPosition {
    pub user: Pubkey,
    pub market: Pubkey,
    pub side: u8,
    pub amount: u64,
    pub claimed: bool,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, PartialEq, Eq, Copy)]
pub enum Outcome {
    Pending,
    Yes,
    No,
    Refunded,
    Tie,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, PartialEq, Eq, Copy)]
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
    #[msg("Asset name exceeds maximum length")]
    AssetNameTooLong,
    #[msg("Identifier exceeds maximum length")]
    IdentifierTooLong,
    #[msg("No treasury fees to collect")]
    NoTreasuryToCollect,
    #[msg("Oracle price stale")]
    OraclePriceStale,
    #[msg("Invalid Oracle Feed Address")]
    InvalidOracleFeed,
    #[msg("Vaults must be empty to close market")]
    VaultsNotEmptied,
    #[msg("Only platform admin can create markets")]
    UnauthorizedMarketCreation,
    #[msg("User Position does not match market (Spoofing Check)")]
    CrossMarketSpoofing,
    #[msg("Treasury wallet not owned by Platform Admin")]
    InvalidTreasuryWallet,
    #[msg("Vault account owner must be Market PDA")]
    InvalidVaultOwner,
    #[msg("Cannot close market with funds unless emergency")]
    SafeCloseViolation,
    // NEW ERRORS
    #[msg("Invalid market configuration")]
    InvalidMarketConfig,
    #[msg("Resolution window expired, use refund")]
    ResolutionTooLate,
}
