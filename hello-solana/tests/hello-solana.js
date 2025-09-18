const anchor = require("@coral-xyz/anchor");
const { assert } = require("chai");

describe("hello-solana", () => {
  // Configure the client to use the local cluster.
  const provider = anchor.AnchorProvider.env();
  anchor.setProvider(provider);

  const program = anchor.workspace.HelloSolana;
  
  // Find the PDA for our counter account
  const [counterPDA] = anchor.web3.PublicKey.findProgramAddressSync(
    [Buffer.from("counter")],
    program.programId
  );

  it("Initializes the counter with a value", async () => {
    const initialCount = new anchor.BN(42);

    try {
      await program.methods
        .initialize(initialCount)
        .accounts({
          counter: counterPDA,
          user: provider.wallet.publicKey,
          systemProgram: anchor.web3.SystemProgram.programId,
        })
        .rpc();

      // Fetch the counter account
      const counter = await program.account.counter.fetch(counterPDA);
      
      assert.ok(counter.count.eq(initialCount), "Counter should be initialized with the correct value");
      console.log(`✅ Counter initialized with value: ${counter.count.toString()}`);
    } catch (error) {
      console.error("Error in initialize test:", error);
      throw error;
    }
  });

  it("Increments the counter", async () => {
    try {
      // Get the counter value before increment
      const counterBefore = await program.account.counter.fetch(counterPDA);
      const countBefore = counterBefore.count;

      // Increment the counter
      await program.methods
        .increment()
        .accounts({
          counter: counterPDA,
          authority: provider.wallet.publicKey,
        })
        .rpc();

      // Fetch the counter after increment
      const counterAfter = await program.account.counter.fetch(counterPDA);
      const countAfter = counterAfter.count;
      
      assert.ok(countAfter.eq(countBefore.add(new anchor.BN(1))), "Counter should be incremented by 1");
      console.log(`✅ Counter incremented from ${countBefore.toString()} to ${countAfter.toString()}`);
    } catch (error) {
      console.error("Error in increment test:", error);
      throw error;
    }
  });

  it("Decrements the counter", async () => {
    try {
      // Get the counter value before decrement
      const counterBefore = await program.account.counter.fetch(counterPDA);
      const countBefore = counterBefore.count;

      // Decrement the counter
      await program.methods
        .decrement()
        .accounts({
          counter: counterPDA,
          authority: provider.wallet.publicKey,
        })
        .rpc();

      // Fetch the counter after decrement
      const counterAfter = await program.account.counter.fetch(counterPDA);
      const countAfter = counterAfter.count;
      
      assert.ok(countAfter.eq(countBefore.sub(new anchor.BN(1))), "Counter should be decremented by 1");
      console.log(`✅ Counter decremented from ${countBefore.toString()} to ${countAfter.toString()}`);
    } catch (error) {
      console.error("Error in decrement test:", error);
      throw error;
    }
  });

  it("Resets the counter to zero", async () => {
    try {
      // Reset the counter
      await program.methods
        .reset()
        .accounts({
          counter: counterPDA,
          authority: provider.wallet.publicKey,
        })
        .rpc();

      // Fetch the counter after reset
      const counter = await program.account.counter.fetch(counterPDA);
      
      assert.ok(counter.count.eq(new anchor.BN(0)), "Counter should be reset to 0");
      console.log(`✅ Counter reset to: ${counter.count.toString()}`);
    } catch (error) {
      console.error("Error in reset test:", error);
      throw error;
    }
  });

  it("Fails to decrement counter below zero", async () => {
    try {
      // Try to decrement when counter is at 0
      await program.methods
        .decrement()
        .accounts({
          counter: counterPDA,
          authority: provider.wallet.publicKey,
        })
        .rpc();
      
      // If we reach here, the test should fail
      assert.fail("Expected error when decrementing below zero");
    } catch (error) {
      // Check if the error is the expected custom error
      assert.include(error.message, "CounterUnderflow", "Should throw CounterUnderflow error");
      console.log("✅ Correctly prevented decrement below zero");
    }
  });
});