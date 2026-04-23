use anchor_lang::prelude::*;
use anchor_lang::system_program;

declare_id!("9qoVHkGeZEnrFC7rZi3tLxRafT2HXwCQ3nGueEGcUdtN"); // replace after deploy

// ── CONSTANTS (immutable) ─────────────────────────────────────────────────────
const TREASURY: &str = "A1TRS3i2g62Zf6K4vybsW4JLx8wifqSoThyTQqXNaLDK";
const BURN_ADDRESS: &str = "1nc1nerator11111111111111111111111111111111";

// Fee split: 50% treasury / 50% burned
const TREASURY_BPS: u64 = 5000;
const BURN_BPS: u64 = 5000;
const BASIS_POINTS: u64 = 10000;

// Platform fee on restaking rewards: 10%
const PLATFORM_FEE_BPS: u64 = 1000;

// Minimum restake: 100 XNT
const MIN_RESTAKE: u64 = 100_000_000_000;

// Slashing: 20% of restaked amount if AVS is attacked on your watch
const SLASH_BPS: u64 = 2000;

// Unbond cooldown: 14 epochs
const UNBOND_EPOCHS: u64 = 14;

// Max AVS registered
const MAX_AVS: usize = 50;
const MAX_OPERATORS: usize = 200;

/// AVS = Actively Validated Service
/// Examples: bridges, oracles, DA layers, rollups
/// They pay validators to secure them using restaked XNT

#[program]
pub mod restaking {
    use super::*;

    /// Initialize the restaking registry (once)
    pub fn initialize(ctx: Context<Initialize>) -> Result<()> {
        let state = &mut ctx.accounts.state;
        state.authority = ctx.accounts.authority.key();
        state.avs_count = 0;
        state.operator_count = 0;
        state.total_restaked = 0;
        state.total_fees_collected = 0;
        state.total_burned = 0;
        state.bump = ctx.bumps.state;
        Ok(())
    }

    /// Register an AVS (bridge, oracle, rollup, etc.)
    /// AVS pays a registration fee and declares their security requirements
    pub fn register_avs(
        ctx: Context<RegisterAvs>,
        name: [u8; 32],
        min_operator_stake: u64,   // min XNT each operator must restake
        reward_rate_bps: u64,      // annual reward rate in bps (e.g. 500 = 5%)
        registration_fee: u64,     // fee paid to join (goes to treasury/burn)
    ) -> Result<()> {
        let state = &mut ctx.accounts.state;
        require!((state.avs_count as usize) < MAX_AVS, RestakingError::MaxAvsReached);
        require!(min_operator_stake >= MIN_RESTAKE, RestakingError::StakeTooSmall);
        require!(reward_rate_bps > 0 && reward_rate_bps <= 3000, RestakingError::InvalidRewardRate);
        require!(registration_fee > 0, RestakingError::InvalidFee);

        // Pay registration fee — split treasury/burn
        let treasury_amt = registration_fee * TREASURY_BPS / BASIS_POINTS;
        let burn_amt = registration_fee - treasury_amt;
        system_program::transfer(CpiContext::new(ctx.accounts.system_program.to_account_info(),
            system_program::Transfer { from: ctx.accounts.avs_authority.to_account_info(), to: ctx.accounts.treasury.to_account_info() }), treasury_amt)?;
        system_program::transfer(CpiContext::new(ctx.accounts.system_program.to_account_info(),
            system_program::Transfer { from: ctx.accounts.avs_authority.to_account_info(), to: ctx.accounts.burn_address.to_account_info() }), burn_amt)?;

        let idx = state.avs_count as usize;
        state.avs_registry[idx] = AvsEntry {
            authority: ctx.accounts.avs_authority.key(),
            name,
            min_operator_stake,
            reward_rate_bps,
            total_secured: 0,
            operator_count: 0,
            active: true,
            registered_epoch: Clock::get()?.epoch,
        };
        state.avs_count += 1;
        state.total_fees_collected += registration_fee;
        state.total_burned += burn_amt;

        emit!(AvsRegistered {
            authority: ctx.accounts.avs_authority.key(),
            min_stake: min_operator_stake,
            reward_rate_bps,
            epoch: Clock::get()?.epoch,
        });

        Ok(())
    }

