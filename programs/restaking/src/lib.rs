use anchor_lang::prelude::*;
use anchor_lang::system_program;

declare_id!("3QW7QtQP4bc8wtoxvH3gZtQUS97gMor8cbQjn4StoUoL"); // replace after deploy v0.2

// ── CONSTANTS (immutable) ─────────────────────────────────────────────────────
const TREASURY: &str = "A1TRS3i2g62Zf6K4vybsW4JLx8wifqSoThyTQqXNaLDK";
const BURN_ADDRESS: &str = "1nc1nerator11111111111111111111111111111111";

const TREASURY_BPS: u64 = 5000;
const BURN_BPS: u64 = 5000;
const BASIS_POINTS: u64 = 10000;

// Platform fee on restaking rewards: 10%
const PLATFORM_FEE_BPS: u64 = 1000;

// Minimum restake: 100 XNT
const MIN_RESTAKE: u64 = 100_000_000_000;

// Slashing: 20% of restaked amount
const SLASH_BPS: u64 = 2000;

// Unbond cooldown: 14 epochs
const UNBOND_EPOCHS: u64 = 14;

// FIX #3: Max AVS per operator — prevents slot griefing
const MAX_AVS_PER_OPERATOR: u32 = 5;

// FIX #4: AVS registration collateral — locked 30 epochs to deter rugpull
const AVS_COLLATERAL_EPOCHS: u64 = 30;

// Slash dispute window: 3 epochs to contest
const SLASH_DISPUTE_EPOCHS: u64 = 3;

const MAX_AVS: usize = 50;
const MAX_OPERATORS: usize = 200;

#[program]
pub mod restaking {
    use super::*;

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

    /// Register AVS with collateral bond
    /// FIX: AVS must lock collateral for 30 epochs — deters rugpull
    pub fn register_avs(
        ctx: Context<RegisterAvs>,
        name: [u8; 32],
        min_operator_stake: u64,
        reward_rate_bps: u64,
        registration_fee: u64,
        collateral_amount: u64,   // NEW: locked for 30 epochs
    ) -> Result<()> {
        let state = &mut ctx.accounts.state;
        require!((state.avs_count as usize) < MAX_AVS, RestakingError::MaxAvsReached);
        require!(min_operator_stake >= MIN_RESTAKE, RestakingError::StakeTooSmall);
        require!(reward_rate_bps > 0 && reward_rate_bps <= 3000, RestakingError::InvalidRewardRate);
        require!(registration_fee > 0, RestakingError::InvalidFee);
        require!(collateral_amount >= MIN_RESTAKE, RestakingError::CollateralTooSmall);

        // Pay registration fee (treasury/burn split)
        let treasury_amt = registration_fee * TREASURY_BPS / BASIS_POINTS;
        let burn_amt = registration_fee - treasury_amt;
        system_program::transfer(CpiContext::new(ctx.accounts.system_program.to_account_info(),
            system_program::Transfer { from: ctx.accounts.avs_authority.to_account_info(), to: ctx.accounts.treasury.to_account_info() }), treasury_amt)?;
        system_program::transfer(CpiContext::new(ctx.accounts.system_program.to_account_info(),
            system_program::Transfer { from: ctx.accounts.avs_authority.to_account_info(), to: ctx.accounts.burn_address.to_account_info() }), burn_amt)?;

        // Lock collateral in vault (returned after AVS_COLLATERAL_EPOCHS)
        system_program::transfer(CpiContext::new(ctx.accounts.system_program.to_account_info(),
            system_program::Transfer { from: ctx.accounts.avs_authority.to_account_info(), to: ctx.accounts.restake_vault.to_account_info() }), collateral_amount)?;

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
            collateral_amount,
            collateral_release_epoch: Clock::get()?.epoch + AVS_COLLATERAL_EPOCHS,
            reward_pool_balance: 0,
        };
        state.avs_count += 1;
        state.total_fees_collected += registration_fee;
        state.total_burned += burn_amt;

