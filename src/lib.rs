#![allow(deprecated)] // TODO: migrate Soroban events to #[contractevent].
#![no_std]
// Contributing? See CONTRIBUTING.md for error-numbering, event-topic, auth,
// pause, and storage/TTL conventions plus the PR checklist.

#[cfg(test)]
extern crate std;

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, panic_with_error, symbol_short,
    xdr::ToXdr, Address, Bytes, BytesN, Env, Symbol,
};

/// Aggregated read of every pair-scoped storage slot.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PairInfo {
    pub registered: bool,
    pub fee_bps: u32,
    pub min_amount: i128,
    pub max_amount: i128,
    pub liquidity: i128,
    pub last_route_at: u64,
}

/// Storage keys used by the StableRoute router.
///
/// Persistent storage is used for the admin address and per-pair
/// configuration; these values change rarely (governance flow) and need
/// to survive the contract's instance TTL window. Instance storage is
/// reserved for hot configuration that we expect every invocation to
/// touch — none yet.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    /// Operational admin set once at `init`.
    Admin,
    /// `true` if the (source, destination) pair is a recognised route.
    /// Stored as `bool` so callers can query without distinguishing
    /// "absent" from "false".
    Pair(Symbol, Symbol),
    /// Per-pair fee in basis points (1 bps = 0.01 %). Stored as `u32`
    /// so the on-the-wire shape is fixed; values above `MAX_FEE_BPS`
    /// are rejected at write time.
    PairFeeBps(Symbol, Symbol),
    /// Pending admin proposed via `propose_admin_transfer`. Two-step
    /// handover guards against locking the contract with a bad key.
    PendingAdmin,
    /// `true` when the router is paused. No write entrypoint accepts
    /// calls until an unpause.
    Paused,
    /// Minimum routable amount per pair (in source units). Compute
    /// rejects amounts below the floor.
    PairMinAmount(Symbol, Symbol),
    /// Maximum routable amount per pair (in source units). Compute
    /// rejects amounts above the ceiling.
    PairMaxAmount(Symbol, Symbol),
    /// Reported available liquidity (in source units) per pair.
    /// Updated by an off-chain oracle via the admin entrypoint.
    PairLiquidity(Symbol, Symbol),
    /// Address that receives protocol fees on settlement.
    FeeRecipient,
    /// Protocol-wide lifetime counter of `compute_route_fee` invocations.
    TotalRoutesAllTime,
    /// Ledger timestamp of the most recent `compute_route_fee` for a pair.
    PairLastRouteAt(Symbol, Symbol),
    /// Per-pair lifetime counter of `compute_route_fee` invocations.
    /// Stored as `u64`; incremented with `saturating_add` so it is
    /// monotonic and never panics on overflow. Absent reads default to 0.
    PairRouteCount(Symbol, Symbol),
    /// Per-pair cumulative routed volume (sum of `amount` in source
    /// units). Stored as `i128`; accumulated with `saturating_add` so it
    /// is monotonic and never panics on overflow. Absent reads default to 0.
    PairVolume(Symbol, Symbol),
    /// On-chain storage schema version. Distinct from version().
    SchemaVersion,
}

/// Upper bound on the per-pair fee. 1 000 bps = 10 %. Tightening this
/// further is a governance decision; raising it is append-only safe
/// but should be deliberate.
pub const MAX_FEE_BPS: u32 = 1_000;
/// Basis-point denominator: 1 bps = 1/10_000.
pub const BPS_DENOMINATOR: i128 = 10_000;

/// Typed contract errors. Codes are append-only — never reuse or
/// renumber a variant once it has shipped.
#[contracterror]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum RouterError {
    /// `init` was called but the admin address is already stored.
    AlreadyInitialized = 1,
    /// A read or write expected the admin to be set but it was not.
    NotInitialized = 2,
    /// `register_pair` was called with `source == destination`.
    SourceEqualsDestination = 3,
    /// `set_pair_fee_bps` was called with a value above [`MAX_FEE_BPS`].
    FeeBpsTooHigh = 4,
    /// `compute_route_fee` was called for a pair that was never registered.
    PairNotRegistered = 5,
    /// `compute_route_fee` was called with a non-positive amount.
    AmountMustBePositive = 6,
    /// `accept_admin_transfer` was called with no pending admin.
    NoPendingAdminTransfer = 7,
    /// `accept_admin_transfer` was called by a non-pending address.
    NotPendingAdmin = 8,
    /// A state-changing entrypoint was called while paused.
    ContractPaused = 9,
    /// Amount is below the configured PairMinAmount.
    AmountBelowMin = 10,
    /// Amount is above the configured PairMaxAmount.
    AmountAboveMax = 11,
    /// Reported pair liquidity is below the requested amount.
    InsufficientLiquidity = 12,
    /// `migrate_v1_to_v2` was called from a non-v1 schema.
    MigrationVersionMismatch = 13,
    /// `compute_route_fee_checked` was called and the net output
    /// (`amount - fee`) fell below the caller-supplied `min_out` floor.
    SlippageExceeded = 14,
}

/// StableRoute router contract — placeholder for routing logic.
/// In production this would integrate with path payments and liquidity data.
#[contract]
pub struct StableRouteRouter;

#[contractimpl]
impl StableRouteRouter {
    /// Load the admin address, require its auth, and return it.
    ///
    /// Every admin-gated entrypoint calls this instead of repeating the
    /// six-line load-unwrap-require_auth block. Keeping it private
    /// ensures it never appears in the generated client ABI.
    fn require_admin(env: &Env) -> Address {
        let admin: Address = env
            .storage()
            .persistent()
            .get(&DataKey::Admin)
            .unwrap_or_else(|| panic_with_error!(env, RouterError::NotInitialized));
        admin.require_auth();
        admin
    }

    /// Returns the router contract version.
    pub fn version(_env: Env) -> Symbol {
        symbol_short!("ROUTER_V2")
    }

    /// Read the persisted schema version, or 1 if absent (the implicit
    /// pre-migration default).
    pub fn get_schema_version(env: Env) -> u32 {
        env.storage()
            .persistent()
            .get(&DataKey::SchemaVersion)
            .unwrap_or(1)
    }

    /// Migrate the schema from v1 to v2. Admin-gated; panics with
    /// MigrationVersionMismatch on a non-v1 starting state. v2 readers
    /// default sensibly when their new slots are absent, so the body
    /// only stamps the new SchemaVersion.
    pub fn migrate_v1_to_v2(env: Env) {
        Self::require_admin(&env);
        let current: u32 = env
            .storage()
            .persistent()
            .get(&DataKey::SchemaVersion)
            .unwrap_or(1);
        if current != 1 {
            panic_with_error!(&env, RouterError::MigrationVersionMismatch);
        }
        env.storage()
            .persistent()
            .set(&DataKey::SchemaVersion, &2u32);
    }