    /// Validator opts in to secure an AVS with restaked XNT
    /// Their bonded XNT now also backs the AVS — earns additional rewards
    pub fn opt_in_avs(
        ctx: Context<OptInAvs>,
        avs_index: u32,
        restake_amount: u64,
    ) -> Result<()> {
        let state = &mut ctx.accounts.state;
        let avs_idx = avs_index as usize;
        require!(avs_idx < state.avs_count as usize, RestakingError::AvsNotFound);
        require!(state.avs_registry[avs_idx].active, RestakingError::AvsInactive);
        require!(restake_amount >= state.avs_registry[avs_idx].min_operator_stake, RestakingError::StakeTooSmall);

        let identity = ctx.accounts.validator_identity.key();

        // Check operator not already opted in to this AVS
        for i in 0..state.operator_count as usize {
            if state.operators[i].identity == identity && state.operators[i].avs_index == avs_index {
                return Err(RestakingError::AlreadyOptedIn.into());
            }
        }
        require!((state.operator_count as usize) < MAX_OPERATORS, RestakingError::MaxOperatorsReached);

        // Lock restaked XNT in vault
        system_program::transfer(CpiContext::new(ctx.accounts.system_program.to_account_info(),
            system_program::Transfer { from: ctx.accounts.validator_identity.to_account_info(), to: ctx.accounts.restake_vault.to_account_info() }), restake_amount)?;

        let op_idx = state.operator_count as usize;
        state.operators[op_idx] = OperatorEntry {
            identity,
            avs_index,
            restaked_amount: restake_amount,
            rewards_earned: 0,
            slashed: false,
            unbonding: false,
            unbond_epoch: 0,
            opted_in_epoch: Clock::get()?.epoch,
        };
        state.operator_count += 1;
        state.avs_registry[avs_idx].total_secured += restake_amount;
        state.avs_registry[avs_idx].operator_count += 1;
        state.total_restaked += restake_amount;

        emit!(OperatorOptedIn {
            identity,
            avs_index,
            restaked: restake_amount,
            epoch: Clock::get()?.epoch,
        });

        Ok(())
    }

    /// Distribute restaking rewards to operators from AVS reward pool
    /// Platform takes 10% (50% treasury / 50% burned)
    pub fn distribute_rewards(
        ctx: Context<DistributeRewards>,
        avs_index: u32,
        total_reward_pool: u64,
    ) -> Result<()> {
        let state = &mut ctx.accounts.state;
        let avs_idx = avs_index as usize;
        require!(avs_idx < state.avs_count as usize, RestakingError::AvsNotFound);

        // Platform fee: 10%
        let platform_fee = total_reward_pool * PLATFORM_FEE_BPS / BASIS_POINTS;
        let treasury_fee = platform_fee * TREASURY_BPS / BASIS_POINTS;
        let burn_fee = platform_fee - treasury_fee;
        let distributable = total_reward_pool - platform_fee;

        // Pay platform fee
        system_program::transfer(CpiContext::new(ctx.accounts.system_program.to_account_info(),
            system_program::Transfer { from: ctx.accounts.avs_reward_pool.to_account_info(), to: ctx.accounts.treasury.to_account_info() }), treasury_fee)?;
        system_program::transfer(CpiContext::new(ctx.accounts.system_program.to_account_info(),
            system_program::Transfer { from: ctx.accounts.avs_reward_pool.to_account_info(), to: ctx.accounts.burn_address.to_account_info() }), burn_fee)?;

        // Distribute proportionally to operators based on restaked amount
        let avs_total_secured = state.avs_registry[avs_idx].total_secured;
        if avs_total_secured == 0 { return Ok(()); }

        for i in 0..state.operator_count as usize {
            if state.operators[i].avs_index == avs_index && !state.operators[i].slashed && !state.operators[i].unbonding {
                let share = distributable * state.operators[i].restaked_amount / avs_total_secured;
                state.operators[i].rewards_earned += share;
            }
        }

        state.total_fees_collected += platform_fee;
        state.total_burned += burn_fee;

        emit!(RewardsDistributed {
            avs_index,
            total_pool: total_reward_pool,
            platform_fee,
            distributed: distributable,
            burned: burn_fee,
            epoch: Clock::get()?.epoch,
        });

        Ok(())
    }