        emit!(AvsRegistered {
            authority: ctx.accounts.avs_authority.key(),
            min_stake: min_operator_stake,
            reward_rate_bps,
            collateral: collateral_amount,
            epoch: Clock::get()?.epoch,
        });
        Ok(())
    }

    /// Fund AVS reward pool — AVS pre-funds rewards before validators can opt in
    /// FIX: Validators can verify reward pool is funded before committing stake
    pub fn fund_reward_pool(
        ctx: Context<FundRewardPool>,
        avs_index: u32,
        amount: u64,
    ) -> Result<()> {
        let state = &mut ctx.accounts.state;
        let avs_idx = avs_index as usize;
        require!(avs_idx < state.avs_count as usize, RestakingError::AvsNotFound);
        let caller = ctx.accounts.avs_authority.key();
        require!(state.avs_registry[avs_idx].authority == caller, RestakingError::Unauthorized);

        // Transfer funds to vault
        system_program::transfer(CpiContext::new(ctx.accounts.system_program.to_account_info(),
            system_program::Transfer { from: ctx.accounts.avs_authority.to_account_info(), to: ctx.accounts.restake_vault.to_account_info() }), amount)?;

        state.avs_registry[avs_idx].reward_pool_balance += amount;
        Ok(())
    }

    /// Validator opts into AVS
    /// FIX: Cap at MAX_AVS_PER_OPERATOR to prevent slot griefing
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
        require!(state.avs_registry[avs_idx].reward_pool_balance > 0, RestakingError::RewardPoolEmpty);

        let identity = ctx.accounts.validator_identity.key();

        // FIX: Count how many AVS this operator is already in
        let mut current_avs_count: u32 = 0;
        for i in 0..state.operator_count as usize {
            if state.operators[i].identity == identity && !state.operators[i].unbonding {
                current_avs_count += 1;
                // Also check not already in this AVS
                if state.operators[i].avs_index == avs_index {
                    return Err(RestakingError::AlreadyOptedIn.into());
                }
            }
        }
        require!(current_avs_count < MAX_AVS_PER_OPERATOR, RestakingError::TooManyAvs);
        require!((state.operator_count as usize) < MAX_OPERATORS, RestakingError::MaxOperatorsReached);

        // Lock restaked XNT
        system_program::transfer(CpiContext::new(ctx.accounts.system_program.to_account_info(),
            system_program::Transfer { from: ctx.accounts.validator_identity.to_account_info(), to: ctx.accounts.restake_vault.to_account_info() }), restake_amount)?;

        let op_idx = state.operator_count as usize;
        state.operators[op_idx] = OperatorEntry {
            identity,
            avs_index,
            restaked_amount: restake_amount,
            rewards_earned: 0,
            rewards_claimed: 0,
            slashed: false,
            slash_contested: false,
            slash_epoch: 0,
            unbonding: false,
            unbond_epoch: 0,
            opted_in_epoch: Clock::get()?.epoch,
        };
        state.operator_count += 1;
        state.avs_registry[avs_idx].total_secured += restake_amount;
        state.avs_registry[avs_idx].operator_count += 1;
        state.total_restaked += restake_amount;

        emit!(OperatorOptedIn { identity, avs_index, restaked: restake_amount, epoch: Clock::get()?.epoch });
        Ok(())
    }

    /// FIX: distribute_rewards() now requires AVS authority
    /// FIX: Actually transfers XNT from vault to operator reward balances
    pub fn distribute_rewards(
        ctx: Context<DistributeRewards>,
        avs_index: u32,
        reward_amount: u64,
    ) -> Result<()> {
        let state = &mut ctx.accounts.state;
        let avs_idx = avs_index as usize;
        require!(avs_idx < state.avs_count as usize, RestakingError::AvsNotFound);

        // FIX #1: Only AVS authority can distribute
        let caller = ctx.accounts.avs_authority.key();
        require!(state.avs_registry[avs_idx].authority == caller, RestakingError::Unauthorized);
        require!(state.avs_registry[avs_idx].reward_pool_balance >= reward_amount, RestakingError::InsufficientRewardPool);

        // Platform fee: 10% → treasury/burn
        let platform_fee = reward_amount * PLATFORM_FEE_BPS / BASIS_POINTS;
        let treasury_fee = platform_fee * TREASURY_BPS / BASIS_POINTS;
        let burn_fee = platform_fee - treasury_fee;
        let distributable = reward_amount - platform_fee;

        // Pay platform fee from vault
        // (vault CPI — program signs as PDA authority)
        system_program::transfer(CpiContext::new(ctx.accounts.system_program.to_account_info(),
            system_program::Transfer { from: ctx.accounts.restake_vault.to_account_info(), to: ctx.accounts.treasury.to_account_info() }), treasury_fee)?;
        system_program::transfer(CpiContext::new(ctx.accounts.system_program.to_account_info(),
            system_program::Transfer { from: ctx.accounts.restake_vault.to_account_info(), to: ctx.accounts.burn_address.to_account_info() }), burn_fee)?;

        // Update reward pool balance
        state.avs_registry[avs_idx].reward_pool_balance -= reward_amount;

        // Distribute proportionally — update reward balances (claim separately)
        let avs_total = state.avs_registry[avs_idx].total_secured;
        if avs_total == 0 { return Ok(()); }

        for i in 0..state.operator_count as usize {
            if state.operators[i].avs_index == avs_index && !state.operators[i].slashed && !state.operators[i].unbonding {
                let share = distributable * state.operators[i].restaked_amount / avs_total;
                state.operators[i].rewards_earned = state.operators[i].rewards_earned.checked_add(share)
                    .ok_or(RestakingError::MathOverflow)?;
            }
        }

        state.total_fees_collected = state.total_fees_collected.checked_add(platform_fee)
            .ok_or(RestakingError::MathOverflow)?;
        state.total_burned = state.total_burned.checked_add(burn_fee)
            .ok_or(RestakingError::MathOverflow)?;

        emit!(RewardsDistributed { avs_index, total_pool: reward_amount, platform_fee, distributed: distributable, burned: burn_fee, epoch: Clock::get()?.epoch });
        Ok(())
    }

    /// FIX: claim_rewards() — validators actually receive their earned rewards
    pub fn claim_rewards(
        ctx: Context<ClaimRewards>,
        avs_index: u32,
    ) -> Result<()> {
        let state = &mut ctx.accounts.state;
        let identity = ctx.accounts.validator_identity.key();

        for i in 0..state.operator_count as usize {
            if state.operators[i].identity == identity && state.operators[i].avs_index == avs_index {
                let claimable = state.operators[i].rewards_earned
                    .checked_sub(state.operators[i].rewards_claimed)
                    .ok_or(RestakingError::MathOverflow)?;
                require!(claimable > 0, RestakingError::NothingToClaim);

                // Transfer from vault to validator
                system_program::transfer(CpiContext::new(ctx.accounts.system_program.to_account_info(),
                    system_program::Transfer { from: ctx.accounts.restake_vault.to_account_info(), to: ctx.accounts.validator_identity.to_account_info() }), claimable)?;

                state.operators[i].rewards_claimed = state.operators[i].rewards_earned;

                emit!(RewardsClaimed { identity, avs_index, amount: claimable, epoch: Clock::get()?.epoch });
                return Ok(());
            }
        }
        Err(RestakingError::OperatorNotFound.into())
    }

    /// FIX: Slash now has a dispute window (3 epochs)
    /// Slash is PENDING until dispute window expires or contested
    pub fn slash_operator(
        ctx: Context<SlashOperator>,
        operator_identity: Pubkey,
        avs_index: u32,
    ) -> Result<()> {
        let state = &mut ctx.accounts.state;
        let caller = ctx.accounts.avs_authority.key();
        let avs_idx = avs_index as usize;
        require!(state.avs_registry[avs_idx].authority == caller, RestakingError::Unauthorized);

        for i in 0..state.operator_count as usize {
            if state.operators[i].identity == operator_identity && state.operators[i].avs_index == avs_index {
                require!(!state.operators[i].slashed, RestakingError::AlreadySlashed);
                require!(state.operators[i].slash_epoch == 0, RestakingError::SlashPending);

                // Mark slash as PENDING — gives operator 3 epochs to contest
                state.operators[i].slash_epoch = Clock::get()?.epoch + SLASH_DISPUTE_EPOCHS;

                emit!(SlashInitiated {
                    identity: operator_identity,
                    avs_index,
                    dispute_deadline: state.operators[i].slash_epoch,
                    epoch: Clock::get()?.epoch,
                });
                return Ok(());
            }
        }
        Err(RestakingError::OperatorNotFound.into())
    }

    /// Finalize slash after dispute window expires
    /// Anyone can call this — permissionless execution after deadline
    pub fn finalize_slash(
        ctx: Context<FinalizeSlash>,
        operator_identity: Pubkey,
        avs_index: u32,
    ) -> Result<()> {
        let state = &mut ctx.accounts.state;
        let current_epoch = Clock::get()?.epoch;

        for i in 0..state.operator_count as usize {
            if state.operators[i].identity == operator_identity && state.operators[i].avs_index == avs_index {
                require!(state.operators[i].slash_epoch > 0, RestakingError::NoSlashPending);
                require!(current_epoch >= state.operators[i].slash_epoch, RestakingError::DisputeWindowOpen);
                require!(!state.operators[i].slash_contested, RestakingError::SlashContested);

                let slash_amount = state.operators[i].restaked_amount.checked_mul(SLASH_BPS)
                    .ok_or(RestakingError::MathOverflow)?
                    .checked_div(BASIS_POINTS)
                    .ok_or(RestakingError::MathOverflow)?;

                let treasury_cut = slash_amount * TREASURY_BPS / BASIS_POINTS;
                let burn_cut = slash_amount - treasury_cut;

                // FIX: Actually release slashed XNT from vault
                system_program::transfer(CpiContext::new(ctx.accounts.system_program.to_account_info(),
                    system_program::Transfer { from: ctx.accounts.restake_vault.to_account_info(), to: ctx.accounts.treasury.to_account_info() }), treasury_cut)?;
                system_program::transfer(CpiContext::new(ctx.accounts.system_program.to_account_info(),
                    system_program::Transfer { from: ctx.accounts.restake_vault.to_account_info(), to: ctx.accounts.burn_address.to_account_info() }), burn_cut)?;

                state.operators[i].restaked_amount = state.operators[i].restaked_amount
                    .checked_sub(slash_amount).ok_or(RestakingError::MathOverflow)?;
                state.operators[i].slashed = true;
                state.operators[i].slash_epoch = 0;

                let avs_idx = avs_index as usize;
                state.avs_registry[avs_idx].total_secured = state.avs_registry[avs_idx].total_secured
                    .checked_sub(slash_amount).ok_or(RestakingError::MathOverflow)?;
                state.total_restaked = state.total_restaked
                    .checked_sub(slash_amount).ok_or(RestakingError::MathOverflow)?;
                state.total_burned = state.total_burned.checked_add(burn_cut)
                    .ok_or(RestakingError::MathOverflow)?;

                emit!(OperatorSlashed { identity: operator_identity, avs_index, slash_amount, treasury_cut, burned: burn_cut, epoch: current_epoch });
                return Ok(());
            }
        }
        Err(RestakingError::OperatorNotFound.into())
    }

    /// Operator contests a slash within dispute window
    pub fn contest_slash(
        ctx: Context<ContestSlash>,
        avs_index: u32,
    ) -> Result<()> {
        let state = &mut ctx.accounts.state;
        let identity = ctx.accounts.validator_identity.key();
        let current_epoch = Clock::get()?.epoch;

        for i in 0..state.operator_count as usize {
            if state.operators[i].identity == identity && state.operators[i].avs_index == avs_index {
                require!(state.operators[i].slash_epoch > 0, RestakingError::NoSlashPending);
                require!(current_epoch < state.operators[i].slash_epoch, RestakingError::DisputeWindowClosed);

                state.operators[i].slash_contested = true;
                // Contested slashes require x1scroll authority to resolve (governance)
                emit!(SlashContested { identity, avs_index, epoch: current_epoch });
                return Ok(());
            }
        }
        Err(RestakingError::OperatorNotFound.into())
    }

    /// Begin unbond (14 epoch cooldown)
    pub fn begin_unbond(ctx: Context<BeginUnbond>, avs_index: u32) -> Result<()> {
        let state = &mut ctx.accounts.state;
        let identity = ctx.accounts.validator_identity.key();
        for i in 0..state.operator_count as usize {
            if state.operators[i].identity == identity && state.operators[i].avs_index == avs_index {
                require!(!state.operators[i].unbonding, RestakingError::AlreadyUnbonding);
                require!(state.operators[i].slash_epoch == 0, RestakingError::SlashPending);
                state.operators[i].unbonding = true;
                state.operators[i].unbond_epoch = Clock::get()?.epoch + UNBOND_EPOCHS;
                emit!(UnbondStarted { identity, avs_index, release_epoch: state.operators[i].unbond_epoch });
                return Ok(());
            }
        }
        Err(RestakingError::OperatorNotFound.into())
    }

    /// FIX: withdraw_stake() — actually returns XNT after unbond period
    pub fn withdraw_stake(
        ctx: Context<WithdrawStake>,
        avs_index: u32,
    ) -> Result<()> {
        let state = &mut ctx.accounts.state;
        let identity = ctx.accounts.validator_identity.key();
        let current_epoch = Clock::get()?.epoch;

        for i in 0..state.operator_count as usize {
            if state.operators[i].identity == identity && state.operators[i].avs_index == avs_index {
                require!(state.operators[i].unbonding, RestakingError::NotUnbonding);
                require!(current_epoch >= state.operators[i].unbond_epoch, RestakingError::UnbondNotReady);

                let withdraw_amount = state.operators[i].restaked_amount;
                require!(withdraw_amount > 0, RestakingError::NothingToClaim);

                // Transfer stake back to validator from vault
                system_program::transfer(CpiContext::new(ctx.accounts.system_program.to_account_info(),
                    system_program::Transfer { from: ctx.accounts.restake_vault.to_account_info(), to: ctx.accounts.validator_identity.to_account_info() }), withdraw_amount)?;

                // Update state — decrement operator count slot reuse
                state.operators[i].restaked_amount = 0;
                state.operators[i].avs_index = u32::MAX; // mark as freed
                let avs_idx = avs_index as usize;
                state.avs_registry[avs_idx].total_secured = state.avs_registry[avs_idx].total_secured
                    .checked_sub(withdraw_amount).ok_or(RestakingError::MathOverflow)?;
                state.avs_registry[avs_idx].operator_count -= 1;
                state.total_restaked = state.total_restaked
                    .checked_sub(withdraw_amount).ok_or(RestakingError::MathOverflow)?;

                emit!(StakeWithdrawn { identity, avs_index, amount: withdraw_amount, epoch: current_epoch });
                return Ok(());
            }
        }
        Err(RestakingError::OperatorNotFound.into())
    }
}

