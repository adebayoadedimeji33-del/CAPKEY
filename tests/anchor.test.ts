import * as anchor from "@coral-xyz/anchor";
import { BN, web3 } from "@coral-xyz/anchor";
import {
  getAssociatedTokenAddressSync,
  createMint,
  mintTo,
  createAssociatedTokenAccount,
  getAccount,
  TOKEN_PROGRAM_ID,
  ASSOCIATED_TOKEN_PROGRAM_ID,
} from "@solana/spl-token";
import { assert } from "chai";

anchor.setProvider(anchor.AnchorProvider.env());
const provider = anchor.getProvider() as anchor.AnchorProvider;

const VAULT_SEED = Buffer.from("capkey-vault");
const DECIMALS = 6;
const ONE = new BN(1_000_000);
const usdc = (n: number) => new BN(n).mul(ONE);

function errCode(e: any): string {
  const m = String(e?.message ?? e);
  const match = m.match(/Error Code: (\w+)/);
  return match ? match[1] : m;
}

async function expectError(promise: Promise<unknown>, expected: string) {
  try {
    await promise;
    assert(false, `expected "${expected}" but the call succeeded`);
  } catch (e) {
    const code = errCode(e);
    assert(code.includes(expected), `expected "${expected}", got: ${code}`);
  }
}

async function getAccountSafe(connection: any, address: any, retries = 6, delayMs = 800) {
  for (let i = 0; i < retries; i++) {
    try {
      return await getAccount(connection, address, "confirmed");
    } catch (e) {
      if (i === retries - 1) throw e;
      await new Promise((r) => setTimeout(r, delayMs));
    }
  }
}

async function send(builder: any, connection: any) {
  const sig = await builder.rpc();
  await connection.confirmTransaction(sig, "confirmed");
  return sig;
}

describe("capkey", () => {
  const program = anchor.workspace.Capkey as anchor.Program;
  const connection = provider.connection;
  const owner = provider.wallet.payer;

  const agent = web3.Keypair.generate();
  const payee = web3.Keypair.generate();
  const stranger = web3.Keypair.generate();

  let mint: web3.PublicKey;
  let vault: web3.PublicKey;
  let vaultAta: web3.PublicKey;
  let ownerAta: web3.PublicKey;
  let payeeAta: web3.PublicKey;
  let strangerAta: web3.PublicKey;

  function payAgentBuilder(recipientAta: web3.PublicKey, amount: BN) {
    return program.methods
      .executePayment(amount)
      .accounts({
        vault,
        agent: agent.publicKey,
        vaultTokenAccount: vaultAta,
        recipientTokenAccount: recipientAta,
        mint,
        tokenProgram: TOKEN_PROGRAM_ID,
        associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID,
      })
      .signers([agent]);
  }

  async function agentPays(recipientAta: web3.PublicKey, amount: BN) {
    return send(payAgentBuilder(recipientAta, amount), connection);
  }

  before(async function () {
    this.timeout(120_000);

    mint = await createMint(connection, owner, owner.publicKey, null, DECIMALS);

    ownerAta = getAssociatedTokenAddressSync(mint, owner.publicKey);
    payeeAta = getAssociatedTokenAddressSync(mint, payee.publicKey);
    strangerAta = getAssociatedTokenAddressSync(mint, stranger.publicKey);

    await createAssociatedTokenAccount(connection, owner, mint, owner.publicKey);
    await createAssociatedTokenAccount(connection, owner, mint, payee.publicKey);
    await createAssociatedTokenAccount(connection, owner, mint, stranger.publicKey);

    await mintTo(connection, owner, mint, ownerAta, owner.publicKey, 1_000_000_000_000n);

    [vault] = web3.PublicKey.findProgramAddressSync(
      [VAULT_SEED, owner.publicKey.toBuffer(), agent.publicKey.toBuffer(), mint.toBuffer()],
      program.programId
    );
    vaultAta = getAssociatedTokenAddressSync(mint, vault, true);
  });

  it("creates the vault and locks the initial budget", async function () {
    this.timeout(60_000);
    await send(
      program.methods
        .createVault(usdc(10))
        .accounts({
          owner: owner.publicKey,
          agent: agent.publicKey,
          vault,
          vaultTokenAccount: vaultAta,
          mint,
          ownerTokenAccount: ownerAta,
          tokenProgram: TOKEN_PROGRAM_ID,
          systemProgram: web3.SystemProgram.programId,
          associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID,
        })
        .signers([agent]),
      connection
    );

    const acct = await program.account.vaultPolicy.fetch(vault);
    assert(acct.isActive, "vault should be active right after creation");
    const bal = await getAccountSafe(connection, vaultAta);
    assert.equal(bal.amount.toString(), usdc(10).toString());
  });

  it("lets the owner set the spending envelope", async function () {
    this.timeout(60_000);
    const expiry = new BN(Math.floor(Date.now() / 1000) + 3600);
    await send(
      program.methods
        .setPolicy(usdc(5), usdc(8), expiry, [payee.publicKey])
        .accounts({ owner: owner.publicKey, vault }),
      connection
    );

    const acct = await program.account.vaultPolicy.fetch(vault);
    assert.equal(acct.maxPerTx.toString(), usdc(5).toString());
  });

  it("lets the agent pay within policy", async function () {
    this.timeout(60_000);
    await agentPays(payeeAta, usdc(2));
    const bal = await getAccountSafe(connection, payeeAta);
    assert.equal(bal.amount.toString(), usdc(2).toString());
  });

  it("blocks a payment above the per-tx cap (onchain revert)", async function () {
    this.timeout(60_000);
    await expectError(agentPays(payeeAta, usdc(6)), "ExceedsMaxPerTx");
  });

  it("blocks payment to a recipient outside the allowlist (onchain revert)", async function () {
    this.timeout(60_000);
    await expectError(agentPays(strangerAta, usdc(1)), "RecipientNotAllowed");
  });

  it("enforces the daily limit across payments", async function () {
    this.timeout(60_000);
    await agentPays(payeeAta, usdc(4));
    await agentPays(payeeAta, usdc(2));
    await expectError(agentPays(payeeAta, usdc(1)), "ExceedsDailyLimit");
  });

  it("revocation is a permanent kill switch", async function () {
    this.timeout(60_000);
    await send(
      program.methods.revokeAgent().accounts({ owner: owner.publicKey, vault }),
      connection
    );
    await expectError(agentPays(payeeAta, usdc(1)), "VaultRevoked");
  });

  it("returns the leftover budget to the owner after revoke", async function () {
    this.timeout(60_000);
    const before = await getAccountSafe(connection, ownerAta);

    await send(
      program.methods.withdrawUnused().accounts({
        owner: owner.publicKey,
        vault,
        vaultTokenAccount: vaultAta,
        ownerTokenAccount: ownerAta,
        mint,
        tokenProgram: TOKEN_PROGRAM_ID,
        associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID,
      }),
      connection
    );

    const after = await getAccountSafe(connection, ownerAta);
    const vaultAfter = await getAccountSafe(connection, vaultAta);
    assert.equal(vaultAfter.amount.toString(), "0");
    assert(after.amount > before.amount, "owner balance should have gone up");
  });
});
