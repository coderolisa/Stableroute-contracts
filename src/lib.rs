#![allow(deprecated)] // TODO: migrate Soroban events to #[contractevent].
#![no_std]
// Contributing? See CONTRIBUTING.md for error-numbering, event-topic, auth,
// pause, and storage/TTL conventions plus the PR checklist.

#[cfg(test)]
extern crate std;

use soroban_sdk::xdr::ToXdr;
use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, panic_with_error, symbol_short, Address,
    Bytes, BytesN, Env, Symbol, Vec,
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
    /// Governance timelock delay, in seconds. When > 0, a proposed admin
    /// handover can only be accepted after the delay has elapsed.
    /// Defaults to 0 (instant) when unset, preserving prior behaviour.
    Timelock,
    /// Earliest ledger timestamp at which the currently pending admin
    /// transfer may be accepted (`propose_admin_transfer` time + delay).
    PendingAdminEta,
    /// Reentrancy guard flag. `true` while a mutating entrypoint is
    /// executing; rejects re-entrant calls with [`RouterError::ReentrantCall`].
    ReentrancyLock,
    /// Per-pair route cooldown in seconds. A non-zero value forces a
    /// minimum gap between successive routes for the pair.
    PairCooldown(Symbol, Symbol),
    /// Absolute ceiling on per-route fees (in source units). When set,
    /// `min(computed_fee, max_fee_absolute)` is charged.
    MaxFeeAbsolute,
    /// Scoped liquidity oracle address. May update pair liquidity but
    /// cannot change fees, pause, rotate admin, or upgrade.
    Oracle,
}