    /// Slash an operator for misbehavior (called by AVS authority)
    /// 20% of restaked XNT slashed — split between AVS + treasury + burned
    pub fn slash_operator(
        ctx: Context<SlashOperator>,
        operator_identity: Pubkey,
        avs_index: u32,
    ) -> Result<()> {
        let state = &mut ctx.accounts.state;
        let caller = ctx.accounts.avs_authority.key();

        // Verify caller is the AVS authority
        let avs_idx = avs_index as usize;
        require!(state.avs_registry[avs_idx].authority == caller, RestakingError::Unauthorized);

        // Find operator
        for i in 0..state.operator_count as usize {
            if state.operators[i].identity == operator_identity && state.operators[i].avs_index == avs_index {
                require!(!state.operators[i].slashed, RestakingError::AlreadySlashed);

                let slash_amount = state.operators[i].restaked_amount * SLASH_BPS / BASIS_POINTS;
                let treasury_cut = slash_amount * TREASURY_BPS / BASIS_POINTS;
                let burn_cut = slash_amount - treasury_cut;

                state.operators[i].restaked_amount -= slash_amount;
                state.operators[i].slashed = true;
                state.avs_registry[avs_idx].total_secured -= slash_amount;
                state.total_restaked -= slash_amount;
                state.total_burned += burn_cut;

                emit!(OperatorSlashed {
                    identity: operator_identity,
                    avs_index,
                    slash_amount,
                    treasury_cut,
                    burned: burn_cut,
                    epoch: Clock::get()?.epoch,
                });

                return Ok(());
            }
        }
        Err(RestakingError::OperatorNotFound.into())
    }

    /// Begin unbonding (14 epoch cooldown)
    pub fn begin_unbond(ctx: Context<BeginUnbond>, avs_index: u32) -> Result<()> {
        let state = &mut ctx.accounts.state;
        let identity = ctx.accounts.validator_identity.key();
        for i in 0..state.operator_count as usize {
            if state.operators[i].identity == identity && state.operators[i].avs_index == avs_index {
                require!(!state.operators[i].unbonding, RestakingError::AlreadyUnbonding);
                state.operators[i].unbonding = true;
                state.operators[i].unbond_epoch = Clock::get()?.epoch + UNBOND_EPOCHS;
                emit!(UnbondStarted { identity, avs_index, release_epoch: state.operators[i].unbond_epoch });
                return Ok(());
            }
        }
        Err(RestakingError::OperatorNotFound.into())
    }
}

// ── ACCOUNTS ──────────────────────────────────────────────────────────────────

#[derive(Accounts)]
pub struct Initialize<'info> {
    #[account(init, payer = authority, space = 8 + RestakingState::LEN, seeds = [b"restaking"], bump)]
    pub state: Account<'info, RestakingState>,
    #[account(mut)]
    pub authority: Signer<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct RegisterAvs<'info> {
    #[account(mut, seeds = [b"restaking"], bump = state.bump)]
    pub state: Account<'info, RestakingState>,
    #[account(mut)]
    pub avs_authority: Signer<'info>,
    /// CHECK: treasury
    #[account(mut, constraint = treasury.key().to_string() == TREASURY @ RestakingError::InvalidTreasury)]
    pub treasury: AccountInfo<'info>,
    /// CHECK: burn
    #[account(mut, constraint = burn_address.key().to_string() == BURN_ADDRESS @ RestakingError::InvalidBurnAddress)]
    pub burn_address: AccountInfo<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct OptInAvs<'info> {
    #[account(mut, seeds = [b"restaking"], bump = state.bump)]
    pub state: Account<'info, RestakingState>,
    #[account(mut)]
    pub validator_identity: Signer<'info>,
    /// CHECK: restake vault
    #[account(mut, seeds = [b"restake-vault"], bump)]
    pub restake_vault: AccountInfo<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct DistributeRewards<'info> {
    #[account(mut, seeds = [b"restaking"], bump = state.bump)]
    pub state: Account<'info, RestakingState>,
    /// CHECK: AVS reward pool
    #[account(mut)]
    pub avs_reward_pool: AccountInfo<'info>,
    /// CHECK: treasury
    #[account(mut, constraint = treasury.key().to_string() == TREASURY @ RestakingError::InvalidTreasury)]
    pub treasury: AccountInfo<'info>,
    /// CHECK: burn
    #[account(mut, constraint = burn_address.key().to_string() == BURN_ADDRESS @ RestakingError::InvalidBurnAddress)]
    pub burn_address: AccountInfo<'info>,
    pub caller: Signer<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct SlashOperator<'info> {
    #[account(mut, seeds = [b"restaking"], bump = state.bump)]
    pub state: Account<'info, RestakingState>,
    pub avs_authority: Signer<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct BeginUnbond<'info> {
    #[account(mut, seeds = [b"restaking"], bump = state.bump)]
    pub state: Account<'info, RestakingState>,
    pub validator_identity: Signer<'info>,
}

