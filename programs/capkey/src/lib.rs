use anchor_lang::prelude::*;
use anchor_spl::associated_token::AssociatedToken;
use anchor_spl::token::{transfer, Mint, Token, TokenAccount, Transfer};

// This must match the program id already deployed on devnet:
// 5w4nmhNaJocH9sCiQgWt6A6DhAoNj7Lr3KjEbAjRLJ1j
declare_id!("5w4nmhNaJocH9sCiQgWt6A6DhAoNj7Lr3KjEbAjRLJ1j");

pub const VAULT_SEED: &[u8] = b"capkey-vault";
pub const DAY_SECONDS: i64 = 86_400;
pub const MAX_ALLOWED_RECIPIENTS: usize = 3;

#[program]
pub mod capkey {
    use super::*;

    /// Create a policy-controlled vault.
    /// - `owner` — the human who controls the budget (signs).
    /// - `agent` — the AI agent's keypair. Signing here binds the agent's
    ///   identity to the vault; it is the only key that may later call
    ///   `execute_payment`.
    /// - `initial_deposit` — token amount (smallest units) moved from the
    ///   owner's token account into the vault in this same transaction.
    pub fn create_vault(ctx: Context<CreateVault>, initial_deposit: u64) -> Result<()> {
        require!(initial_deposit > 0, CapkeyError::ZeroAmount);

        let vault = &mut ctx.accounts.vault;
        vault.owner = ctx.accounts.owner.key();
        vault.agent = ctx.accounts.agent.key();
        vault.token_mint = ctx.accounts.mint.key();
        vault.allowed_recipients = [Pubkey::default(); MAX_ALLOWED_RECIPIENTS];
        vault.recipient_count = 0;
        vault.max_per_tx = 0;
        vault.daily_limit = 0;
        vault.amount_spent_today = 0;
        vault.last_reset_day = 0;
        vault.expiry_ts = i64::MAX; // no expiry until the owner calls set_policy
        vault.is_active = true;
        vault.bump = ctx.bumps.vault;

        let cpi_accounts = Transfer {
            from: ctx.accounts.owner_token_account.to_account_info(),
            to: ctx.accounts.vault_token_account.to_account_info(),
            authority: ctx.accounts.owner.to_account_info(),
        };
        transfer(CpiContext::new(ctx.accounts.token_program.to_account_info(), cpi_accounts), initial_deposit)?;

        Ok(())
    }

    /// Top up the vault with more budget. Owner only.
    pub fn fund_vault(ctx: Context<FundVault>, amount: u64) -> Result<()> {
        require!(amount > 0, CapkeyError::ZeroAmount);
        let cpi_accounts = Transfer {
            from: ctx.accounts.owner_token_account.to_account_info(),
            to: ctx.accounts.vault_token_account.to_account_info(),
            authority: ctx.accounts.owner.to_account_info(),
        };
        transfer(CpiContext::new(ctx.accounts.token_program.to_account_info(), cpi_accounts), amount)?;
        Ok(())
    }

    /// Set / update the spending envelope. Owner only.
    /// Setting an empty `allowed_recipients` acts as a soft pause: no
    /// payments can succeed until the allowlist is repopulated, without
    /// destroying the vault (unlike `revoke_agent`).
    pub fn set_policy(
        ctx: Context<SetPolicy>,
        max_per_tx: u64,
        daily_limit: u64,
        expiry_ts: i64,
        allowed_recipients: Vec<Pubkey>,
    ) -> Result<()> {
        require!(
            allowed_recipients.len() <= MAX_ALLOWED_RECIPIENTS,
            CapkeyError::TooManyRecipients
        );
        require!(daily_limit >= max_per_tx, CapkeyError::PolicyInconsistent);

        let vault = &mut ctx.accounts.vault;
        vault.max_per_tx = max_per_tx;
        vault.daily_limit = daily_limit;
        vault.expiry_ts = expiry_ts;
        for (i, recipient) in allowed_recipients.iter().enumerate() {
            vault.allowed_recipients[i] = *recipient;
        }
        vault.recipient_count = allowed_recipients.len() as u8;
        Ok(())
    }