/// Upper bound on the per-pair fee. 1 000 bps = 10 %. Tightening this
/// further is a governance decision; raising it is append-only safe
/// but should be deliberate.
pub const MAX_FEE_BPS: u32 = 1_000;
/// Basis-point denominator: 1 bps = 1/10_000.
pub const BPS_DENOMINATOR: i128 = 10_000;
/// Maximum number of entries in a single batch operation
/// (`register_pairs`, `set_pair_fees_bps`). Kept modest to bound
/// per-transaction gas costs.
pub const MAX_BATCH_SIZE: u32 = 100;

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
    /// `accept_admin_transfer` was called before the governance timelock
    /// delay elapsed.
    TimelockNotElapsed = 14,
    /// Caller does not have the required role for the operation.
    /// Returned by `set_pair_liquidity` when the caller is neither admin
    /// nor the configured oracle.
    NotAuthorized = 15,
    /// A re-entrant invocation was attempted while a mutating entrypoint
    /// held the reentrancy lock.
    ReentrantCall = 16,
    /// `compute_route_fee` was called for a pair before the per-pair
    /// route cooldown elapsed.
    RouteCooldownActive = 17,
    /// A batch entrypoint was called with more entries than
    /// [`MAX_BATCH_SIZE`].
    BatchTooLarge = 18,
    /// A batch entrypoint was called with no entries.
    EmptyBatch = 19,
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

    /// Require that `(source, destination)` was previously registered via
    /// [`Self::register_pair`]; panics with
    /// [`RouterError::PairNotRegistered`] otherwise.
    ///
    /// Every per-pair config setter (`set_pair_fee_bps`,
    /// `set_pair_min_amount`, `set_pair_max_amount`, `set_pair_liquidity`)
    /// calls this after its own admin/sign validation so a config write can
    /// never create an orphan storage slot for a corridor an operator never
    /// registered. Reuses the same [`RouterError::PairNotRegistered`] (#5)
    /// that `compute_route_fee` and `quote_route` already raise, keeping
    /// one error code for "this pair does not exist" across the contract.
    fn require_pair_registered(env: &Env, source: &Symbol, destination: &Symbol) {
        if !env
            .storage()
            .persistent()
            .get::<_, bool>(&DataKey::Pair(source.clone(), destination.clone()))
            .unwrap_or(false)
        {
            panic_with_error!(env, RouterError::PairNotRegistered);
        }
    }

    /// Acquire the reentrancy lock; panics [`RouterError::ReentrantCall`]
    /// if already held. Paired with [`Self::exit_nonreentrant`] on every
    /// return path so that a re-entrant invocation (for example via a
    /// future malicious token callback) is rejected instead of operating
    /// on partially-applied effects.
    fn enter_nonreentrant(env: &Env) {
        if env
            .storage()
            .persistent()
            .get(&DataKey::ReentrancyLock)
            .unwrap_or(false)
        {
            panic_with_error!(env, RouterError::ReentrantCall);
        }
        env.storage()
            .persistent()
            .set(&DataKey::ReentrancyLock, &true);
    }

    /// Release the reentrancy lock. Must be called before every return
    /// from a guarded entrypoint, including the success path, so that
    /// back-to-back calls work.
    fn exit_nonreentrant(env: &Env) {
        env.storage()
            .persistent()
            .set(&DataKey::ReentrancyLock, &false);
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

    /// Deploy-time constructor — sets the operational admin **atomically**
    /// at contract instantiation.
    ///
    /// Running as the constructor closes the init front-running window:
    /// the admin slot is written in the same transaction that deploys the
    /// contract (`register(StableRouteRouter, (admin,))`), so there is no
    /// observable deployed-but-uninitialized state for an attacker to race
    /// a separate `init` call into. Requires `admin.require_auth()` and
    /// emits the `init` event for indexers.
    pub fn __constructor(env: Env, admin: Address) {
        admin.require_auth();
        env.storage().persistent().set(&DataKey::Admin, &admin);
        env.events().publish((symbol_short!("init"),), admin);
    }

    /// Legacy initializer, retained for ABI compatibility only.
    ///
    /// The admin is now set by [`Self::__constructor`] at deploy time, so
    /// the slot is always populated and this entrypoint can never claim
    /// it. It unconditionally panics with
    /// [`RouterError::AlreadyInitialized`], preserving the historical
    /// `#1` semantics for any client still calling `init` post-deploy and
    /// guaranteeing an attacker can never seize the admin role via `init`.
    pub fn init(env: Env, admin: Address) {
        let _ = admin;
        panic_with_error!(&env, RouterError::AlreadyInitialized);
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

    /// Read the configured governance timelock delay, in seconds
    /// (0 when unset — handover is instant).
    pub fn get_timelock(env: Env) -> u64 {
        env.storage()
            .persistent()
            .get(&DataKey::Timelock)
            .unwrap_or(0)
    }

    /// Admin sets the governance timelock delay (seconds). Applies to the
    /// **next** `propose_admin_transfer`; already-queued actions keep the
    /// eta they were stamped with. Pass 0 to disable (instant handover).
    pub fn set_timelock(env: Env, delay_seconds: u64) {
        Self::require_admin(&env);
        env.storage()
            .persistent()
            .set(&DataKey::Timelock, &delay_seconds);
    }

    /// Read the earliest timestamp at which the pending admin transfer may
    /// be accepted, or `None` when no transfer is queued.
    pub fn get_pending_admin_eta(env: Env) -> Option<u64> {
        env.storage().persistent().get(&DataKey::PendingAdminEta)
    }

    /// Cancel a pending handover, clearing both the pending admin and its
    /// queued eta. No-op if none is pending.
    pub fn cancel_admin_transfer(env: Env) {
        Self::require_admin(&env);
        env.storage().persistent().remove(&DataKey::PendingAdmin);
        env.storage().persistent().remove(&DataKey::PendingAdminEta);
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
        // Honour the governance timelock: the handover cannot execute until
        // its stamped eta has been reached.
        let eta: u64 = env
            .storage()
            .persistent()
            .get(&DataKey::PendingAdminEta)
            .unwrap_or(0);
        if env.ledger().timestamp() < eta {
            panic_with_error!(&env, RouterError::TimelockNotElapsed);
        }
        env.storage()
            .persistent()
            .set(&DataKey::Admin, &caller.clone());
        env.storage().persistent().remove(&DataKey::PendingAdmin);
        env.storage().persistent().remove(&DataKey::PendingAdminEta);
        env.events().publish((symbol_short!("executed"),), caller);
    }

    /// Step 1 of admin handover. Current admin proposes a new admin;
    /// the new admin must then accept via `accept_admin_transfer` once the
    /// governance timelock (if any) has elapsed.
    ///
    /// Stamps `PendingAdminEta = now + timelock` and emits a `queued`
    /// event carrying the new admin and the eta so watchers get a warning
    /// window before control can actually change hands.
    pub fn propose_admin_transfer(env: Env, new_admin: Address) {
        Self::require_admin(&env);
        let delay: u64 = env
            .storage()
            .persistent()
            .get(&DataKey::Timelock)
            .unwrap_or(0);
        let eta = env.ledger().timestamp().saturating_add(delay);
        env.storage()
            .persistent()
            .set(&DataKey::PendingAdmin, &new_admin.clone());
        env.storage()
            .persistent()
            .set(&DataKey::PendingAdminEta, &eta);
        env.events()
            .publish((symbol_short!("queued"),), (new_admin, eta));
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
    ///
    /// **Registration-first invariant:** `set_pair_fee_bps`,
    /// `set_pair_min_amount`, `set_pair_max_amount`, and
    /// `set_pair_liquidity` all require the pair to already be registered
    /// here, and panic with [`RouterError::PairNotRegistered`] (#5)
    /// otherwise. Always call `register_pair` before configuring a
    /// corridor's fee, bounds, or liquidity.
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

    /// Register multiple `(source, destination)` pairs in a single
    /// admin-gated call. Each entry is validated identically to
    /// [`Self::register_pair`] and gets its own `pair_reg` event.
    ///
    /// **All-or-nothing:** if any entry fails validation the entire
    /// transaction is rolled back (Soroban transactions are atomic), so
    /// callers must ensure every pair is valid before invoking this. The
    /// batch must contain at least one entry; an empty batch panics with
    /// [`RouterError::EmptyBatch`]. The batch is also capped at
    /// [`MAX_BATCH_SIZE`] entries to bound gas; exceeding it panics with
    /// [`RouterError::BatchTooLarge`].
    pub fn register_pairs(env: Env, pairs: Vec<(Symbol, Symbol)>) {
        if env
            .storage()
            .persistent()
            .get(&DataKey::Paused)
            .unwrap_or(false)
        {
            panic_with_error!(&env, RouterError::ContractPaused);
        }
        Self::require_admin(&env);
        if pairs.len() == 0 {
            panic_with_error!(&env, RouterError::EmptyBatch);
        }
        if pairs.len() > MAX_BATCH_SIZE {
            panic_with_error!(&env, RouterError::BatchTooLarge);
        }
        for (source, destination) in pairs.iter() {
            if source == destination {
                panic_with_error!(&env, RouterError::SourceEqualsDestination);
            }
            env.storage()
                .persistent()
                .set(&DataKey::Pair(source.clone(), destination.clone()), &true);
            env.events()
                .publish((symbol_short!("pair_reg"),), (source, destination));
        }
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
        let fee = Self::apply_fee_cap(&env, fee);
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

    /// Clamp `fee` to the configured absolute ceiling when one is set.
    /// Both the relative `MAX_FEE_BPS` bound and this absolute bound apply;
    /// the tighter of the two wins. No-op when no absolute cap is configured.
    fn apply_fee_cap(env: &Env, fee: i128) -> i128 {
        match env
            .storage()
            .persistent()
            .get::<_, i128>(&DataKey::MaxFeeAbsolute)
        {
            Some(cap) => fee.min(cap),
            None => fee,
        }
    }

    /// Read the absolute per-route fee ceiling, or `None` when unset.
    pub fn get_max_fee_absolute(env: Env) -> Option<i128> {
        env.storage().persistent().get(&DataKey::MaxFeeAbsolute)
    }

    /// Admin sets the absolute per-route fee ceiling (in source units).
    /// Rejects negative caps with `AmountMustBePositive` (#6). A cap of `0`
    /// makes every route effectively free. Emits a `maxfee` event. The cap
    /// composes with `MAX_FEE_BPS`: a route is charged
    /// `min(amount * fee_bps / 10_000, max_fee_absolute)`.
    pub fn set_max_fee_absolute(env: Env, max_fee: i128) {
        Self::require_admin(&env);
        if max_fee < 0 {
            panic_with_error!(&env, RouterError::AmountMustBePositive);
        }
        env.storage()
            .persistent()
            .set(&DataKey::MaxFeeAbsolute, &max_fee);
        env.events().publish((symbol_short!("maxfee"),), max_fee);
    }

    /// Read the reported liquidity for a pair (0 when absent).
    pub fn get_pair_liquidity(env: Env, source: Symbol, destination: Symbol) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::PairLiquidity(source, destination))
            .unwrap_or(0)
    }

    /// Read the configured liquidity oracle, if any.
    pub fn get_oracle(env: Env) -> Option<Address> {
        env.storage().persistent().get(&DataKey::Oracle)
    }

    /// Admin sets (or rotates) the scoped liquidity oracle.
    ///
    /// Admin-gated. The oracle may update pair liquidity via
    /// [`Self::set_pair_liquidity`] and **nothing else** — it cannot set
    /// fees, pause, rotate admin, or upgrade. Emits `oracle_set`.
    pub fn set_oracle(env: Env, oracle: Address) {
        Self::require_admin(&env);
        env.storage().persistent().set(&DataKey::Oracle, &oracle);
        // Topic shortened to satisfy the 9-char `symbol_short!` limit.
        env.events().publish((symbol_short!("orac_set"),), oracle);
    }

    /// Admin revokes the scoped liquidity oracle.
    ///
    /// Admin-gated (panics with [`RouterError::NotInitialized`] (#2) when
    /// no admin is set, like every other admin entrypoint). Removes
    /// `DataKey::Oracle` so [`Self::set_pair_liquidity`] once again
    /// accepts **only the admin**: its dual-auth check
    /// (`caller != admin && Some(caller) != oracle`) naturally degrades to
    /// admin-only when the slot is absent, because `Some(caller)` can
    /// never equal `None`. This is the recovery path for a compromised
    /// oracle key — unlike [`Self::set_oracle`] (which can only rotate to
    /// a new address, leaving *some* oracle authorized), `remove_oracle`
    /// returns the contract to an admin-only liquidity feed.
    ///
    /// Idempotent: removing when no oracle is configured is a clean
    /// no-op. Emits `orac_rm` carrying the previously configured oracle
    /// (`None` on a no-op) so indexers can audit revocations.
    pub fn remove_oracle(env: Env) {
        Self::require_admin(&env);
        let removed: Option<Address> = env.storage().persistent().get(&DataKey::Oracle);
        env.storage().persistent().remove(&DataKey::Oracle);
        env.events().publish((symbol_short!("orac_rm"),), removed);
    }

    /// Set the reported liquidity for a pair (source units).
    ///
    /// Dual-authorized: `caller` must be **either** the admin **or** the
    /// configured oracle, and must `require_auth()`. This implements
    /// least privilege — the frequently rotated oracle key can keep the
    /// liquidity feed fresh without holding governance power. When no
    /// oracle is configured (never set, or revoked via
    /// [`Self::remove_oracle`]) the `Some(caller) != oracle` comparison is
    /// always true, so only the admin is accepted. Any other
    /// caller is rejected with [`RouterError::NotAuthorized`].
    ///
    /// Requires the pair to already be registered via
    /// [`Self::register_pair`]; rejects an unregistered pair with
    /// [`RouterError::PairNotRegistered`] (#5) so liquidity can never be
    /// configured for a corridor that was never (or no longer) enabled.
    pub fn set_pair_liquidity(
        env: Env,
        caller: Address,
        source: Symbol,
        destination: Symbol,
        liquidity: i128,
    ) {
        caller.require_auth();
        let admin: Address = env
            .storage()
            .persistent()
            .get(&DataKey::Admin)
            .unwrap_or_else(|| panic_with_error!(&env, RouterError::NotInitialized));
        let oracle: Option<Address> = env.storage().persistent().get(&DataKey::Oracle);
        if caller != admin && Some(caller.clone()) != oracle {
            panic_with_error!(&env, RouterError::NotAuthorized);
        }
        if liquidity < 0 {
            panic_with_error!(&env, RouterError::AmountMustBePositive);
        }
        Self::require_pair_registered(&env, &source, &destination);
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
    ///
    /// Requires the pair to already be registered via
    /// [`Self::register_pair`]; rejects an unregistered pair with
    /// [`RouterError::PairNotRegistered`] (#5) so the maximum can never be
    /// configured for a corridor that was never (or no longer) enabled.
    pub fn set_pair_max_amount(env: Env, source: Symbol, destination: Symbol, max_amount: i128) {
        Self::require_admin(&env);
        if max_amount <= 0 {
            panic_with_error!(&env, RouterError::AmountMustBePositive);
        }
        Self::require_pair_registered(&env, &source, &destination);
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
    ///
    /// Requires the pair to already be registered via
    /// [`Self::register_pair`]; rejects an unregistered pair with
    /// [`RouterError::PairNotRegistered`] (#5) so the minimum can never be
    /// configured for a corridor that was never (or no longer) enabled.
    pub fn set_pair_min_amount(env: Env, source: Symbol, destination: Symbol, min_amount: i128) {
        Self::require_admin(&env);
        if min_amount < 0 {
            panic_with_error!(&env, RouterError::AmountMustBePositive);
        }
        Self::require_pair_registered(&env, &source, &destination);
        env.storage()
            .persistent()
            .set(&DataKey::PairMinAmount(source, destination), &min_amount);
    }

    /// Clear all pair-scoped config that should not survive unregister + re-register.
    ///
    /// This intentionally excludes route counters, cumulative volume, and last-route timestamp;
    /// those operational-history slots are tracked separately from live pair configuration.
    fn clear_pair_config(env: &Env, source: Symbol, destination: Symbol) {
        let storage = env.storage().persistent();
        storage.remove(&DataKey::PairFeeBps(source.clone(), destination.clone()));
        storage.remove(&DataKey::PairMinAmount(source.clone(), destination.clone()));
        storage.remove(&DataKey::PairMaxAmount(source.clone(), destination.clone()));
        storage.remove(&DataKey::PairLiquidity(source, destination));
    }

    /// Unregister a previously-registered pair. Admin-gated and idempotent.
    ///
    /// Also clears the pair's fee, min amount, max amount, and liquidity config slots so
    /// re-registering the same corridor starts from documented defaults instead of reviving
    /// stale config.
    pub fn unregister_pair(env: Env, source: Symbol, destination: Symbol) {
        Self::require_admin(&env);
        env.storage()
            .persistent()
            .remove(&DataKey::Pair(source.clone(), destination.clone()));
        Self::clear_pair_config(&env, source.clone(), destination.clone());
        env.events().publish(
            (symbol_short!("unreg"),),
            (source.clone(), destination.clone()),
        );
        env.events()
            .publish((symbol_short!("cfg_clr"),), (source, destination));
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
    ///
    /// Requires the pair to already be registered via
    /// [`Self::register_pair`]; rejects an unregistered pair with
    /// [`RouterError::PairNotRegistered`] (#5) so the fee can never be
    /// configured for a corridor that was never (or no longer) enabled.
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
        Self::require_pair_registered(&env, &source, &destination);
        env.storage().persistent().set(
            &DataKey::PairFeeBps(source.clone(), destination.clone()),
            &fee_bps,
        );
        env.events()
            .publish((symbol_short!("fee_set"),), (source, destination, fee_bps));
    }

    /// Set the routing fee in basis points for multiple registered pairs
    /// in a single admin-gated call. Each entry is validated identically
    /// to [`Self::set_pair_fee_bps`] and gets its own `fee_set` event.
    ///
    /// **All-or-nothing:** if any entry fails validation the entire
    /// transaction is rolled back (Soroban transactions are atomic), so
    /// callers must ensure every entry is well-formed before invoking
    /// this. Requires at least one entry; an empty batch panics with
    /// [`RouterError::EmptyBatch`]. Capped at [`MAX_BATCH_SIZE`] entries;
    /// exceeding it panics with [`RouterError::BatchTooLarge`].
    pub fn set_pair_fees_bps(env: Env, entries: Vec<(Symbol, Symbol, u32)>) {
        if env
            .storage()
            .persistent()
            .get(&DataKey::Paused)
            .unwrap_or(false)
        {
            panic_with_error!(&env, RouterError::ContractPaused);
        }
        Self::require_admin(&env);
        if entries.len() == 0 {
            panic_with_error!(&env, RouterError::EmptyBatch);
        }
        if entries.len() > MAX_BATCH_SIZE {
            panic_with_error!(&env, RouterError::BatchTooLarge);
        }
        for (source, destination, fee_bps) in entries.iter() {
            if fee_bps > MAX_FEE_BPS {
                panic_with_error!(&env, RouterError::FeeBpsTooHigh);
            }
            Self::require_pair_registered(&env, &source, &destination);
            env.storage().persistent().set(
                &DataKey::PairFeeBps(source.clone(), destination.clone()),
                &fee_bps,
            );
            env.events()
                .publish((symbol_short!("fee_set"),), (source, destination, fee_bps));
        }
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
    ///
    /// Honours the emergency stop: while the router is paused this
    /// entrypoint panics with [`RouterError::ContractPaused`] so no route
    /// can be recorded (the `TotalRoutesAllTime` counter, the
    /// `PairLastRouteAt` stamp, and the `route` event are all gated). The
    /// read-only `quote_route` is intentionally left available while
    /// paused so integrators can keep planning routes for when the router
    /// resumes.
    ///
    /// # Checks/effects ordering
    ///
    /// Registered-pair, amount-bound, liquidity, and cooldown guards all
    /// pass before any route business effect is applied. Only after those
    /// checks does the function debit liquidity, update counters and
    /// timestamps, and emit route events.
    ///
    /// # Liquidity consumption
    ///
    /// After passing all pre-condition checks, the function debits `amount`
    /// from the stored `PairLiquidity` via saturating subtraction. If the
    /// liquidity slot is unset (i.e. reads as `i128::MAX` — the unbounded
    /// sentinel) the decrement is skipped entirely, preserving the "no
    /// oracle configured" behaviour. When a decrement does occur a
    /// `liq_used` event carrying `(source, destination, remaining_liquidity)`
    /// is emitted. The slot TTL is extended on each write.
    pub fn compute_route_fee(env: Env, source: Symbol, destination: Symbol, amount: i128) -> i128 {
        if env
            .storage()
            .persistent()
            .get(&DataKey::Paused)
            .unwrap_or(false)
        {
            panic_with_error!(&env, RouterError::ContractPaused);
        }
        if amount <= 0 {
            panic_with_error!(env, RouterError::AmountMustBePositive);
        }

        // CHECKS: all state-dependent preconditions stay read-only so a
        // rejected route leaves no storage write or event behind.
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

        // Acquire the reentrancy lock only after all route guards pass,
        // immediately before the write/event phase.
        Self::enter_nonreentrant(&env);

        // EFFECTS: after all route guards above have passed, debit
        // liquidity, write counters/timestamps, and emit events.
        // When no oracle has set a liquidity value the pair is treated as
        // unbounded — no decrement and no liq_used event are emitted.
        if liquidity != i128::MAX {
            let remaining = liquidity.saturating_sub(amount);
            env.storage().persistent().set(
                &DataKey::PairLiquidity(source.clone(), destination.clone()),
                &remaining,
            );
            env.events().publish(
                (symbol_short!("liq_used"),),
                (source.clone(), destination.clone(), remaining),
            );
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
        let fee = amount
            .checked_mul(fee_bps as i128)
            .map(|n| n / BPS_DENOMINATOR)
            .unwrap_or(0);
        let fee = Self::apply_fee_cap(&env, fee);
        Self::exit_nonreentrant(&env);
        fee
    }

    /// Compute a deterministic, direction-sensitive route identifier for a
    /// `(source, destination)` pair.
    ///
    /// The tag is `keccak256(xdr(source) || xdr(destination))`: a stable
    /// 32-byte digest that depends on the encoded inputs in order. Properties:
    ///
    /// - **Deterministic** — the same `(source, destination)` always hashes to
    ///   the same value, so an off-chain backend can recompute it and correlate
    ///   on-chain routes without storing a mapping.
    /// - **Direction-sensitive** — `source` is hashed before `destination`, so
    ///   `route_tag(USDC, EURC) != route_tag(EURC, USDC)`. Each leg of a pair
    ///   gets its own identifier.
    ///
    /// Returns the digest as a [`BytesN<32>`].
    pub fn route_tag(env: Env, source: Symbol, destination: Symbol) -> BytesN<32> {
        // Build the pre-image deterministically: the XDR encoding of `source`
        // followed by the XDR encoding of `destination`. Ordering the appends
        // this way is what makes the tag direction-sensitive.
        let mut buf = Bytes::new(&env);
        buf.append(&source.to_xdr(&env));
        buf.append(&destination.to_xdr(&env));
        env.crypto().keccak256(&buf).to_bytes()
    }

    /// Replace the contract's WASM in-place so the router can be patched
    /// without losing pair state. Admin-gated; emits an `upgraded` event
    /// carrying the new hash so indexers and watchers can audit upgrades.
    ///
    /// ## Trade-off: not paused-gated
    ///
    /// An emergency pause should arguably still allow the admin to deploy a
    /// fix. We therefore skip the `ContractPaused` check — a paused router
    /// can be upgraded, which is consistent with fixing the bug that caused
    /// the pause. The admin can already unpause, so there is no escalation
    /// path through this exception.
    pub fn upgrade(env: Env, new_wasm_hash: BytesN<32>) {
        Self::require_admin(&env);
        env.events()
            .publish((symbol_short!("upgraded"),), &new_wasm_hash);
        env.deployer().update_current_contract_wasm(new_wasm_hash);
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use proptest::prelude::*;
    use soroban_sdk::{
        symbol_short,
        testutils::{Address as _, Events, Ledger},
        IntoVal,
    };

    /// Register a USDC→EURC pair with `fee_bps` and unbounded liquidity,
    /// returning a ready client. Shared by the property tests below.
    fn setup_pair_with_fee(env: &Env, fee_bps: u32) -> StableRouteRouterClient<'_> {
        let (client, _admin) = setup_initialized(env);
        client.register_pair(&symbol_short!("USDC"), &symbol_short!("EURC"));
        client.set_pair_fee_bps(&symbol_short!("USDC"), &symbol_short!("EURC"), &fee_bps);
        client
    }

    proptest! {
        // Fixed case count keeps CI deterministic and fast.
        #![proptest_config(ProptestConfig { cases: 96, ..ProptestConfig::default() })]

        /// Invariant: the fee never exceeds the routed amount and is never
        /// negative, for any valid fee_bps and amount. `amount * fee_bps`
        /// stays well within i128 (amount < 1e24, fee_bps <= 1000).
        #[test]
        fn prop_fee_within_amount(
            amount in 1i128..1_000_000_000_000_000_000_000_000i128,
            fee_bps in 0u32..=MAX_FEE_BPS,
        ) {
            let env = Env::default();
            let client = setup_pair_with_fee(&env, fee_bps);
            let fee = client.compute_route_fee(
                &symbol_short!("USDC"),
                &symbol_short!("EURC"),
                &amount,
            );
            prop_assert!(fee >= 0);
            prop_assert!(fee <= amount);
        }

        /// Invariant: a zero fee_bps always yields a zero fee.
        #[test]
        fn prop_zero_fee_bps_is_free(
            amount in 1i128..1_000_000_000_000_000_000i128,
        ) {
            let env = Env::default();
            let client = setup_pair_with_fee(&env, 0);
            let fee = client.compute_route_fee(
                &symbol_short!("USDC"),
                &symbol_short!("EURC"),
                &amount,
            );
            prop_assert_eq!(fee, 0);
        }

        /// Invariant: `quote_route` reports the same fee as
        /// `compute_route_fee` for identical config, and fee + net == amount.
        #[test]
        fn prop_quote_matches_compute(
            amount in 1i128..1_000_000_000_000_000_000i128,
            fee_bps in 0u32..=MAX_FEE_BPS,
        ) {
            let env = Env::default();
            let client = setup_pair_with_fee(&env, fee_bps);
            let (quoted_fee, net) = client.quote_route(
                &symbol_short!("USDC"),
                &symbol_short!("EURC"),
                &amount,
            );
            let computed_fee = client.compute_route_fee(
                &symbol_short!("USDC"),
                &symbol_short!("EURC"),
                &amount,
            );
            prop_assert_eq!(quoted_fee, computed_fee);
            prop_assert_eq!(quoted_fee + net, amount);
        }
    }

    /// Deploy the router with `admin` set atomically via the constructor
    /// (`register(StableRouteRouter, (admin,))`) — the front-run-safe path.
    fn setup_initialized(env: &Env) -> (StableRouteRouterClient<'_>, Address) {
        let (client, admin, _id) = setup_initialized_with_id(env);
        (client, admin)
    }

    /// Like [`setup_initialized`] but also returns the contract id so tests
    /// can reach into the contract's own storage via `env.as_contract`.
    fn setup_initialized_with_id(env: &Env) -> (StableRouteRouterClient<'_>, Address, Address) {
        env.mock_all_auths();
        let admin = Address::generate(env);
        let contract_id = env.register(StableRouteRouter, (admin.clone(),));
        let client = StableRouteRouterClient::new(env, &contract_id);
        (client, admin, contract_id)
    }

    /// Register a router without constructor args so legacy pre-init tests can
    /// assert uninitialized admin-gated entrypoints still fail cleanly.
    fn setup_uninitialized(env: &Env) -> StableRouteRouterClient<'_> {
        env.mock_all_auths();
        let contract_id = env.register(StableRouteRouter, ());
        StableRouteRouterClient::new(env, &contract_id)
    }

    #[test]
    fn test_version() {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let contract_id = env.register(StableRouteRouter, (admin,));
        let client = StableRouteRouterClient::new(&env, &contract_id);
        let v = client.version();
        assert_eq!(v, symbol_short!("ROUTER_V2"));
    }

    #[test]
    fn test_route_tag() {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let contract_id = env.register(StableRouteRouter, (admin,));
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

    // --- reentrancy guard ---

    /// The normal success path must RELEASE the reentrancy lock, so two
    /// consecutive `compute_route_fee` calls on the same pair both succeed.
    /// If the lock leaked, the second call would panic with #16.
    #[test]
    fn test_compute_route_fee_releases_lock_for_consecutive_calls() {
        let env = Env::default();
        let (client, _admin) = setup_initialized(&env);
        client.register_pair(&symbol_short!("USDC"), &symbol_short!("EURC"));
        client.set_pair_fee_bps(&symbol_short!("USDC"), &symbol_short!("EURC"), &50u32);

        let first = client.compute_route_fee(
            &symbol_short!("USDC"),
            &symbol_short!("EURC"),
            &1_000_000_i128,
        );
        let second = client.compute_route_fee(
            &symbol_short!("USDC"),
            &symbol_short!("EURC"),
            &1_000_000_i128,
        );

        assert_eq!(first, 5_000);
        assert_eq!(second, 5_000);
        assert_eq!(client.get_total_routes_all_time(), 2);
    }

    /// When the reentrancy lock is already held, `compute_route_fee` must
    /// reject the call with ReentrantCall (#16). We simulate the in-flight
    /// state by setting the lock directly in the contract's storage, which
    /// is exactly what a re-entrant inner call would observe.
    #[test]
    #[should_panic(expected = "Error(Contract, #16)")]
    fn test_compute_route_fee_rejects_reentry() {
        let env = Env::default();
        let (client, _admin, contract_id) = setup_initialized_with_id(&env);
        client.register_pair(&symbol_short!("USDC"), &symbol_short!("EURC"));
        client.set_pair_fee_bps(&symbol_short!("USDC"), &symbol_short!("EURC"), &50u32);

        // Simulate the lock being already held (as it would be mid-call).
        env.as_contract(&contract_id, || {
            env.storage()
                .persistent()
                .set(&DataKey::ReentrancyLock, &true);
        });

        client.compute_route_fee(
            &symbol_short!("USDC"),
            &symbol_short!("EURC"),
            &1_000_000_i128,
        );
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

    /// The emergency stop must block route accounting: while paused,
    /// `compute_route_fee` panics with `ContractPaused` (#9) and never
    /// touches the counter / timestamp.
    #[test]
    #[should_panic(expected = "Error(Contract, #9)")]
    fn test_compute_route_fee_rejects_when_paused() {
        let env = Env::default();
        let (client, _admin) = setup_initialized(&env);
        client.register_pair(&symbol_short!("USDC"), &symbol_short!("EURC"));
        client.set_pair_fee_bps(&symbol_short!("USDC"), &symbol_short!("EURC"), &50u32);
        client.pause();
        client.compute_route_fee(
            &symbol_short!("USDC"),
            &symbol_short!("EURC"),
            &1_000_000_i128,
        );
    }

    /// Routing resumes cleanly after an unpause, and no route was recorded
    /// during the paused window.
    #[test]
    fn test_compute_route_fee_resumes_after_unpause() {
        let env = Env::default();
        let (client, _admin) = setup_initialized(&env);
        client.register_pair(&symbol_short!("USDC"), &symbol_short!("EURC"));
        client.set_pair_fee_bps(&symbol_short!("USDC"), &symbol_short!("EURC"), &50u32);
        client.pause();
        client.unpause();
        let fee = client.compute_route_fee(
            &symbol_short!("USDC"),
            &symbol_short!("EURC"),
            &1_000_000_i128,
        );
        assert_eq!(fee, 5_000);
        assert_eq!(client.get_total_routes_all_time(), 1);
    }

    /// Read-only quotes stay available while paused (documented policy:
    /// block state-mutating routes, keep quotes open for planning).
    #[test]
    fn test_quote_route_allowed_while_paused() {
        let env = Env::default();
        let (client, _admin) = setup_initialized(&env);
        client.register_pair(&symbol_short!("USDC"), &symbol_short!("EURC"));
        client.set_pair_fee_bps(&symbol_short!("USDC"), &symbol_short!("EURC"), &100u32);
        client.pause();
        assert_eq!(
            client.quote_route(&symbol_short!("USDC"), &symbol_short!("EURC"), &1_000i128),
            (10, 990)
        );
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
        assert_eq!(client.get_pending_admin_eta(), None);
    }

    // --- #21: governance timelock ---

    /// Timelock defaults to 0 (instant handover) when unset.
    #[test]
    fn test_timelock_defaults_to_zero() {
        let env = Env::default();
        let (client, _admin) = setup_initialized(&env);
        assert_eq!(client.get_timelock(), 0);
    }

    /// With a delay set, accepting the handover before the eta is rejected
    /// with TimelockNotElapsed (#14).
    #[test]
    #[should_panic(expected = "Error(Contract, #14)")]
    fn test_timelock_blocks_early_accept() {
        let env = Env::default();
        env.ledger().set_timestamp(1_000);
        let (client, _admin) = setup_initialized(&env);
        client.set_timelock(&100);
        let next_admin = Address::generate(&env);
        client.propose_admin_transfer(&next_admin);
        assert_eq!(client.get_pending_admin_eta(), Some(1_100));
        client.accept_admin_transfer(&next_admin); // still at t=1_000
    }

    /// After the delay elapses, the handover executes normally.
    #[test]
    fn test_timelock_allows_accept_after_delay() {
        let env = Env::default();
        env.ledger().set_timestamp(1_000);
        let (client, _admin) = setup_initialized(&env);
        client.set_timelock(&100);
        let next_admin = Address::generate(&env);
        client.propose_admin_transfer(&next_admin);
        env.ledger().set_timestamp(1_100);
        client.accept_admin_transfer(&next_admin);
        assert_eq!(client.get_admin(), Some(next_admin));
        assert_eq!(client.get_pending_admin_eta(), None);
    }

    /// Cancelling a queued transfer clears both the pending admin and eta.
    #[test]
    fn test_timelock_cancel_clears_queue() {
        let env = Env::default();
        env.ledger().set_timestamp(1_000);
        let (client, _admin) = setup_initialized(&env);
        client.set_timelock(&100);
        let next_admin = Address::generate(&env);
        client.propose_admin_transfer(&next_admin);
        client.cancel_admin_transfer();
        assert_eq!(client.get_pending_admin(), None);
        assert_eq!(client.get_pending_admin_eta(), None);
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
    fn test_pair_lifecycle_events_have_exact_payloads_and_counts() {
        let env = Env::default();
        let (client, admin) = setup_initialized(&env);
        let src = symbol_short!("USDC");
        let dest = symbol_short!("EURC");

        let init_payloads = event_payloads(&env, symbol_short!("init"));
        assert_eq!(init_payloads.len(), 1, "constructor emits one init event");
        let init_admin: Address = soroban_sdk::TryFromVal::try_from_val(&env, &init_payloads[0])
            .expect("init event data decodes to admin address");
        assert_eq!(init_admin, admin);

        client.register_pair(&src, &dest);
        let pair_reg_payloads = event_payloads(&env, symbol_short!("pair_reg"));
        assert_eq!(
            pair_reg_payloads.len(),
            1,
            "register_pair emits one pair_reg event"
        );
        let pair_reg: (Symbol, Symbol) =
            soroban_sdk::TryFromVal::try_from_val(&env, &pair_reg_payloads[0])
                .expect("pair_reg event data decodes to pair tuple");
        assert_eq!(pair_reg, (src.clone(), dest.clone()));

        client.set_pair_fee_bps(&src, &dest, &25u32);
        let fee_set_payloads = event_payloads(&env, symbol_short!("fee_set"));
        assert_eq!(
            fee_set_payloads.len(),
            1,
            "set_pair_fee_bps emits one fee_set event"
        );
        let fee_set: (Symbol, Symbol, u32) =
            soroban_sdk::TryFromVal::try_from_val(&env, &fee_set_payloads[0])
                .expect("fee_set event data decodes to pair and fee");
        assert_eq!(fee_set, (src.clone(), dest.clone(), 25u32));

        client.set_pair_liquidity(&admin, &src, &dest, &1_000i128);
        let liq_set_payloads = event_payloads(&env, symbol_short!("liq_set"));
        assert_eq!(
            liq_set_payloads.len(),
            1,
            "set_pair_liquidity emits one liq_set event"
        );
        let liq_set: (Symbol, Symbol, i128) =
            soroban_sdk::TryFromVal::try_from_val(&env, &liq_set_payloads[0])
                .expect("liq_set event data decodes to pair and liquidity");
        assert_eq!(liq_set, (src.clone(), dest.clone(), 1_000i128));

        client.unregister_pair(&src, &dest);
        let unreg_payloads = event_payloads(&env, symbol_short!("unreg"));
        assert_eq!(
            unreg_payloads.len(),
            1,
            "unregister_pair emits one unreg event"
        );
        let unreg: (Symbol, Symbol) =
            soroban_sdk::TryFromVal::try_from_val(&env, &unreg_payloads[0])
                .expect("unreg event data decodes to pair tuple");
        assert_eq!(unreg, (src.clone(), dest.clone()));

        let cfg_clr_payloads = event_payloads(&env, symbol_short!("cfg_clr"));
        assert_eq!(
            cfg_clr_payloads.len(),
            1,
            "unregister_pair emits one cfg_clr companion event"
        );
        let cfg_clr: (Symbol, Symbol) =
            soroban_sdk::TryFromVal::try_from_val(&env, &cfg_clr_payloads[0])
                .expect("cfg_clr event data decodes to pair tuple");
        assert_eq!(cfg_clr, (src, dest));
    }

    #[test]
    fn test_unregister_never_registered_pair_is_clean_noop_with_event() {
        let env = Env::default();
        let (client, _admin) = setup_initialized(&env);
        let src = symbol_short!("USDC");
        let dest = symbol_short!("EURC");

        assert!(!client.is_pair_registered(&src, &dest));
        client.unregister_pair(&src, &dest);

        let unreg_payloads = event_payloads(&env, symbol_short!("unreg"));
        assert_eq!(
            unreg_payloads.len(),
            1,
            "no-op unregister still documents one lifecycle event"
        );
        let unreg: (Symbol, Symbol) =
            soroban_sdk::TryFromVal::try_from_val(&env, &unreg_payloads[0])
                .expect("unreg event data decodes to pair tuple");
        assert_eq!(unreg, (src.clone(), dest.clone()));
        let cfg_clr_payloads = event_payloads(&env, symbol_short!("cfg_clr"));
        assert_eq!(
            cfg_clr_payloads.len(),
            1,
            "no-op unregister still documents one config clear event"
        );
        let cfg_clr: (Symbol, Symbol) =
            soroban_sdk::TryFromVal::try_from_val(&env, &cfg_clr_payloads[0])
                .expect("cfg_clr event data decodes to pair tuple");
        assert_eq!(cfg_clr, (src, dest));
        assert!(!client.is_pair_registered(&symbol_short!("USDC"), &symbol_short!("EURC")));
    }

    #[test]
    fn test_reregister_after_unregister_restores_pair_with_clean_config_defaults() {
        let env = Env::default();
        let (client, admin) = setup_initialized(&env);
        let src = symbol_short!("USDC");
        let dest = symbol_short!("EURC");

        client.register_pair(&src, &dest);
        assert_eq!(
            event_payloads(&env, symbol_short!("pair_reg")).len(),
            1,
            "initial register should emit one pair_reg event"
        );
        client.set_pair_fee_bps(&src, &dest, &42u32);
        client.set_pair_min_amount(&src, &dest, &10i128);
        client.set_pair_max_amount(&src, &dest, &1_000i128);
        client.set_pair_liquidity(&admin, &src, &dest, &500i128);
        client.unregister_pair(&src, &dest);
        assert_eq!(
            event_payloads(&env, symbol_short!("unreg")).len(),
            1,
            "single unregister should emit one unreg event"
        );

        assert!(!client.is_pair_registered(&src, &dest));
        assert_eq!(client.get_pair_fee_bps(&src, &dest), 0);
        assert_eq!(client.get_pair_min_amount(&src, &dest), 0);
        assert_eq!(client.get_pair_max_amount(&src, &dest), i128::MAX);
        assert_eq!(client.get_pair_liquidity(&src, &dest), 0);

        client.register_pair(&src, &dest);
        assert_eq!(
            event_payloads(&env, symbol_short!("pair_reg")).len(),
            1,
            "re-register should emit one pair_reg event"
        );

        assert!(client.is_pair_registered(&src, &dest));
        assert_eq!(client.get_pair_fee_bps(&src, &dest), 0);
        assert_eq!(client.get_pair_min_amount(&src, &dest), 0);
        assert_eq!(client.get_pair_max_amount(&src, &dest), i128::MAX);
        assert_eq!(client.get_pair_liquidity(&src, &dest), 0);
    }

    #[test]
    fn test_pair_limits_liquidity_and_info_round_trip() {
        let env = Env::default();
        let (client, admin) = setup_initialized(&env);
        client.register_pair(&symbol_short!("USDC"), &symbol_short!("EURC"));
        assert!(!client.is_pair_active(&symbol_short!("USDC"), &symbol_short!("EURC")));

        client.set_pair_fee_bps(&symbol_short!("USDC"), &symbol_short!("EURC"), &25u32);
        client.set_pair_min_amount(&symbol_short!("USDC"), &symbol_short!("EURC"), &10i128);
        client.set_pair_max_amount(&symbol_short!("USDC"), &symbol_short!("EURC"), &1_000i128);
        client.set_pair_liquidity(
            &admin,
            &symbol_short!("USDC"),
            &symbol_short!("EURC"),
            &500i128,
        );

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
        let (client, admin) = setup_initialized(&env);
        client.register_pair(&symbol_short!("USDC"), &symbol_short!("EURC"));
        client.set_pair_liquidity(
            &admin,
            &symbol_short!("USDC"),
            &symbol_short!("EURC"),
            &10i128,
        );
        client.compute_route_fee(&symbol_short!("USDC"), &symbol_short!("EURC"), &11i128);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #6)")]
    fn test_set_pair_liquidity_rejects_negative_value() {
        let env = Env::default();
        let (client, admin) = setup_initialized(&env);
        client.set_pair_liquidity(
            &admin,
            &symbol_short!("USDC"),
            &symbol_short!("EURC"),
            &-1i128,
        );
    }

    // --- #22: scoped oracle role ---

    /// The oracle (a non-admin) can update pair liquidity.
    #[test]
    fn test_oracle_can_update_liquidity() {
        let env = Env::default();
        let (client, _admin) = setup_initialized(&env);
        let oracle = Address::generate(&env);
        client.set_oracle(&oracle);
        assert_eq!(client.get_oracle(), Some(oracle.clone()));
        client.register_pair(&symbol_short!("USDC"), &symbol_short!("EURC"));
        client.set_pair_liquidity(
            &oracle,
            &symbol_short!("USDC"),
            &symbol_short!("EURC"),
            &777i128,
        );
        assert_eq!(
            client.get_pair_liquidity(&symbol_short!("USDC"), &symbol_short!("EURC")),
            777
        );
    }

    /// Admin retains the ability to update liquidity directly.
    #[test]
    fn test_admin_can_still_update_liquidity() {
        let env = Env::default();
        let (client, admin) = setup_initialized(&env);
        let oracle = Address::generate(&env);
        client.set_oracle(&oracle);
        client.register_pair(&symbol_short!("USDC"), &symbol_short!("EURC"));
        client.set_pair_liquidity(
            &admin,
            &symbol_short!("USDC"),
            &symbol_short!("EURC"),
            &10i128,
        );
        assert_eq!(
            client.get_pair_liquidity(&symbol_short!("USDC"), &symbol_short!("EURC")),
            10
        );
    }

    /// A caller that is neither admin nor oracle is rejected with
    /// NotAuthorized (#15).
    #[test]
    #[should_panic(expected = "Error(Contract, #15)")]
    fn test_random_caller_cannot_update_liquidity() {
        let env = Env::default();
        let (client, _admin) = setup_initialized(&env);
        let stranger = Address::generate(&env);
        client.set_pair_liquidity(
            &stranger,
            &symbol_short!("USDC"),
            &symbol_short!("EURC"),
            &1i128,
        );
    }

    /// The oracle role is strictly scoped: the oracle cannot set the
    /// oracle (an admin-only governance action).
    #[test]
    #[should_panic]
    fn test_oracle_cannot_call_admin_entrypoint() {
        let env = Env::default();
        let admin = Address::generate(&env);
        let oracle = Address::generate(&env);
        env.mock_all_auths();
        let contract_id = env.register(StableRouteRouter, (admin.clone(),));
        let client = StableRouteRouterClient::new(&env, &contract_id);
        client.set_oracle(&oracle);
        // Oracle attempts an admin-only action (pause). Authorize only the
        // oracle so admin.require_auth() must fail.
        env.mock_auths(&[soroban_sdk::testutils::MockAuth {
            address: &oracle,
            invoke: &soroban_sdk::testutils::MockAuthInvoke {
                contract: &contract_id,
                fn_name: "pause",
                args: ().into_val(&env),
                sub_invokes: &[],
            },
        }]);
        client.pause();
    }

    // --- oracle revocation (remove_oracle) ---

    /// End-to-end revocation flow: the oracle can update liquidity while
    /// configured, and is fully locked out after `remove_oracle`.
    #[test]
    fn test_oracle_can_update_before_removal_but_not_after() {
        let env = Env::default();
        let (client, _admin) = setup_initialized(&env);
        let oracle = Address::generate(&env);
        client.set_oracle(&oracle);
        client.register_pair(&symbol_short!("USDC"), &symbol_short!("EURC"));

        // Before removal the oracle drives the liquidity feed.
        client.set_pair_liquidity(
            &oracle,
            &symbol_short!("USDC"),
            &symbol_short!("EURC"),
            &500i128,
        );
        assert_eq!(
            client.get_pair_liquidity(&symbol_short!("USDC"), &symbol_short!("EURC")),
            500
        );

        client.remove_oracle();
        assert_eq!(client.get_oracle(), None);

        // After removal the same key is rejected with NotAuthorized (#15).
        let err = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            client.set_pair_liquidity(
                &oracle,
                &symbol_short!("USDC"),
                &symbol_short!("EURC"),
                &999i128,
            )
        }));
        assert!(err.is_err());
        // The blocked call left the last accepted value untouched.
        assert_eq!(
            client.get_pair_liquidity(&symbol_short!("USDC"), &symbol_short!("EURC")),
            500
        );
    }

    /// A revoked oracle is rejected with exactly NotAuthorized (#15) —
    /// the same code any other stranger gets.
    #[test]
    #[should_panic(expected = "Error(Contract, #15)")]
    fn test_removed_oracle_rejected_with_not_authorized() {
        let env = Env::default();
        let (client, _admin) = setup_initialized(&env);
        let oracle = Address::generate(&env);
        client.set_oracle(&oracle);
        client.register_pair(&symbol_short!("USDC"), &symbol_short!("EURC"));
        client.remove_oracle();
        client.set_pair_liquidity(
            &oracle,
            &symbol_short!("USDC"),
            &symbol_short!("EURC"),
            &1i128,
        );
    }

    /// The admin keeps the liquidity feed after the oracle is revoked:
    /// the dual-auth check degrades to admin-only when the slot is absent.
    #[test]
    fn test_admin_can_still_update_liquidity_after_removal() {
        let env = Env::default();
        let (client, admin) = setup_initialized(&env);
        let oracle = Address::generate(&env);
        client.set_oracle(&oracle);
        client.register_pair(&symbol_short!("USDC"), &symbol_short!("EURC"));
        client.remove_oracle();
        client.set_pair_liquidity(
            &admin,
            &symbol_short!("USDC"),
            &symbol_short!("EURC"),
            &42i128,
        );
        assert_eq!(
            client.get_pair_liquidity(&symbol_short!("USDC"), &symbol_short!("EURC")),
            42
        );
    }

    /// Removal is idempotent: removing when a previous removal (or nothing)
    /// left the slot empty is a clean no-op and the getter stays None.
    #[test]
    fn test_remove_oracle_is_idempotent() {
        let env = Env::default();
        let (client, _admin) = setup_initialized(&env);
        // Remove when never set — clean no-op.
        assert_eq!(client.get_oracle(), None);
        client.remove_oracle();
        assert_eq!(client.get_oracle(), None);
        // Set, remove, then remove again — second removal is also a no-op.
        let oracle = Address::generate(&env);
        client.set_oracle(&oracle);
        client.remove_oracle();
        client.remove_oracle();
        assert_eq!(client.get_oracle(), None);
    }

    /// `remove_oracle` emits one `orac_rm` event per call, carrying the
    /// previously configured oracle (`None` on a no-op removal).
    #[test]
    fn test_remove_oracle_emits_orac_rm_event() {
        let env = Env::default();
        let (client, _admin) = setup_initialized(&env);
        let oracle = Address::generate(&env);
        client.set_oracle(&oracle);
        client.remove_oracle();
        let payloads = event_payloads(&env, symbol_short!("orac_rm"));
        assert_eq!(payloads.len(), 1, "remove_oracle emits one orac_rm event");
        let removed: Option<Address> = soroban_sdk::TryFromVal::try_from_val(&env, &payloads[0])
            .expect("orac_rm event data decodes to Option<Address>");
        assert_eq!(removed, Some(oracle));
    }

    /// A no-op removal still emits `orac_rm`, with `None` as the payload,
    /// so indexers observe every revocation attempt.
    #[test]
    fn test_remove_oracle_noop_emits_event_with_none() {
        let env = Env::default();
        let (client, _admin) = setup_initialized(&env);
        client.remove_oracle();
        let payloads = event_payloads(&env, symbol_short!("orac_rm"));
        assert_eq!(payloads.len(), 1);
        let removed: Option<Address> = soroban_sdk::TryFromVal::try_from_val(&env, &payloads[0])
            .expect("orac_rm event data decodes to Option<Address>");
        assert_eq!(removed, None);
    }

    /// After removal the oracle can be set again (rotation to a fresh key
    /// once the incident is resolved).
    #[test]
    fn test_oracle_can_be_reconfigured_after_removal() {
        let env = Env::default();
        let (client, _admin) = setup_initialized(&env);
        let compromised = Address::generate(&env);
        client.set_oracle(&compromised);
        client.remove_oracle();
        let fresh = Address::generate(&env);
        client.set_oracle(&fresh);
        assert_eq!(client.get_oracle(), Some(fresh.clone()));
        client.register_pair(&symbol_short!("USDC"), &symbol_short!("EURC"));
        client.set_pair_liquidity(
            &fresh,
            &symbol_short!("USDC"),
            &symbol_short!("EURC"),
            &7i128,
        );
        assert_eq!(
            client.get_pair_liquidity(&symbol_short!("USDC"), &symbol_short!("EURC")),
            7
        );
    }

    /// `remove_oracle` is admin-gated: a caller without the admin's auth
    /// is rejected, so a compromised oracle cannot un-revoke itself or
    /// grief the admin by clearing the slot.
    #[test]
    #[should_panic]
    fn test_remove_oracle_requires_admin_auth() {
        let env = Env::default();
        let admin = Address::generate(&env);
        let oracle = Address::generate(&env);
        env.mock_all_auths();
        let contract_id = env.register(StableRouteRouter, (admin.clone(),));
        let client = StableRouteRouterClient::new(&env, &contract_id);
        client.set_oracle(&oracle);
        // Authorize only the oracle so admin.require_auth() must fail.
        env.mock_auths(&[soroban_sdk::testutils::MockAuth {
            address: &oracle,
            invoke: &soroban_sdk::testutils::MockAuthInvoke {
                contract: &contract_id,
                fn_name: "remove_oracle",
                args: ().into_val(&env),
                sub_invokes: &[],
            },
        }]);
        client.remove_oracle();
    }

    /// Missing-admin path reuses NotInitialized (#2), like every other
    /// admin-gated entrypoint.
    #[test]
    #[should_panic(expected = "Error(Contract, #2)")]
    fn test_remove_oracle_panics_when_uninitialized() {
        let env = Env::default();
        let (client, _admin, contract_id) = setup_initialized_with_id(&env);
        env.as_contract(&contract_id, || {
            env.storage().persistent().remove(&DataKey::Admin);
        });
        client.remove_oracle();
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

    // --- compute_route_fee side-effect tests ---

    /// Register `(source, destination)` and set its fee so that
    /// `compute_route_fee` clears every guard (pair registered, fee set,
    /// min/max unset → permissive, liquidity unset → defaults to i128::MAX).
    /// Returns the live client for chaining assertions.
    fn setup_routable_pair<'a>(
        env: &'a Env,
        source: &Symbol,
        destination: &Symbol,
        fee_bps: u32,
    ) -> StableRouteRouterClient<'a> {
        let (client, _admin) = setup_initialized(env);
        client.register_pair(source, destination);
        client.set_pair_fee_bps(source, destination, &fee_bps);
        client
    }

    /// Scan the test-host's current contract events and return the decoded
    /// `data` payloads of every event whose single topic matches `topic`.
    fn event_payloads(env: &Env, topic: Symbol) -> std::vec::Vec<soroban_sdk::Val> {
        use soroban_sdk::{
            xdr::{ContractEventBody, ScVal},
            TryFromVal, Val,
        };
        env.events()
            .all()
            .events()
            .iter()
            .filter_map(|event| {
                let ContractEventBody::V0(body) = &event.body;
                let topics = body.topics.as_slice();
                if topics.len() != 1 {
                    return None;
                }
                let ScVal::Symbol(raw_topic) = &topics[0] else {
                    return None;
                };
                let actual_topic =
                    Symbol::try_from_val(env, raw_topic).expect("event topic decodes to Symbol");
                if actual_topic == topic {
                    Some(Val::try_from_val(env, &body.data).expect("event data decodes to Val"))
                } else {
                    None
                }
            })
            .collect()
    }

    fn route_event_payloads(env: &Env) -> std::vec::Vec<soroban_sdk::Val> {
        event_payloads(env, symbol_short!("route"))
    }

    #[test]
    fn test_compute_route_fee_emits_route_event_with_payload() {
        let env = Env::default();
        let src = symbol_short!("USDC");
        let dest = symbol_short!("EURC");
        let amount = 1_000_000_i128;
        let client = setup_routable_pair(&env, &src, &dest, 50u32);

        client.compute_route_fee(&src, &dest, &amount);

        // Exactly one `route` event, carrying (source, destination, amount).
        let payloads = route_event_payloads(&env);
        assert_eq!(payloads.len(), 1, "exactly one route event expected");
        let decoded: (Symbol, Symbol, i128) =
            soroban_sdk::TryFromVal::try_from_val(&env, &payloads[0])
                .expect("route data decodes to (Symbol, Symbol, i128)");
        assert_eq!(decoded, (src, dest, amount));
    }

    #[test]
    fn test_compute_route_fee_stamps_pair_last_route_at() {
        let env = Env::default();
        let src = symbol_short!("USDC");
        let dest = symbol_short!("EURC");
        let client = setup_routable_pair(&env, &src, &dest, 50u32);

        // None before any route touches the pair.
        assert_eq!(client.get_pair_last_route_at(&src, &dest), None);

        env.ledger().set_timestamp(12345);
        client.compute_route_fee(&src, &dest, &1_000_i128);

        assert_eq!(client.get_pair_last_route_at(&src, &dest), Some(12345));
    }

    #[test]
    fn test_compute_route_fee_counter_is_global_across_pairs() {
        let env = Env::default();
        // Pair A.
        let a_src = symbol_short!("USDC");
        let a_dest = symbol_short!("EURC");
        let client = setup_routable_pair(&env, &a_src, &a_dest, 50u32);
        // Pair B (different pair) registered on the same contract instance.
        let b_src = symbol_short!("XLM");
        let b_dest = symbol_short!("USDC");
        client.register_pair(&b_src, &b_dest);
        client.set_pair_fee_bps(&b_src, &b_dest, &50u32);

        assert_eq!(client.get_total_routes_all_time(), 0);
        client.compute_route_fee(&a_src, &a_dest, &1_000_i128);
        assert_eq!(client.get_total_routes_all_time(), 1);
        client.compute_route_fee(&b_src, &b_dest, &1_000_i128);
        // The lifetime counter is protocol-wide, not per-pair.
        assert_eq!(client.get_total_routes_all_time(), 2);
    }

    #[test]
    fn test_quote_route_does_not_mutate_counter_or_emit_route_event() {
        let env = Env::default();
        let src = symbol_short!("USDC");
        let dest = symbol_short!("EURC");
        let client = setup_routable_pair(&env, &src, &dest, 100u32);

        let routes_before = client.get_total_routes_all_time();
        let route_events_before = route_event_payloads(&env).len();

        let (fee, net) = client.quote_route(&src, &dest, &1_000_i128);
        assert_eq!((fee, net), (10, 990));

        // quote_route is read-only: counter unchanged, no new `route` event.
        assert_eq!(client.get_total_routes_all_time(), routes_before);
        assert_eq!(route_event_payloads(&env).len(), route_events_before);
    }

    // --- require_admin helper contract tests ---

    /// After the refactor, every admin-gated entrypoint must still reject a
    /// non-admin caller. We test `pause` as a representative; the helper is
    /// shared, so this covers all entrypoints structurally.
    #[test]
    #[should_panic]
    fn test_require_admin_rejects_unauthorized_caller() {
        let env = Env::default();
        let admin = Address::generate(&env);
        let attacker = Address::generate(&env);
        // Deploy with the real admin set atomically by the constructor.
        env.mock_all_auths();
        let contract_id = env.register(StableRouteRouter, (admin.clone(),));
        let client = StableRouteRouterClient::new(&env, &contract_id);
        // Now call pause as the attacker — only the attacker is authorized,
        // so admin.require_auth() inside pause must fail.
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

    // --- #20: init front-running hardening ---

    /// The constructor sets the admin atomically at deploy time — there is
    /// no deployed-but-uninitialized window.
    #[test]
    fn test_constructor_sets_admin_at_deploy() {
        let env = Env::default();
        let (client, admin) = setup_initialized(&env);
        assert_eq!(client.get_admin(), Some(admin));
    }

    /// An attacker who observes the freshly deployed router cannot seize
    /// the admin role by calling the legacy `init`: it always rejects with
    /// AlreadyInitialized (#1) because the slot is already populated.
    #[test]
    #[should_panic(expected = "Error(Contract, #1)")]
    fn test_attacker_cannot_seize_admin_via_init() {
        let env = Env::default();
        let (client, _admin) = setup_initialized(&env);
        let attacker = Address::generate(&env);
        client.init(&attacker);
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

    /// The constructor requires an admin argument, so tests cannot create the
    /// old deployed-but-uninitialized state with zero constructor args.
    #[test]
    #[should_panic]
    fn test_constructor_rejects_missing_admin_arg() {
        let env = Env::default();
        let _client = setup_uninitialized(&env);
    }

    // --- liquidity consumption model ---

    fn liq_used_event_payloads(env: &Env) -> std::vec::Vec<soroban_sdk::Val> {
        event_payloads(env, symbol_short!("liq_used"))
    }

    fn setup_liquidity_pair(env: &Env) -> (StableRouteRouterClient<'_>, Address, Symbol, Symbol) {
        let (client, admin) = setup_initialized(env);
        let (s, d) = (symbol_short!("USDC"), symbol_short!("EURC"));
        client.register_pair(&s, &d);
        client.set_pair_fee_bps(&s, &d, &50u32);
        (client, admin, s, d)
    }

    #[test]
    fn test_liquidity_decremented_by_amount_after_route() {
        let env = Env::default();
        let (client, admin, s, d) = setup_liquidity_pair(&env);
        client.set_pair_liquidity(&admin, &s, &d, &1_000i128);
        assert_eq!(client.get_pair_liquidity(&s, &d), 1_000);

        let _fee = client.compute_route_fee(&s, &d, &300i128);
        assert_eq!(client.get_pair_liquidity(&s, &d), 700);
    }

    #[test]
    fn test_exact_liquidity_route_consumes_all() {
        let env = Env::default();
        let (client, admin, s, d) = setup_liquidity_pair(&env);
        client.set_pair_liquidity(&admin, &s, &d, &500i128);

        let _fee = client.compute_route_fee(&s, &d, &500i128);
        assert_eq!(client.get_pair_liquidity(&s, &d), 0);
    }

    #[test]
    fn test_repeated_routes_exhaust_liquidity() {
        let env = Env::default();
        let (client, admin, s, d) = setup_liquidity_pair(&env);
        client.set_pair_liquidity(&admin, &s, &d, &100i128);

        client.compute_route_fee(&s, &d, &40i128);
        assert_eq!(client.get_pair_liquidity(&s, &d), 60);

        client.compute_route_fee(&s, &d, &30i128);
        assert_eq!(client.get_pair_liquidity(&s, &d), 30);

        client.compute_route_fee(&s, &d, &30i128);
        assert_eq!(client.get_pair_liquidity(&s, &d), 0);

        let err = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            client.compute_route_fee(&s, &d, &1i128)
        }));
        assert!(err.is_err());
    }

    #[test]
    fn test_insufficient_liquidity_after_partial_consume() {
        let env = Env::default();
        let (client, admin, s, d) = setup_liquidity_pair(&env);
        client.set_pair_liquidity(&admin, &s, &d, &100i128);

        client.compute_route_fee(&s, &d, &60i128);
        assert_eq!(client.get_pair_liquidity(&s, &d), 40);

        let err = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            client.compute_route_fee(&s, &d, &50i128)
        }));
        assert!(err.is_err());
        assert_eq!(client.get_pair_liquidity(&s, &d), 40);
    }

    #[test]
    fn test_unset_liquidity_stays_unbounded() {
        let env = Env::default();
        let (client, _admin, s, d) = setup_liquidity_pair(&env);
        assert_eq!(client.get_pair_liquidity(&s, &d), 0);
        let _fee = client.compute_route_fee(&s, &d, &1_000_000i128);
        assert_eq!(client.get_pair_liquidity(&s, &d), 0);
        assert_eq!(liq_used_event_payloads(&env).len(), 0);
    }

    #[test]
    fn test_liq_used_event_emitted_with_remaining() {
        let env = Env::default();
        let (client, admin, s, d) = setup_liquidity_pair(&env);
        client.set_pair_liquidity(&admin, &s, &d, &1_000i128);

        client.compute_route_fee(&s, &d, &300i128);

        let payloads = liq_used_event_payloads(&env);
        assert_eq!(payloads.len(), 1);
        let decoded: (Symbol, Symbol, i128) =
            soroban_sdk::TryFromVal::try_from_val(&env, &payloads[0])
                .expect("liq_used data decodes to (Symbol, Symbol, i128)");
        assert_eq!(decoded, (s, d, 700i128));
    }

    #[test]
    fn test_oracle_top_up_after_consumption() {
        let env = Env::default();
        let (client, admin, s, d) = setup_liquidity_pair(&env);
        client.set_pair_liquidity(&admin, &s, &d, &500i128);

        client.compute_route_fee(&s, &d, &300i128);
        assert_eq!(client.get_pair_liquidity(&s, &d), 200);

        client.set_pair_liquidity(&admin, &s, &d, &1_000i128);
        assert_eq!(client.get_pair_liquidity(&s, &d), 1_000);

        let _fee = client.compute_route_fee(&s, &d, &800i128);
        assert_eq!(client.get_pair_liquidity(&s, &d), 200);
    }

    #[test]
    fn test_zero_liquidity_after_drain_allows_top_up() {
        let env = Env::default();
        let (client, admin, s, d) = setup_liquidity_pair(&env);
        client.set_pair_liquidity(&admin, &s, &d, &100i128);

        client.compute_route_fee(&s, &d, &100i128);
        assert_eq!(client.get_pair_liquidity(&s, &d), 0);

        let err = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            client.compute_route_fee(&s, &d, &1i128)
        }));
        assert!(err.is_err());

        client.set_pair_liquidity(&admin, &s, &d, &50i128);
        let _fee = client.compute_route_fee(&s, &d, &50i128);
        assert_eq!(client.get_pair_liquidity(&s, &d), 0);
    }

    #[test]
    fn test_liq_used_event_not_emitted_for_unset() {
        let env = Env::default();
        let (client, _admin, s, d) = setup_liquidity_pair(&env);

        let _fee = client.compute_route_fee(&s, &d, &999_999i128);

        assert_eq!(liq_used_event_payloads(&env).len(), 0);
    }

    #[test]
    fn test_liq_used_and_route_both_emitted() {
        let env = Env::default();
        let (client, admin, s, d) = setup_liquidity_pair(&env);
        client.set_pair_liquidity(&admin, &s, &d, &500i128);

        client.compute_route_fee(&s, &d, &200i128);

        let liq_payloads = liq_used_event_payloads(&env);
        assert_eq!(liq_payloads.len(), 1);
        let liq_used: (Symbol, Symbol, i128) =
            soroban_sdk::TryFromVal::try_from_val(&env, &liq_payloads[0])
                .expect("liq_used data decodes");
        assert_eq!(liq_used, (s.clone(), d.clone(), 300i128));

        let route_payloads = route_event_payloads(&env);
        assert_eq!(route_payloads.len(), 1);
        let route: (Symbol, Symbol, i128) =
            soroban_sdk::TryFromVal::try_from_val(&env, &route_payloads[0])
                .expect("route data decodes");
        assert_eq!(route, (s, d, 200i128));
    }

    #[test]
    fn test_cooldown_blocked_route_has_no_business_side_effects() {
        let env = Env::default();
        let (client, admin, s, d) = setup_liquidity_pair(&env);
        client.set_pair_liquidity(&admin, &s, &d, &1_000i128);
        client.set_pair_cooldown(&s, &d, &60u64);

        env.ledger().set_timestamp(1_000);
        client.compute_route_fee(&s, &d, &100i128);

        assert_eq!(client.get_pair_liquidity(&s, &d), 900);
        assert_eq!(client.get_pair_route_count(&s, &d), 1);
        assert_eq!(client.get_pair_volume(&s, &d), 100);
        assert_eq!(client.get_pair_last_route_at(&s, &d), Some(1_000));
        let liq_events_before = liq_used_event_payloads(&env).len();
        let route_events_before = route_event_payloads(&env).len();

        env.ledger().set_timestamp(1_030);
        let err = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            client.compute_route_fee(&s, &d, &200i128)
        }));

        assert!(err.is_err());
        assert_eq!(client.get_pair_liquidity(&s, &d), 900);
        assert_eq!(client.get_pair_route_count(&s, &d), 1);
        assert_eq!(client.get_pair_volume(&s, &d), 100);
        assert_eq!(client.get_pair_last_route_at(&s, &d), Some(1_000));
        assert_eq!(liq_used_event_payloads(&env).len(), liq_events_before);
        assert_eq!(route_event_payloads(&env).len(), route_events_before);
    }

    #[test]
    fn test_multiple_pairs_independent_liquidity() {
        let env = Env::default();
        let (client, admin) = setup_initialized(&env);
        let a_src = symbol_short!("USDC");
        let a_dst = symbol_short!("EURC");
        let b_src = symbol_short!("XLM");
        let b_dst = symbol_short!("USDC");
        client.register_pair(&a_src, &a_dst);
        client.register_pair(&b_src, &b_dst);
        client.set_pair_fee_bps(&a_src, &a_dst, &10u32);
        client.set_pair_fee_bps(&b_src, &b_dst, &10u32);
        client.set_pair_liquidity(&admin, &a_src, &a_dst, &500i128);
        client.set_pair_liquidity(&admin, &b_src, &b_dst, &300i128);

        client.compute_route_fee(&a_src, &a_dst, &200i128);
        assert_eq!(client.get_pair_liquidity(&a_src, &a_dst), 300);
        assert_eq!(client.get_pair_liquidity(&b_src, &b_dst), 300);

        client.compute_route_fee(&b_src, &b_dst, &100i128);
        assert_eq!(client.get_pair_liquidity(&a_src, &a_dst), 300);
        assert_eq!(client.get_pair_liquidity(&b_src, &b_dst), 200);
    }

    #[test]
    fn test_saturating_sub_never_underflows() {
        let env = Env::default();
        let (client, admin, s, d) = setup_liquidity_pair(&env);
        client.set_pair_liquidity(&admin, &s, &d, &1i128);

        let _fee = client.compute_route_fee(&s, &d, &1i128);
        assert_eq!(client.get_pair_liquidity(&s, &d), 0);
    }
}