// ── STATE ─────────────────────────────────────────────────────────────────────

#[account]
pub struct RestakingState {
    pub authority: Pubkey,
    pub avs_count: u32,
    pub operator_count: u32,
    pub total_restaked: u64,
    pub total_fees_collected: u64,
    pub total_burned: u64,
    pub bump: u8,
    pub avs_registry: [AvsEntry; 50],
    pub operators: [OperatorEntry; 200],
}

impl RestakingState {
    pub const LEN: usize = 32 + 4 + 4 + 8 + 8 + 8 + 1
        + (AvsEntry::LEN * 50)
        + (OperatorEntry::LEN * 200);
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy)]
pub struct AvsEntry {
    pub authority: Pubkey,
    pub name: [u8; 32],
    pub min_operator_stake: u64,
    pub reward_rate_bps: u64,
    pub total_secured: u64,
    pub operator_count: u32,
    pub active: bool,
    pub registered_epoch: u64,
}
impl AvsEntry { pub const LEN: usize = 32 + 32 + 8 + 8 + 8 + 4 + 1 + 8; }

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy)]
pub struct OperatorEntry {
    pub identity: Pubkey,
    pub avs_index: u32,
    pub restaked_amount: u64,
    pub rewards_earned: u64,
    pub slashed: bool,
    pub unbonding: bool,
    pub unbond_epoch: u64,
    pub opted_in_epoch: u64,
}
impl OperatorEntry { pub const LEN: usize = 32 + 4 + 8 + 8 + 1 + 1 + 8 + 8; }

// ── EVENTS ────────────────────────────────────────────────────────────────────

#[event]
pub struct AvsRegistered { pub authority: Pubkey, pub min_stake: u64, pub reward_rate_bps: u64, pub epoch: u64 }
#[event]
pub struct OperatorOptedIn { pub identity: Pubkey, pub avs_index: u32, pub restaked: u64, pub epoch: u64 }
#[event]
pub struct RewardsDistributed { pub avs_index: u32, pub total_pool: u64, pub platform_fee: u64, pub distributed: u64, pub burned: u64, pub epoch: u64 }
#[event]
pub struct OperatorSlashed { pub identity: Pubkey, pub avs_index: u32, pub slash_amount: u64, pub treasury_cut: u64, pub burned: u64, pub epoch: u64 }
#[event]
pub struct UnbondStarted { pub identity: Pubkey, pub avs_index: u32, pub release_epoch: u64 }

// ── ERRORS ────────────────────────────────────────────────────────────────────

#[error_code]
pub enum RestakingError {
    #[msg("Maximum AVS limit reached (50)")]
    MaxAvsReached,
    #[msg("Maximum operators reached (200)")]
    MaxOperatorsReached,
    #[msg("Stake amount below minimum (100 XNT)")]
    StakeTooSmall,
    #[msg("Invalid reward rate (1-3000 bps)")]
    InvalidRewardRate,
    #[msg("Invalid fee amount")]
    InvalidFee,
    #[msg("AVS not found")]
    AvsNotFound,
    #[msg("AVS is inactive")]
    AvsInactive,
    #[msg("Already opted into this AVS")]
    AlreadyOptedIn,
    #[msg("Operator not found")]
    OperatorNotFound,
    #[msg("Operator already slashed")]
    AlreadySlashed,
    #[msg("Already in unbonding period")]
    AlreadyUnbonding,
    #[msg("Unauthorized — only AVS authority can slash")]
    Unauthorized,
    #[msg("Invalid treasury address")]
    InvalidTreasury,
    #[msg("Invalid burn address")]
    InvalidBurnAddress,
}
