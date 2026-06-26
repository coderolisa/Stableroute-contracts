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
| 14 | `TimelockNotElapsed` | `accept_admin_transfer` | Governance timelock has not yet elapsed. |
| 15 | `NotAuthorized` | `set_pair_liquidity` | Caller is neither admin nor the configured oracle. |
| 16 | `ReentrantCall` | `compute_route_fee` | Re-entrant invocation detected; lock already held. |
| 17 | `RouteCooldownActive` | `compute_route_fee` | Pair rate-limited; cooldown not yet elapsed. |

> **Maintainers:** when you append a new `RouterError` variant, add a row
> here with the next sequential code. Never edit an existing code/row.

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

## Upgrades

The router supports in-place WASM upgrades via the admin-gated `upgrade` entrypoint,
so bug fixes can be deployed without losing pair state, admin configuration, or
route history.

**Flow:**

1. Build the new WASM artifact:
   ```bash
   cargo build --target wasm32-unknown-unknown --release
   ```
2. Install the WASM on-chain and obtain its hash:
   ```bash
   soroban lab build \
     --copy-to target/wasm32-unknown-unknown/release/stableroute_contracts.wasm

   soroban contract install \
     --source <admin-key> \
     --network <network> \
     --wasm target/wasm32-unknown-unknown/release/stableroute_contracts.wasm
   ```
   The command prints a `BytesN<32>` WASM hash (e.g. `cafebabe...`).

3. Call the `upgrade` entrypoint as the admin:
   ```bash
   soroban contract invoke \
     --source <admin-key> \
     --network <network> \
     --id <contract-id> \
     -- \
     upgrade \
     --new_wasm_hash cafebabe...
   ```

**Security notes:**

- Only the admin (`DataKey::Admin`) can call `upgrade`; the entrypoint uses
  `require_admin` and will panic with `NotInitialized` (#2) if the contract
  has not been initialised.
- The call emits an `upgraded` event carrying the new WASM hash, providing a
  censorable audit trail for indexers and off-chain watchers.
- Storage (`DataKey` slots) is preserved across the upgrade — the admin, all
  registered pairs, fees, liquidity reports, route counters, and configuration
  survive the WASM replacement.
- `upgrade` is intentionally **not** paused-gated: the admin should be able to
  fix a bug even while the contract is emergency-stopped. The admin can always
  unpause, so there is no escalation path through this exception.

## License

MIT
