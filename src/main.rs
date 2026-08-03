use std::net::SocketAddr;
use std::path::PathBuf;

use tracing_subscriber::EnvFilter;

use legal_service_router::api;
use legal_service_router::catalog;
use legal_service_router::db;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,legal_service_router=debug")),
        )
        .init();

    let database_url =
        std::env::var("DATABASE_URL").unwrap_or_else(|_| "legal-router.db".to_string());
    let bind_addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_string());

    let pool = db::init_pool(&database_url)?;
    tracing::info!(database = %database_url, "database initialized");

    if db::get_active_version(&pool)?.is_none() {
        if let Some(content) = load_seed_catalog() {
            match catalog::parse_catalog(&content) {
                Ok(cat) => match db::import_catalog(&pool, &cat) {
                    Ok(()) => tracing::info!(version = %cat.catalog_version, "seed catalog imported"),
                    Err(e) => tracing::warn!(error = %e, "failed to import seed catalog"),
                },
                Err(e) => tracing::warn!(error = %e, "failed to parse seed catalog"),
            }
        }
    }

    let app = api::build_router(pool);
    let addr: SocketAddr = bind_addr.parse()?;
    tracing::info!(%addr, "legal service router listening");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

fn load_seed_catalog() -> Option<String> {
    let candidates = [
        PathBuf::from("materials/service-catalog.json"),
        PathBuf::from("../materials/service-catalog.json"),
    ];
    for path in candidates {
        if path.exists() {
            if let Ok(content) = std::fs::read_to_string(&path) {
                tracing::info!(?path, "loaded seed catalog");
                return Some(content);
            }
        }
    }
    None
}