/// Registration-before-configuration guard. `set_pair_fee_bps`,
/// `set_pair_min_amount`, `set_pair_max_amount`, and `set_pair_liquidity`
/// must reject an unregistered pair with `PairNotRegistered` (#5), succeed
/// once `register_pair` has been called, and reject again after
/// `unregister_pair` removes the registration. This closes the orphan-config
/// hole: an admin can no longer write fee/bounds/liquidity for a corridor
/// that was never (or no longer) an active route.
#[cfg(test)]
mod test_registration_before_configuration {
    use super::*;
    use soroban_sdk::{symbol_short, testutils::Address as _};

    fn setup(env: &Env) -> (StableRouteRouterClient<'_>, Address) {
        env.mock_all_auths();
        let admin = Address::generate(env);
        let id = env.register(StableRouteRouter, (admin.clone(),));
        (StableRouteRouterClient::new(env, &id), admin)
    }

    // --- set_pair_fee_bps ---

    #[test]
    #[should_panic(expected = "Error(Contract, #5)")]
    fn test_set_pair_fee_bps_rejects_unregistered_pair() {
        let env = Env::default();
        let (client, _admin) = setup(&env);
        client.set_pair_fee_bps(&symbol_short!("USDC"), &symbol_short!("EURC"), &50u32);
    }

