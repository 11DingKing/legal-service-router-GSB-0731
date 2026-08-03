//! Server bootstrap.
//!
//! Env:
//!   ROUTER_DB   SQLite file path (default ./legal-service-router.db)
//!   PORT        listen port (default 8080)
//! Args:
//!   --import <path>   import a catalog JSON file at startup (activates it)

use legal_service_router::api;
use legal_service_router::db;
use legal_service_router::model::CatalogImport;

#[tokio::main]
async fn main() {
    let db_path = std::env::var("ROUTER_DB").unwrap_or_else(|_| "./legal-service-router.db".into());
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8080);

    let pool = db::open_pool(&db_path).expect("failed to open database");

    // Optional startup import: `--import <catalog.json>`
    let args: Vec<String> = std::env::args().collect();
    if let Some(pos) = args.iter().position(|a| a == "--import") {
        let path = args.get(pos + 1).expect("--import requires a file path");
        let raw = std::fs::read_to_string(path).expect("failed to read catalog file");
        let cat: CatalogImport =
            serde_json::from_str(&raw).expect("catalog file is not valid JSON for CatalogImport");
        match db::import_catalog(&pool, &cat) {
            Ok(resp) => println!(
                "imported catalog {} ({} points, {} closures)",
                resp.version, resp.points, resp.closures
            ),
            Err(db::DbError::DuplicateVersion(v)) => {
                println!("catalog {v} already imported; leaving existing data untouched")
            }
            Err(e) => panic!("failed to import catalog: {e}"),
        }
    }

    let app = api::build_app(pool);
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("failed to bind");
    println!("legal-service-router listening on http://{addr}");
    axum::serve(listener, app).await.expect("server error");
}
