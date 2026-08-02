//! Binary entry point. Starts the Axum server backed by a SQLite store.
//!
//! Environment:
//! - `DB_PATH`  SQLite file path (default `router.db`).
//! - `BIND`     listen address (default `127.0.0.1:8080`).
//! - `SEED_CATALOG` path to a catalog JSON to import+activate on boot if the
//!   store has no active version yet (default `materials/service-catalog.json`
//!   when present).

use std::sync::Arc;

use legal_service_router::{api, store::Store};

#[tokio::main]
async fn main() {
    let db_path = std::env::var("DB_PATH").unwrap_or_else(|_| "router.db".to_string());
    let bind = std::env::var("BIND").unwrap_or_else(|_| "127.0.0.1:8080".to_string());

    let store = Arc::new(Store::open(&db_path).expect("open store"));

    // Seed the initial catalog on first boot for convenience.
    let (_versions, active) = store.list_versions();
    if active.is_none() {
        let seed = std::env::var("SEED_CATALOG")
            .unwrap_or_else(|_| "materials/service-catalog.json".to_string());
        if let Ok(bytes) = std::fs::read(&seed) {
            match store.import_catalog(&bytes, true) {
                Ok(v) => eprintln!("seeded catalog version {v} from {seed}"),
                Err(e) => eprintln!("seed skipped: {e}"),
            }
        }
    }

    let app = api::router(store);
    let listener = tokio::net::TcpListener::bind(&bind).await.expect("bind");
    eprintln!("legal-service-router listening on http://{bind}");
    axum::serve(listener, app).await.expect("serve");
}