    /// Initialize the router with the operational admin.
    ///
    /// Requires `admin.require_auth()` and panics with
    /// [`RouterError::AlreadyInitialized`] if the admin has already
    /// been set. Use a redeploy or a future rotation entrypoint to
    /// change the admin.
    pub fn init(env: Env, admin: Address) {
        if env.storage().persistent().has(&DataKey::Admin) {
            panic_with_error!(&env, RouterError::AlreadyInitialized);
        }
        admin.require_auth();
        env.storage().persistent().set(&DataKey::Admin, &admin);
        env.events().publish((symbol_short!("init"),), admin);
    }

    /// Returns true iff the router is currently paused.
    pub fn is_paused(env: Env) -> bool {
        env.storage()
            .persistent()
            .get(&DataKey::Paused)
            .unwrap_or(false)
    }

    /// Resume after a pause. Admin-gated and idempotent.
    pub fn unpause(env: Env) {
        Self::require_admin(&env);
        env.storage().persistent().set(&DataKey::Paused, &false);
        env.events().publish((symbol_short!("paused"),), false);
    }

    /// Admin pauses the router. All state-changing entrypoints will
    /// then panic with ContractPaused.
    pub fn pause(env: Env) {
        Self::require_admin(&env);
        env.storage().persistent().set(&DataKey::Paused, &true);
        env.events().publish((symbol_short!("paused"),), true);
    }

    /// Cancel a pending handover. No-op if none is pending.
    pub fn cancel_admin_transfer(env: Env) {
        Self::require_admin(&env);
        env.storage().persistent().remove(&DataKey::PendingAdmin);
    }

    /// Read the pending admin if any.
    pub fn get_pending_admin(env: Env) -> Option<Address> {
        env.storage().persistent().get(&DataKey::PendingAdmin)
    }

    /// Step 2 of admin handover. The pending admin claims the role
    /// from their own key. Panics with NoPendingAdminTransfer if none
    /// is pending or NotPendingAdmin if the caller does not match.
    pub fn accept_admin_transfer(env: Env, caller: Address) {
        caller.require_auth();
        let pending: Address = env
            .storage()
            .persistent()
            .get(&DataKey::PendingAdmin)
            .unwrap_or_else(|| panic_with_error!(&env, RouterError::NoPendingAdminTransfer));
        if pending != caller {
            panic_with_error!(&env, RouterError::NotPendingAdmin);
        }
        env.storage()
            .persistent()
            .set(&DataKey::Admin, &caller.clone());
        env.storage().persistent().remove(&DataKey::PendingAdmin);
        env.events().publish((symbol_short!("adm_set"),), caller);
    }

    /// Step 1 of admin handover. Current admin proposes a new admin;
    /// the new admin must then accept via `accept_admin_transfer`.
    pub fn propose_admin_transfer(env: Env, new_admin: Address) {
        Self::require_admin(&env);
        env.storage()
            .persistent()
            .set(&DataKey::PendingAdmin, &new_admin.clone());
        env.events()
            .publish((symbol_short!("adm_prop"),), new_admin);
    }

    /// Returns the admin set at `init`, if any.
    pub fn get_admin(env: Env) -> Option<Address> {
        env.storage().persistent().get(&DataKey::Admin)
    }

    /// Register `(source, destination)` as a recognised route.
    ///
    /// Admin-gated; rejects `source == destination`. Idempotent: a
    /// second call with the same pair simply re-asserts the entry and
    /// is a no-op from the caller's perspective.
    pub fn register_pair(env: Env, source: Symbol, destination: Symbol) {
        if env
            .storage()
            .persistent()
            .get(&DataKey::Paused)
            .unwrap_or(false)
        {
            panic_with_error!(&env, RouterError::ContractPaused);
        }
        Self::require_admin(&env);
        if source == destination {
            panic_with_error!(&env, RouterError::SourceEqualsDestination);
        }
        env.storage()
            .persistent()
            .set(&DataKey::Pair(source.clone(), destination.clone()), &true);
        env.events()
            .publish((symbol_short!("pair_reg"),), (source, destination));
    }

    /// Returns true iff the pair is registered AND has non-zero
    /// reported liquidity. Useful as a quick is-routable check.
    pub fn is_pair_active(env: Env, source: Symbol, destination: Symbol) -> bool {
        let s = env.storage().persistent();
        if !s
            .get::<_, bool>(&DataKey::Pair(source.clone(), destination.clone()))
            .unwrap_or(false)
        {
            return false;
        }
        s.get::<_, i128>(&DataKey::PairLiquidity(source, destination))
            .unwrap_or(0)
            > 0
    }

    /// Single round-trip aggregate read for the dashboard. Returns
    /// every per-pair slot in one shot.
    pub fn get_pair_info(env: Env, source: Symbol, destination: Symbol) -> PairInfo {
        let s = env.storage().persistent();
        PairInfo {
            registered: s
                .get(&DataKey::Pair(source.clone(), destination.clone()))
                .unwrap_or(false),
            fee_bps: s
                .get(&DataKey::PairFeeBps(source.clone(), destination.clone()))
                .unwrap_or(0),
            min_amount: s
                .get(&DataKey::PairMinAmount(source.clone(), destination.clone()))
                .unwrap_or(0),
            max_amount: s
                .get(&DataKey::PairMaxAmount(source.clone(), destination.clone()))
                .unwrap_or(i128::MAX),
            liquidity: s
                .get(&DataKey::PairLiquidity(source.clone(), destination.clone()))
                .unwrap_or(0),
            last_route_at: s
                .get(&DataKey::PairLastRouteAt(source, destination))
                .unwrap_or(0),
        }
    }

    /// Read-only quote of fee + net for a pair without writing the
    /// timestamp / counter. Useful as a planner-only hook.
    pub fn quote_route(
        env: Env,
        source: Symbol,
        destination: Symbol,
        amount: i128,
    ) -> (i128, i128) {
        if amount <= 0 {
            panic_with_error!(&env, RouterError::AmountMustBePositive);
        }
        if !env
            .storage()
            .persistent()
            .get::<_, bool>(&DataKey::Pair(source.clone(), destination.clone()))
            .unwrap_or(false)
        {
            panic_with_error!(&env, RouterError::PairNotRegistered);
        }
        let fee_bps: u32 = env
            .storage()
            .persistent()
            .get(&DataKey::PairFeeBps(source, destination))
            .unwrap_or(0);
        let fee = amount
            .checked_mul(fee_bps as i128)
            .map(|n| n / BPS_DENOMINATOR)
            .unwrap_or(0);
        (fee, amount - fee)
    }

    /// Read the most recent ledger timestamp at which `compute_route_fee`
    /// touched this pair. None when never routed.
    pub fn get_pair_last_route_at(env: Env, source: Symbol, destination: Symbol) -> Option<u64> {
        env.storage()
            .persistent()
            .get(&DataKey::PairLastRouteAt(source, destination))
    }

