mod db;
mod error;
mod models;
mod repositories;
mod routes;
mod services;
#[cfg(test)]
mod smoke_tests;

use std::net::SocketAddr;

use axum::http::{header, HeaderValue, Method};
use tower_http::{cors::CorsLayer, trace::TraceLayer};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();

    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "roya=info,tower_http=info".into()))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let database_url =
        std::env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite://roya.db".to_string());
    let allow_negative = std::env::var("ALLOW_NEGATIVE_BALANCE")
        .map(|v| v == "true" || v == "1")
        .unwrap_or(false);
    let allow_negative_stock = std::env::var("ALLOW_NEGATIVE_STOCK")
        .map(|v| v == "true" || v == "1")
        .unwrap_or(true);
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3000);

    tracing::info!(%database_url, allow_negative, allow_negative_stock, port, "starting roya");

    let pool = db::create_pool(&database_url).await?;
    tracing::info!("database ready (migrations applied)");

    let state = routes::AppState::new(pool, allow_negative, allow_negative_stock);

    let cors = CorsLayer::new()
        .allow_origin(HeaderValue::from_static("*"))
        .allow_methods([Method::GET, Method::POST, Method::PUT, Method::DELETE])
        .allow_headers([header::CONTENT_TYPE]);

    let app = routes::router(state)
        .layer(TraceLayer::new_for_http())
        .layer(cors);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    tracing::info!("listening on http://localhost:{port}");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