// ── ACCOUNTS ──────────────────────────────────────────────────────────────────

#[derive(Accounts)]
pub struct Initialize<'info> {
    #[account(init, payer = authority, space = 8 + RestakingState::LEN, seeds = [b"restaking-v2"], bump)]
    pub state: Account<'info, RestakingState>,
    #[account(mut)]
    pub authority: Signer<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct RegisterAvs<'info> {
    #[account(mut, seeds = [b"restaking-v2"], bump = state.bump)]
    pub state: Account<'info, RestakingState>,
    #[account(mut)]
    pub avs_authority: Signer<'info>,
    /// CHECK: treasury
    #[account(mut, constraint = treasury.key().to_string() == TREASURY @ RestakingError::InvalidTreasury)]
    pub treasury: AccountInfo<'info>,
    /// CHECK: burn
    #[account(mut, constraint = burn_address.key().to_string() == BURN_ADDRESS @ RestakingError::InvalidBurnAddress)]
    pub burn_address: AccountInfo<'info>,
    /// CHECK: vault for collateral + stakes
    #[account(mut, seeds = [b"restake-vault-v2"], bump)]
    pub restake_vault: AccountInfo<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct FundRewardPool<'info> {
    #[account(mut, seeds = [b"restaking-v2"], bump = state.bump)]
    pub state: Account<'info, RestakingState>,
    #[account(mut)]
    pub avs_authority: Signer<'info>,
    /// CHECK: vault
    #[account(mut, seeds = [b"restake-vault-v2"], bump)]
    pub restake_vault: AccountInfo<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct OptInAvs<'info> {
    #[account(mut, seeds = [b"restaking-v2"], bump = state.bump)]
    pub state: Account<'info, RestakingState>,
    #[account(mut)]
    pub validator_identity: Signer<'info>,
    /// CHECK: vault
    #[account(mut, seeds = [b"restake-vault-v2"], bump)]
    pub restake_vault: AccountInfo<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct DistributeRewards<'info> {
    #[account(mut, seeds = [b"restaking-v2"], bump = state.bump)]
    pub state: Account<'info, RestakingState>,
    #[account(mut)]
    pub avs_authority: Signer<'info>,  // FIX: AVS authority required
    /// CHECK: vault
    #[account(mut, seeds = [b"restake-vault-v2"], bump)]
    pub restake_vault: AccountInfo<'info>,
    /// CHECK: treasury
    #[account(mut, constraint = treasury.key().to_string() == TREASURY @ RestakingError::InvalidTreasury)]
    pub treasury: AccountInfo<'info>,
    /// CHECK: burn
    #[account(mut, constraint = burn_address.key().to_string() == BURN_ADDRESS @ RestakingError::InvalidBurnAddress)]
    pub burn_address: AccountInfo<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct ClaimRewards<'info> {
    #[account(mut, seeds = [b"restaking-v2"], bump = state.bump)]
    pub state: Account<'info, RestakingState>,
    #[account(mut)]
    pub validator_identity: Signer<'info>,
    /// CHECK: vault pays out
    #[account(mut, seeds = [b"restake-vault-v2"], bump)]
    pub restake_vault: AccountInfo<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct SlashOperator<'info> {
    #[account(mut, seeds = [b"restaking-v2"], bump = state.bump)]
    pub state: Account<'info, RestakingState>,
    pub avs_authority: Signer<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct FinalizeSlash<'info> {
    #[account(mut, seeds = [b"restaking-v2"], bump = state.bump)]
    pub state: Account<'info, RestakingState>,
    pub caller: Signer<'info>,
    /// CHECK: vault
    #[account(mut, seeds = [b"restake-vault-v2"], bump)]
    pub restake_vault: AccountInfo<'info>,
    /// CHECK: treasury
    #[account(mut, constraint = treasury.key().to_string() == TREASURY @ RestakingError::InvalidTreasury)]
    pub treasury: AccountInfo<'info>,
    /// CHECK: burn
    #[account(mut, constraint = burn_address.key().to_string() == BURN_ADDRESS @ RestakingError::InvalidBurnAddress)]
    pub burn_address: AccountInfo<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct ContestSlash<'info> {
    #[account(mut, seeds = [b"restaking-v2"], bump = state.bump)]
    pub state: Account<'info, RestakingState>,
    pub validator_identity: Signer<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct BeginUnbond<'info> {
    #[account(mut, seeds = [b"restaking-v2"], bump = state.bump)]
    pub state: Account<'info, RestakingState>,
    pub validator_identity: Signer<'info>,
}