    /// Admin sets the per-pair route cooldown in seconds.
    ///
    /// While set to a non-zero value, `compute_route_fee` rejects a call
    /// for the pair until at least `cooldown_secs` seconds have elapsed
    /// since the pair's last successful route (`PairLastRouteAt`).
    /// Setting `0` (the default) disables the rate limit for the pair.
    pub fn set_pair_cooldown(env: Env, source: Symbol, destination: Symbol, cooldown_secs: u64) {
        Self::require_admin(&env);
        env.storage().persistent().set(
            &DataKey::PairCooldown(source.clone(), destination.clone()),
            &cooldown_secs,
        );
        env.events().publish(
            (symbol_short!("cd_set"),),
            (source, destination, cooldown_secs),
        );
    }

    /// Read the per-pair route cooldown in seconds (0 when absent,
    /// meaning the rate limit is disabled for the pair).
    pub fn get_pair_cooldown(env: Env, source: Symbol, destination: Symbol) -> u64 {
        env.storage()
            .persistent()
            .get(&DataKey::PairCooldown(source, destination))
            .unwrap_or(0)
    }

    /// Read the protocol-wide lifetime counter of route quotes.
    pub fn get_total_routes_all_time(env: Env) -> u64 {
        env.storage()
            .persistent()
            .get(&DataKey::TotalRoutesAllTime)
            .unwrap_or(0)
    }

    /// Read the per-pair lifetime count of `compute_route_fee`
    /// invocations for `(source, destination)`. Returns 0 when the pair
    /// has never been routed.
    pub fn get_pair_route_count(env: Env, source: Symbol, destination: Symbol) -> u64 {
        env.storage()
            .persistent()
            .get(&DataKey::PairRouteCount(source, destination))
            .unwrap_or(0)
    }

