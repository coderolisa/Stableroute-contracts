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

See [CONTRIBUTING.md](CONTRIBUTING.md) for the contract conventions (error
numbering, event-topic limits, admin-auth and pause patterns, storage/TTL
tiers) and the PR checklist.

1. Fork the repo and create a branch from `main`.
2. Make changes; keep formatting, linting, tests, WASM build, and coverage passing.
3. Open a PR; CI must be green.
4. Follow the project’s code style (enforced by `rustfmt`).

### Internal helper conventions

**`require_admin`** — every admin-gated entrypoint in `StableRouteRouter` calls the private `fn require_admin(env: &Env) -> Address` helper instead of repeating the load-unwrap-require_auth block inline. When adding a new admin-gated entrypoint, start the body with `Self::require_admin(&env);`. Do not duplicate the pattern manually.

## Testing notes

### `compute_route_fee` side-effect matrix

`compute_route_fee` is the only mutating read path. On success it performs three
side effects, each covered by a dedicated test in `src/lib.rs`:

| Side effect | Storage / event | Test |
|-------------|-----------------|------|
| Lifetime counter | `DataKey::TotalRoutesAllTime` (saturating, protocol-wide) | `test_compute_route_fee_counter_is_global_across_pairs` |
| Last-route timestamp | `DataKey::PairLastRouteAt` ← `env.ledger().timestamp()` | `test_compute_route_fee_stamps_pair_last_route_at` |
| Emitted event | topic `route`, data `(source, destination, amount)` | `test_compute_route_fee_emits_route_event_with_payload` |

`quote_route` is the read-only twin and must perform **none** of these. The
parity guard `test_quote_route_does_not_mutate_counter_or_emit_route_event`
asserts the counter is unchanged and no new `route` event is emitted after a
quote.

The `route_event_payloads` test helper scans the accumulated host events
(init / register / fee_set all emit too) and returns only the decoded payloads
of events whose single topic is `route`.

## License

MIT