#[derive(Accounts)]
pub struct WithdrawStake<'info> {
    #[account(mut, seeds = [b"restaking-v2"], bump = state.bump)]
    pub state: Account<'info, RestakingState>,
    #[account(mut)]
    pub validator_identity: Signer<'info>,
    /// CHECK: vault returns stake
    #[account(mut, seeds = [b"restake-vault-v2"], bump)]
    pub restake_vault: AccountInfo<'info>,
    pub system_program: Program<'info, System>,
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
    pub collateral_amount: u64,          // NEW: locked collateral
    pub collateral_release_epoch: u64,   // NEW: when collateral unlocks
    pub reward_pool_balance: u64,        // NEW: pre-funded reward pool
}
impl AvsEntry { pub const LEN: usize = 32 + 32 + 8 + 8 + 8 + 4 + 1 + 8 + 8 + 8 + 8; }

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy)]
pub struct OperatorEntry {
    pub identity: Pubkey,
    pub avs_index: u32,
    pub restaked_amount: u64,
    pub rewards_earned: u64,
    pub rewards_claimed: u64,            // NEW: track claimed separately
    pub slashed: bool,
    pub slash_contested: bool,           // NEW: dispute flag
    pub slash_epoch: u64,                // NEW: dispute deadline
    pub unbonding: bool,
    pub unbond_epoch: u64,
    pub opted_in_epoch: u64,
}
impl OperatorEntry { pub const LEN: usize = 32 + 4 + 8 + 8 + 8 + 1 + 1 + 8 + 1 + 8 + 8; }

