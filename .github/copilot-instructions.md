# Copilot Instructions for flash-market

## Project Overview
- **flash-market** is a Rust-based Solana program (on-chain smart contract) with supporting TypeScript scripts and tests.
- The main on-chain logic is in `programs/flash_pred/` (see `src/lib.rs`).
- Off-chain scripts (keepers, tests) are in `scripts/` and `tests/`.

## Architecture & Data Flow
- **On-chain:**
  - Rust Solana program in `programs/flash_pred/`.
  - Entry point: `src/lib.rs` defines program logic, instruction handlers, and state.
- **Off-chain:**
  - TypeScript scripts in `scripts/` interact with the Solana program (e.g., `keeper.ts`).
  - Tests in `tests/` (e.g., `flash_pred.ts`) use Anchor/TypeScript to test on-chain logic.

## Build & Test Workflows
- **Build Solana program:**
  - From repo root: `anchor build` or `cargo build-bpf --manifest-path programs/flash_pred/Cargo.toml`
- **Run tests:**
  - TypeScript tests: `anchor test` (runs tests in `tests/`)
  - Individual test: `npx ts-node tests/flash_pred.ts`
- **Deploy:**
  - Use `anchor deploy` for deploying to localnet/devnet.

## Conventions & Patterns
- **Rust:**
  - Use Anchor framework macros (`#[program]`, `#[derive(Accounts)]`, etc.)
  - State and instruction handlers are grouped in `lib.rs`.
- **TypeScript:**
  - Use Anchor's generated IDL for program interaction.
  - Scripts/tests import program IDL and use AnchorProvider.
- **Migrations:**
  - Use `migrations/` for Anchor migration scripts if needed.

## Integration Points
- **Solana/Anchor:**
  - Relies on Anchor CLI and Solana toolchain.
  - `Anchor.toml` configures cluster, program, and test settings.
- **External:**
  - May depend on Solana devnet/mainnet and external oracles (see scripts).

## Key Files & Directories
- `programs/flash_pred/src/lib.rs`: Main on-chain logic
- `scripts/keeper.ts`: Off-chain keeper script
- `tests/flash_pred.ts`: Main test suite
- `Anchor.toml`: Anchor project config
- `Cargo.toml`: Rust dependencies

## Example Patterns
- **Instruction handler:**
  ```rust
  #[program]
  pub mod flash_pred { ... }
  ```
- **TypeScript test setup:**
  ```ts
  const provider = AnchorProvider.env();
  anchor.setProvider(provider);
  ```

## Tips for AI Agents
- Always check `Anchor.toml` and `Cargo.toml` for config and dependencies.
- When adding new instructions, update both Rust and TypeScript IDL usage.
- Use Anchor macros and patterns for new state/instruction definitions.
- For new scripts/tests, follow the structure in `scripts/` and `tests/`.