    #[test]
    fn test_set_pair_fee_bps_succeeds_after_register() {
        let env = Env::default();
        let (client, _admin) = setup(&env);
        let (s, d) = (symbol_short!("USDC"), symbol_short!("EURC"));
        client.register_pair(&s, &d);
        client.set_pair_fee_bps(&s, &d, &50u32);
        assert_eq!(client.get_pair_fee_bps(&s, &d), 50);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #5)")]
    fn test_set_pair_fee_bps_rejects_after_unregister() {
        let env = Env::default();
        let (client, _admin) = setup(&env);
        let (s, d) = (symbol_short!("USDC"), symbol_short!("EURC"));
        client.register_pair(&s, &d);
        client.unregister_pair(&s, &d);
        client.set_pair_fee_bps(&s, &d, &50u32);
    }

    // --- set_pair_min_amount ---

    #[test]
    #[should_panic(expected = "Error(Contract, #5)")]
    fn test_set_pair_min_amount_rejects_unregistered_pair() {
        let env = Env::default();
        let (client, _admin) = setup(&env);
        client.set_pair_min_amount(&symbol_short!("USDC"), &symbol_short!("EURC"), &10i128);
    }

    #[test]
    fn test_set_pair_min_amount_succeeds_after_register() {
        let env = Env::default();
        let (client, _admin) = setup(&env);
        let (s, d) = (symbol_short!("USDC"), symbol_short!("EURC"));
        client.register_pair(&s, &d);
        client.set_pair_min_amount(&s, &d, &10i128);
        assert_eq!(client.get_pair_min_amount(&s, &d), 10);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #5)")]
    fn test_set_pair_min_amount_rejects_after_unregister() {
        let env = Env::default();
        let (client, _admin) = setup(&env);
        let (s, d) = (symbol_short!("USDC"), symbol_short!("EURC"));
        client.register_pair(&s, &d);
        client.unregister_pair(&s, &d);
        client.set_pair_min_amount(&s, &d, &10i128);
    }

    // --- set_pair_max_amount ---

    #[test]
    #[should_panic(expected = "Error(Contract, #5)")]
    fn test_set_pair_max_amount_rejects_unregistered_pair() {
        let env = Env::default();
        let (client, _admin) = setup(&env);
        client.set_pair_max_amount(&symbol_short!("USDC"), &symbol_short!("EURC"), &1_000i128);
    }

    #[test]
    fn test_set_pair_max_amount_succeeds_after_register() {
        let env = Env::default();
        let (client, _admin) = setup(&env);
        let (s, d) = (symbol_short!("USDC"), symbol_short!("EURC"));
        client.register_pair(&s, &d);
        client.set_pair_max_amount(&s, &d, &1_000i128);
        assert_eq!(client.get_pair_max_amount(&s, &d), 1_000);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #5)")]
    fn test_set_pair_max_amount_rejects_after_unregister() {
        let env = Env::default();
        let (client, _admin) = setup(&env);
        let (s, d) = (symbol_short!("USDC"), symbol_short!("EURC"));
        client.register_pair(&s, &d);
        client.unregister_pair(&s, &d);
        client.set_pair_max_amount(&s, &d, &1_000i128);
    }

    // --- set_pair_liquidity ---

    #[test]
    #[should_panic(expected = "Error(Contract, #5)")]
    fn test_set_pair_liquidity_rejects_unregistered_pair() {
        let env = Env::default();
        let (client, admin) = setup(&env);
        client.set_pair_liquidity(
            &admin,
            &symbol_short!("USDC"),
            &symbol_short!("EURC"),
            &500i128,
        );
    }

    #[test]
    fn test_set_pair_liquidity_succeeds_after_register() {
        let env = Env::default();
        let (client, admin) = setup(&env);
        let (s, d) = (symbol_short!("USDC"), symbol_short!("EURC"));
        client.register_pair(&s, &d);
        client.set_pair_liquidity(&admin, &s, &d, &500i128);
        assert_eq!(client.get_pair_liquidity(&s, &d), 500);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #5)")]
    fn test_set_pair_liquidity_rejects_after_unregister() {
        let env = Env::default();
        let (client, admin) = setup(&env);
        let (s, d) = (symbol_short!("USDC"), symbol_short!("EURC"));
        client.register_pair(&s, &d);
        client.unregister_pair(&s, &d);
        client.set_pair_liquidity(&admin, &s, &d, &500i128);
    }

    /// Sign/cap validation still fires for an unregistered pair: the
    /// existing negative/zero checks run before the new registration
    /// guard, so callers get the more specific error first.
    #[test]
    #[should_panic(expected = "Error(Contract, #6)")]
    fn test_set_pair_liquidity_negative_check_precedes_registration_check() {
        let env = Env::default();
        let (client, admin) = setup(&env);
        client.set_pair_liquidity(
            &admin,
            &symbol_short!("USDC"),
            &symbol_short!("EURC"),
            &-1i128,
        );
    }
}

/// Issue #14 — pause/unpause gating across state-changing entrypoints.
/// Covers the default-false flag, event emission, the `ContractPaused` (#9)
/// rejection on gated entrypoints, recovery after unpause, and idempotency.
#[cfg(test)]
mod test_i14_pause_gating {
    use super::*;
    use soroban_sdk::{
        symbol_short,
        testutils::{Address as _, Events},
    };

    /// Deploy a router with all auths mocked.
    fn setup(env: &Env) -> StableRouteRouterClient<'_> {
        env.mock_all_auths();
        let admin = Address::generate(env);
        let id = env.register(StableRouteRouter, (admin,));
        let client = StableRouteRouterClient::new(env, &id);
        client
    }

    #[test]
    fn test_is_paused_defaults_false_and_toggles() {
        let env = Env::default();
        let client = setup(&env);
        assert!(!client.is_paused());
        client.pause();
        assert!(client.is_paused());
        client.unpause();
        assert!(!client.is_paused());
    }

    #[test]
    fn test_pause_emits_event() {
        let env = Env::default();
        let client = setup(&env);
        client.pause();
        // pause() publishes a `paused` event; assert one was emitted.
        assert!(!env.events().all().events().is_empty());
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #9)")]
    fn test_register_pair_rejected_while_paused() {
        let env = Env::default();
        let client = setup(&env);
        client.pause();
        client.register_pair(&symbol_short!("USDC"), &symbol_short!("EURC"));
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #9)")]
    fn test_set_pair_fee_bps_rejected_while_paused() {
        let env = Env::default();
        let client = setup(&env);
        client.register_pair(&symbol_short!("USDC"), &symbol_short!("EURC"));
        client.pause();
        client.set_pair_fee_bps(&symbol_short!("USDC"), &symbol_short!("EURC"), &10u32);
    }

    #[test]
    fn test_gated_entrypoint_succeeds_after_unpause() {
        let env = Env::default();
        let client = setup(&env);
        client.pause();
        client.unpause();
        client.register_pair(&symbol_short!("USDC"), &symbol_short!("EURC"));
        assert!(client.is_pair_registered(&symbol_short!("USDC"), &symbol_short!("EURC")));
    }

    #[test]
    fn test_double_pause_and_double_unpause_idempotent() {
        let env = Env::default();
        let client = setup(&env);
        client.pause();
        client.pause();
        assert!(client.is_paused());
        client.unpause();
        client.unpause();
        assert!(!client.is_paused());
    }
}

/// Issue #15 — min/max amount and liquidity guards in `compute_route_fee`.
/// Covers at-bound acceptance, below-min (#10), above-max (#11), and
/// over-liquidity (#12) rejection, the unset sentinels, and negative
/// liquidity rejection (#6).
#[cfg(test)]
mod test_i15_bounds_liquidity {
    use super::*;
    use soroban_sdk::{symbol_short, testutils::Address as _};

    /// Register a pair with all auths mocked; returns the client and pair ids.
    fn setup_pair(env: &Env) -> (StableRouteRouterClient<'_>, Address, Symbol, Symbol) {
        env.mock_all_auths();
        let admin = Address::generate(env);
        let id = env.register(StableRouteRouter, (admin.clone(),));
        let client = StableRouteRouterClient::new(env, &id);
        let (s, d) = (symbol_short!("USDC"), symbol_short!("EURC"));
        client.register_pair(&s, &d);
        (client, admin, s, d)
    }

    #[test]
    fn test_min_amount_at_bound_is_accepted() {
        let env = Env::default();
        let (client, _admin, s, d) = setup_pair(&env);
        client.set_pair_min_amount(&s, &d, &100i128);
        assert_eq!(client.get_pair_min_amount(&s, &d), 100);
        // Exactly at the floor is accepted (fee 0, no bps configured).
        assert_eq!(client.compute_route_fee(&s, &d, &100i128), 0);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #10)")]
    fn test_below_min_rejected() {
        let env = Env::default();
        let (client, _admin, s, d) = setup_pair(&env);
        client.set_pair_min_amount(&s, &d, &100i128);
        client.compute_route_fee(&s, &d, &99i128);
    }

    #[test]
    fn test_max_amount_at_bound_is_accepted() {
        let env = Env::default();
        let (client, _admin, s, d) = setup_pair(&env);
        client.set_pair_max_amount(&s, &d, &1_000i128);
        assert_eq!(client.get_pair_max_amount(&s, &d), 1_000);
        assert_eq!(client.compute_route_fee(&s, &d, &1_000i128), 0);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #11)")]
    fn test_above_max_rejected() {
        let env = Env::default();
        let (client, _admin, s, d) = setup_pair(&env);
        client.set_pair_max_amount(&s, &d, &1_000i128);
        client.compute_route_fee(&s, &d, &1_001i128);
    }

    #[test]
    fn test_liquidity_at_bound_is_accepted() {
        let env = Env::default();
        let (client, admin, s, d) = setup_pair(&env);
        client.set_pair_liquidity(&admin, &s, &d, &500i128);
        assert_eq!(client.get_pair_liquidity(&s, &d), 500);
        // amount == reported liquidity is allowed.
        assert_eq!(client.compute_route_fee(&s, &d, &500i128), 0);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #12)")]
    fn test_above_liquidity_rejected() {
        let env = Env::default();
        let (client, admin, s, d) = setup_pair(&env);
        client.set_pair_liquidity(&admin, &s, &d, &500i128);
        client.compute_route_fee(&s, &d, &501i128);
    }

    #[test]
    fn test_unset_bounds_behave_as_unbounded() {
        let env = Env::default();
        let (client, _admin, s, d) = setup_pair(&env);
        // Defaults: min 0, max i128::MAX, liquidity unset => unbounded.
        assert_eq!(client.get_pair_min_amount(&s, &d), 0);
        assert_eq!(client.get_pair_max_amount(&s, &d), i128::MAX);
        assert_eq!(client.get_pair_liquidity(&s, &d), 0);
        assert_eq!(client.compute_route_fee(&s, &d, &1i128), 0);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #6)")]
    fn test_set_pair_liquidity_rejects_negative() {
        let env = Env::default();
        let (client, admin, s, d) = setup_pair(&env);
        client.set_pair_liquidity(&admin, &s, &d, &-1i128);
    }
}

/// Issue #16 — fee-computation arithmetic at extreme amounts.
/// Exercises the `checked_mul` overflow path (returns 0), truncating integer
/// division, quote/compute parity, and the saturating route counter.
#[cfg(test)]
mod test_i16_fee_arithmetic {
    use super::*;
    use soroban_sdk::{symbol_short, testutils::Address as _};

    /// Register a pair with wide bounds and liquidity so the boundary guards
    /// never pre-empt the arithmetic path under test.
    fn setup_pair(env: &Env) -> (StableRouteRouterClient<'_>, Symbol, Symbol) {
        env.mock_all_auths();
        let admin = Address::generate(env);
        let id = env.register(StableRouteRouter, (admin.clone(),));
        let client = StableRouteRouterClient::new(env, &id);
        let (s, d) = (symbol_short!("USDC"), symbol_short!("EURC"));
        client.register_pair(&s, &d);
        client.set_pair_max_amount(&s, &d, &i128::MAX);
        client.set_pair_liquidity(&admin, &s, &d, &i128::MAX);
        (client, s, d)
    }

    #[test]
    fn test_overflow_path_returns_zero() {
        let env = Env::default();
        let (client, s, d) = setup_pair(&env);
        client.set_pair_fee_bps(&s, &d, &2u32);
        // 2 * i128::MAX overflows checked_mul, so the fee defaults to 0
        // instead of panicking.
        assert_eq!(client.compute_route_fee(&s, &d, &i128::MAX), 0);
    }

    #[test]
    fn test_truncating_division_rounds_toward_zero() {
        let env = Env::default();
        let (client, s, d) = setup_pair(&env);
        client.set_pair_fee_bps(&s, &d, &3u32);
        // 12_345 * 3 / 10_000 = 3.7035 -> truncates to 3.
        assert_eq!(client.compute_route_fee(&s, &d, &12_345i128), 3);
    }

    #[test]
    fn test_quote_matches_compute_fee() {
        let env = Env::default();
        let (client, s, d) = setup_pair(&env);
        client.set_pair_fee_bps(&s, &d, &50u32);
        let (qfee, qnet) = client.quote_route(&s, &d, &1_000_000i128);
        let cfee = client.compute_route_fee(&s, &d, &1_000_000i128);
        assert_eq!(qfee, cfee);
        assert_eq!(qnet, 1_000_000 - qfee);
    }

    #[test]
    fn test_route_counter_increments_and_never_panics() {
        let env = Env::default();
        let (client, s, d) = setup_pair(&env);
        client.set_pair_fee_bps(&s, &d, &10u32);
        assert_eq!(client.get_total_routes_all_time(), 0);
        client.compute_route_fee(&s, &d, &1_000i128);
        assert_eq!(client.get_total_routes_all_time(), 1);
        client.compute_route_fee(&s, &d, &1_000i128);
        assert_eq!(client.get_total_routes_all_time(), 2);
    }
}

/// Issue #17 — schema migration path and `get_schema_version` defaults.
/// Covers the default-of-1, the v1->v2 stamp, the double-migration guard
/// (#13), and the admin-auth requirement.
#[cfg(test)]
mod test_i17_migration {
    use super::*;
    use soroban_sdk::testutils::{Address as _, MockAuth, MockAuthInvoke};
    use soroban_sdk::IntoVal;

    fn setup(env: &Env) -> StableRouteRouterClient<'_> {
        env.mock_all_auths();
        let admin = Address::generate(env);
        let id = env.register(StableRouteRouter, (admin,));
        let client = StableRouteRouterClient::new(env, &id);
        client
    }

    #[test]
    fn test_schema_version_defaults_to_one() {
        let env = Env::default();
        let client = setup(&env);
        assert_eq!(client.get_schema_version(), 1);
    }

    #[test]
    fn test_migrate_advances_to_two() {
        let env = Env::default();
        let client = setup(&env);
        client.migrate_v1_to_v2();
        assert_eq!(client.get_schema_version(), 2);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #13)")]
    fn test_double_migrate_rejected() {
        let env = Env::default();
        let client = setup(&env);
        client.migrate_v1_to_v2();
        client.migrate_v1_to_v2();
    }

    #[test]
    #[should_panic]
    fn test_migrate_requires_admin_auth() {
        let env = Env::default();
        let admin = Address::generate(&env);
        let id = Address::generate(&env);
        env.mock_auths(&[MockAuth {
            address: &admin,
            invoke: &MockAuthInvoke {
                contract: &id,
                fn_name: "__constructor",
                args: (admin.clone(),).into_val(&env),
                sub_invokes: &[],
            },
        }]);
        env.register_at(&id, StableRouteRouter, (admin,));
        let client = StableRouteRouterClient::new(&env, &id);
        client.migrate_v1_to_v2();
    }
}

/// Issue #18 — aggregate read surface: `get_pair_info` defaults/values,
/// `is_pair_active`, `quote_route` non-mutation + parity, and
/// `get_pair_last_route_at` before/after a route.
#[cfg(test)]
mod test_i18_read_surface {
    use super::*;
    use soroban_sdk::{
        symbol_short,
        testutils::{Address as _, Ledger},
    };

    fn setup(env: &Env) -> (StableRouteRouterClient<'_>, Address) {
        env.mock_all_auths();
        let admin = Address::generate(env);
        let id = env.register(StableRouteRouter, (admin.clone(),));
        let client = StableRouteRouterClient::new(env, &id);
        (client, admin)
    }

    #[test]
    fn test_pair_info_defaults_for_unconfigured_pair() {
        let env = Env::default();
        let (client, _admin) = setup(&env);
        let info = client.get_pair_info(&symbol_short!("USDC"), &symbol_short!("EURC"));
        assert_eq!(
            info,
            PairInfo {
                registered: false,
                fee_bps: 0,
                min_amount: 0,
                max_amount: i128::MAX,
                liquidity: 0,
                last_route_at: 0,
            }
        );
    }

    #[test]
    fn test_pair_info_reflects_configuration() {
        let env = Env::default();
        let (client, admin) = setup(&env);
        let (s, d) = (symbol_short!("USDC"), symbol_short!("EURC"));
        client.register_pair(&s, &d);
        client.set_pair_fee_bps(&s, &d, &25u32);
        client.set_pair_min_amount(&s, &d, &10i128);
        client.set_pair_max_amount(&s, &d, &1_000i128);
        let admin = client.get_admin().expect("constructor stores admin");
        client.set_pair_liquidity(&admin, &s, &d, &500i128);
        let info = client.get_pair_info(&s, &d);
        assert!(info.registered);
        assert_eq!(info.fee_bps, 25);
        assert_eq!(info.min_amount, 10);
        assert_eq!(info.max_amount, 1_000);
        assert_eq!(info.liquidity, 500);
    }

    #[test]
    fn test_is_pair_active_requires_registration_and_liquidity() {
        let env = Env::default();
        let (client, admin) = setup(&env);
        let (s, d) = (symbol_short!("USDC"), symbol_short!("EURC"));
        assert!(!client.is_pair_active(&s, &d));
        client.register_pair(&s, &d);
        // Registered but zero liquidity is still inactive.
        assert!(!client.is_pair_active(&s, &d));
        let admin = client.get_admin().expect("constructor stores admin");
        client.set_pair_liquidity(&admin, &s, &d, &1i128);
        assert!(client.is_pair_active(&s, &d));
    }

    #[test]
    fn test_quote_route_is_non_mutating_and_matches_compute() {
        let env = Env::default();
        let (client, _admin) = setup(&env);
        let (s, d) = (symbol_short!("USDC"), symbol_short!("EURC"));
        client.register_pair(&s, &d);
        client.set_pair_fee_bps(&s, &d, &100u32);
        let (qfee, _net) = client.quote_route(&s, &d, &1_000i128);
        // Quote leaves the counter and timestamp untouched.
        assert_eq!(client.get_total_routes_all_time(), 0);
        assert_eq!(client.get_pair_last_route_at(&s, &d), None);
        // And reports the same fee compute_route_fee would.
        assert_eq!(qfee, client.compute_route_fee(&s, &d, &1_000i128));
    }

    #[test]
    fn test_last_route_at_none_before_some_after() {
        let env = Env::default();
        let (client, _admin) = setup(&env);
        let (s, d) = (symbol_short!("USDC"), symbol_short!("EURC"));
        client.register_pair(&s, &d);
        env.ledger().set_timestamp(424_242);
        assert_eq!(client.get_pair_last_route_at(&s, &d), None);
        client.compute_route_fee(&s, &d, &1_000i128);
        assert_eq!(client.get_pair_last_route_at(&s, &d), Some(424_242));
    }
}

/// Issue #19 — negative authorization coverage. The shared `setup_initialized`
/// uses `mock_all_auths`, so no existing test proves a wrong/missing signer is
/// rejected. Each test here initialises with a scoped mock authorising only
/// `init`, then invokes an admin entrypoint with no matching auth and asserts
/// the call panics. A positive control confirms the call works with auth.
#[cfg(test)]
mod test_i19_authorization {
    use super::*;
    use soroban_sdk::{
        symbol_short,
        testutils::{Address as _, MockAuth, MockAuthInvoke},
        IntoVal,
    };

    /// Register the constructor with only constructor auth for `admin`; later
    /// privileged calls are intentionally left unauthorised.
    fn setup_scoped(env: &Env) -> StableRouteRouterClient<'_> {
        let admin = Address::generate(env);
        let id = Address::generate(env);
        env.mock_auths(&[MockAuth {
            address: &admin,
            invoke: &MockAuthInvoke {
                contract: &id,
                fn_name: "__constructor",
                args: (admin.clone(),).into_val(env),
                sub_invokes: &[],
            },
        }]);
        env.register_at(&id, StableRouteRouter, (admin,));
        let client = StableRouteRouterClient::new(env, &id);
        client
    }

    #[test]
    #[should_panic]
    fn test_register_pair_requires_admin() {
        let env = Env::default();
        let client = setup_scoped(&env);
        client.register_pair(&symbol_short!("USDC"), &symbol_short!("EURC"));
    }

    #[test]
    #[should_panic]
    fn test_unregister_pair_requires_admin() {
        let env = Env::default();
        let client = setup_scoped(&env);
        client.unregister_pair(&symbol_short!("USDC"), &symbol_short!("EURC"));
    }

    #[test]
    #[should_panic]
    fn test_set_pair_fee_bps_requires_admin() {
        let env = Env::default();
        let client = setup_scoped(&env);
        client.set_pair_fee_bps(&symbol_short!("USDC"), &symbol_short!("EURC"), &10u32);
    }

    #[test]
    #[should_panic]
    fn test_set_pair_liquidity_requires_admin() {
        let env = Env::default();
        let client = setup_scoped(&env);
        let caller = Address::generate(&env);
        client.set_pair_liquidity(
            &caller,
            &symbol_short!("USDC"),
            &symbol_short!("EURC"),
            &10i128,
        );
    }

    #[test]
    #[should_panic]
    fn test_set_pair_min_amount_requires_admin() {
        let env = Env::default();
        let client = setup_scoped(&env);
        client.set_pair_min_amount(&symbol_short!("USDC"), &symbol_short!("EURC"), &1i128);
    }

    #[test]
    #[should_panic]
    fn test_set_pair_max_amount_requires_admin() {
        let env = Env::default();
        let client = setup_scoped(&env);
        client.set_pair_max_amount(&symbol_short!("USDC"), &symbol_short!("EURC"), &1i128);
    }

    #[test]
    #[should_panic]
    fn test_set_fee_recipient_requires_admin() {
        let env = Env::default();
        let client = setup_scoped(&env);
        client.set_fee_recipient(&Address::generate(&env));
    }

    #[test]
    #[should_panic]
    fn test_pause_requires_admin() {
        let env = Env::default();
        let client = setup_scoped(&env);
        client.pause();
    }

    #[test]
    #[should_panic]
    fn test_unpause_requires_admin() {
        let env = Env::default();
        let client = setup_scoped(&env);
        client.unpause();
    }

    #[test]
    #[should_panic]
    fn test_propose_admin_transfer_requires_admin() {
        let env = Env::default();
        let client = setup_scoped(&env);
        client.propose_admin_transfer(&Address::generate(&env));
    }

    #[test]
    #[should_panic]
    fn test_migrate_requires_admin() {
        let env = Env::default();
        let client = setup_scoped(&env);
        client.migrate_v1_to_v2();
    }

    /// Positive control: with the admin's auth supplied, the call succeeds.
    #[test]
    fn test_admin_can_register_with_auth() {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let id = env.register(StableRouteRouter, (admin,));
        let client = StableRouteRouterClient::new(&env, &id);
        client.register_pair(&symbol_short!("USDC"), &symbol_short!("EURC"));
        assert!(client.is_pair_registered(&symbol_short!("USDC"), &symbol_short!("EURC")));
    }
}

/// Issue #41 — absolute per-route fee ceiling. Both the relative MAX_FEE_BPS
/// and the optional absolute MaxFeeAbsolute apply; the tighter wins. The cap
/// is unset by default (backward compatible).
#[cfg(test)]
mod test_i41_fee_cap {
    use super::*;
    use soroban_sdk::{symbol_short, testutils::Address as _};

    fn setup_pair(env: &Env) -> (StableRouteRouterClient<'_>, Symbol, Symbol) {
        env.mock_all_auths();
        let admin = Address::generate(env);
        let id = env.register(StableRouteRouter, (admin,));
        let client = StableRouteRouterClient::new(env, &id);
        let (s, d) = (symbol_short!("USDC"), symbol_short!("EURC"));
        client.register_pair(&s, &d);
        client.set_pair_fee_bps(&s, &d, &100u32); // 1%
        (client, s, d)
    }

    #[test]
    fn test_no_absolute_cap_by_default() {
        let env = Env::default();
        let (client, s, d) = setup_pair(&env);
        assert_eq!(client.get_max_fee_absolute(), None);
        // 1_000_000 * 1% = 10_000, unclamped.
        assert_eq!(client.compute_route_fee(&s, &d, &1_000_000i128), 10_000);
    }

    #[test]
    fn test_fee_below_cap_is_unaffected() {
        let env = Env::default();
        let (client, s, d) = setup_pair(&env);
        client.set_max_fee_absolute(&50_000i128);
        assert_eq!(client.get_max_fee_absolute(), Some(50_000));
        // 10_000 < 50_000 -> unchanged.
        assert_eq!(client.compute_route_fee(&s, &d, &1_000_000i128), 10_000);
    }

    #[test]
    fn test_fee_above_cap_is_clamped() {
        let env = Env::default();
        let (client, s, d) = setup_pair(&env);
        client.set_max_fee_absolute(&5_000i128);
        // Proportional fee 10_000 clamped down to the 5_000 ceiling.
        assert_eq!(client.compute_route_fee(&s, &d, &1_000_000i128), 5_000);
    }

    #[test]
    fn test_cap_of_zero_makes_routes_free() {
        let env = Env::default();
        let (client, s, d) = setup_pair(&env);
        client.set_max_fee_absolute(&0i128);
        assert_eq!(client.compute_route_fee(&s, &d, &1_000_000i128), 0);
    }

    #[test]
    fn test_quote_and_compute_agree_under_cap() {
        let env = Env::default();
        let (client, s, d) = setup_pair(&env);
        client.set_max_fee_absolute(&5_000i128);
        let (qfee, qnet) = client.quote_route(&s, &d, &1_000_000i128);
        assert_eq!(qfee, 5_000);
        assert_eq!(qnet, 1_000_000 - 5_000);
        assert_eq!(qfee, client.compute_route_fee(&s, &d, &1_000_000i128));
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #6)")]
    fn test_negative_cap_rejected() {
        let env = Env::default();
        let (client, _s, _d) = setup_pair(&env);
        client.set_max_fee_absolute(&-1i128);
    }
}

#[cfg(test)]
mod test_batch {
    use super::*;
    use soroban_sdk::{symbol_short, testutils::Address as _, vec};

    fn setup(env: &Env) -> (StableRouteRouterClient<'_>, Address) {
        env.mock_all_auths();
        let admin = Address::generate(env);
        let id = env.register(StableRouteRouter, (admin.clone(),));
        let client = StableRouteRouterClient::new(env, &id);
        (client, admin)
    }

    fn setup_without_admin(env: &Env) -> StableRouteRouterClient<'_> {
        env.mock_all_auths();
        let admin = Address::generate(env);
        let id = env.register(StableRouteRouter, (admin,));
        env.as_contract(&id, || {
            env.storage().persistent().remove(&DataKey::Admin);
        });
        StableRouteRouterClient::new(env, &id)
    }

    #[test]
    fn test_register_pairs_happy_path() {
        let env = Env::default();
        let (client, _admin) = setup(&env);
        client.register_pairs(&vec![
            &env,
            (symbol_short!("USDC"), symbol_short!("EURC")),
            (symbol_short!("XLM"), symbol_short!("USDC")),
            (symbol_short!("ETH"), symbol_short!("BTC")),
        ]);
        assert!(client.is_pair_registered(&symbol_short!("USDC"), &symbol_short!("EURC")));
        assert!(client.is_pair_registered(&symbol_short!("XLM"), &symbol_short!("USDC")));
        assert!(client.is_pair_registered(&symbol_short!("ETH"), &symbol_short!("BTC")));
    }

    #[test]
    fn test_register_pairs_single_entry_succeeds() {
        let env = Env::default();
        let (client, _admin) = setup(&env);
        client.register_pairs(&vec![&env, (symbol_short!("USDC"), symbol_short!("EURC"))]);
        assert!(client.is_pair_registered(&symbol_short!("USDC"), &symbol_short!("EURC")));
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #19)")]
    fn test_register_pairs_rejects_empty_batch() {
        let env = Env::default();
        let (client, _admin) = setup(&env);
        client.register_pairs(&vec![&env]);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #3)")]
    fn test_register_pairs_atomic_rollback_on_identity() {
        let env = Env::default();
        let (client, _admin) = setup(&env);
        client.register_pairs(&vec![
            &env,
            (symbol_short!("USDC"), symbol_short!("EURC")),
            (symbol_short!("XLM"), symbol_short!("XLM")),
            (symbol_short!("ETH"), symbol_short!("BTC")),
        ]);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #9)")]
    fn test_register_pairs_rejects_when_paused() {
        let env = Env::default();
        let (client, _admin) = setup(&env);
        client.pause();
        client.register_pairs(&vec![&env, (symbol_short!("USDC"), symbol_short!("EURC"))]);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #18)")]
    fn test_register_pairs_rejects_too_large_batch() {
        let env = Env::default();
        let (client, _admin) = setup(&env);
        let mut pairs = std::vec::Vec::new();
        for i in 0..MAX_BATCH_SIZE + 1 {
            pairs.push((
                Symbol::new(&env, &std::format!("SRC{}", i)),
                Symbol::new(&env, &std::format!("DST{}", i)),
            ));
        }
        client.register_pairs(&Vec::from_slice(&env, &pairs));
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #2)")]
    fn test_register_pairs_panics_when_uninitialized() {
        let env = Env::default();
        let client = setup_without_admin(&env);
        client.register_pairs(&vec![&env, (symbol_short!("USDC"), symbol_short!("EURC"))]);
    }

    #[test]
    fn test_set_pair_fees_bps_happy_path() {
        let env = Env::default();
        let (client, _admin) = setup(&env);
        client.register_pairs(&vec![
            &env,
            (symbol_short!("USDC"), symbol_short!("EURC")),
            (symbol_short!("XLM"), symbol_short!("USDC")),
        ]);
        client.set_pair_fees_bps(&vec![
            &env,
            (symbol_short!("USDC"), symbol_short!("EURC"), 25u32),
            (symbol_short!("XLM"), symbol_short!("USDC"), 50u32),
        ]);
        assert_eq!(
            client.get_pair_fee_bps(&symbol_short!("USDC"), &symbol_short!("EURC")),
            25
        );
        assert_eq!(
            client.get_pair_fee_bps(&symbol_short!("XLM"), &symbol_short!("USDC")),
            50
        );
    }

    #[test]
    fn test_set_pair_fees_bps_single_entry_succeeds() {
        let env = Env::default();
        let (client, _admin) = setup(&env);
        client.register_pairs(&vec![&env, (symbol_short!("USDC"), symbol_short!("EURC"))]);
        client.set_pair_fees_bps(&vec![
            &env,
            (symbol_short!("USDC"), symbol_short!("EURC"), 25u32),
        ]);
        assert_eq!(
            client.get_pair_fee_bps(&symbol_short!("USDC"), &symbol_short!("EURC")),
            25
        );
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #19)")]
    fn test_set_pair_fees_bps_rejects_empty_batch() {
        let env = Env::default();
        let (client, _admin) = setup(&env);
        client.set_pair_fees_bps(&vec![&env]);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #4)")]
    fn test_set_pair_fees_bps_atomic_rollback_on_high_fee() {
        let env = Env::default();
        let (client, _admin) = setup(&env);
        client.register_pairs(&vec![
            &env,
            (symbol_short!("USDC"), symbol_short!("EURC")),
            (symbol_short!("XLM"), symbol_short!("USDC")),
        ]);
        client.set_pair_fees_bps(&vec![
            &env,
            (symbol_short!("USDC"), symbol_short!("EURC"), 25u32),
            (symbol_short!("XLM"), symbol_short!("USDC"), MAX_FEE_BPS + 1),
        ]);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #5)")]
    fn test_set_pair_fees_bps_rejects_unregistered_pair() {
        let env = Env::default();
        let (client, _admin) = setup(&env);
        client.set_pair_fees_bps(&vec![
            &env,
            (symbol_short!("USDC"), symbol_short!("EURC"), 25u32),
        ]);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #9)")]
    fn test_set_pair_fees_bps_rejects_when_paused() {
        let env = Env::default();
        let (client, _admin) = setup(&env);
        client.register_pairs(&vec![&env, (symbol_short!("USDC"), symbol_short!("EURC"))]);
        client.pause();
        client.set_pair_fees_bps(&vec![
            &env,
            (symbol_short!("USDC"), symbol_short!("EURC"), 25u32),
        ]);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #18)")]
    fn test_set_pair_fees_bps_rejects_too_large_batch() {
        let env = Env::default();
        let (client, _admin) = setup(&env);
        let mut entries = std::vec::Vec::new();
        for i in 0..MAX_BATCH_SIZE + 1 {
            entries.push((
                Symbol::new(&env, &std::format!("SRC{}", i)),
                Symbol::new(&env, &std::format!("DST{}", i)),
                10u32,
            ));
        }
        client.set_pair_fees_bps(&Vec::from_slice(&env, &entries));
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #2)")]
    fn test_set_pair_fees_bps_panics_when_uninitialized() {
        let env = Env::default();
        let client = setup_without_admin(&env);
        client.set_pair_fees_bps(&vec![
            &env,
            (symbol_short!("USDC"), symbol_short!("EURC"), 25u32),
        ]);
    }
}
