use anchor_lang::prelude::*;

declare_id!("CKxGfhpH821yFtUKMLQjmbBxrDirV9yDxA2HPuqBFNgA");

#[program]
pub mod hello_solana {
    use super::*;

    pub fn initialize(ctx: Context<Initialize>, initial_count: u64) -> Result<()> {
        let counter = &mut ctx.accounts.counter;
        counter.count = initial_count;
        counter.bump = ctx.bumps.counter;
        counter.authority = ctx.accounts.user.key();
        msg!("Counter initialized with value: {} by authority: {}", initial_count, counter.authority);
        msg!("Greetings from: {:?}", ctx.program_id);
        Ok(())
    }

    pub fn increment(ctx: Context<Increment>) -> Result<()> {
        let counter = &mut ctx.accounts.counter;
        counter.count = counter.count
            .checked_add(1)
            .ok_or(ErrorCode::CounterOverflow)?;
        msg!("Counter incremented to: {}", counter.count);
        Ok(())
    }

    pub fn decrement(ctx: Context<Decrement>) -> Result<()> {
        let counter = &mut ctx.accounts.counter;
        counter.count = counter.count
            .checked_sub(1)
            .ok_or(ErrorCode::CounterUnderflow)?;
        msg!("Counter decremented to: {}", counter.count);
        Ok(())
    }

    pub fn reset(ctx: Context<Reset>) -> Result<()> {
        let counter = &mut ctx.accounts.counter;
        counter.count = 0;
        msg!("Counter reset to: {} by authority: {}", counter.count, counter.authority);
        Ok(())
    }
}

#[derive(Accounts)]
pub struct Initialize<'info> {
    #[account(
        init,
        payer = user,
        space = 8 + Counter::INIT_SPACE,
        seeds = [b"counter"],
        bump
    )]
    pub counter: Account<'info, Counter>,
    #[account(mut)]
    pub user: Signer<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Increment<'info> {
    #[account(
        mut,
        seeds = [b"counter"],
        bump = counter.bump,
        has_one = authority
    )]
    pub counter: Account<'info, Counter>,
    pub authority: Signer<'info>,
}

#[derive(Accounts)]
pub struct Decrement<'info> {
    #[account(
        mut,
        seeds = [b"counter"],
        bump = counter.bump,
        has_one = authority
    )]
    pub counter: Account<'info, Counter>,
    pub authority: Signer<'info>,
}

#[derive(Accounts)]
pub struct Reset<'info> {
    #[account(
        mut,
        seeds = [b"counter"],
        bump = counter.bump,
        has_one = authority
    )]
    pub counter: Account<'info, Counter>,
    pub authority: Signer<'info>,
}

#[account]
#[derive(InitSpace)]
pub struct Counter {
    pub count: u64,
    pub bump: u8,
    pub authority: Pubkey,
}

#[error_code]
pub enum ErrorCode {
    #[msg("Counter cannot go below zero")]
    CounterUnderflow,
    #[msg("Counter overflow: maximum value reached")]
    CounterOverflow,
}
