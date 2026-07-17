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
| 14 | `TimelockNotElapsed` | `accept_admin_transfer` | Governance timelock has not elapsed yet; retry after the queued ETA. |
| 15 | `NotAuthorized` | `set_pair_liquidity` | Caller is neither the admin nor the configured oracle. |
| 16 | `ReentrantCall` | `compute_route_fee` | Route accounting was re-entered while locked; retry only after the first call completes. |
| 17 | `RouteCooldownActive` | `compute_route_fee` | Pair cooldown has not elapsed since the previous routed amount. |
| 18 | `BatchTooLarge` | `register_pairs`, `set_pair_fees_bps` | Batch length exceeds `MAX_BATCH_SIZE` (100). Split into smaller batches. |
| 19 | `EmptyBatch` | `register_pairs`, `set_pair_fees_bps` | Batch contains no entries. Provide at least one pair or fee update. |
| 20 | `CooldownTooLarge` | `set_pair_cooldown` | Cooldown exceeds `MAX_COOLDOWN_SECS` (2,592,000 seconds = 30 days). Use a smaller value. |

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

`unregister_pair` also clears the pair's live config slots (`PairFeeBps`,
`PairMinAmount`, `PairMaxAmount`, `PairLiquidity`) before emitting a
`cfg_clr` companion event. Re-registering the same pair therefore starts from
the documented defaults instead of reviving stale fee, bounds, or liquidity
values.

### Per-pair metric lifecycle: preserved by default, explicitly reset on demand

`PairRouteCount`, `PairVolume`, and `PairLastRouteAt` are operational-history
slots tracked separately from live pair configuration. `unregister_pair`
**deliberately does not touch them** — a pair's lifetime route count,
cumulative volume, and last-route timestamp survive an unregister/register
cycle by default. This is existing, unchanged behaviour: unregistering and
re-registering the same corridor keeps its history intact unless you opt
into a reset.

When a re-listed pair should start a fresh operational life instead of
inheriting stale metrics from its previous listing, call the admin-gated
`purge_pair_metrics(source, destination)`. It removes `PairRouteCount`,
`PairVolume`, and `PairLastRouteAt` for the pair and emits a `pair_mrst`
event with `(source, destination)`. It does not touch pair registration or
config (fee/bounds/liquidity) — call `unregister_pair`/`clear_pair_config`
separately for that. `purge_pair_metrics` can be called before or after an
unregister/register cycle, or at any time an admin wants to zero a pair's
history.

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

### Reentrancy guard test coverage

