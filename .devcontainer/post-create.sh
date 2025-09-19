#!/bin/bash

echo "--- Starting Post-Create Setup ---"

# Install essential build tools
sudo apt-get update && sudo apt-get install -y build-essential pkg-config libssl-dev

# Install Rust and Cargo
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"

# Install Solana CLI
sh -c "$(curl -sSfL https://release.solana.com/stable/install)"
export PATH="/home/codespace/.local/share/solana/install/active_release/bin:$PATH"

# Install Anchor Version Manager (avm) and Anchor
cargo install --git https://github.com/coral-xyz/anchor avm --locked --force
avm install 0.30.0
avm use 0.30.0

# Install Node.js dependencies
npm install

echo "--- Post-Create Setup Complete ---"