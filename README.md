# CAPKEY

Onchain spending-policy vaults for AI agents — built on Solana.

**Live program (devnet):** [`5w4nmhNaJocH9sCiQgWt6A6DhAoNj7Lr3KjEbAjRLJ1j`](https://explorer.solana.com/address/5w4nmhNaJocH9sCiQgWt6A6DhAoNj7Lr3KjEbAjRLJ1j?cluster=devnet)

## The problem

Every AI agent that needs to take an action involving money faces the same bad choice: hand it your entire wallet, or approve every single transaction yourself — which defeats the point of it being autonomous. Either way, a compromised or prompt-injected agent can drain you.

## The fix

CAPKEY replaces "here's my wallet" with an onchain allowance. The owner deposits USDC into a program-owned vault and sets a policy:

- **per-transaction cap**
- **daily cap** (resets automatically)
- **recipient allowlist** (up to 3 approved destinations)
- **expiry**
- **instant, permanent revoke**

The agent can only ever spend inside that policy. Every check — cap, daily limit, allowlist, expiry, revoked status — is enforced **inside the Solana program itself**. A compromised agent can't just skip the check; the chain rejects the transaction outright.

## Why not just use a token delegate?

SPL's `approve` gives a delegate one number — a spending cap, nothing else. CAPKEY enforces a full policy in a single instruction, and the owner can recover unspent funds after revoking — a plain delegate can't do either.

## Instructions

| Instruction | What it does |
|---|---|
| `create_vault` | Creates the vault (PDA), binds one owner + one agent + one mint, takes the initial deposit |
| `fund_vault` | Owner tops up the budget |
| `set_policy` | Owner sets/updates caps, allowlist, expiry |
| `execute_payment` | Agent attempts a payment — reverts onchain if it violates policy |
| `revoke_agent` | Owner's kill switch — permanent |
| `withdraw_unused` | Owner recovers the remaining balance, only after revoke or expiry |

## Proof

- **8/8 tests passing** against a real Solana test validator (see `tests/`)
- **Live demo:** open `capkey-demo.html` in a browser (needs Phantom, set to Devnet), connect, and click through: fund → pay within policy → over-cap payment rejected onchain → payment to a non-approved recipient rejected onchain → revoke → post-revoke payment rejected → funds recovered
- Full walkthrough with screenshots and the actual demo log: `capkey-proof-pack.html`

## Roadmap

- **x402 facilitator** — CAPKEY as the facilitator behind an x402-gated endpoint, so any resource server can plug in and get policy-checked, onchain-settled agent payments.
- **Phase 2 — "Leash"** — the same policy primitive guarding Hyperliquid order/position risk, not just token transfers.

## Built for

Colosseum's Crypto World's Fair — built solo.