    /// The agent executes a payment. If ANY policy check fails, the whole
    /// transaction reverts and no funds move — this is the entire value
    /// proposition, enforced by the chain, not by the client.
    pub fn execute_payment(ctx: Context<ExecutePayment>, amount: u64) -> Result<()> {
        require!(amount > 0, CapkeyError::ZeroAmount);

        let vault = &mut ctx.accounts.vault;

        // 1. Kill switch.
        require!(vault.is_active, CapkeyError::VaultRevoked);

        // 2. Expiry.
        let now = Clock::get()?.unix_timestamp;
        require!(now < vault.expiry_ts, CapkeyError::VaultExpired);

        // 3. Per-transaction cap.
        require!(amount <= vault.max_per_tx, CapkeyError::ExceedsMaxPerTx);

        // 4. Daily cap (resets at UTC midnight, lazily).
        let today = now / DAY_SECONDS;
        if vault.last_reset_day != today {
            vault.amount_spent_today = 0;
            vault.last_reset_day = today;
        }
        require!(
            amount + vault.amount_spent_today <= vault.daily_limit,
            CapkeyError::ExceedsDailyLimit
        );

        // 5. Recipient allowlist: the owner of the destination token
        //    account must be an approved recipient.
        let recipient = ctx.accounts.recipient_token_account.owner;
        let mut allowed = false;
        for i in 0..vault.recipient_count as usize {
            if vault.allowed_recipients[i] == recipient {
                allowed = true;
            }
        }
        require!(allowed, CapkeyError::RecipientNotAllowed);

        // All checks passed — commit state, then move funds.
        vault.amount_spent_today += amount;
        let daily_remaining = vault.daily_limit - vault.amount_spent_today;

        let cpi_accounts = Transfer {
            from: ctx.accounts.vault_token_account.to_account_info(),
            to: ctx.accounts.recipient_token_account.to_account_info(),
            authority: vault.to_account_info(),
        };
        let signer_seeds: &[&[&[u8]]] = &[&[
            VAULT_SEED,
            vault.owner.as_ref(),
            vault.agent.as_ref(),
            vault.token_mint.as_ref(),
            &[vault.bump],
        ]];
        transfer(
            CpiContext::new_with_signer(ctx.accounts.token_program.to_account_info(), cpi_accounts, signer_seeds),
            amount,
        )?;

        emit!(PaymentExecuted {
            vault: vault.key(),
            agent: vault.agent,
            recipient,
            amount,
            daily_remaining,
        });

        Ok(())
    }

    /// One-click kill switch. Irreversible: a revoked vault can never pay
    /// again. The owner recovers the remaining balance via `withdraw_unused`.
    pub fn revoke_agent(ctx: Context<RevokeAgent>) -> Result<()> {
        ctx.accounts.vault.is_active = false;
        emit!(Revoked { vault: ctx.accounts.vault.key() });
        Ok(())
    }

    /// Owner drains the remaining balance back to their own token account.
    /// Only legal once the vault can no longer spend — revoked or expired —
    /// so there's never a window where owner and agent race the same funds.
    pub fn withdraw_unused(ctx: Context<WithdrawUnused>) -> Result<()> {
        let vault = &ctx.accounts.vault;
        let now = Clock::get()?.unix_timestamp;

        require!(
            !vault.is_active || now >= vault.expiry_ts,
            CapkeyError::VaultStillActive
        );

        let amount = ctx.accounts.vault_token_account.amount;
        require!(amount > 0, CapkeyError::NothingToWithdraw);

        let signer_seeds: &[&[&[u8]]] = &[&[
            VAULT_SEED,
            vault.owner.as_ref(),
            vault.agent.as_ref(),
            vault.token_mint.as_ref(),
            &[vault.bump],
        ]];
        let cpi_accounts = Transfer {
            from: ctx.accounts.vault_token_account.to_account_info(),
            to: ctx.accounts.owner_token_account.to_account_info(),
            authority: vault.to_account_info(),
        };
        transfer(
            CpiContext::new_with_signer(ctx.accounts.token_program.to_account_info(), cpi_accounts, signer_seeds),
            amount,
        )?;

        emit!(Withdrawn { vault: vault.key(), owner: vault.owner, amount });

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Events (the dashboard subscribes to these)
// ---------------------------------------------------------------------------

#[event]
pub struct PaymentExecuted {
    pub vault: Pubkey,
    pub agent: Pubkey,
    pub recipient: Pubkey,
    pub amount: u64,
    pub daily_remaining: u64,
}

#[event]
pub struct Revoked {
    pub vault: Pubkey,
}

#[event]
pub struct Withdrawn {
    pub vault: Pubkey,
    pub owner: Pubkey,
    pub amount: u64,
}

// ---------------------------------------------------------------------------
// The vault account — the entire product, bound to (owner, agent, mint).
// PDA seeds: ["capkey-vault", owner, agent, mint]
// ---------------------------------------------------------------------------

#[account]
pub struct VaultPolicy {
    pub owner: Pubkey,
    pub agent: Pubkey,
    pub token_mint: Pubkey,
    pub allowed_recipients: [Pubkey; MAX_ALLOWED_RECIPIENTS],
    pub recipient_count: u8,
    pub max_per_tx: u64,
    pub daily_limit: u64,
    pub amount_spent_today: u64,
    pub last_reset_day: i64,
    pub expiry_ts: i64,
    pub is_active: bool,
    pub bump: u8,
}

impl VaultPolicy {
    pub const SPACE: usize = 8
        + std::mem::size_of::<Pubkey>() * 6
        + std::mem::size_of::<u8>() * 2
        + std::mem::size_of::<u64>() * 3
        + std::mem::size_of::<i64>() * 2
        + std::mem::size_of::<bool>();
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[error_code]
pub enum CapkeyError {
    #[msg("Amount must be greater than zero.")]
    ZeroAmount,
    #[msg("Vault has been revoked by the owner.")]
    VaultRevoked,
    #[msg("Vault policy has expired.")]
    VaultExpired,
    #[msg("Amount exceeds the per-transaction cap.")]
    ExceedsMaxPerTx,
    #[msg("Amount exceeds the remaining daily limit.")]
    ExceedsDailyLimit,
    #[msg("Recipient token account is not on the vault allowlist.")]
    RecipientNotAllowed,
    #[msg("Allowlist may contain at most 3 recipients.")]
    TooManyRecipients,
    #[msg("Daily limit must be >= per-transaction cap.")]
    PolicyInconsistent,
    #[msg("Agent does not match the agent bound to this vault.")]
    WrongAgent,
    #[msg("Vault is still active and not yet expired. Revoke it or wait for expiry before withdrawing.")]
    VaultStillActive,
    #[msg("Vault holds no tokens. Nothing to withdraw.")]
    NothingToWithdraw,
}

// ---------------------------------------------------------------------------
// Accounts (who/what each instruction needs)
// ---------------------------------------------------------------------------

#[derive(Accounts)]
pub struct CreateVault<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,
    pub agent: Signer<'info>,

    #[account(
        init,
        payer = owner,
        space = VaultPolicy::SPACE,
        seeds = [VAULT_SEED, owner.key().as_ref(), agent.key().as_ref(), mint.key().as_ref()],
        bump
    )]
    pub vault: Account<'info, VaultPolicy>,

