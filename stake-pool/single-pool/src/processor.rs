//! program state processor

use {
    crate::{
        error::SinglePoolError, instruction::SinglePoolInstruction, MINT_DECIMALS,
        POOL_AUTHORITY_PREFIX, POOL_MINT_PREFIX, POOL_STAKE_PREFIX, VOTE_STATE_END,
        VOTE_STATE_START,
    },
    borsh::BorshDeserialize,
    mpl_token_metadata::{
        instruction::{create_metadata_accounts_v3, update_metadata_accounts_v2},
        pda::find_metadata_account,
        state::DataV2,
    },
    solomka_program::{
        account_info::{next_account_info, AccountInfo},
        borsh::try_from_slice_unchecked,
        entrypoint::ProgramResult,
        msg,
        native_token::LAMPORTS_PER_SOL,
        program::invoke_signed,
        program_error::ProgramError,
        program_pack::Pack,
        pubkey::Pubkey,
        rent::Rent,
        stake::{
            self,
            state::{Meta, Stake, StakeState},
        },
        stake_history::Epoch,
        system_instruction, system_program,
        sysvar::{clock::Clock, Sysvar},
        vote::program as vote_program,
    },
    spl_token::state::Mint,
};

/// Calculate pool tokens to mint, given outstanding token supply, pool active stake, and deposit active stake
fn calculate_deposit_amount(
    pre_token_supply: u64,
    pre_pool_stake: u64,
    user_stake_to_deposit: u64,
) -> Option<u64> {
    if pre_pool_stake == 0 || pre_token_supply == 0 {
        Some(user_stake_to_deposit)
    } else {
        u64::try_from(
            (user_stake_to_deposit as u128)
                .checked_mul(pre_token_supply as u128)?
                .checked_div(pre_pool_stake as u128)?,
        )
        .ok()
    }
}

/// Calculate pool stake to return, given outstanding token supply, pool active stake, and tokens to redeem
fn calculate_withdraw_amount(
    pre_token_supply: u64,
    pre_pool_stake: u64,
    user_tokens_to_burn: u64,
) -> Option<u64> {
    let numerator = (user_tokens_to_burn as u128).checked_mul(pre_pool_stake as u128)?;
    let denominator = pre_token_supply as u128;
    if numerator < denominator || denominator == 0 {
        Some(0)
    } else {
        u64::try_from(numerator.checked_div(denominator)?).ok()
    }
}

/// Deserialize the stake state from AccountInfo
fn get_stake_state(stake_account_info: &AccountInfo) -> Result<(Meta, Stake), ProgramError> {
    let stake_state = try_from_slice_unchecked::<StakeState>(&stake_account_info.data.borrow())?;

    match stake_state {
        StakeState::Stake(meta, stake) => Ok((meta, stake)),
        _ => Err(SinglePoolError::WrongStakeState.into()),
    }
}

/// Deserialize the stake amount from AccountInfo
fn get_stake_amount(stake_account_info: &AccountInfo) -> Result<u64, ProgramError> {
    Ok(get_stake_state(stake_account_info)?.1.delegation.stake)
}

/// Determine if stake is active
fn is_stake_active_without_history(stake: &Stake, current_epoch: Epoch) -> bool {
    stake.delegation.activation_epoch < current_epoch
        && stake.delegation.deactivation_epoch == Epoch::MAX
}

/// Check pool stake account address for the validator vote account
fn check_pool_stake_address(
    program_id: &Pubkey,
    vote_account_address: &Pubkey,
    address: &Pubkey,
) -> Result<u8, ProgramError> {
    let (pool_stake_address, bump_seed) =
        crate::find_pool_stake_address_and_bump(program_id, vote_account_address);
    if *address != pool_stake_address {
        msg!(
            "Incorrect pool stake address for vote {}, expected {}, received {}",
            vote_account_address,
            pool_stake_address,
            address
        );
        Err(SinglePoolError::InvalidPoolStakeAccount.into())
    } else {
        Ok(bump_seed)
    }
}