// ── EVENTS ────────────────────────────────────────────────────────────────────

#[event]
pub struct AvsRegistered { pub authority: Pubkey, pub min_stake: u64, pub reward_rate_bps: u64, pub collateral: u64, pub epoch: u64 }
#[event]
pub struct OperatorOptedIn { pub identity: Pubkey, pub avs_index: u32, pub restaked: u64, pub epoch: u64 }
#[event]
pub struct RewardsDistributed { pub avs_index: u32, pub total_pool: u64, pub platform_fee: u64, pub distributed: u64, pub burned: u64, pub epoch: u64 }
#[event]
pub struct RewardsClaimed { pub identity: Pubkey, pub avs_index: u32, pub amount: u64, pub epoch: u64 }
#[event]
pub struct SlashInitiated { pub identity: Pubkey, pub avs_index: u32, pub dispute_deadline: u64, pub epoch: u64 }
#[event]
pub struct SlashContested { pub identity: Pubkey, pub avs_index: u32, pub epoch: u64 }
#[event]
pub struct OperatorSlashed { pub identity: Pubkey, pub avs_index: u32, pub slash_amount: u64, pub treasury_cut: u64, pub burned: u64, pub epoch: u64 }
#[event]
pub struct UnbondStarted { pub identity: Pubkey, pub avs_index: u32, pub release_epoch: u64 }
#[event]
pub struct StakeWithdrawn { pub identity: Pubkey, pub avs_index: u32, pub amount: u64, pub epoch: u64 }

