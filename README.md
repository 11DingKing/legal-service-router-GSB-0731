# Legal Service Router

Blank 0-1 baseline for constraint-based routing to accessible public legal-service locations. It contains domain fixtures, not an implementation.

## Source material

`materials/service-catalog.json` fixes service-point IDs, capability names, catalog versions, closure events, and home-service eligibility examples.

## Required delivery contract

- Rust stable, Axum, and SQLite; no external map or route API.
- Filter mandatory service and accessibility capabilities before comparing deterministic reachability cost.
- Persist immutable routing snapshots tied to a catalog version and return an exclusion/reason chain.
- Native verification: `cargo test` and `cargo run`.
- Document schema setup, endpoint examples, tie-breaking, hot reload, and concurrency behavior.

Docker is outside the project contract.