/// Check pool authority address for the validator vote account
fn check_pool_authority_address(
    program_id: &Pubkey,
    vote_account_address: &Pubkey,
    address: &Pubkey,
) -> Result<u8, ProgramError> {
    let (pool_authority_address, bump_seed) =
        crate::find_pool_authority_address_and_bump(program_id, vote_account_address);
    if *address != pool_authority_address {
        msg!(
            "Incorrect pool authority address for vote {}, expected {}, received {}",
            vote_account_address,
            pool_authority_address,
            address
        );
        Err(SinglePoolError::InvalidPoolAuthority.into())
    } else {
        Ok(bump_seed)
    }
}

/// Check pool mint address for the validator vote account
fn check_pool_mint_address(
    program_id: &Pubkey,
    vote_account_address: &Pubkey,
    address: &Pubkey,
) -> Result<u8, ProgramError> {
    let (pool_mint_address, bump_seed) =
        crate::find_pool_mint_address_and_bump(program_id, vote_account_address);
    if *address != pool_mint_address {
        msg!(
            "Incorrect pool mint address for vote {}, expected {}, received {}",
            vote_account_address,
            pool_mint_address,
            address
        );
        Err(SinglePoolError::InvalidPoolMint.into())
    } else {
        Ok(bump_seed)
    }
}

/// Check vote account is owned by the vote program and not a legacy variant
fn check_vote_account(vote_account_info: &AccountInfo) -> Result<(), ProgramError> {
    check_account_owner(vote_account_info, &vote_program::id())?;

    let vote_account_data = &vote_account_info.try_borrow_data()?;
    let state_variant = vote_account_data
        .get(..VOTE_STATE_START)
        .and_then(|s| s.try_into().ok())
        .ok_or(SinglePoolError::UnparseableVoteAccount)?;

    match u32::from_le_bytes(state_variant) {
        1 => Ok(()),
        0 => Err(SinglePoolError::LegacyVoteAccount.into()),
        _ => Err(SinglePoolError::UnparseableVoteAccount.into()),
    }
}

/// Check mpl metadata account address for the pool mint
fn check_mpl_metadata_account_address(
    metadata_address: &Pubkey,
    pool_mint: &Pubkey,
) -> Result<(), ProgramError> {
    let (metadata_account_pubkey, _) = find_metadata_account(pool_mint);
    if metadata_account_pubkey != *metadata_address {
        Err(SinglePoolError::InvalidMetadataAccount.into())
    } else {
        Ok(())
    }
}

/// Check system program address
fn check_system_program(program_id: &Pubkey) -> Result<(), ProgramError> {
    if *program_id != system_program::id() {
        msg!(
            "Expected system program {}, received {}",
            system_program::id(),
            program_id
        );
        Err(ProgramError::IncorrectProgramId)
    } else {
        Ok(())
    }
}

/// Check token program address
fn check_token_program(address: &Pubkey) -> Result<(), ProgramError> {
    if *address != spl_token::id() {
        msg!(
            "Incorrect token program, expected {}, received {}",
            spl_token::id(),
            address
        );
        Err(ProgramError::IncorrectProgramId)
    } else {
        Ok(())
    }
}

/// Check stake program address
fn check_stake_program(program_id: &Pubkey) -> Result<(), ProgramError> {
    if *program_id != stake::program::id() {
        msg!(
            "Expected stake program {}, received {}",
            stake::program::id(),
            program_id
        );
        Err(ProgramError::IncorrectProgramId)
    } else {
        Ok(())
    }
}

/// Check mpl metadata program
fn check_mpl_metadata_program(program_id: &Pubkey) -> Result<(), ProgramError> {
    if *program_id != mpl_token_metadata::id() {
        msg!(
            "Expected mpl metadata program {}, received {}",
            mpl_token_metadata::id(),
            program_id
        );
        Err(ProgramError::IncorrectProgramId)
    } else {
        Ok(())
    }
}

/// Check account owner is the given program
fn check_account_owner(
    account_info: &AccountInfo,
    program_id: &Pubkey,
) -> Result<(), ProgramError> {
    if *program_id != *account_info.owner {
        msg!(
            "Expected account to be owned by program {}, received {}",
            program_id,
            account_info.owner
        );
        Err(ProgramError::IncorrectProgramId)
    } else {
        Ok(())
    }
}

