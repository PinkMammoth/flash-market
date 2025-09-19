import * as anchor from "@coral-xyz/anchor";
import { Program, BN } from "@coral-xyz/anchor";
import {
  PublicKey,
  SystemProgram,
  Keypair,
  SYSVAR_RENT_PUBKEY,
  Transaction,
} from "@solana/web3.js";
import {
  TOKEN_PROGRAM_ID,
  createMint,
  getOrCreateAssociatedTokenAccount,
  mintTo,
  getAccount,
} from "@solana/spl-token";
import assert from "assert";

// A small helper program deployed on localnet to write data to an account
const MOCK_WRITER_PROGRAM_ID = new PublicKey("gockemGfVwL3sU3KFRf3a9nB5Dk2d3cK228EwG2eH3S");

describe("flash_pred integration tests", () => {
  const provider = anchor.AnchorProvider.env();
  anchor.setProvider(provider);
  const program = anchor.workspace.FlashPred as Program;

  // #region Helpers
  const airdrop = async (pubkey: PublicKey, amount = 2e9) => {
    const sig = await provider.connection.requestAirdrop(pubkey, amount);
    const blockhash = await provider.connection.getLatestBlockhash();
    await provider.connection.confirmTransaction({
        signature: sig,
        ...blockhash
    });
  };

  const setupUsdcFor = async (wallet: Keypair, amount = 1_000_000_000) => {
    const ata = (
      await getOrCreateAssociatedTokenAccount(
        provider.connection,
        provider.wallet.payer,
        usdcMint,
        wallet.publicKey
      )
    ).address;
    await mintTo(
      provider.connection,
      provider.wallet.payer,
      usdcMint,
      ata,
      provider.wallet.payer,
      amount
    );
    return ata;
  };

  function buildMockPythPriceAccount(
    price: bigint,
    expo: number,
    conf: bigint
  ) {
    const buf = Buffer.alloc(3312, 0); // Standard Pyth v2 price account size
    buf.writeUInt32LE(0xa1b2c3d4, 0); // magic
    buf.writeUInt32LE(2, 4); // version
    buf.writeUInt32LE(2, 8); // account type (Price)
    buf.writeInt32LE(expo, 20); // price exponent
    buf.writeBigInt64LE(price, 32); // price
    buf.writeBigUInt64LE(conf, 40); // confidence
    buf.writeUInt32LE(1, 48); // status (Trading)
    const pubSlot = BigInt(Math.floor(Date.now() / 1000));
    buf.writeBigUInt64LE(pubSlot, 56); // pub_slot
    return buf;
  }
  // #endregion

  let usdcMint: PublicKey;

  before(async () => {
    // Global one-time setup
    await airdrop(provider.wallet.publicKey);
    usdcMint = await createMint(
      provider.connection,
      provider.wallet.payer,
      provider.wallet.publicKey,
      null,
      6
    );
  });

  // -----------------------------------------------------------
  // HAPPY PATH: Full Create -> Bet -> Resolve -> Claim Lifecycle
  // -----------------------------------------------------------
  describe("Happy Path", () => {
    const creator = Keypair.generate();
    const keeper = Keypair.generate();
    const bettorYes = Keypair.generate();
    const bettorNo = Keypair.generate();
    const mockedPythAccount = Keypair.generate();

    let marketPda: PublicKey;
    let yesVaultAta: PublicKey;
    let noVaultAta: PublicKey;
    let bettorYesAta: PublicKey;
    let bettorNoAta: PublicKey;
    let userPosYes: PublicKey;
    let userPosNo: PublicKey;

    const yesBetAmount = 100_000_000; // 100 USDC
    const noBetAmount = 200_000_000; // 200 USDC
    const strikePrice = 63000 * 1_000_000; // 63k scaled to 1e6

    const durationSecs = 3; // 3 seconds for quick testing
    const cutoffSecs = 1;
    const graceSecs = 1;
    const maxDelaySecs = 30;

    before(async () => {
      // Setup accounts and PDAs
      await Promise.all(
        [creator, keeper, bettorYes, bettorNo].map((kp) => airdrop(kp.publicKey))
      );
      bettorYesAta = await setupUsdcFor(bettorYes);
      bettorNoAta = await setupUsdcFor(bettorNo);

      [marketPda] = await PublicKey.findProgramAddress(
        [Buffer.from("market"), creator.publicKey.toBuffer()],
        program.programId
      );

      yesVaultAta = (await getOrCreateAssociatedTokenAccount(provider.connection, provider.wallet.payer, usdcMint, marketPda, true)).address;
      noVaultAta = (await getOrCreateAssociatedTokenAccount(provider.connection, provider.wallet.payer, usdcMint, marketPda, true)).address;

      userPosYes = (await PublicKey.findProgramAddress([Buffer.from("userpos"), marketPda.toBuffer(), bettorYes.publicKey.toBuffer()], program.programId))[0];
      userPosNo = (await PublicKey.findProgramAddress([Buffer.from("userpos"), marketPda.toBuffer(), bettorNo.publicKey.toBuffer()], program.programId))[0];

      // Setup Mock Pyth Account
      const finalPrice = BigInt(63500 * 1_000_000); // Price ABOVE strike
      const pythData = buildMockPythPriceAccount(finalPrice, -6, BigInt(10000));
      const space = pythData.length;
      const lamports = await provider.connection.getMinimumBalanceForRentExemption(space);

      const createIx = SystemProgram.createAccount({ fromPubkey: provider.wallet.publicKey, newAccountPubkey: mockedPythAccount.publicKey, space, lamports, programId: MOCK_WRITER_PROGRAM_ID });
      await provider.sendAndConfirm(new Transaction().add(createIx), [mockedPythAccount]);
      await provider.connection.sendAndConfirm(new Transaction().add({ keys: [{ pubkey: mockedPythAccount.publicKey, isSigner: false, isWritable: true }], programId: MOCK_WRITER_PROGRAM_ID, data: pythData, }));

      // Create market
      await program.methods
        .createMarket("BTC-USD", new BN(strikePrice), new BN(durationSecs), new BN(cutoffSecs), new BN(graceSecs), new BN(maxDelaySecs))
        .accounts({ market: marketPda, creator: creator.publicKey, keeper: keeper.publicKey, pythPriceFeed: mockedPythAccount.publicKey, systemProgram: SystemProgram.programId, tokenProgram: TOKEN_PROGRAM_ID, rent: SYSVAR_RENT_PUBKEY, })
        .signers([creator])
        .rpc();
    });

    it("places YES and NO bets", async () => {
        await program.methods.placeBet(new BN(yesBetAmount), { yes: {} })
            .accounts({ market: marketPda, user: bettorYes.publicKey, userTokenAccount: bettorYesAta, yesVault: yesVaultAta, noVault: noVaultAta, userPosition: userPosYes, tokenProgram: TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId, rent: SYSVAR_RENT_PUBKEY, })
            .signers([bettorYes]).rpc();

        await program.methods.placeBet(new BN(noBetAmount), { no: {} })
            .accounts({ market: marketPda, user: bettorNo.publicKey, userTokenAccount: bettorNoAta, yesVault: yesVaultAta, noVault: noVaultAta, userPosition: userPosNo, tokenProgram: TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId, rent: SYSVAR_RENT_PUBKEY, })
            .signers([bettorNo]).rpc();

        const yesPos = await program.account.userPosition.fetch(userPosYes);
        const noPos = await program.account.userPosition.fetch(userPosNo);
        assert.equal(yesPos.amount.toNumber(), yesBetAmount);
        assert.equal(noPos.amount.toNumber(), noBetAmount);
    });

    it("resolves market and allows winner to claim", async () => {
      await new Promise((r) => setTimeout(r, (durationSecs + graceSecs) * 1000));

      await program.methods.resolveMarket()
        .accounts({ market: marketPda, keeper: keeper.publicKey, pythPriceFeed: mockedPythAccount.publicKey, })
        .signers([keeper]).rpc();

      const market = await program.account.market.fetch(marketPda);
      assert.ok(market.outcome.yes, "Market outcome should be YES");

      const beforeYes = (await getAccount(provider.connection, bettorYesAta)).amount;
      await program.methods.claimWinnings()
        .accounts({ market: marketPda, userPosition: userPosYes, user: bettorYes.publicKey, userTokenAccount: bettorYesAta, yesVault: yesVaultAta, noVault: noVaultAta, tokenProgram: TOKEN_PROGRAM_ID, })
        .signers([bettorYes]).rpc();
      const afterYes = (await getAccount(provider.connection, bettorYesAta)).amount;

      assert.equal(Number(afterYes) - Number(beforeYes), yesBetAmount + noBetAmount);
    });
  });

  // -----------------------------------------------------------
  // SAFETY FEATURES (Cutoff Buffer)
  // -----------------------------------------------------------
  describe("Safety Features", () => {
    const creator = Keypair.generate();
    const bettor = Keypair.generate();
    let marketPda: PublicKey, bettorAta: PublicKey, yesVaultAta: PublicKey, noVaultAta: PublicKey, userPos: PublicKey;
    const durationSecs = 5;
    const cutoffSecs = 3;

    before(async () => {
      await Promise.all([creator, bettor].map(kp => airdrop(kp.publicKey)));
      bettorAta = await setupUsdcFor(bettor);
      [marketPda] = await PublicKey.findProgramAddress([Buffer.from("market"), creator.publicKey.toBuffer()], program.programId);
      yesVaultAta = (await getOrCreateAssociatedTokenAccount(provider.connection, provider.wallet.payer, usdcMint, marketPda, true)).address;
      noVaultAta = (await getOrCreateAssociatedTokenAccount(provider.connection, provider.wallet.payer, usdcMint, marketPda, true)).address;
      userPos = (await PublicKey.findProgramAddress([Buffer.from("userpos"), marketPda.toBuffer(), bettor.publicKey.toBuffer()], program.programId))[0];
      await program.methods.createMarket("BTC-USD", new BN(60000 * 1e6), new BN(durationSecs), new BN(cutoffSecs), new BN(2), new BN(10))
        .accounts({ market: marketPda, creator: creator.publicKey, keeper: creator.publicKey, pythPriceFeed: SystemProgram.programId, systemProgram: SystemProgram.programId, tokenProgram: TOKEN_PROGRAM_ID, rent: SYSVAR_RENT_PUBKEY, })
        .signers([creator]).rpc();
    });

    it("rejects bets inside cutoff window", async () => {
      await new Promise(r => setTimeout(r, (durationSecs - cutoffSecs + 1) * 1000));
      let threw = false;
      try {
        await program.methods.placeBet(new BN(50_000_000), { yes: {} })
            .accounts({ market: marketPda, user: bettor.publicKey, userTokenAccount: bettorAta, yesVault: yesVaultAta, noVault: noVaultAta, userPosition: userPos, tokenProgram: TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId, rent: SYSVAR_RENT_PUBKEY, })
            .signers([bettor]).rpc();
      } catch (err) {
        threw = true;
        assert.include(err.message, "Betting window has closed");
      }
      assert.ok(threw, "Bet inside cutoff window should fail");
    });
  });

  // -----------------------------------------------------------
  // REFUND PATH
  // -----------------------------------------------------------
  describe("Refund Path", () => {
    const creator = Keypair.generate();
    const bettor = Keypair.generate();
    let marketPda: PublicKey, bettorAta: PublicKey, yesVaultAta: PublicKey, noVaultAta: PublicKey, userPos: PublicKey;
    const betAmount = 100_000_000;
    const durationSecs = 2;
    const maxDelaySecs = 3;

    before(async () => {
        await Promise.all([creator, bettor].map(kp => airdrop(kp.publicKey)));
        bettorAta = await setupUsdcFor(bettor);
        [marketPda] = await PublicKey.findProgramAddress([Buffer.from("market"), creator.publicKey.toBuffer()], program.programId);
        yesVaultAta = (await getOrCreateAssociatedTokenAccount(provider.connection, provider.wallet.payer, usdcMint, marketPda, true)).address;
        noVaultAta = (await getOrCreateAssociatedTokenAccount(provider.connection, provider.wallet.payer, usdcMint, marketPda, true)).address;
        userPos = (await PublicKey.findProgramAddress([Buffer.from("userpos"), marketPda.toBuffer(), bettor.publicKey.toBuffer()], program.programId))[0];
        await program.methods.createMarket("BTC-USD", new BN(59000 * 1e6), new BN(durationSecs), new BN(10), new BN(2), new BN(maxDelaySecs))
            .accounts({ market: marketPda, creator: creator.publicKey, keeper: creator.publicKey, pythPriceFeed: SystemProgram.programId, systemProgram: SystemProgram.programId, tokenProgram: TOKEN_PROGRAM_ID, rent: SYSVAR_RENT_PUBKEY, })
            .signers([creator]).rpc();
        await program.methods.placeBet(new BN(betAmount), { yes: {} })
            .accounts({ market: marketPda, user: bettor.publicKey, userTokenAccount: bettorAta, yesVault: yesVaultAta, noVault: noVaultAta, userPosition: userPos, tokenProgram: TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId, rent: SYSVAR_RENT_PUBKEY, })
            .signers([bettor]).rpc();
    });

    it("refunds stake when no resolution occurs", async () => {
      await new Promise(r => setTimeout(r, (durationSecs + maxDelaySecs + 1) * 1000));
      const before = (await getAccount(provider.connection, bettorAta)).amount;
      await program.methods.refundUnsettlable()
        .accounts({ market: marketPda, userPosition: userPos, user: bettor.publicKey, userTokenAccount: bettorAta, yesVault: yesVaultAta, noVault: noVaultAta, tokenProgram: TOKEN_PROGRAM_ID, })
        .signers([bettor]).rpc();
      const after = (await getAccount(provider.connection, bettorAta)).amount;
      assert.equal(Number(after) - Number(before), betAmount, "Bettor should receive full refund");
    });
  });
});