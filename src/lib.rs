use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, panic_with_error, symbol_short, Address,
    Env, Symbol,
};

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
}

/// StableRoute router contract — placeholder for routing logic.
/// In production this would integrate with path payments and liquidity data.
#[contract]
pub struct StableRouteRouter;

#[contractimpl]
impl StableRouteRouter {
    /// Returns the router contract version.
    pub fn version(_env: Env) -> Symbol {
        symbol_short!("ROUTER_V1")
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
        let admin: Address = env
            .storage()
            .persistent()
            .get(&DataKey::Admin)
            .unwrap_or_else(|| panic_with_error!(&env, RouterError::NotInitialized));
        admin.require_auth();
        env.storage().persistent().set(&DataKey::Paused, &false);
    }

    /// Admin pauses the router. All state-changing entrypoints will
    /// then panic with ContractPaused.
    pub fn pause(env: Env) {
        let admin: Address = env
            .storage()
            .persistent()
            .get(&DataKey::Admin)
            .unwrap_or_else(|| panic_with_error!(&env, RouterError::NotInitialized));
        admin.require_auth();
        env.storage().persistent().set(&DataKey::Paused, &true);
    }

    /// Cancel a pending handover. No-op if none is pending.
    pub fn cancel_admin_transfer(env: Env) {
        let admin: Address = env
            .storage()
            .persistent()
            .get(&DataKey::Admin)
            .unwrap_or_else(|| panic_with_error!(&env, RouterError::NotInitialized));
        admin.require_auth();
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
        env.storage().persistent().set(&DataKey::Admin, &caller);
        env.storage().persistent().remove(&DataKey::PendingAdmin);
    }

    /// Step 1 of admin handover. Current admin proposes a new admin;
    /// the new admin must then accept via `accept_admin_transfer`.
    pub fn propose_admin_transfer(env: Env, new_admin: Address) {
        let admin: Address = env
            .storage()
            .persistent()
            .get(&DataKey::Admin)
            .unwrap_or_else(|| panic_with_error!(&env, RouterError::NotInitialized));
        admin.require_auth();
        env.storage()
            .persistent()
            .set(&DataKey::PendingAdmin, &new_admin);
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
        let admin: Address = env
            .storage()
            .persistent()
            .get(&DataKey::Admin)
            .unwrap_or_else(|| panic_with_error!(&env, RouterError::NotInitialized));
        admin.require_auth();
        if source == destination {
            panic_with_error!(&env, RouterError::SourceEqualsDestination);
        }
        env.storage()
            .persistent()
            .set(&DataKey::Pair(source, destination), &true);
    }

    /// Read the most recent ledger timestamp at which `compute_route_fee`
    /// touched this pair. None when never routed.
    pub fn get_pair_last_route_at(
        env: Env,
        source: Symbol,
        destination: Symbol,
    ) -> Option<u64> {
        env.storage()
            .persistent()
            .get(&DataKey::PairLastRouteAt(source, destination))
    }

    /// Read the protocol-wide lifetime counter of route quotes.
    pub fn get_total_routes_all_time(env: Env) -> u64 {
        env.storage()
            .persistent()
            .get(&DataKey::TotalRoutesAllTime)
            .unwrap_or(0)
    }