    #[account(
        init,
        payer = owner,
        associated_token::mint = mint,
        associated_token::authority = vault
    )]
    pub vault_token_account: Account<'info, TokenAccount>,

    #[account(mut)]
    pub mint: Account<'info, Mint>,

    #[account(mut, associated_token::mint = mint, associated_token::authority = owner)]
    pub owner_token_account: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
    pub associated_token_program: Program<'info, AssociatedToken>,
}

#[derive(Accounts)]
pub struct FundVault<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,

    #[account(
        mut,
        seeds = [VAULT_SEED, owner.key().as_ref(), vault.agent.as_ref(), vault.token_mint.as_ref()],
        bump = vault.bump,
        has_one = owner
    )]
    pub vault: Account<'info, VaultPolicy>,

    #[account(mut, associated_token::mint = mint, associated_token::authority = vault)]
    pub vault_token_account: Account<'info, TokenAccount>,

    pub mint: Account<'info, Mint>,

    #[account(mut, associated_token::mint = mint, associated_token::authority = owner)]
    pub owner_token_account: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
}

#[derive(Accounts)]
pub struct SetPolicy<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,

    #[account(
        mut,
        seeds = [VAULT_SEED, owner.key().as_ref(), vault.agent.as_ref(), vault.token_mint.as_ref()],
        bump = vault.bump,
        has_one = owner
    )]
    pub vault: Account<'info, VaultPolicy>,
}

#[derive(Accounts)]
pub struct ExecutePayment<'info> {
    #[account(
        mut,
        seeds = [VAULT_SEED, vault.owner.as_ref(), vault.agent.as_ref(), vault.token_mint.as_ref()],
        bump = vault.bump,
        has_one = agent @ CapkeyError::WrongAgent
    )]
    pub vault: Account<'info, VaultPolicy>,

    pub agent: Signer<'info>,

    #[account(mut, associated_token::mint = mint, associated_token::authority = vault)]
    pub vault_token_account: Account<'info, TokenAccount>,

    #[account(mut, constraint = recipient_token_account.mint == mint.key() @ CapkeyError::RecipientNotAllowed)]
    pub recipient_token_account: Account<'info, TokenAccount>,

    pub mint: Account<'info, Mint>,
    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
}

#[derive(Accounts)]
pub struct RevokeAgent<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,

    #[account(
        mut,
        seeds = [VAULT_SEED, owner.key().as_ref(), vault.agent.as_ref(), vault.token_mint.as_ref()],
        bump = vault.bump,
        has_one = owner
    )]
    pub vault: Account<'info, VaultPolicy>,
}

#[derive(Accounts)]
pub struct WithdrawUnused<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,

    #[account(
        mut,
        seeds = [VAULT_SEED, owner.key().as_ref(), vault.agent.as_ref(), vault.token_mint.as_ref()],
        bump = vault.bump,
        has_one = owner
    )]
    pub vault: Account<'info, VaultPolicy>,

    #[account(mut, associated_token::mint = mint, associated_token::authority = vault)]
    pub vault_token_account: Account<'info, TokenAccount>,

    #[account(mut, associated_token::mint = mint, associated_token::authority = owner)]
    pub owner_token_account: Account<'info, TokenAccount>,

    pub mint: Account<'info, Mint>,
    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
}
