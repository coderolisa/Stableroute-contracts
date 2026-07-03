# StableRoute Router — Entrypoint & Event Reference

Authoritative on-chain ABI for `StableRouteRouter` ([`src/lib.rs`](../src/lib.rs)).
Every public entrypoint and emitted event is listed below, grouped by
subsystem. Error codes referenced here are documented in the
`RouterError` table in the [README](../README.md).

**Auth legend:** _admin_ = `require_admin` (the stored `Admin` must sign) ·
_pending_ = the proposed pending admin must sign · _none_ = no auth.

## Lifecycle

| Entrypoint | Auth | Params | Returns | Errors | Event |
|-----------|------|--------|---------|--------|-------|
| `init` | admin | `admin: Address` | — | `AlreadyInitialized` (#1) | `init(admin)` |
| `version` | none | — | `Symbol` (`ROUTER_V2`) | — | — |
| `get_schema_version` | none | — | `u32` | — | — |
| `migrate_v1_to_v2` | admin | — | — | `NotInitialized` (#2), `MigrationVersionMismatch` (#13) | — |

## Admin / governance

| Entrypoint | Auth | Params | Returns | Errors | Event |
|-----------|------|--------|---------|--------|-------|
| `get_admin` | none | — | `Option<Address>` | — | — |
| `propose_admin_transfer` | admin | `new_admin: Address` | — | `NotInitialized` (#2) | `adm_prop(new_admin)` |
| `accept_admin_transfer` | pending | `caller: Address` | — | `NoPendingAdminTransfer` (#7), `NotPendingAdmin` (#8) | `adm_set(caller)` |
| `cancel_admin_transfer` | admin | — | — | `NotInitialized` (#2) | — |
| `get_pending_admin` | none | — | `Option<Address>` | — | — |

## Pause (emergency stop)

| Entrypoint | Auth | Params | Returns | Errors | Event |
|-----------|------|--------|---------|--------|-------|
| `pause` | admin | — | — | `NotInitialized` (#2) | `paused(true)` |
| `unpause` | admin | — | — | `NotInitialized` (#2) | `paused(false)` |
| `is_paused` | none | — | `bool` | — | — |

## Pairs

| Entrypoint | Auth | Params | Returns | Errors | Event |
|-----------|------|--------|---------|--------|-------|
| `register_pair` | admin | `source: Symbol, destination: Symbol` | — | `ContractPaused` (#9), `NotInitialized` (#2), `SourceEqualsDestination` (#3) | `pair_reg(source, destination)` |
| `unregister_pair` | admin | `source: Symbol, destination: Symbol` | — | `NotInitialized` (#2) | `unreg(source, destination)`, `cfg_clr(source, destination)` |
| `is_pair_registered` | none | `source: Symbol, destination: Symbol` | `bool` | — | — |
| `is_pair_active` | none | `source: Symbol, destination: Symbol` | `bool` | — | — |
| `get_pair_info` | none | `source: Symbol, destination: Symbol` | `PairInfo` | — | — |

## Fees

| Entrypoint | Auth | Params | Returns | Errors | Event |
|-----------|------|--------|---------|--------|-------|
| `set_pair_fee_bps` | admin | `source: Symbol, destination: Symbol, fee_bps: u32` | — | `ContractPaused` (#9), `NotInitialized` (#2), `FeeBpsTooHigh` (#4) | `fee_set(source, destination, fee_bps)` |
| `get_pair_fee_bps` | none | `source: Symbol, destination: Symbol` | `u32` | — | — |
| `set_fee_recipient` | admin | `recipient: Address` | — | `NotInitialized` (#2) | — |
| `get_fee_recipient` | none | — | `Option<Address>` | — | — |

## Bounds & liquidity

| Entrypoint | Auth | Params | Returns | Errors | Event |
|-----------|------|--------|---------|--------|-------|
| `set_pair_min_amount` | admin | `source, destination: Symbol, min_amount: i128` | — | `NotInitialized` (#2), `AmountMustBePositive` (#6) | — |
| `get_pair_min_amount` | none | `source, destination: Symbol` | `i128` | — | — |
| `set_pair_max_amount` | admin | `source, destination: Symbol, max_amount: i128` | — | `NotInitialized` (#2), `AmountMustBePositive` (#6) | — |
| `get_pair_max_amount` | none | `source, destination: Symbol` | `i128` | — | — |
| `set_pair_liquidity` | admin | `source, destination: Symbol, liquidity: i128` | — | `NotInitialized` (#2), `AmountMustBePositive` (#6) | `liq_set(source, destination, liquidity)` |
| `get_pair_liquidity` | none | `source, destination: Symbol` | `i128` | — | — |

## Routing

| Entrypoint | Auth | Params | Returns | Errors | Event |
|-----------|------|--------|---------|--------|-------|
| `compute_route_fee` | none | `source, destination: Symbol, amount: i128` | `i128` (fee) | `AmountMustBePositive` (#6), `PairNotRegistered` (#5), `AmountBelowMin` (#10), `AmountAboveMax` (#11), `InsufficientLiquidity` (#12) | `route(source, destination, amount)` |
| `quote_route` | none | `source, destination: Symbol, amount: i128` | `(i128 fee, i128 net)` | `AmountMustBePositive` (#6), `PairNotRegistered` (#5) | — |
| `get_pair_last_route_at` | none | `source, destination: Symbol` | `Option<u64>` | — | — |
| `get_total_routes_all_time` | none | — | `u64` | — | — |
| `route_tag` | none | `source, destination: Symbol` | `(Symbol, Symbol)` | — | — |

## Event catalog

Every event is published with a single `symbol_short!` topic and a data
payload tuple. Topic symbols are capped at 9 characters.

| Topic | Payload | Emitted by |
|-------|---------|-----------|
| `init` | `admin: Address` | `init` |
| `adm_prop` | `new_admin: Address` | `propose_admin_transfer` |
| `adm_set` | `caller: Address` | `accept_admin_transfer` |
| `paused` | `bool` | `pause` / `unpause` |
| `pair_reg` | `(source, destination): (Symbol, Symbol)` | `register_pair` |
| `unreg` | `(source, destination): (Symbol, Symbol)` | `unregister_pair` |
| `cfg_clr` | `(source, destination): (Symbol, Symbol)` | `unregister_pair` |
| `fee_set` | `(source, destination, fee_bps): (Symbol, Symbol, u32)` | `set_pair_fee_bps` |
| `liq_set` | `(source, destination, liquidity): (Symbol, Symbol, i128)` | `set_pair_liquidity` |
| `route` | `(source, destination, amount): (Symbol, Symbol, i128)` | `compute_route_fee` |

> Keep this catalog in sync with the `symbol_short!(...)` calls in
> `src/lib.rs` whenever an entrypoint or event is added or changed.