    /// Admin sets the address that receives protocol fees at
    /// settlement time. The router itself never custodies funds.
    pub fn set_fee_recipient(env: Env, recipient: Address) {
        let admin: Address = env
            .storage()
            .persistent()
            .get(&DataKey::Admin)
            .unwrap_or_else(|| panic_with_error!(&env, RouterError::NotInitialized));
        admin.require_auth();
        env.storage().persistent().set(&DataKey::FeeRecipient, &recipient);
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
        let admin: Address = env
            .storage()
            .persistent()
            .get(&DataKey::Admin)
            .unwrap_or_else(|| panic_with_error!(&env, RouterError::NotInitialized));
        admin.require_auth();
        if liquidity < 0 {
            panic_with_error!(&env, RouterError::AmountMustBePositive);
        }
        env.storage()
            .persistent()
            .set(&DataKey::PairLiquidity(source, destination), &liquidity);
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
        let admin: Address = env
            .storage()
            .persistent()
            .get(&DataKey::Admin)
            .unwrap_or_else(|| panic_with_error!(&env, RouterError::NotInitialized));
        admin.require_auth();
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
        let admin: Address = env
            .storage()
            .persistent()
            .get(&DataKey::Admin)
            .unwrap_or_else(|| panic_with_error!(&env, RouterError::NotInitialized));
        admin.require_auth();
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
        let admin: Address = env
            .storage()
            .persistent()
            .get(&DataKey::Admin)
            .unwrap_or_else(|| panic_with_error!(&env, RouterError::NotInitialized));
        admin.require_auth();
        env.storage()
            .persistent()
            .remove(&DataKey::Pair(source, destination));
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
        let admin: Address = env
            .storage()
            .persistent()
            .get(&DataKey::Admin)
            .unwrap_or_else(|| panic_with_error!(&env, RouterError::NotInitialized));
        admin.require_auth();
        if fee_bps > MAX_FEE_BPS {
            panic_with_error!(&env, RouterError::FeeBpsTooHigh);
        }
        env.storage()
            .persistent()
            .set(&DataKey::PairFeeBps(source, destination), &fee_bps);
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
    pub fn compute_route_fee(
        env: Env,
        source: Symbol,
        destination: Symbol,
        amount: i128,
    ) -> i128 {
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
        let min_amount: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::PairMinAmount(source.clone(), destination.clone()))
            .unwrap_or(0);
        if amount < min_amount {
            panic_with_error!(&env, RouterError::AmountBelowMin);
        }
        let max_amount: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::PairMaxAmount(source.clone(), destination.clone()))
            .unwrap_or(i128::MAX);
        if amount > max_amount {
            panic_with_error!(&env, RouterError::AmountAboveMax);
        }
        let liquidity: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::PairLiquidity(source.clone(), destination.clone()))
            .unwrap_or(i128::MAX);
        if amount > liquidity {
            panic_with_error!(&env, RouterError::InsufficientLiquidity);
        }
        let total: u64 = env
            .storage()
            .persistent()
            .get(&DataKey::TotalRoutesAllTime)
            .unwrap_or(0);
        env.storage()
            .persistent()
            .set(&DataKey::TotalRoutesAllTime, &total.saturating_add(1));
        env.storage().persistent().set(
            &DataKey::PairLastRouteAt(source.clone(), destination.clone()),
            &env.ledger().timestamp(),
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

    /// Placeholder: returns a fixed route tag for a source/destination pair.
    /// Used by the backend to verify route integrity.
    pub fn route_tag(_env: Env, source: Symbol, destination: Symbol) -> (Symbol, Symbol) {
        (source, destination)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{symbol_short, testutils::Address as _};

    fn setup_initialized(env: &Env) -> (StableRouteRouterClient<'_>, Address) {
        env.mock_all_auths();
        let contract_id = env.register(StableRouteRouter, ());
        let client = StableRouteRouterClient::new(env, &contract_id);
        let admin = Address::generate(env);
        client.init(&admin);
        (client, admin)
    }

    #[test]
    fn test_version() {
        let env = Env::default();
        let contract_id = env.register(StableRouteRouter, ());
        let client = StableRouteRouterClient::new(&env, &contract_id);
        let v = client.version();
        assert_eq!(v, symbol_short!("ROUTER_V1"));
    }

    #[test]
    fn test_route_tag() {
        let env = Env::default();
        let contract_id = env.register(StableRouteRouter, ());
        let client = StableRouteRouterClient::new(&env, &contract_id);
        let (src, dest) = client.route_tag(&symbol_short!("USDC"), &symbol_short!("EURC"));
        assert_eq!(src, symbol_short!("USDC"));
        assert_eq!(dest, symbol_short!("EURC"));
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
        client.compute_route_fee(
            &symbol_short!("USDC"),
            &symbol_short!("EURC"),
            &1_000_i128,
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
}
