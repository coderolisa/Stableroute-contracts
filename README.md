# stableroute-contracts

Soroban smart contracts for [StableRoute](https://github.com/your-org/stableroute) — Stellar liquidity routing protocol.

## What this repo contains

- **StableRouteRouter** — Soroban contract placeholder for routing metadata and route integrity (version, route tags). Production logic will integrate with path payments and liquidity data.

## Security

See **[`SECURITY.md`](SECURITY.md)** for the router's trust model (single
admin, two-step transfer, pause), known limitations, and the responsible
-disclosure process. Report vulnerabilities privately via the StableRoute
Discord — <https://discord.gg/37aCpusvx> — not as public issues.

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

## Error reference (`RouterError`)

Every contract panic surfaces to clients as `Error(Contract, #N)`. The
table below is the authoritative map from code to meaning. Codes are
**append-only**: a variant is never reused or renumbered once shipped, so
integrations can hard-code these numbers safely and only ever need to
learn about *new* (higher) codes. Source of truth: the `RouterError` enum
in [`src/lib.rs`](src/lib.rs).

| Code | Variant | Raised by | Meaning / remedy |
|-----:|---------|-----------|------------------|
| 1 | `AlreadyInitialized` | `init` | Admin already set; the contract is initialized. No action. |
| 2 | `NotInitialized` | every admin-gated entrypoint (`pause`, `set_*`, …) | Admin not set yet — call `init` first. |
| 3 | `SourceEqualsDestination` | `register_pair` | A route's source and destination must differ. |
| 4 | `FeeBpsTooHigh` | `set_pair_fee_bps` | Fee exceeds `MAX_FEE_BPS` (1000 bps = 10%). Lower the fee. |
| 5 | `PairNotRegistered` | `compute_route_fee`, `quote_route` | Register the pair before routing/quoting. |
| 6 | `AmountMustBePositive` | `compute_route_fee`, `quote_route`, `set_pair_liquidity`, `set_pair_min_amount`, `set_pair_max_amount` | Amount/value must be positive (or non-negative where noted). |
| 7 | `NoPendingAdminTransfer` | `accept_admin_transfer` | No handover is pending; nothing to accept. |
| 8 | `NotPendingAdmin` | `accept_admin_transfer` | Caller is not the proposed pending admin. |
| 9 | `ContractPaused` | state-mutating entrypoints (`register_pair`, `set_pair_fee_bps`, …) | Router is paused; retry after `unpause`. |
| 10 | `AmountBelowMin` | `compute_route_fee` | Amount is below the pair's configured minimum. |
| 11 | `AmountAboveMax` | `compute_route_fee` | Amount is above the pair's configured maximum. |
| 12 | `InsufficientLiquidity` | `compute_route_fee` | Reported pair liquidity is below the requested amount. |
| 13 | `MigrationVersionMismatch` | `migrate_v1_to_v2` | Schema is not at v1; migration already applied. |
| 14 | `TimelockNotElapsed` | `accept_admin_transfer` | Governance timelock delay has not elapsed since `propose_admin_transfer`. Wait until the ETA. |
| 15 | `ReentrantCall` | `compute_route_fee` | The reentrancy lock was already held (re-entrant invocation). Should not occur in normal use. |
| 16 | `NotAuthorized` | `set_pair_liquidity` | Caller is neither the admin nor the configured oracle. |
| 17 | `RouteCooldownActive` | `compute_route_fee` | Called again for the pair before its configured cooldown window elapsed. |

> **Maintainers:** when you append a new `RouterError` variant, add a row
> here with the next sequential code. Never edit an existing code/row.

### Registration-first invariant

`register_pair` must be called for `(source, destination)` before any of
its per-pair config setters:

- `set_pair_fee_bps`
- `set_pair_min_amount`
- `set_pair_max_amount`
- `set_pair_liquidity`

Each setter checks `DataKey::Pair(source, destination)` after its own
admin/sign validation and rejects an unregistered (or since-unregistered)
pair with `PairNotRegistered` (#5) — the same error `compute_route_fee`
and `quote_route` already raise. This prevents an admin from writing
fee/bounds/liquidity config for a corridor that was never enabled, which
would otherwise waste storage rent and pollute future pair enumeration.

`unregister_pair` does **not** clear the config slots it leaves behind
(`PairFeeBps`, `PairMinAmount`, `PairMaxAmount`, `PairLiquidity`); a later
`register_pair` for the same pair silently revives the old values. Whether
`unregister_pair` should also clear those slots, or refuse to run while
they're non-default, is a follow-up cleanup question and is out of scope
for the registration guard above.

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