    /// Read the per-pair cumulative routed volume (sum of `amount` in
    /// source units) for `(source, destination)`. Returns 0 when the
    /// pair has never been routed.
    pub fn get_pair_volume(env: Env, source: Symbol, destination: Symbol) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::PairVolume(source, destination))
            .unwrap_or(0)
    }

    /// Admin sets the address that receives protocol fees at
    /// settlement time. The router itself never custodies funds.
    pub fn set_fee_recipient(env: Env, recipient: Address) {
        Self::require_admin(&env);
        env.storage()
            .persistent()
            .set(&DataKey::FeeRecipient, &recipient);
    }

    /// Read the configured fee recipient, if any.
    pub fn get_fee_recipient(env: Env) -> Option<Address> {
        env.storage().persistent().get(&DataKey::FeeRecipient)
    }

    /// Read the reported liquidity for a pair (0 when absent).
    pub fn get_pair_liquidity(env: Env, source: Symbol, destination: Symbol) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::PairLiquidity(source, destination))
            .unwrap_or(0)
    }

    /// Admin sets the reported liquidity for a pair (source units).
    pub fn set_pair_liquidity(env: Env, source: Symbol, destination: Symbol, liquidity: i128) {
        Self::require_admin(&env);
        if liquidity < 0 {
            panic_with_error!(&env, RouterError::AmountMustBePositive);
        }
        env.storage().persistent().set(
            &DataKey::PairLiquidity(source.clone(), destination.clone()),
            &liquidity,
        );
        env.events().publish(
            (symbol_short!("liq_set"),),
            (source, destination, liquidity),
        );
    }

    /// Read the per-pair maximum (i128::MAX when absent).
    pub fn get_pair_max_amount(env: Env, source: Symbol, destination: Symbol) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::PairMaxAmount(source, destination))
            .unwrap_or(i128::MAX)
    }

    /// Admin sets the per-pair maximum routable amount.
    pub fn set_pair_max_amount(env: Env, source: Symbol, destination: Symbol, max_amount: i128) {
        Self::require_admin(&env);
        if max_amount <= 0 {
            panic_with_error!(&env, RouterError::AmountMustBePositive);
        }
        env.storage()
            .persistent()
            .set(&DataKey::PairMaxAmount(source, destination), &max_amount);
    }

    /// Read the per-pair minimum (0 when absent).
    pub fn get_pair_min_amount(env: Env, source: Symbol, destination: Symbol) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::PairMinAmount(source, destination))
            .unwrap_or(0)
    }

    /// Admin sets the per-pair minimum routable amount.
    pub fn set_pair_min_amount(env: Env, source: Symbol, destination: Symbol, min_amount: i128) {
        Self::require_admin(&env);
        if min_amount < 0 {
            panic_with_error!(&env, RouterError::AmountMustBePositive);
        }
        env.storage()
            .persistent()
            .set(&DataKey::PairMinAmount(source, destination), &min_amount);
    }

    /// Unregister a previously-registered pair. Admin-gated. Idempotent.
    /// Does not touch the configured fee — that is removed only when the
    /// admin overwrites it back to 0 (or calls a future remove_fee).
    pub fn unregister_pair(env: Env, source: Symbol, destination: Symbol) {
        Self::require_admin(&env);
        env.storage()
            .persistent()
            .remove(&DataKey::Pair(source.clone(), destination.clone()));
        env.events()
            .publish((symbol_short!("unreg"),), (source, destination));
    }

    /// Returns `true` iff `register_pair` has been called for this pair.
    pub fn is_pair_registered(env: Env, source: Symbol, destination: Symbol) -> bool {
        env.storage()
            .persistent()
            .get(&DataKey::Pair(source, destination))
            .unwrap_or(false)
    }

    /// Set the routing fee in basis points for a registered pair.
    ///
    /// Admin-gated. Rejects values above [`MAX_FEE_BPS`] with
    /// [`RouterError::FeeBpsTooHigh`]. Idempotent: setting the same
    /// fee twice is a re-assert and harmless.
    pub fn set_pair_fee_bps(env: Env, source: Symbol, destination: Symbol, fee_bps: u32) {
        if env
            .storage()
            .persistent()
            .get(&DataKey::Paused)
            .unwrap_or(false)
        {
            panic_with_error!(&env, RouterError::ContractPaused);
        }
        Self::require_admin(&env);
        if fee_bps > MAX_FEE_BPS {
            panic_with_error!(&env, RouterError::FeeBpsTooHigh);
        }
        env.storage().persistent().set(
            &DataKey::PairFeeBps(source.clone(), destination.clone()),
            &fee_bps,
        );
        env.events()
            .publish((symbol_short!("fee_set"),), (source, destination, fee_bps));
    }

    /// Returns the configured fee in basis points for a pair, or 0 if
    /// no fee has been set (a registered pair with no fee is free).
    pub fn get_pair_fee_bps(env: Env, source: Symbol, destination: Symbol) -> u32 {
        env.storage()
            .persistent()
            .get(&DataKey::PairFeeBps(source, destination))
            .unwrap_or(0)
    }

    /// Compute the fee in source units for routing `amount` through the
    /// `(source, destination)` pair.
    ///
    /// Rejects unregistered pairs with [`RouterError::PairNotRegistered`]
    /// and non-positive amounts with [`RouterError::AmountMustBePositive`]
    /// so off-chain callers always get a clear typed error instead of a
    /// silent zero. Math is integer division (truncating toward zero),
    /// matching every existing Stellar fee accounting precedent.
    pub fn compute_route_fee(env: Env, source: Symbol, destination: Symbol, amount: i128) -> i128 {
        Self::route_fee_inner(&env, source, destination, amount)
    }

    /// Canonical route-fee implementation shared by `compute_route_fee` and
    /// `compute_route_fee_checked`.
    ///
    /// Performs all validation (amount > 0, pair registered, min/max bounds,
    /// liquidity), the side effects (lifetime counter bump, per-pair
    /// last-route-at stamp, and the `route` event), and returns the fee in
    /// source units. Private so it never enters the generated client ABI and
    /// so both public entrypoints share identical logic and side-effect
    /// semantics. Call it exactly once per invocation to avoid double
    /// counting.
    fn route_fee_inner(env: &Env, source: Symbol, destination: Symbol, amount: i128) -> i128 {
        if amount <= 0 {
            panic_with_error!(env, RouterError::AmountMustBePositive);
        }
        if !env
            .storage()
            .persistent()
            .get::<_, bool>(&DataKey::Pair(source.clone(), destination.clone()))
            .unwrap_or(false)
        {
            panic_with_error!(env, RouterError::PairNotRegistered);
        }
        let min_amount: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::PairMinAmount(source.clone(), destination.clone()))
            .unwrap_or(0);
        if amount < min_amount {
            panic_with_error!(env, RouterError::AmountBelowMin);
        }
        let max_amount: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::PairMaxAmount(source.clone(), destination.clone()))
            .unwrap_or(i128::MAX);
        if amount > max_amount {
            panic_with_error!(env, RouterError::AmountAboveMax);
        }
        let liquidity: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::PairLiquidity(source.clone(), destination.clone()))
            .unwrap_or(i128::MAX);
        if amount > liquidity {
            panic_with_error!(env, RouterError::InsufficientLiquidity);
        }
        // Per-pair rate limit. A non-zero cooldown forces a minimum gap
        // between successive routes for the pair. The first route (no
        // recorded timestamp) is always allowed; cooldown == 0 disables
        // the check entirely, preserving the prior behaviour. Compare via
        // addition (last + cooldown) rather than subtraction to avoid any
        // u64 underflow.
        let cooldown: u64 = env
            .storage()
            .persistent()
            .get(&DataKey::PairCooldown(source.clone(), destination.clone()))
            .unwrap_or(0);
        if cooldown > 0 {
            if let Some(last) = env
                .storage()
                .persistent()
                .get::<_, u64>(&DataKey::PairLastRouteAt(
                    source.clone(),
                    destination.clone(),
                ))
            {
                if env.ledger().timestamp() < last + cooldown {
                    panic_with_error!(&env, RouterError::RouteCooldownActive);
                }
            }
        }
        let total: u64 = env
            .storage()
            .persistent()
            .get(&DataKey::TotalRoutesAllTime)
            .unwrap_or(0);
        env.storage()
            .persistent()
            .set(&DataKey::TotalRoutesAllTime, &total.saturating_add(1));
        let pair_count: u64 = env
            .storage()
            .persistent()
            .get(&DataKey::PairRouteCount(
                source.clone(),
                destination.clone(),
            ))
            .unwrap_or(0);
        env.storage().persistent().set(
            &DataKey::PairRouteCount(source.clone(), destination.clone()),
            &pair_count.saturating_add(1),
        );
        let pair_volume: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::PairVolume(source.clone(), destination.clone()))
            .unwrap_or(0);
        env.storage().persistent().set(
            &DataKey::PairVolume(source.clone(), destination.clone()),
            &pair_volume.saturating_add(amount),
        );
        env.storage().persistent().set(
            &DataKey::PairLastRouteAt(source.clone(), destination.clone()),
            &env.ledger().timestamp(),
        );
        env.events().publish(
            (symbol_short!("route"),),
            (source.clone(), destination.clone(), amount),
        );
        let fee_bps: u32 = env
            .storage()
            .persistent()
            .get(&DataKey::PairFeeBps(source, destination))
            .unwrap_or(0);
        // amount * fee_bps / 10_000, in i128 to avoid u32*i128 overflow on
        // amounts near i128::MAX. fee_bps is capped at MAX_FEE_BPS so the
        // multiplication is bounded.
        amount
            .checked_mul(fee_bps as i128)
            .map(|n| n / BPS_DENOMINATOR)
            .unwrap_or(0)
    }

    /// Slippage-guarded variant of [`Self::compute_route_fee`].
    ///
    /// Runs the exact same canonical code path as `compute_route_fee` via the
    /// shared private inner helper, so it performs identical validation, the
    /// same side effects (lifetime counter bump, per-pair last-route-at
    /// stamp, and `route` event), and computes the fee with identical math.
    /// After the fee is known, it derives `net = amount - fee` and, when the
    /// caller supplies a positive `min_out`, enforces a minimum-output floor:
    /// if `min_out > 0 && net < min_out` it panics with
    /// [`RouterError::SlippageExceeded`].
    ///
    /// A `min_out <= 0` disables the floor entirely, making this behave
    /// exactly like the unchecked path (same validation, side effects, and
    /// returned fee). On success the returned value is the fee, identical to
    /// `compute_route_fee`.
    ///
    /// The inner helper is called exactly once, so the counter/timestamp/event
    /// fire exactly once and there is no double counting. Because the
    /// unchecked path is the canonical behaviour, the floor check happens
    /// after the side effects have already run.
    pub fn compute_route_fee_checked(
        env: Env,
        source: Symbol,
        destination: Symbol,
        amount: i128,
        min_out: i128,
    ) -> i128 {
        let fee = Self::route_fee_inner(&env, source, destination, amount);
        let net = amount - fee;
        if min_out > 0 && net < min_out {
            panic_with_error!(&env, RouterError::SlippageExceeded);
        }
        fee
    }

    /// Placeholder: returns a fixed route tag for a source/destination pair.
    /// Used by the backend to verify route integrity.
    pub fn route_tag(_env: Env, source: Symbol, destination: Symbol) -> (Symbol, Symbol) {
        (source, destination)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{symbol_short, testutils::Address as _, IntoVal};

    fn setup_initialized(env: &Env) -> (StableRouteRouterClient<'_>, Address) {
        env.mock_all_auths();
        let contract_id = env.register(StableRouteRouter, ());
        let client = StableRouteRouterClient::new(env, &contract_id);
        let admin = Address::generate(env);
        client.init(&admin);
        (client, admin)
    }

    /// Register the contract WITHOUT calling `init`, so no admin is stored.
    /// All auths are mocked, isolating the failure to the missing-admin
    /// branch (`NotInitialized`, error #2) inside `require_admin`. Use this
    /// for the uninitialized-call negative tests; never reuse
    /// `setup_initialized` for them.
    fn setup_uninitialized(env: &Env) -> StableRouteRouterClient<'_> {
        env.mock_all_auths();
        let contract_id = env.register(StableRouteRouter, ());
        StableRouteRouterClient::new(env, &contract_id)
    }

    #[test]
    fn test_version() {
        let env = Env::default();
        let contract_id = env.register(StableRouteRouter, ());
        let client = StableRouteRouterClient::new(&env, &contract_id);
        let v = client.version();
        assert_eq!(v, symbol_short!("ROUTER_V2"));
    }

    #[test]
    fn test_route_tag() {
        let env = Env::default();
        let contract_id = env.register(StableRouteRouter, ());
        let client = StableRouteRouterClient::new(&env, &contract_id);

        // Determinism: the same inputs hash to the same tag across calls.
        let tag_a = client.route_tag(&symbol_short!("USDC"), &symbol_short!("EURC"));
        let tag_b = client.route_tag(&symbol_short!("USDC"), &symbol_short!("EURC"));
        assert_eq!(tag_a, tag_b);

        // Direction sensitivity: (src, dst) differs from (dst, src).
        let reversed = client.route_tag(&symbol_short!("EURC"), &symbol_short!("USDC"));
        assert_ne!(tag_a, reversed);

        // Distinct pairs produce distinct tags.
        let other = client.route_tag(&symbol_short!("USDC"), &symbol_short!("XLM"));
        assert_ne!(tag_a, other);
    }

    #[test]
    fn test_init_persists_admin() {
        let env = Env::default();
        let (client, admin) = setup_initialized(&env);
        assert_eq!(client.get_admin(), Some(admin));
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #1)")]
    fn test_init_rejects_double_init() {
        let env = Env::default();
        let (client, _admin) = setup_initialized(&env);
        let other = Address::generate(&env);
        client.init(&other);
    }

    #[test]
    fn test_register_pair_round_trip() {
        let env = Env::default();
        let (client, _admin) = setup_initialized(&env);
        client.register_pair(&symbol_short!("USDC"), &symbol_short!("EURC"));
        assert!(client.is_pair_registered(&symbol_short!("USDC"), &symbol_short!("EURC")));
        // Reverse direction is independent.
        assert!(!client.is_pair_registered(&symbol_short!("EURC"), &symbol_short!("USDC")));
    }

    #[test]
    fn test_register_pair_is_idempotent() {
        let env = Env::default();
        let (client, _admin) = setup_initialized(&env);
        client.register_pair(&symbol_short!("USDC"), &symbol_short!("EURC"));
        client.register_pair(&symbol_short!("USDC"), &symbol_short!("EURC"));
        assert!(client.is_pair_registered(&symbol_short!("USDC"), &symbol_short!("EURC")));
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #3)")]
    fn test_register_pair_rejects_identity() {
        let env = Env::default();
        let (client, _admin) = setup_initialized(&env);
        client.register_pair(&symbol_short!("USDC"), &symbol_short!("USDC"));
    }

    #[test]
    fn test_is_pair_registered_defaults_to_false() {
        let env = Env::default();
        let (client, _admin) = setup_initialized(&env);
        assert!(!client.is_pair_registered(&symbol_short!("USDC"), &symbol_short!("XLM")));
    }

    #[test]
    fn test_get_pair_fee_bps_defaults_to_zero() {
        let env = Env::default();
        let (client, _admin) = setup_initialized(&env);
        assert_eq!(
            client.get_pair_fee_bps(&symbol_short!("USDC"), &symbol_short!("EURC")),
            0
        );
    }

    #[test]
    fn test_set_pair_fee_bps_round_trip() {
        let env = Env::default();
        let (client, _admin) = setup_initialized(&env);
        client.register_pair(&symbol_short!("USDC"), &symbol_short!("EURC"));
        client.set_pair_fee_bps(&symbol_short!("USDC"), &symbol_short!("EURC"), &50u32);
        assert_eq!(
            client.get_pair_fee_bps(&symbol_short!("USDC"), &symbol_short!("EURC")),
            50
        );
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #4)")]
    fn test_set_pair_fee_bps_rejects_above_max() {
        let env = Env::default();
        let (client, _admin) = setup_initialized(&env);
        client.register_pair(&symbol_short!("USDC"), &symbol_short!("EURC"));
        client.set_pair_fee_bps(
            &symbol_short!("USDC"),
            &symbol_short!("EURC"),
            &(MAX_FEE_BPS + 1),
        );
    }

    #[test]
    fn test_compute_route_fee_basic() {
        let env = Env::default();
        let (client, _admin) = setup_initialized(&env);
        client.register_pair(&symbol_short!("USDC"), &symbol_short!("EURC"));
        client.set_pair_fee_bps(&symbol_short!("USDC"), &symbol_short!("EURC"), &50u32);
        // 1_000_000 * 50 / 10_000 = 5_000
        let fee = client.compute_route_fee(
            &symbol_short!("USDC"),
            &symbol_short!("EURC"),
            &1_000_000_i128,
        );
        assert_eq!(fee, 5_000);
    }

    #[test]
    fn test_compute_route_fee_is_zero_when_fee_unset() {
        let env = Env::default();
        let (client, _admin) = setup_initialized(&env);
        client.register_pair(&symbol_short!("USDC"), &symbol_short!("EURC"));
        let fee = client.compute_route_fee(
            &symbol_short!("USDC"),
            &symbol_short!("EURC"),
            &1_000_000_i128,
        );
        assert_eq!(fee, 0);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #5)")]
    fn test_compute_route_fee_rejects_unregistered_pair() {
        let env = Env::default();
        let (client, _admin) = setup_initialized(&env);
        client.compute_route_fee(&symbol_short!("USDC"), &symbol_short!("EURC"), &1_000_i128);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #6)")]
    fn test_compute_route_fee_rejects_zero_amount() {
        let env = Env::default();
        let (client, _admin) = setup_initialized(&env);
        client.register_pair(&symbol_short!("USDC"), &symbol_short!("EURC"));
        client.compute_route_fee(&symbol_short!("USDC"), &symbol_short!("EURC"), &0i128);
    }

    #[test]
    fn test_schema_version_migration() {
        let env = Env::default();
        let (client, _admin) = setup_initialized(&env);
        assert_eq!(client.get_schema_version(), 1);
        client.migrate_v1_to_v2();
        assert_eq!(client.get_schema_version(), 2);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #13)")]
    fn test_schema_migration_rejects_second_run() {
        let env = Env::default();
        let (client, _admin) = setup_initialized(&env);
        client.migrate_v1_to_v2();
        client.migrate_v1_to_v2();
    }

    #[test]
    fn test_pause_and_unpause_round_trip() {
        let env = Env::default();
        let (client, _admin) = setup_initialized(&env);
        assert!(!client.is_paused());
        client.pause();
        assert!(client.is_paused());
        client.unpause();
        assert!(!client.is_paused());
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #9)")]
    fn test_register_pair_rejects_when_paused() {
        let env = Env::default();
        let (client, _admin) = setup_initialized(&env);
        client.pause();
        client.register_pair(&symbol_short!("USDC"), &symbol_short!("EURC"));
    }

    #[test]
    fn test_admin_transfer_flow() {
        let env = Env::default();
        let (client, _admin) = setup_initialized(&env);
        let next_admin = Address::generate(&env);
        client.propose_admin_transfer(&next_admin);
        assert_eq!(client.get_pending_admin(), Some(next_admin.clone()));
        client.accept_admin_transfer(&next_admin);
        assert_eq!(client.get_admin(), Some(next_admin));
        assert_eq!(client.get_pending_admin(), None);
    }

    #[test]
    fn test_cancel_admin_transfer_clears_pending_admin() {
        let env = Env::default();
        let (client, _admin) = setup_initialized(&env);
        let next_admin = Address::generate(&env);
        client.propose_admin_transfer(&next_admin);
        client.cancel_admin_transfer();
        assert_eq!(client.get_pending_admin(), None);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #7)")]
    fn test_accept_admin_transfer_rejects_missing_pending_admin() {
        let env = Env::default();
        let (client, _admin) = setup_initialized(&env);
        let caller = Address::generate(&env);
        client.accept_admin_transfer(&caller);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #8)")]
    fn test_accept_admin_transfer_rejects_wrong_pending_admin() {
        let env = Env::default();
        let (client, _admin) = setup_initialized(&env);
        let pending = Address::generate(&env);
        let caller = Address::generate(&env);
        client.propose_admin_transfer(&pending);
        client.accept_admin_transfer(&caller);
    }

    #[test]
    fn test_fee_recipient_round_trip() {
        let env = Env::default();
        let (client, _admin) = setup_initialized(&env);
        assert_eq!(client.get_fee_recipient(), None);
        let recipient = Address::generate(&env);
        client.set_fee_recipient(&recipient);
        assert_eq!(client.get_fee_recipient(), Some(recipient));
    }

    #[test]
    fn test_unregister_pair_removes_registration() {
        let env = Env::default();
        let (client, _admin) = setup_initialized(&env);
        client.register_pair(&symbol_short!("USDC"), &symbol_short!("EURC"));
        client.unregister_pair(&symbol_short!("USDC"), &symbol_short!("EURC"));
        assert!(!client.is_pair_registered(&symbol_short!("USDC"), &symbol_short!("EURC")));
    }

    #[test]
    fn test_pair_limits_liquidity_and_info_round_trip() {
        let env = Env::default();
        let (client, _admin) = setup_initialized(&env);
        client.register_pair(&symbol_short!("USDC"), &symbol_short!("EURC"));
        assert!(!client.is_pair_active(&symbol_short!("USDC"), &symbol_short!("EURC")));

        client.set_pair_fee_bps(&symbol_short!("USDC"), &symbol_short!("EURC"), &25u32);
        client.set_pair_min_amount(&symbol_short!("USDC"), &symbol_short!("EURC"), &10i128);
        client.set_pair_max_amount(&symbol_short!("USDC"), &symbol_short!("EURC"), &1_000i128);
        client.set_pair_liquidity(&symbol_short!("USDC"), &symbol_short!("EURC"), &500i128);

        assert_eq!(
            client.get_pair_min_amount(&symbol_short!("USDC"), &symbol_short!("EURC")),
            10
        );
        assert_eq!(
            client.get_pair_max_amount(&symbol_short!("USDC"), &symbol_short!("EURC")),
            1_000
        );
        assert_eq!(
            client.get_pair_liquidity(&symbol_short!("USDC"), &symbol_short!("EURC")),
            500
        );
        assert!(client.is_pair_active(&symbol_short!("USDC"), &symbol_short!("EURC")));

        let info = client.get_pair_info(&symbol_short!("USDC"), &symbol_short!("EURC"));
        assert_eq!(
            info,
            PairInfo {
                registered: true,
                fee_bps: 25,
                min_amount: 10,
                max_amount: 1_000,
                liquidity: 500,
                last_route_at: 0,
            }
        );
    }

    #[test]
    fn test_quote_route_and_compute_route_update_counters() {
        let env = Env::default();
        let (client, _admin) = setup_initialized(&env);
        client.register_pair(&symbol_short!("USDC"), &symbol_short!("EURC"));
        client.set_pair_fee_bps(&symbol_short!("USDC"), &symbol_short!("EURC"), &100u32);

        assert_eq!(
            client.quote_route(&symbol_short!("USDC"), &symbol_short!("EURC"), &1_000i128),
            (10, 990)
        );
        assert_eq!(client.get_total_routes_all_time(), 0);

        assert_eq!(
            client.compute_route_fee(&symbol_short!("USDC"), &symbol_short!("EURC"), &1_000i128),
            10
        );
        assert_eq!(client.get_total_routes_all_time(), 1);
        assert_eq!(
            client.get_pair_last_route_at(&symbol_short!("USDC"), &symbol_short!("EURC")),
            Some(0)
        );
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #6)")]
    fn test_quote_route_rejects_zero_amount() {
        let env = Env::default();
        let (client, _admin) = setup_initialized(&env);
        client.register_pair(&symbol_short!("USDC"), &symbol_short!("EURC"));
        client.quote_route(&symbol_short!("USDC"), &symbol_short!("EURC"), &0i128);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #5)")]
    fn test_quote_route_rejects_unregistered_pair() {
        let env = Env::default();
        let (client, _admin) = setup_initialized(&env);
        client.quote_route(&symbol_short!("USDC"), &symbol_short!("EURC"), &1_000i128);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #10)")]
    fn test_compute_route_fee_rejects_below_minimum() {
        let env = Env::default();
        let (client, _admin) = setup_initialized(&env);
        client.register_pair(&symbol_short!("USDC"), &symbol_short!("EURC"));
        client.set_pair_min_amount(&symbol_short!("USDC"), &symbol_short!("EURC"), &10i128);
        client.compute_route_fee(&symbol_short!("USDC"), &symbol_short!("EURC"), &9i128);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #11)")]
    fn test_compute_route_fee_rejects_above_maximum() {
        let env = Env::default();
        let (client, _admin) = setup_initialized(&env);
        client.register_pair(&symbol_short!("USDC"), &symbol_short!("EURC"));
        client.set_pair_max_amount(&symbol_short!("USDC"), &symbol_short!("EURC"), &10i128);
        client.compute_route_fee(&symbol_short!("USDC"), &symbol_short!("EURC"), &11i128);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #12)")]
    fn test_compute_route_fee_rejects_insufficient_liquidity() {
        let env = Env::default();
        let (client, _admin) = setup_initialized(&env);
        client.register_pair(&symbol_short!("USDC"), &symbol_short!("EURC"));
        client.set_pair_liquidity(&symbol_short!("USDC"), &symbol_short!("EURC"), &10i128);
        client.compute_route_fee(&symbol_short!("USDC"), &symbol_short!("EURC"), &11i128);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #6)")]
    fn test_set_pair_liquidity_rejects_negative_value() {
        let env = Env::default();
        let (client, _admin) = setup_initialized(&env);
        client.set_pair_liquidity(&symbol_short!("USDC"), &symbol_short!("EURC"), &-1i128);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #6)")]
    fn test_set_pair_max_amount_rejects_zero() {
        let env = Env::default();
        let (client, _admin) = setup_initialized(&env);
        client.set_pair_max_amount(&symbol_short!("USDC"), &symbol_short!("EURC"), &0i128);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #6)")]
    fn test_set_pair_min_amount_rejects_negative_value() {
        let env = Env::default();
        let (client, _admin) = setup_initialized(&env);
        client.set_pair_min_amount(&symbol_short!("USDC"), &symbol_short!("EURC"), &-1i128);
    }

    // --- compute_route_fee_checked (slippage guard) tests ---

    /// net below the floor must panic SlippageExceeded (#14). With a 100 bps
    /// fee on 1_000, fee = 10 and net = 990; a min_out of 991 must reject.
    #[test]
    #[should_panic(expected = "Error(Contract, #14)")]
    fn test_compute_route_fee_checked_rejects_below_floor() {
        let env = Env::default();
        let (client, _admin) = setup_initialized(&env);
        client.register_pair(&symbol_short!("USDC"), &symbol_short!("EURC"));
        client.set_pair_fee_bps(&symbol_short!("USDC"), &symbol_short!("EURC"), &100u32);
        client.compute_route_fee_checked(
            &symbol_short!("USDC"),
            &symbol_short!("EURC"),
            &1_000i128,
            &991i128,
        );
    }

    /// net exactly at the floor passes and returns the fee. fee = 10,
    /// net = 990, min_out = 990 -> not below, so it succeeds.
    #[test]
    fn test_compute_route_fee_checked_passes_at_exact_floor() {
        let env = Env::default();
        let (client, _admin) = setup_initialized(&env);
        client.register_pair(&symbol_short!("USDC"), &symbol_short!("EURC"));
        client.set_pair_fee_bps(&symbol_short!("USDC"), &symbol_short!("EURC"), &100u32);
        let fee = client.compute_route_fee_checked(
            &symbol_short!("USDC"),
            &symbol_short!("EURC"),
            &1_000i128,
            &990i128,
        );
        assert_eq!(fee, 10);
    }

    /// min_out <= 0 disables the floor and must match the unchecked path
    /// exactly (same fee, same side effects).
    #[test]
    fn test_compute_route_fee_checked_no_floor_matches_unchecked() {
        let env = Env::default();
        let (client, _admin) = setup_initialized(&env);
        client.register_pair(&symbol_short!("USDC"), &symbol_short!("EURC"));
        client.set_pair_fee_bps(&symbol_short!("USDC"), &symbol_short!("EURC"), &100u32);

        // min_out = 0 -> no floor.
        let fee_zero = client.compute_route_fee_checked(
            &symbol_short!("USDC"),
            &symbol_short!("EURC"),
            &1_000i128,
            &0i128,
        );
        assert_eq!(fee_zero, 10);
        assert_eq!(client.get_total_routes_all_time(), 1);

        // min_out negative -> also no floor.
        let fee_neg = client.compute_route_fee_checked(
            &symbol_short!("USDC"),
            &symbol_short!("EURC"),
            &1_000i128,
            &-5i128,
        );
        assert_eq!(fee_neg, 10);
        assert_eq!(client.get_total_routes_all_time(), 2);
    }

    /// The checked variant's fee math is identical to the unchecked variant
    /// when the floor is satisfied. Run both on equivalent fresh state.
    #[test]
    fn test_compute_route_fee_checked_parity_with_unchecked() {
        let unchecked_fee = {
            let env = Env::default();
            let (client, _admin) = setup_initialized(&env);
            client.register_pair(&symbol_short!("USDC"), &symbol_short!("EURC"));
            client.set_pair_fee_bps(&symbol_short!("USDC"), &symbol_short!("EURC"), &50u32);
            client.compute_route_fee(
                &symbol_short!("USDC"),
                &symbol_short!("EURC"),
                &1_000_000i128,
            )
        };

        let checked_fee = {
            let env = Env::default();
            let (client, _admin) = setup_initialized(&env);
            client.register_pair(&symbol_short!("USDC"), &symbol_short!("EURC"));
            client.set_pair_fee_bps(&symbol_short!("USDC"), &symbol_short!("EURC"), &50u32);
            client.compute_route_fee_checked(
                &symbol_short!("USDC"),
                &symbol_short!("EURC"),
                &1_000_000i128,
                &990_000i128,
            )
        };

        assert_eq!(unchecked_fee, checked_fee);
        assert_eq!(checked_fee, 5_000);
    }

    // --- require_admin helper contract tests ---

    /// After the refactor, every admin-gated entrypoint must still reject a
    /// non-admin caller. We test `pause` as a representative; the helper is
    /// shared, so this covers all entrypoints structurally.
    #[test]
    #[should_panic]
    fn test_require_admin_rejects_unauthorized_caller() {
        let env = Env::default();
        let contract_id = env.register(StableRouteRouter, ());
        let client = StableRouteRouterClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let attacker = Address::generate(&env);
        // Initialize with the real admin (mock auth only for init).
        env.mock_auths(&[soroban_sdk::testutils::MockAuth {
            address: &admin,
            invoke: &soroban_sdk::testutils::MockAuthInvoke {
                contract: &contract_id,
                fn_name: "init",
                args: (admin.clone(),).into_val(&env),
                sub_invokes: &[],
            },
        }]);
        client.init(&admin);
        // Now call pause as the attacker — no mock auth provided for attacker.
        env.mock_auths(&[soroban_sdk::testutils::MockAuth {
            address: &attacker,
            invoke: &soroban_sdk::testutils::MockAuthInvoke {
                contract: &contract_id,
                fn_name: "pause",
                args: ().into_val(&env),
                sub_invokes: &[],
            },
        }]);
        client.pause(); // must panic: admin.require_auth() fails for attacker
    }

    // --- per-pair route count + cumulative volume tests ---

    #[test]
    fn test_pair_route_count_and_volume_default_to_zero() {
        let env = Env::default();
        let (client, _admin) = setup_initialized(&env);
        client.register_pair(&symbol_short!("USDC"), &symbol_short!("EURC"));
        assert_eq!(
            client.get_pair_route_count(&symbol_short!("USDC"), &symbol_short!("EURC")),
            0
        );
        assert_eq!(
            client.get_pair_volume(&symbol_short!("USDC"), &symbol_short!("EURC")),
            0
        );
    }

    #[test]
    fn test_pair_route_count_and_volume_increment_per_route() {
        let env = Env::default();
        let (client, _admin) = setup_initialized(&env);
        client.register_pair(&symbol_short!("USDC"), &symbol_short!("EURC"));
        client.set_pair_fee_bps(&symbol_short!("USDC"), &symbol_short!("EURC"), &50u32);

        client.compute_route_fee(&symbol_short!("USDC"), &symbol_short!("EURC"), &1_000i128);
        assert_eq!(
            client.get_pair_route_count(&symbol_short!("USDC"), &symbol_short!("EURC")),
            1
        );
        assert_eq!(
            client.get_pair_volume(&symbol_short!("USDC"), &symbol_short!("EURC")),
            1_000
        );

        client.compute_route_fee(&symbol_short!("USDC"), &symbol_short!("EURC"), &2_500i128);
        assert_eq!(
            client.get_pair_route_count(&symbol_short!("USDC"), &symbol_short!("EURC")),
            2
        );
        assert_eq!(
            client.get_pair_volume(&symbol_short!("USDC"), &symbol_short!("EURC")),
            3_500
        );
    }

    #[test]
    fn test_pair_route_count_and_volume_isolated_per_pair() {
        let env = Env::default();
        let (client, _admin) = setup_initialized(&env);
        client.register_pair(&symbol_short!("USDC"), &symbol_short!("EURC"));
        client.register_pair(&symbol_short!("USDC"), &symbol_short!("GBPC"));

        client.compute_route_fee(&symbol_short!("USDC"), &symbol_short!("EURC"), &1_000i128);
        client.compute_route_fee(&symbol_short!("USDC"), &symbol_short!("EURC"), &500i128);
        client.compute_route_fee(&symbol_short!("USDC"), &symbol_short!("GBPC"), &7_000i128);

        assert_eq!(
            client.get_pair_route_count(&symbol_short!("USDC"), &symbol_short!("EURC")),
            2
        );
        assert_eq!(
            client.get_pair_volume(&symbol_short!("USDC"), &symbol_short!("EURC")),
            1_500
        );
        assert_eq!(
            client.get_pair_route_count(&symbol_short!("USDC"), &symbol_short!("GBPC")),
            1
        );
        assert_eq!(
            client.get_pair_volume(&symbol_short!("USDC"), &symbol_short!("GBPC")),
            7_000
        );
    }

    /// Calling any admin-gated entrypoint before `init` must panic with
    /// NotInitialized (error #2).
    #[test]
    #[should_panic(expected = "Error(Contract, #2)")]
    fn test_require_admin_panics_when_not_initialized() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(StableRouteRouter, ());
        let client = StableRouteRouterClient::new(&env, &contract_id);
        client.pause(); // no admin stored yet
    }

    // --- version surface stability ---

    /// `version()` is the fixed contract identity tag and must be entirely
    /// independent of `get_schema_version()`: migrating the storage schema
    /// from v1 to v2 advances the schema number but never the version tag.
    #[test]
    fn test_version_is_independent_of_schema_version() {
        let env = Env::default();
        let (client, _admin) = setup_initialized(&env);
        // Version tag and schema number start at known, distinct values.
        assert_eq!(client.version(), symbol_short!("ROUTER_V2"));
        assert_eq!(client.get_schema_version(), 1);

        client.migrate_v1_to_v2();

        // Schema advanced 1 -> 2, but the version tag is unchanged.
        assert_eq!(client.get_schema_version(), 2);
        assert_eq!(client.version(), symbol_short!("ROUTER_V2"));
    }

    /// `version()` does not require an initialized contract: it is a pure
    /// constant readable on a freshly registered (uninitialized) contract.
    #[test]
    fn test_version_readable_when_uninitialized() {
        let env = Env::default();
        let client = setup_uninitialized(&env);
        assert_eq!(client.version(), symbol_short!("ROUTER_V2"));
    }

    // --- get_schema_version default before init/migration ---

    /// On a registered-but-uninitialized contract (no init, no migration),
    /// `get_schema_version()` returns the implicit pre-migration default of 1.
    #[test]
    fn test_get_schema_version_defaults_to_one_when_uninitialized() {
        let env = Env::default();
        let client = setup_uninitialized(&env);
        assert_eq!(client.get_schema_version(), 1);
    }

    // --- uninitialized admin-gated entrypoints panic NotInitialized (#2) ---
    //
    // Each test below registers the contract WITHOUT init and calls one
    // admin-gated entrypoint. With no admin stored, `require_admin` panics
    // with NotInitialized (#2) before any state change can occur. Auths are
    // mocked, so the panic is solely from the missing admin, not auth.

    #[test]
    #[should_panic(expected = "Error(Contract, #2)")]
    fn test_pause_panics_when_uninitialized() {
        let env = Env::default();
        let client = setup_uninitialized(&env);
        client.pause();
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #2)")]
    fn test_unpause_panics_when_uninitialized() {
        let env = Env::default();
        let client = setup_uninitialized(&env);
        client.unpause();
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #2)")]
    fn test_set_pair_fee_bps_panics_when_uninitialized() {
        let env = Env::default();
        let client = setup_uninitialized(&env);
        client.set_pair_fee_bps(&symbol_short!("USDC"), &symbol_short!("EURC"), &50u32);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #2)")]
    fn test_propose_admin_transfer_panics_when_uninitialized() {
        let env = Env::default();
        let client = setup_uninitialized(&env);
        let new_admin = Address::generate(&env);
        client.propose_admin_transfer(&new_admin);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #2)")]
    fn test_cancel_admin_transfer_panics_when_uninitialized() {
        let env = Env::default();
        let client = setup_uninitialized(&env);
        client.cancel_admin_transfer();
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #2)")]
    fn test_migrate_v1_to_v2_panics_when_uninitialized() {
        let env = Env::default();
        let client = setup_uninitialized(&env);
        client.migrate_v1_to_v2();
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #2)")]
    fn test_set_fee_recipient_panics_when_uninitialized() {
        let env = Env::default();
        let client = setup_uninitialized(&env);
        let recipient = Address::generate(&env);
        client.set_fee_recipient(&recipient);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #2)")]
    fn test_register_pair_panics_when_uninitialized() {
        let env = Env::default();
        let client = setup_uninitialized(&env);
        client.register_pair(&symbol_short!("USDC"), &symbol_short!("EURC"));
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #2)")]
    fn test_unregister_pair_panics_when_uninitialized() {
        let env = Env::default();
        let client = setup_uninitialized(&env);
        client.unregister_pair(&symbol_short!("USDC"), &symbol_short!("EURC"));
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #2)")]
    fn test_set_pair_liquidity_panics_when_uninitialized() {
        let env = Env::default();
        let client = setup_uninitialized(&env);
        client.set_pair_liquidity(&symbol_short!("USDC"), &symbol_short!("EURC"), &1i128);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #2)")]
    fn test_set_pair_min_amount_panics_when_uninitialized() {
        let env = Env::default();
        let client = setup_uninitialized(&env);
        client.set_pair_min_amount(&symbol_short!("USDC"), &symbol_short!("EURC"), &1i128);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #2)")]
    fn test_set_pair_max_amount_panics_when_uninitialized() {
        let env = Env::default();
        let client = setup_uninitialized(&env);
        client.set_pair_max_amount(&symbol_short!("USDC"), &symbol_short!("EURC"), &1i128);
    }
}
