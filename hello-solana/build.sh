#!/bin/bash

# Set up environment paths
export PATH="/home/runner/.local/share/solana/install/active_release/bin:/home/runner/workspace/.local/share/.cargo/bin:$PATH"
. "/home/runner/workspace/.local/share/.cargo/env" 2>/dev/null || true

echo "Building Solana program..."
echo "Solana CLI version: $(solana --version)"
echo "Anchor CLI version: $(anchor --version)"

# Build the program
anchor build

echo "Build completed!"

# Check if build was successful
if [ -f "target/deploy/hello_solana.so" ]; then
    echo "✅ Program binary created: target/deploy/hello_solana.so"
else
    echo "⚠️ Program binary not found, but build artifacts are present."
fi

# Show program ID
echo "Program ID: $(solana-keygen pubkey target/deploy/hello_solana-keypair.json 2>/dev/null || echo 'Keypair not found')"