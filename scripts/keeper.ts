import * as anchor from "@coral-xyz/anchor";
import { Program, BN } from "@coral-xyz/anchor";
import { Keypair, Connection, PublicKey } from "@solana/web3.js";
import { FlashPred } from "../target/types/flash_pred"; // Adjust path to your IDL
import * as fs from 'fs';

// --- CONFIGURATION ---

// 1. Connection and Program Setup
const RPC_URL = "https://api.devnet.solana.com"; // Use "http://127.0.0.1:8899" for localnet
const PROGRAM_ID = new PublicKey("flashPred11111111111111111111111111111111111"); // Replace with your program ID

// 2. Keeper Wallet
// Load your keeper keypair from a file.
// IMPORTANT: Never commit your keypair file to a public repository.
// Use environment variables or a secure secret management system in production.
const KEEPER_KEYPAIR_PATH = "/path/to/your/keeper-keypair.json";

// 3. Polling Interval
// How often the bot checks for markets to resolve (in milliseconds).
const POLLING_INTERVAL_MS = 15000; // 15 seconds

// --- SCRIPT LOGIC ---

async function main() {
    // Initialize connection and provider
    const connection = new Connection(RPC_URL, "confirmed");

    const keeperWallet = Keypair.fromSecretKey(
        Buffer.from(JSON.parse(fs.readFileSync(KEEPER_KEYPAIR_PATH, "utf-8")))
    );

    const provider = new anchor.AnchorProvider(
        connection,
        new anchor.Wallet(keeperWallet),
        { commitment: "confirmed" }
    );

    // Load the program
    const program = new anchor.Program<FlashPred>(
        require("../target/idl/flash_pred.json"), // Adjust path to your IDL
        PROGRAM_ID,
        provider
    );

    console.log(`🚀 Keeper bot started for program: ${program.programId}`);
    console.log(`🔑 Keeper wallet address: ${keeperWallet.publicKey.toBase58()}`);
    console.log(`📡 Watching for markets every ${POLLING_INTERVAL_MS / 1000} seconds...`);

    // Main loop to check and resolve markets
    setInterval(async () => {
        try {
            console.log("\nChecking for resolvable markets...");

            // Fetch all market accounts
            const markets = await program.account.market.all();
            if (markets.length === 0) {
                console.log("No markets found.");
                return;
            }

            const now = Math.floor(Date.now() / 1000);

            // Filter for markets that are pending and past their expiry + grace period
            const resolvableMarkets = markets.filter(market => {
                const outcomeIsPending = Object.keys(market.account.outcome)[0] === 'pending';
                const isPastExpiry = now >= (market.account.expiryTs.toNumber() + market.account.graceSecs.toNumber());
                return outcomeIsPending && isPastExpiry;
            });

            if (resolvableMarkets.length === 0) {
                console.log("No markets are ready to be resolved.");
                return;
            }

            console.log(`Found ${resolvableMarkets.length} market(s) to resolve.`);

            // Process each resolvable market
            for (const market of resolvableMarkets) {
                const marketPda = market.publicKey;
                console.log(`Attempting to resolve market: ${marketPda.toBase58()}`);

                try {
                    const txSignature = await program.methods
                        .resolveMarket()
                        .accounts({
                            market: marketPda,
                            keeper: keeperWallet.publicKey,
                            pythPriceFeed: market.account.pythPriceFeed, // Use the Pyth feed stored in the market account
                        })
                        .signers([keeperWallet]) // The keeper must sign
                        .rpc();

                    console.log(`✅ Successfully resolved market ${marketPda.toBase58()}`);
                    console.log(`   Transaction signature: ${txSignature}`);

                } catch (err) {
                    console.error(`❌ Failed to resolve market ${marketPda.toBase58()}:`, err.message);
                }
            }
        } catch (error) {
            console.error("An error occurred in the main loop:", error);
        }
    }, POLLING_INTERVAL_MS);
}

main().catch(err => {
    console.error("Failed to start the keeper bot:", err);
    process.exit(1);
});