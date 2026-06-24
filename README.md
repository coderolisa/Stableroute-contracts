# stableroute-contracts

Soroban smart contracts for [StableRoute](https://github.com/your-org/stableroute) — Stellar liquidity routing protocol.

## What this repo contains

- **StableRouteRouter** — Soroban contract placeholder for routing metadata and route integrity (version, route tags). Production logic will integrate with path payments and liquidity data.

## Route identifiers (`route_tag`)

`route_tag(source, destination)` returns a deterministic 32-byte identifier
(`BytesN<32>`) for a routing leg, computed on-chain via
`keccak256(xdr(source) || xdr(destination))`.

- **Deterministic** — identical `(source, destination)` inputs always yield the
  same tag. The off-chain backend can recompute the tag with the same encoding
  and correlate on-chain routes without persisting a lookup table.
- **Direction-sensitive** — `source` is hashed before `destination`, so
  `route_tag(USDC, EURC)` and `route_tag(EURC, USDC)` are different identifiers.
  Each direction of a pair has its own tag.
- **Collision-resistant** — keccak256 provides cryptographic collision
  resistance, so distinct pairs map to distinct tags with overwhelming
  probability.

> Note: `route_tag` previously returned `(Symbol, Symbol)` (an echo of its
> inputs). It now returns `BytesN<32>`. This is an intentional breaking change
> to the contract ABI.

## Prerequisites

- [Rust](https://rustup.rs/) (stable, with `rustfmt`)
- Optional: [Soroban CLI](https://soroban.stellar.org/docs/tools/cli) for deployment

## Setup (contributors)

1. Clone the repo and enter the directory:
   ```bash
   git clone <repo-url> && cd stableroute-contracts
   ```
2. Install Rust (if needed):
   ```bash
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
   rustup component add rustfmt clippy
   rustup target add wasm32-unknown-unknown
   cargo install cargo-llvm-cov
   ```
3. Build and test:
   ```bash
   cargo build
   cargo clippy --all-targets -- -D warnings
   cargo test
   cargo build --target wasm32-unknown-unknown --release
   cargo llvm-cov --all-targets --fail-under-lines 95
   ```
4. Check formatting:
   ```bash
   cargo fmt --all -- --check
   ```

## Commands

| Command | Description |
|--------|-------------|
| `cargo build` | Build the contracts |
| `cargo test` | Run unit tests |
| `cargo clippy --all-targets -- -D warnings` | Treat Rust lints and warnings as CI failures |
| `cargo build --target wasm32-unknown-unknown --release` | Build the deployable Soroban WASM artifact |
| `cargo llvm-cov --all-targets --fail-under-lines 95` | Report coverage and fail below 95 percent line coverage |
| `cargo fmt --all` | Format code |
| `cargo fmt --all -- --check` | CI: verify formatting |

## CI/CD

On every push/PR to `main`, GitHub Actions runs:

- `cargo fmt --all -- --check`
- `cargo build`
- `cargo clippy --all-targets -- -D warnings`
- `cargo test`
- `cargo build --target wasm32-unknown-unknown --release`
- `cargo llvm-cov --all-targets --fail-under-lines 95`

Ensure these pass locally before pushing.

## Contributing

1. Fork the repo and create a branch from `main`.
2. Make changes; keep formatting, linting, tests, WASM build, and coverage passing.
3. Open a PR; CI must be green.
4. Follow the project’s code style (enforced by `rustfmt`).

### Internal helper conventions

**`require_admin`** — every admin-gated entrypoint in `StableRouteRouter` calls the private `fn require_admin(env: &Env) -> Address` helper instead of repeating the load-unwrap-require_auth block inline. When adding a new admin-gated entrypoint, start the body with `Self::require_admin(&env);`. Do not duplicate the pattern manually.

## Rate limiting: per-pair route cooldown

`StableRouteRouter` supports an optional, per-pair rate limit on
`compute_route_fee` to dampen route spam and front-running pressure.

- **Configuration** — the admin sets a cooldown (in seconds) per pair with
  `set_pair_cooldown(source, destination, cooldown_secs)` and reads it back
  with `get_pair_cooldown(source, destination)`. The default is `0`, which
  **disables** the rate limit for the pair.
- **Enforcement** — when a non-zero cooldown is configured,
  `compute_route_fee` rejects a call with `RouteCooldownActive` (error #14)
  if it arrives before `cooldown_secs` have elapsed since the pair's last
  successful route. Concretely, a call at ledger time `now` is rejected when
  `now < last_route_at + cooldown_secs`. The boundary is inclusive: a call
  exactly at `last_route_at + cooldown_secs` is allowed.
- **First route** — the first route for a pair (no recorded
  `PairLastRouteAt`) is always allowed, regardless of the cooldown.
- **Scope** — the limit is per `(source, destination)` pair and is keyed off
  the existing `PairLastRouteAt` timestamp, so it adds no new write on the
  hot path beyond the timestamp that was already being stamped.

## License

MIT
