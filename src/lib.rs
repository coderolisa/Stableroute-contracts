use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, Env, Symbol,
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

    /// Placeholder: returns a fixed route tag for a source/destination pair.
    /// Used by the backend to verify route integrity.
    pub fn route_tag(_env: Env, source: Symbol, destination: Symbol) -> (Symbol, Symbol) {
        (source, destination)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::symbol_short;

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
}
