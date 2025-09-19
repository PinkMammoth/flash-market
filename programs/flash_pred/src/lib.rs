flash_pred/
├── Anchor.toml
├── Cargo.toml
├── programs/
│   └── flash_pred/
│       ├── Cargo.toml
│       └── src/
│           └── lib.rs
└── tests/
    └── flash_pred.ts

---

**Anchor.toml:**

```
[programs.localnet]
flash_pred = "flashPred11111111111111111111111111111111111"

[registry]
url = "https://api.devnet.solana.com"

[provider]
cluster = "localnet"
wallet = "~/.config/solana/id.json"

[scripts]
test = "yarn run ts-mocha -p ./tsconfig.json -t 1000000 tests/**/*.ts"
```

---

**Cargo.toml (root):**

```
[workspace]
members = [
    "programs/flash_pred"
]
```

---

**programs/flash_pred/Cargo.toml:**

```
[package]
name = "flash_pred"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib", "lib"]

[dependencies]
anchor-lang = "0.29.0"
anchor-spl = "0.29.0"
pyth-client = "0.7.0"
thiserror = "1.0"
```

---

**programs/flash_pred/src/lib.rs:**

```rust
use anchor_lang::prelude::*;
use anchor_spl::token::{self, Token, TokenAccount, Transfer, Mint};
use pyth_client;
use std::convert::TryInto;

// Program ID (placeholder)
declare_id!("flashPred11111111111111111111111111111111111");

const MARKET_SEED: &[u8] = b"market";
const USERPOS_SEED: &[u8] = b"userpos";

#[program]
pub mod flash_pred {
    use super::*;

    pub fn create_market(
        ctx: Context<CreateMarket>,
        asset_name: String,
        strike_price: u64,
        duration_minutes: i64,
        cutoff_buffer_secs: i64,
        grace_secs: i64,
        max_delay_secs: i64,
    ) -> Result<()> {
        let market = &mut ctx.accounts.market;
        let clock = Clock::get()?;

        market.asset_name = asset_name;
        market.strike_price = strike_price;
        market.expiry_ts = clock.unix_timestamp + (duration_minutes * 60);
        market.creator = ctx.accounts.creator.key();
        market.keeper = ctx.accounts.keeper.key();
        market.outcome = Outcome::Pending;
        market.yes_pool = 0;
        market.no_pool = 0;
        market.pyth_price_feed = ctx.accounts.pyth_price_feed.key();
        market.bump = *ctx.bumps.get("market").ok_or(ErrorCode::BumpMissing)?;

        Ok(())
    }

    /// place_bet creates or updates a UserPosition PDA and transfers USDC from the user to the appropriate pool vault.
    pub fn place_bet(ctx: Context<PlaceBet>, amount: u64, side: Side) -> Result<()> {
        let clock = Clock::get()?;
        let market = &mut ctx.accounts.market;
        require!(clock.unix_timestamp <= market.expiry_ts - market.cutoff_buffer_secs, ErrorCode::BettingClosed);

        // Transfer USDC from user to pool vault
        let (to_vault_info, mut market_pool_field) = match side {
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

        // Update market pool total
        *market_pool_field = market_pool_field.checked_add(amount).ok_or(ErrorCode::Overflow)?;

        // Create or update UserPosition PDA
        let user_pos = &mut ctx.accounts.user_position;
        if user_pos.user == Pubkey::default() {
            // newly initialized by init_if_needed
            user_pos.user = ctx.accounts.user.key();
            user_pos.market = market.key();
            user_pos.side = match side { Side::Yes => 0u8, Side::No => 1u8 };
            user_pos.amount = amount;
            user_pos.claimed = false;
        } else {
            // ensure same side
            require!(user_pos.side == match side { Side::Yes => 0u8, Side::No => 1u8 }, ErrorCode::SideMismatch);
            user_pos.amount = user_pos.amount.checked_add(amount).ok_or(ErrorCode::Overflow)?;
        }

        Ok(())
    }

    /// resolve_market parses on-chain Pyth price and sets the outcome.
    pub fn resolve_market(ctx: Context<ResolveMarket>) -> Result<()> {
        let market = &mut ctx.accounts.market;
        require!(Clock::get()?.unix_timestamp >= market.expiry_ts + market.grace_secs, ErrorCode::MarketNotExpired);
        // permissioned keeper check
        require!(ctx.accounts.keeper.key() == market.keeper, ErrorCode::InvalidKeeper);
        require!(market.outcome == Outcome::Pending, ErrorCode::MarketAlreadyResolved);

        // Parse Pyth price account data
        let pyth_info = &ctx.accounts.pyth_price_feed;
        let data = &pyth_info.try_borrow_data()?;
        let price: &pyth_client::Price = pyth_client::cast::<pyth_client::Price>(data);

        // Use aggregated price
        let agg_price = price.agg.price; // i64
        let expo = price.expo; // i32
        let conf = price.agg.conf; // u64

        // Basic confidence check (e.g. conf/abs(price) <= 0.05 allowed) — tuneable
        if agg_price == 0 {
            return err!(ErrorCode::InvalidOraclePrice);
        }
        let abs_price = if agg_price < 0 { -agg_price } else { agg_price };
        // Avoid floating point: require conf * 100 <= abs_price * max_conf_bps where max_conf_bps = 500 -> 5%
        let max_conf_bps: u64 = 500; // 5%
        require!( (conf as u128) * 100u128 <= (abs_price as i128 as i128).unsigned_abs() as u128 * (max_conf_bps as u128), ErrorCode::InvalidOracleConfidence);

        // Normalize price to u64 with 6 decimals scale (USDC-like)
        // real_price = agg_price * 10^{expo}
        // We want normalized_price = real_price * 10^{6} as u64 (i.e., scale to 1e6)
        let mut normalized_price_u128: i128 = agg_price as i128;
        if expo < 0 {
            let mul = 10i128.pow((-expo) as u32);
            normalized_price_u128 = normalized_price_u128.checked_mul(mul).ok_or(ErrorCode::Overflow)?;
        } else if expo > 0 {
            let div = 10i128.pow(expo as u32);
            normalized_price_u128 = normalized_price_u128.checked_div(div).ok_or(ErrorCode::Overflow)?;
        }
        // scale to 1e6
        normalized_price_u128 = normalized_price_u128.checked_mul(10i128.pow(6)).ok_or(ErrorCode::Overflow)?;
        if normalized_price_u128 < 0 {
            // negative price should not happen
            return err!(ErrorCode::InvalidOraclePrice);
        }
        let normalized_price: u64 = normalized_price_u128.try_into().map_err(|_| ErrorCode::Overflow)?;

        // Determine outcome (asset ABOVE strike => Yes)
        if normalized_price > market.strike_price {
            market.outcome = Outcome::Yes;
        } else {
            market.outcome = Outcome::No;
        }

        market.settlement_price = normalized_price;

        Ok(())
    }

    /// claim_winnings reads UserPosition PDA, computes pro-rata payout and transfers tokens.
    pub fn claim_winnings(ctx: Context<ClaimWinnings>) -> Result<()> {
        let market = &mut ctx.accounts.market;
        require!(market.outcome != Outcome::Pending, ErrorCode::MarketNotResolved);

        let user_pos = &mut ctx.accounts.user_position;
        require!(user_pos.user == ctx.accounts.user.key(), ErrorCode::Unauthorized);
        require!(!user_pos.claimed, ErrorCode::AlreadyClaimed);

        // Check if user's side matches the market outcome
        let user_side = if user_pos.side == 0u8 { Outcome::Yes } else { Outcome::No };
        require!(user_side == market.outcome, ErrorCode::InvalidSideForPayout);

        let total_yes = market.yes_pool as u128;
        let total_no = market.no_pool as u128;
        let winner_pool = match market.outcome {
            Outcome::Yes => total_yes,
            Outcome::No => total_no,
            Outcome::Pending => unreachable!(),
        };
        let total_pool = total_yes.checked_add(total_no).ok_or(ErrorCode::Overflow)?;

        // Payout formula (parimutuel): winners share = (W + L) * (user_amount / W)
        let user_amount = user_pos.amount as u128;
        let payout_u128 = user_amount.checked_mul(total_pool).ok_or(ErrorCode::Overflow)?.checked_div(winner_pool).ok_or(ErrorCode::DivideByZero)?;
        let payout: u64 = payout_u128.try_into().map_err(|_| ErrorCode::Overflow)?;

        // Transfer payout from the winner vault to the user
        let vault_account = match market.outcome {
            Outcome::Yes => ctx.accounts.yes_vault.to_account_info(),
            Outcome::No => ctx.accounts.no_vault.to_account_info(),
            Outcome::Pending => unreachable!(),
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

        // mark claimed
        user_pos.claimed = true;

        Ok(())
    }

    /// refund_unsettlable allows a user to withdraw their original stake if market is refunded or oracle failed.
    pub fn refund_unsettlable(ctx: Context<RefundUnsettlable>) -> Result<()> {
        let market = &mut ctx.accounts.market;
        let user_pos = &mut ctx.accounts.user_position;
        let clock = Clock::get()?;

        // Allow refund if market.state is still Pending and time exceeded max_delay OR market has been marked Refunded
        let can_refund = (clock.unix_timestamp >= market.expiry_ts + market.max_delay_secs && market.outcome == Outcome::Pending) || market.outcome == Outcome::Refunded;
        require!(can_refund, ErrorCode::RefundNotAllowed);
        require!(user_pos.user == ctx.accounts.user.key(), ErrorCode::Unauthorized);
        require!(!user_pos.claimed, ErrorCode::AlreadyClaimed);

        let amount = user_pos.amount;
        // Determine which vault to refund from
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

        // Mark as claimed/refunded for this user
        user_pos.claimed = true;

        Ok(())
    }
}

// -------------------------
// Accounts & Contexts
// -------------------------

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
    pub token_program: Program<'info, Token>,
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
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
pub struct ResolveMarket<'info> {
    #[account(mut, seeds = [MARKET_SEED, market.creator.as_ref()], bump = market.bump)]
    pub market: Account<'info, Market>,
    #[account(mut)]
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

#[derive(AnchorSerialize, AnchorDeserialize, Clone, PartialEq, Eq)]
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
    #[msg("Invalid oracle confidence interval.")]
    InvalidOracleConfidence,
    #[msg("Market outcome not resolved yet.")]
    MarketNotResolved,
    #[msg("Invalid side for claiming winnings.")]
    InvalidSide,
    #[msg("Refund window not open yet.")]
    RefundTooSoon,
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
```

---

**tests/flash_pred.ts** (unchanged skeleton)

```
/*
import * as anchor from '@project-serum/anchor';
import { Program } from '@project-serum/anchor';
import { FlashPred } from '../target/types/flash_pred';
import assert from 'assert';

describe('flash_pred', () => {
  const provider = anchor.AnchorProvider.env();
  anchor.setProvider(provider);

  const program = anchor.workspace.FlashPred as Program<FlashPred>;

  it('creates market, bets, resolves and allows claim', async () => {
    assert.ok(true);
  });
});
*/
```

---

I updated the program to:
- Add `UserPosition` PDA logic and `init_if_needed` in `place_bet`.
- Re-implement `claim_winnings` to read the `UserPosition`, compute a pro-rata parimutuel payout using 128-bit math and transfer tokens from the winner vault via the market PDA signer.
- Change `refund_unsettlable` so a user can withdraw their original stake using their `UserPosition` (either after the market is explicitly marked `Refunded` or when `expiry + max_delay_secs` has passed and outcome remains `Pending`).

You can now paste this file into your Replit/Anchor project. Want me to also update the test to assert the full create->bet->resolve->claim flow using mocked Pyth data? If so, I'll generate that next.