/// Minimum delegation to create a pool
/// We floor at 1sol to avoid over-minting tokens before the relevant feature is active
fn minimum_delegation() -> Result<u64, ProgramError> {
    Ok(std::cmp::max(
        stake::tools::get_minimum_delegation()?,
        LAMPORTS_PER_SOL,
    ))
}

/// Program state handler.
pub struct Processor {}
impl Processor {
    #[allow(clippy::too_many_arguments)]
    fn stake_merge<'a>(
        vote_account_key: &Pubkey,
        source_account: AccountInfo<'a>,
        authority: AccountInfo<'a>,
        bump_seed: u8,
        destination_account: AccountInfo<'a>,
        clock: AccountInfo<'a>,
        stake_history: AccountInfo<'a>,
    ) -> Result<(), ProgramError> {
        let authority_seeds = &[
            POOL_AUTHORITY_PREFIX,
            vote_account_key.as_ref(),
            &[bump_seed],
        ];
        let signers = &[&authority_seeds[..]];

        invoke_signed(
            &stake::instruction::merge(destination_account.key, source_account.key, authority.key)
                [0],
            &[
                destination_account,
                source_account,
                clock,
                stake_history,
                authority,
            ],
            signers,
        )
    }

    fn stake_split<'a>(
        vote_account_key: &Pubkey,
        stake_account: AccountInfo<'a>,
        authority: AccountInfo<'a>,
        bump_seed: u8,
        amount: u64,
        split_stake: AccountInfo<'a>,
    ) -> Result<(), ProgramError> {
        let authority_seeds = &[
            POOL_AUTHORITY_PREFIX,
            vote_account_key.as_ref(),
            &[bump_seed],
        ];
        let signers = &[&authority_seeds[..]];

        let split_instruction =
            stake::instruction::split(stake_account.key, authority.key, amount, split_stake.key);

        invoke_signed(
            split_instruction.last().unwrap(),
            &[stake_account, split_stake, authority],
            signers,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn stake_authorize<'a>(
        vote_account_key: &Pubkey,
        stake_account: AccountInfo<'a>,
        stake_authority: AccountInfo<'a>,
        bump_seed: u8,
        new_stake_authority: &Pubkey,
        clock: AccountInfo<'a>,
    ) -> Result<(), ProgramError> {
        let authority_seeds = &[
            POOL_AUTHORITY_PREFIX,
            vote_account_key.as_ref(),
            &[bump_seed],
        ];
        let signers = &[&authority_seeds[..]];

        let authorize_instruction = stake::instruction::authorize(
            stake_account.key,
            stake_authority.key,
            new_stake_authority,
            stake::state::StakeAuthorize::Staker,
            None,
        );

        invoke_signed(
            &authorize_instruction,
            &[
                stake_account.clone(),
                clock.clone(),
                stake_authority.clone(),
            ],
            signers,
        )?;

        let authorize_instruction = stake::instruction::authorize(
            stake_account.key,
            stake_authority.key,
            new_stake_authority,
            stake::state::StakeAuthorize::Withdrawer,
            None,
        );
        invoke_signed(
            &authorize_instruction,
            &[stake_account, clock, stake_authority],
            signers,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn stake_withdraw<'a>(
        vote_account_key: &Pubkey,
        stake_account: AccountInfo<'a>,
        stake_authority: AccountInfo<'a>,
        bump_seed: u8,
        destination_account: AccountInfo<'a>,
        clock: AccountInfo<'a>,
        stake_history: AccountInfo<'a>,
        lamports: u64,
    ) -> Result<(), ProgramError> {
        let authority_seeds = &[
            POOL_AUTHORITY_PREFIX,
            vote_account_key.as_ref(),
            &[bump_seed],
        ];
        let signers = &[&authority_seeds[..]];

        let withdraw_instruction = stake::instruction::withdraw(
            stake_account.key,
            stake_authority.key,
            destination_account.key,
            lamports,
            None,
        );

        invoke_signed(
            &withdraw_instruction,
            &[
                stake_account,
                destination_account,
                clock,
                stake_history,
                stake_authority,
            ],
            signers,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn token_mint_to<'a>(
        vote_account_key: &Pubkey,
        token_program: AccountInfo<'a>,
        mint: AccountInfo<'a>,
        destination: AccountInfo<'a>,
        authority: AccountInfo<'a>,
        bump_seed: u8,
        amount: u64,
    ) -> Result<(), ProgramError> {
        let authority_seeds = &[
            POOL_AUTHORITY_PREFIX,
            vote_account_key.as_ref(),
            &[bump_seed],
        ];
        let signers = &[&authority_seeds[..]];

        let ix = spl_token::instruction::mint_to(
            token_program.key,
            mint.key,
            destination.key,
            authority.key,
            &[],
            amount,
        )?;

        invoke_signed(&ix, &[mint, destination, authority], signers)
    }

    #[allow(clippy::too_many_arguments)]
    fn token_burn<'a>(
        vote_account_key: &Pubkey,
        token_program: AccountInfo<'a>,
        burn_account: AccountInfo<'a>,
        mint: AccountInfo<'a>,
        authority: AccountInfo<'a>,
        bump_seed: u8,
        amount: u64,
    ) -> Result<(), ProgramError> {
        let authority_seeds = &[
            POOL_AUTHORITY_PREFIX,
            vote_account_key.as_ref(),
            &[bump_seed],
        ];
        let signers = &[&authority_seeds[..]];

        let ix = spl_token::instruction::burn(
            token_program.key,
            burn_account.key,
            mint.key,
            authority.key,
            &[],
            amount,
        )?;

        invoke_signed(&ix, &[burn_account, mint, authority], signers)
    }

    fn process_initialize_pool(program_id: &Pubkey, accounts: &[AccountInfo]) -> ProgramResult {
        let account_info_iter = &mut accounts.iter();
        let vote_account_info = next_account_info(account_info_iter)?;
        let pool_stake_info = next_account_info(account_info_iter)?;
        let pool_authority_info = next_account_info(account_info_iter)?;
        let pool_mint_info = next_account_info(account_info_iter)?;
        let rent_info = next_account_info(account_info_iter)?;
        let rent = &Rent::from_account_info(rent_info)?;
        let clock_info = next_account_info(account_info_iter)?;
        let stake_history_info = next_account_info(account_info_iter)?;
        let stake_config_info = next_account_info(account_info_iter)?;
        let system_program_info = next_account_info(account_info_iter)?;
        let token_program_info = next_account_info(account_info_iter)?;
        let stake_program_info = next_account_info(account_info_iter)?;

        check_vote_account(vote_account_info)?;
        let stake_bump_seed =
            check_pool_stake_address(program_id, vote_account_info.key, pool_stake_info.key)?;
        let authority_bump_seed = check_pool_authority_address(
            program_id,
            vote_account_info.key,
            pool_authority_info.key,
        )?;
        let mint_bump_seed =
            check_pool_mint_address(program_id, vote_account_info.key, pool_mint_info.key)?;
        check_system_program(system_program_info.key)?;
        check_token_program(token_program_info.key)?;
        check_stake_program(stake_program_info.key)?;

        let stake_seeds = &[
            POOL_STAKE_PREFIX,
            vote_account_info.key.as_ref(),
            &[stake_bump_seed],
        ];
        let stake_signers = &[&stake_seeds[..]];

        let authority_seeds = &[
            POOL_AUTHORITY_PREFIX,
            vote_account_info.key.as_ref(),
            &[authority_bump_seed],
        ];
        let authority_signers = &[&authority_seeds[..]];

        let mint_seeds = &[
            POOL_MINT_PREFIX,
            vote_account_info.key.as_ref(),
            &[mint_bump_seed],
        ];
        let mint_signers = &[&mint_seeds[..]];

        // create the pool mint. user has already transferred in rent
        let mint_space = spl_token::state::Mint::LEN;

        invoke_signed(
            &system_instruction::allocate(pool_mint_info.key, mint_space as u64),
            &[pool_mint_info.clone()],
            mint_signers,
        )?;

        invoke_signed(
            &system_instruction::assign(pool_mint_info.key, token_program_info.key),
            &[pool_mint_info.clone()],
            mint_signers,
        )?;

        invoke_signed(
            &spl_token::instruction::initialize_mint2(
                token_program_info.key,
                pool_mint_info.key,
                pool_authority_info.key,
                None,
                MINT_DECIMALS,
            )?,
            &[pool_mint_info.clone()],
            authority_signers,
        )?;

        // create the pool stake account. user has already transferred in rent plus at least the minimum
        let minimum_delegation = minimum_delegation()?;
        let stake_space = std::mem::size_of::<stake::state::StakeState>();
        let stake_rent_plus_initial = rent
            .minimum_balance(stake_space)
            .saturating_add(minimum_delegation);

        if pool_stake_info.lamports() < stake_rent_plus_initial {
            return Err(SinglePoolError::WrongRentAmount.into());
        }

        let authorized = stake::state::Authorized::auto(pool_authority_info.key);

        invoke_signed(
            &system_instruction::allocate(pool_stake_info.key, stake_space as u64),
            &[pool_stake_info.clone()],
            stake_signers,
        )?;

        invoke_signed(
            &system_instruction::assign(pool_stake_info.key, stake_program_info.key),
            &[pool_stake_info.clone()],
            stake_signers,
        )?;

        invoke_signed(
            &stake::instruction::initialize_checked(pool_stake_info.key, &authorized),
            &[
                pool_stake_info.clone(),
                rent_info.clone(),
                pool_authority_info.clone(),
                pool_authority_info.clone(),
            ],
            authority_signers,
        )?;

        // delegate stake so it activates
        invoke_signed(
            &stake::instruction::delegate_stake(
                pool_stake_info.key,
                pool_authority_info.key,
                vote_account_info.key,
            ),
            &[
                pool_stake_info.clone(),
                vote_account_info.clone(),
                clock_info.clone(),
                stake_history_info.clone(),
                stake_config_info.clone(),
                pool_authority_info.clone(),
            ],
            authority_signers,
        )?;

        Ok(())
    }

    fn process_deposit_stake(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        vote_account_address: &Pubkey,
    ) -> ProgramResult {
        let account_info_iter = &mut accounts.iter();
        let pool_stake_info = next_account_info(account_info_iter)?;
        let pool_authority_info = next_account_info(account_info_iter)?;
        let pool_mint_info = next_account_info(account_info_iter)?;
        let user_stake_info = next_account_info(account_info_iter)?;
        let user_token_account_info = next_account_info(account_info_iter)?;
        let user_lamport_account_info = next_account_info(account_info_iter)?;
        let clock_info = next_account_info(account_info_iter)?;
        let clock = &Clock::from_account_info(clock_info)?;
        let stake_history_info = next_account_info(account_info_iter)?;
        let token_program_info = next_account_info(account_info_iter)?;
        let stake_program_info = next_account_info(account_info_iter)?;

        check_pool_stake_address(program_id, vote_account_address, pool_stake_info.key)?;
        let bump_seed = check_pool_authority_address(
            program_id,
            vote_account_address,
            pool_authority_info.key,
        )?;
        check_pool_mint_address(program_id, vote_account_address, pool_mint_info.key)?;
        check_token_program(token_program_info.key)?;
        check_stake_program(stake_program_info.key)?;

        if pool_stake_info.key == user_stake_info.key {
            return Err(SinglePoolError::InvalidPoolAccountUsage.into());
        }

        let minimum_delegation = minimum_delegation()?;

        let (_, pool_stake_state) = get_stake_state(pool_stake_info)?;
        let pre_pool_stake = pool_stake_state
            .delegation
            .stake
            .saturating_sub(minimum_delegation);
        msg!("Available stake pre merge {}", pre_pool_stake);

        // user can deposit active stake into an active pool or inactive stake into an activating pool
        let (_, user_stake_state) = get_stake_state(user_stake_info)?;
        if is_stake_active_without_history(&pool_stake_state, clock.epoch)
            != is_stake_active_without_history(&user_stake_state, clock.epoch)
        {
            return Err(SinglePoolError::WrongStakeState.into());
        }

        // merge the user stake account, which is preauthed to us, into the pool stake account
        // this merge succeeding implicitly validates authority/lockup of the user stake account
        Self::stake_merge(
            vote_account_address,
            user_stake_info.clone(),
            pool_authority_info.clone(),
            bump_seed,
            pool_stake_info.clone(),
            clock_info.clone(),
            stake_history_info.clone(),
        )?;

        let (pool_stake_meta, pool_stake_state) = get_stake_state(pool_stake_info)?;
        let post_pool_stake = pool_stake_state
            .delegation
            .stake
            .saturating_sub(minimum_delegation);
        let post_pool_lamports = pool_stake_info.lamports();
        msg!("Available stake post merge {}", post_pool_stake);

        // stake lamports added, as a stake difference
        let stake_added = post_pool_stake
            .checked_sub(pre_pool_stake)
            .ok_or(SinglePoolError::ArithmeticOverflow)?;

        // we calculate absolute rather than relative to deposit amount to allow claiming lamports mistakenly transferred in
        let excess_lamports = post_pool_lamports
            .checked_sub(pool_stake_state.delegation.stake)
            .and_then(|amount| amount.checked_sub(pool_stake_meta.rent_exempt_reserve))
            .ok_or(SinglePoolError::ArithmeticOverflow)?;

        // sanity check: we have not somehow gone below the minimum
        if post_pool_stake < minimum_delegation {
            return Err(SinglePoolError::UnexpectedMathError.into());
        }

        // sanity check: the user stake account is empty
        if user_stake_info.lamports() != 0 {
            return Err(SinglePoolError::UnexpectedMathError.into());
        }

        let token_supply = {
            let pool_mint_data = pool_mint_info.try_borrow_data()?;
            let pool_mint = Mint::unpack_from_slice(&pool_mint_data)?;
            pool_mint.supply
        };

        // deposit amount is determined off stake because we return excess rent
        let new_pool_tokens = calculate_deposit_amount(token_supply, pre_pool_stake, stake_added)
            .ok_or(SinglePoolError::UnexpectedMathError)?;

        if new_pool_tokens == 0 {
            return Err(SinglePoolError::DepositTooSmall.into());
        }

        // mint tokens to the user corresponding to their stake deposit
        Self::token_mint_to(
            vote_account_address,
            token_program_info.clone(),
            pool_mint_info.clone(),
            user_token_account_info.clone(),
            pool_authority_info.clone(),
            bump_seed,
            new_pool_tokens,
        )?;

        // return the lamports their stake account previously held for rent-exemption
        if excess_lamports > 0 {
            Self::stake_withdraw(
                vote_account_address,
                pool_stake_info.clone(),
                pool_authority_info.clone(),
                bump_seed,
                user_lamport_account_info.clone(),
                clock_info.clone(),
                stake_history_info.clone(),
                excess_lamports,
            )?;
        }

        Ok(())
    }

    fn process_withdraw_stake(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        vote_account_address: &Pubkey,
        user_stake_authority: &Pubkey,
        token_amount: u64,
    ) -> ProgramResult {
        let account_info_iter = &mut accounts.iter();
        let pool_stake_info = next_account_info(account_info_iter)?;
        let pool_authority_info = next_account_info(account_info_iter)?;
        let pool_mint_info = next_account_info(account_info_iter)?;
        let user_stake_info = next_account_info(account_info_iter)?;
        let user_token_account_info = next_account_info(account_info_iter)?;
        let clock_info = next_account_info(account_info_iter)?;
        let token_program_info = next_account_info(account_info_iter)?;
        let stake_program_info = next_account_info(account_info_iter)?;

        check_pool_stake_address(program_id, vote_account_address, pool_stake_info.key)?;
        let bump_seed = check_pool_authority_address(
            program_id,
            vote_account_address,
            pool_authority_info.key,
        )?;
        check_pool_mint_address(program_id, vote_account_address, pool_mint_info.key)?;
        check_token_program(token_program_info.key)?;
        check_stake_program(stake_program_info.key)?;

        if pool_stake_info.key == user_stake_info.key {
            return Err(SinglePoolError::InvalidPoolAccountUsage.into());
        }

        let minimum_delegation = minimum_delegation()?;

        let pre_pool_stake = get_stake_amount(pool_stake_info)?.saturating_sub(minimum_delegation);
        msg!("Available stake pre split {}", pre_pool_stake);

        let token_supply = {
            let pool_mint_data = pool_mint_info.try_borrow_data()?;
            let pool_mint = Mint::unpack_from_slice(&pool_mint_data)?;
            pool_mint.supply
        };

        // withdraw amount is determined off stake just like deposit amount
        let withdraw_stake = calculate_withdraw_amount(token_supply, pre_pool_stake, token_amount)
            .ok_or(SinglePoolError::UnexpectedMathError)?;

        if withdraw_stake == 0 {
            return Err(SinglePoolError::WithdrawalTooSmall.into());
        }

        // the second case should never be true, but its best to be sure
        if withdraw_stake > pre_pool_stake || withdraw_stake == pool_stake_info.lamports() {
            return Err(SinglePoolError::WithdrawalTooLarge.into());
        }

        // burn user tokens corresponding to the amount of stake they wish to withdraw
        Self::token_burn(
            vote_account_address,
            token_program_info.clone(),
            user_token_account_info.clone(),
            pool_mint_info.clone(),
            pool_authority_info.clone(),
            bump_seed,
            token_amount,
        )?;

        // split stake into a blank stake account the user has created for this purpose
        Self::stake_split(
            vote_account_address,
            pool_stake_info.clone(),
            pool_authority_info.clone(),
            bump_seed,
            withdraw_stake,
            user_stake_info.clone(),
        )?;

        // assign both authorities on the new stake account to the user
        Self::stake_authorize(
            vote_account_address,
            user_stake_info.clone(),
            pool_authority_info.clone(),
            bump_seed,
            user_stake_authority,
            clock_info.clone(),
        )?;

        let post_pool_stake = get_stake_amount(pool_stake_info)?.saturating_sub(minimum_delegation);
        msg!("Available stake post split {}", post_pool_stake);

        Ok(())
    }

    fn process_create_pool_token_metadata(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        vote_account_address: &Pubkey,
    ) -> ProgramResult {
        let account_info_iter = &mut accounts.iter();
        let pool_authority_info = next_account_info(account_info_iter)?;
        let pool_mint_info = next_account_info(account_info_iter)?;
        let payer_info = next_account_info(account_info_iter)?;
        let metadata_info = next_account_info(account_info_iter)?;
        let mpl_token_metadata_program_info = next_account_info(account_info_iter)?;
        let system_program_info = next_account_info(account_info_iter)?;

        let bump_seed = check_pool_authority_address(
            program_id,
            vote_account_address,
            pool_authority_info.key,
        )?;
        check_pool_mint_address(program_id, vote_account_address, pool_mint_info.key)?;
        check_system_program(system_program_info.key)?;
        check_account_owner(payer_info, &system_program::id())?;
        check_mpl_metadata_program(mpl_token_metadata_program_info.key)?;
        check_mpl_metadata_account_address(metadata_info.key, pool_mint_info.key)?;

        if !payer_info.is_signer {
            msg!("Payer did not sign metadata creation");
            return Err(SinglePoolError::SignatureMissing.into());
        }

        // checking the mint exists confirms pool is initialized
        {
            let pool_mint_data = pool_mint_info.try_borrow_data()?;
            let _ = Mint::unpack_from_slice(&pool_mint_data)?;
        }

        let vote_address_str = vote_account_address.to_string();
        let token_name = format!("SPL Single Pool {}", &vote_address_str[0..15]);
        let token_symbol = format!("st{}", &vote_address_str[0..7]);

        let new_metadata_instruction = create_metadata_accounts_v3(
            *mpl_token_metadata_program_info.key,
            *metadata_info.key,
            *pool_mint_info.key,
            *pool_authority_info.key,
            *payer_info.key,
            *pool_authority_info.key,
            token_name,
            token_symbol,
            "".to_string(),
            None,
            0,
            true,
            true,
            None,
            None,
            None,
        );

        let authority_seeds = &[
            POOL_AUTHORITY_PREFIX,
            vote_account_address.as_ref(),
            &[bump_seed],
        ];
        let signers = &[&authority_seeds[..]];

        invoke_signed(
            &new_metadata_instruction,
            &[
                metadata_info.clone(),
                pool_mint_info.clone(),
                pool_authority_info.clone(),
                payer_info.clone(),
                pool_authority_info.clone(),
                system_program_info.clone(),
            ],
            signers,
        )?;

        Ok(())
    }

    fn process_update_pool_token_metadata(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        name: String,
        symbol: String,
        uri: String,
    ) -> ProgramResult {
        let account_info_iter = &mut accounts.iter();
        let vote_account_info = next_account_info(account_info_iter)?;
        let pool_authority_info = next_account_info(account_info_iter)?;
        let authorized_withdrawer_info = next_account_info(account_info_iter)?;
        let metadata_info = next_account_info(account_info_iter)?;
        let mpl_token_metadata_program_info = next_account_info(account_info_iter)?;

        check_vote_account(vote_account_info)?;
        let bump_seed = check_pool_authority_address(
            program_id,
            vote_account_info.key,
            pool_authority_info.key,
        )?;
        let pool_mint_address = crate::find_pool_mint_address(program_id, vote_account_info.key);
        check_mpl_metadata_program(mpl_token_metadata_program_info.key)?;
        check_mpl_metadata_account_address(metadata_info.key, &pool_mint_address)?;

        // we use authorized_withdrawer to authenticate the caller controls the vote account
        // this is safer than using an authorized_voter since those keys live hot
        // and validator-operators we spoke with indicated this would be their preference as well
        let vote_account_data = &vote_account_info.try_borrow_data()?;
        let vote_account_withdrawer = vote_account_data
            .get(VOTE_STATE_START..VOTE_STATE_END)
            .map(Pubkey::new)
            .ok_or(SinglePoolError::UnparseableVoteAccount)?;

        if *authorized_withdrawer_info.key != vote_account_withdrawer {
            msg!("Vote account authorized withdrawer does not match the account provided.");
            return Err(SinglePoolError::InvalidMetadataSigner.into());
        }

        if !authorized_withdrawer_info.is_signer {
            msg!("Vote account authorized withdrawer did not sign metadata update.");
            return Err(SinglePoolError::SignatureMissing.into());
        }

        let update_metadata_accounts_instruction = update_metadata_accounts_v2(
            *mpl_token_metadata_program_info.key,
            *metadata_info.key,
            *pool_authority_info.key,
            None,
            Some(DataV2 {
                name,
                symbol,
                uri,
                seller_fee_basis_points: 0,
                creators: None,
                collection: None,
                uses: None,
            }),
            None,
            Some(true),
        );

        let authority_seeds = &[
            POOL_AUTHORITY_PREFIX,
            vote_account_info.key.as_ref(),
            &[bump_seed],
        ];
        let signers = &[&authority_seeds[..]];

        invoke_signed(
            &update_metadata_accounts_instruction,
            &[metadata_info.clone(), pool_authority_info.clone()],
            signers,
        )?;

        Ok(())
    }

    /// Processes [Instruction](enum.Instruction.html).
    pub fn process(program_id: &Pubkey, accounts: &[AccountInfo], input: &[u8]) -> ProgramResult {
        let instruction = SinglePoolInstruction::try_from_slice(input)?;
        match instruction {
            SinglePoolInstruction::InitializePool => {
                msg!("Instruction: InitializePool");
                Self::process_initialize_pool(program_id, accounts)
            }
            SinglePoolInstruction::DepositStake {
                vote_account_address,
            } => {
                msg!("Instruction: DepositStake");
                Self::process_deposit_stake(program_id, accounts, &vote_account_address)
            }
            SinglePoolInstruction::WithdrawStake {
                vote_account_address,
                user_stake_authority,
                token_amount,
            } => {
                msg!("Instruction: WithdrawStake");
                Self::process_withdraw_stake(
                    program_id,
                    accounts,
                    &vote_account_address,
                    &user_stake_authority,
                    token_amount,
                )
            }
            SinglePoolInstruction::CreateTokenMetadata {
                vote_account_address,
            } => {
                msg!("Instruction: CreateTokenMetadata");
                Self::process_create_pool_token_metadata(
                    program_id,
                    accounts,
                    &vote_account_address,
                )
            }
            SinglePoolInstruction::UpdateTokenMetadata { name, symbol, uri } => {
                msg!("Instruction: UpdateTokenMetadata");
                Self::process_update_pool_token_metadata(program_id, accounts, name, symbol, uri)
            }
        }
    }
}
