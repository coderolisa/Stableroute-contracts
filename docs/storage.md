# StableRoute — Storage Model & DataKey Reference

Authoritative reference for the router's on-chain storage
(`src/lib.rs`). Every `DataKey` variant is listed with its key shape, value
type, storage tier, default-when-absent, the entrypoints that read/write
it, and its TTL class. Defaults are cross-checked against the `unwrap_or`
values in the source.

## Sentinel conventions

- An **absent `bool`** reads as `false` (registration, paused).
- **`i128::MAX`** is the "unbounded" sentinel for `PairMaxAmount` and for
  liquidity *inside fee computation*.
- **`0`** is the default for counters, fees, and `PairMinAmount`.
- An **absent `Option`** stays `None` (admin, pending admin, fee recipient,
  last-route timestamp) — distinct from a zero value.

## DataKey table

| DataKey | Key shape | Value | Tier | Default when absent | Read by | Written by |
|---|---|---|---|---|---|---|
| `Admin` | singleton | `Address` | persistent | `None` | `get_admin`, `require_admin` | `init`, `accept_admin_transfer` |
| `PendingAdmin` | singleton | `Address` | persistent | `None` | `get_pending_admin` | `propose_admin_transfer`; removed by `accept`/`cancel` |
| `Paused` | singleton | `bool` | persistent | `false` | `is_paused`, pause gates | `pause`, `unpause` |
| `FeeRecipient` | singleton | `Address` | persistent | `None` | `get_fee_recipient` | `set_fee_recipient` |
| `TotalRoutesAllTime` | singleton | `u64` | persistent | `0` | `get_total_routes_all_time` | `compute_route_fee` |
| `SchemaVersion` | singleton | `u32` | persistent | `1` | `get_schema_version` | `migrate_v1_to_v2` |
| `Pair` | `(Symbol, Symbol)` | `bool` | persistent | `false` | `is_pair_registered`, `is_pair_active`, `get_pair_info`, `compute_route_fee`, `quote_route` | `register_pair`; removed by `unregister_pair` |
| `PairFeeBps` | `(Symbol, Symbol)` | `u32` | persistent | `0` | `get_pair_fee_bps`, `get_pair_info`, compute/quote | `set_pair_fee_bps`; cleared by `unregister_pair` |
| `PairMinAmount` | `(Symbol, Symbol)` | `i128` | persistent | `0` | `get_pair_min_amount`, `get_pair_info`, `compute_route_fee` | `set_pair_min_amount`; cleared by `unregister_pair` |
| `PairMaxAmount` | `(Symbol, Symbol)` | `i128` | persistent | `i128::MAX` | `get_pair_max_amount`, `get_pair_info`, `compute_route_fee` | `set_pair_max_amount`; cleared by `unregister_pair` |
| `PairLiquidity` | `(Symbol, Symbol)` | `i128` | persistent | `0`† | `get_pair_liquidity`, `get_pair_info`, `is_pair_active`, `compute_route_fee`† | `set_pair_liquidity`; cleared by `unregister_pair` |
| `PairLastRouteAt` | `(Symbol, Symbol)` | `u64` | persistent | `None` | `get_pair_last_route_at`, `get_pair_info` (as `0`) | `compute_route_fee` |

† **Liquidity default is context-dependent.** `get_pair_liquidity`,
`get_pair_info`, and `is_pair_active` treat an absent slot as `0`. But
`compute_route_fee` reads it with `unwrap_or(i128::MAX)` — i.e. an
unconfigured pair is treated as having *unbounded* liquidity for routing.
Set an explicit liquidity value to enforce the `InsufficientLiquidity`
(#12) guard.

## Storage tier & TTL

All slots live in **persistent** storage; the contract uses no instance or
temporary storage today (the `DataKey` doc comment reserves instance
storage for future hot config). Persistent entries are subject to state
archival once their TTL lapses: a pair configured long ago but not routed
recently can have its entries archived and must be restored (bumped)
before use. The mitigation is a TTL-extension ("bump") pass on
frequently-read keys; any future TTL-bumping work is the reference
mitigation for this archival risk.

## Versioning

`version()` returns the compiled contract version (`ROUTER_V2`);
`get_schema_version()` returns the persisted storage-layout version
(defaults to `1`, advanced to `2` by `migrate_v1_to_v2`). The two are
independent — see the migration entrypoints in `src/lib.rs`.
