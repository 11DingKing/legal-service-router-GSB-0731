mod api;
mod capacity;
mod db;
mod models;
mod router;

#[cfg(test)]
mod tests;

use std::sync::Arc;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let db_path = std::env::var("DATABASE_URL").unwrap_or_else(|_| "legal-router.db".to_string());
    let db = Arc::new(
        db::Db::open_file(&db_path).expect("failed to open database"),
    );

    let listener_addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:3000".to_string());
    let listener = tokio::net::TcpListener::bind(&listener_addr)
        .await
        .expect("failed to bind");

    tracing::info!("legal-service-router listening on {}", listener_addr);

    let app = api::app(db);
    axum::serve(listener, app).await.expect("server error");
}
