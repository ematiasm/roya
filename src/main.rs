mod db;
mod error;
mod models;
mod repositories;
mod routes;
mod security;
mod services;
#[cfg(test)]
mod smoke_tests;

use std::net::SocketAddr;

use axum::http::{header, HeaderValue, Method};
use tower_http::{cors::AllowOrigin, cors::CorsLayer, trace::TraceLayer};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use crate::security::SessionPolicy;
use crate::services::identity::ThrottleConfig;

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
    let enforce_credit_limit = std::env::var("ENFORCE_CREDIT_LIMIT")
        .map(|v| v == "true" || v == "1")
        .unwrap_or(true);
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3000);

    // Identity (S1b): the session/cookie policy, the CORS origin list and the
    // login throttle, all read here so `AppState` stays a plain construction.
    let admin_password = std::env::var("ROYA_ADMIN_PASSWORD").ok();
    let session_ttl_hours: i64 = std::env::var("ROYA_SESSION_TTL_HOURS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(12);
    let cookie_secure = std::env::var("ROYA_COOKIE_SECURE")
        .map(|v| v == "true" || v == "1")
        .unwrap_or(false);
    let throttle_attempts: u32 = std::env::var("ROYA_LOGIN_THROTTLE_ATTEMPTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5);
    let throttle_seconds: i64 = std::env::var("ROYA_LOGIN_THROTTLE_SECONDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(60);
    // Comma-separated; unset means same-origin only: no wildcard and no
    // Access-Control-Allow-Origin header is ever emitted.
    let allowed_origins: Option<Vec<String>> = std::env::var("ROYA_ALLOWED_ORIGINS")
        .ok()
        .map(|v| {
            v.split(',')
                .map(|o| o.trim().to_string())
                .filter(|o| !o.is_empty())
                .collect()
        })
        .filter(|list: &Vec<String>| !list.is_empty());

    let policy = SessionPolicy::new(session_ttl_hours, cookie_secure);
    let throttle = ThrottleConfig {
        max_failures: throttle_attempts,
        cooldown: chrono::Duration::seconds(throttle_seconds),
        ..ThrottleConfig::default()
    };

    tracing::info!(
        %database_url,
        allow_negative,
        allow_negative_stock,
        enforce_credit_limit,
        session_ttl_hours,
        cookie_secure,
        throttle_attempts,
        throttle_seconds,
        port,
        "starting roya"
    );

    let pool = db::create_pool(&database_url).await?;
    tracing::info!("database ready (migrations applied)");

    let state = routes::AppState::new_with_identity(
        pool,
        allow_negative,
        allow_negative_stock,
        enforce_credit_limit,
        policy,
        throttle,
    );

    // Bootstrap (AC1): seed the administrator when none exists. With
    // ROYA_ADMIN_PASSWORD set the operator's password is used as-is; without
    // it a password is generated and logged here exactly once — the single
    // place it is ever visible. An environment-supplied password is never
    // logged.
    let bootstrap = state
        .identity_service
        .bootstrap_admin(admin_password.as_deref())
        .await?;
    if let Some(generated) = &bootstrap.generated_password {
        tracing::warn!(
            "generated bootstrap administrator password (shown once, then never again): {generated}"
        );
    }
    tracing::info!(
        created = bootstrap.created,
        "identity bootstrap ready: an administrator can log in"
    );

    let cors = CorsLayer::new()
        .allow_methods([Method::GET, Method::POST, Method::PUT, Method::DELETE])
        .allow_headers([header::CONTENT_TYPE]);
    let cors = match allowed_origins {
        Some(origins) => {
            let headers = origins
                .iter()
                .map(|origin| HeaderValue::from_str(origin))
                .collect::<Result<Vec<_>, _>>()?;
            cors.allow_origin(AllowOrigin::list(headers))
        }
        // Same-origin by default: the wildcard is gone with S1b.
        None => cors,
    };

    let app = routes::router(state)
        .layer(TraceLayer::new_for_http())
        .layer(cors);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    tracing::info!("listening on http://localhost:{port}");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