The test suite includes a test-only malicious callback mock that attempts to
re-enter `compute_route_fee` while `DataKey::ReentrancyLock` is held. This
verifies the router rejects nested entry with `ReentrantCall` (#16) and that a
successful route leaves the lock cleared for the next legitimate call.

## Liquidity consumption model

`compute_route_fee` debits the routed `amount` from the pair's stored
`PairLiquidity` on every successful route. This ensures the on-chain
liquidity figure reflects consumption between oracle updates, preventing
repeated routes from exceeding real available liquidity.

### Behaviour

- **Set liquidity:** When an oracle or admin has called
  `set_pair_liquidity`, the stored value is decreased by `amount` via
  saturating subtraction and persisted. A `liq_used` event with
  `(source, destination, remaining_liquidity)` is emitted. The slot TTL is
  extended on each write.
- **Unset liquidity (unbounded sentinel):** When `PairLiquidity` has never
  been written it reads as `i128::MAX` inside `compute_route_fee`. The
  decrement is **skipped entirely** — no storage write and no `liq_used`
  event — preserving the "no oracle configured" behaviour. The public
  getter `get_pair_liquidity` still returns `0` for absent slots.
- **InsufficientLiquidity:** The existing guard (`RouterError::InsufficientLiquidity`,
  code #12) fires when `amount > stored_liquidity`.
- **Oracle top-up:** The oracle (or admin) can replenish liquidity at any
  time via `set_pair_liquidity`. The new value overwrites whatever remains,
  resetting the consumption window.

### Event reference

| Topic | Data | Emitted by | Meaning |
|-------|------|-----------|---------|
| `liq_used` | `(source, destination, remaining_liquidity)` | `compute_route_fee` | Liquidity decremented by routed amount |
| `liq_set` | `(source, destination, liquidity)` | `set_pair_liquidity` | Oracle/admin set/replenished liquidity |

### `compute_route_fee` side-effect matrix

`compute_route_fee` is the only mutating read path. On success it performs three
side effects, each covered by a dedicated test in `src/lib.rs`:

| Side effect | Storage / event | Test |
|-------------|-----------------|------|
| Lifetime counter | `DataKey::TotalRoutesAllTime` (saturating, protocol-wide) | `test_compute_route_fee_counter_is_global_across_pairs` |
| Last-route timestamp | `DataKey::PairLastRouteAt` ← `env.ledger().timestamp()` | `test_compute_route_fee_stamps_pair_last_route_at` |
| Liquidity debit | `DataKey::PairLiquidity` ← `max(0, liquidity - amount)` | `test_liquidity_decremented_by_amount_after_route` |
| Emitted event | topic `route`, data `(source, destination, amount)` | `test_compute_route_fee_emits_route_event_with_payload` |
| Emitted event | topic `liq_used`, data `(source, destination, remaining)` | `test_liq_used_event_emitted_with_remaining` |

`quote_route` is the read-only twin and must perform **none** of these. The
parity guard `test_quote_route_does_not_mutate_counter_or_emit_route_event`
asserts the counter is unchanged and no new `route` event is emitted after a
quote.

The `route_event_payloads` test helper scans the current host event buffer and
returns only the decoded payloads of events whose single topic is `route`.

### Pair lifecycle event and idempotency matrix

Pair lifecycle tests assert the exact one-event payload emitted by each
lifecycle entrypoint before any later contract call refreshes the host event
buffer:

| Entrypoint | Topic | Data payload | Test |
|------------|-------|--------------|------|
| constructor | `init` | `admin` | `test_pair_lifecycle_events_have_exact_payloads_and_counts` |
| `register_pair` | `pair_reg` | `(source, destination)` | `test_pair_lifecycle_events_have_exact_payloads_and_counts` |
| `register_pairs` | `pair_reg` (per entry) | `(source, destination)` (per entry) | `test_register_pairs_happy_path` |
| `set_pair_fee_bps` | `fee_set` | `(source, destination, fee_bps)` | `test_pair_lifecycle_events_have_exact_payloads_and_counts` |
| `set_pair_fees_bps` | `fee_set` (per entry) | `(source, destination, fee_bps)` (per entry) | `test_set_pair_fees_bps_happy_path` |
| `set_pair_liquidity` | `liq_set` | `(source, destination, liquidity)` | `test_pair_lifecycle_events_have_exact_payloads_and_counts` |
| `unregister_pair` | `unreg` | `(source, destination)` | `test_pair_lifecycle_events_have_exact_payloads_and_counts` |
| `unregister_pair` | `cfg_clr` | `(source, destination)` | `test_pair_lifecycle_events_have_exact_payloads_and_counts` |
| `compute_route_fee` | `liq_used` | `(source, destination, remaining_liquidity)` | `test_liq_used_event_emitted_with_remaining` |
| `purge_pair_metrics` | `pair_mrst` | `(source, destination)` | `test_purge_pair_metrics_resets_counters_and_emits_event` |

Two edge-case tests guard idempotency and storage boundaries: unregistering a
never-registered pair stays a clean no-op while still emitting the lifecycle
and config-clear events, and re-registering after unregister restores the pair
with fee, bounds, and liquidity reset to their documented defaults.

Metrics behave differently from config: `test_unregister_then_reregister_preserves_metrics_by_default`
confirms `PairRouteCount`/`PairVolume`/`PairLastRouteAt` survive an
unregister + re-register cycle unchanged, while
`test_purge_pair_metrics_resets_counters_and_emits_event` and
`test_purge_pair_metrics_does_not_touch_registration_or_config` confirm the
explicit `purge_pair_metrics` entrypoint zeroes only those three metrics
slots without disturbing registration or config.

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