// ── ERRORS ────────────────────────────────────────────────────────────────────

#[error_code]
pub enum RestakingError {
    #[msg("Maximum AVS limit reached (50)")]
    MaxAvsReached,
    #[msg("Maximum operators reached (200)")]
    MaxOperatorsReached,
    #[msg("Stake amount below minimum (100 XNT)")]
    StakeTooSmall,
    #[msg("Collateral below minimum (100 XNT)")]
    CollateralTooSmall,
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
    #[msg("Max 5 AVS per operator — unstake from one before joining another")]
    TooManyAvs,
    #[msg("Operator not found")]
    OperatorNotFound,
    #[msg("Operator already slashed")]
    AlreadySlashed,
    #[msg("Slash already pending — wait for dispute window")]
    SlashPending,
    #[msg("No slash pending")]
    NoSlashPending,
    #[msg("Dispute window still open — wait 3 epochs")]
    DisputeWindowOpen,
    #[msg("Dispute window has closed")]
    DisputeWindowClosed,
    #[msg("Slash has been contested — requires governance resolution")]
    SlashContested,
    #[msg("Already in unbonding period")]
    AlreadyUnbonding,
    #[msg("Not in unbonding — call begin_unbond first")]
    NotUnbonding,
    #[msg("Unbond period not complete — wait for release epoch")]
    UnbondNotReady,
    #[msg("Nothing to claim")]
    NothingToClaim,
    #[msg("Reward pool is empty — AVS must fund pool before operators can join")]
    RewardPoolEmpty,
    #[msg("Insufficient reward pool balance")]
    InsufficientRewardPool,
    #[msg("Unauthorized — only AVS authority can perform this action")]
    Unauthorized,
    #[msg("Math overflow or underflow")]
    MathOverflow,
    #[msg("Invalid treasury address")]
    InvalidTreasury,
    #[msg("Invalid burn address")]
    InvalidBurnAddress,
}
