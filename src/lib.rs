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
}

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

    /// Returns `true` iff `register_pair` has been called for this pair.
    pub fn is_pair_registered(env: Env, source: Symbol, destination: Symbol) -> bool {
        env.storage()
            .persistent()
            .get(&DataKey::Pair(source, destination))
            .unwrap_or(false)
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
}
