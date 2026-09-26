pub mod api;
pub mod customers_api;
pub mod customers_web;
pub mod documents_web;
pub mod identity_api;
pub mod identity_web;
pub mod inventory_api;
pub mod inventory_web;
pub mod purchases_api;
pub mod purchases_web;
pub mod roles_web;
pub mod sales_api;
pub mod sales_web;
pub mod settings_web;
pub mod setup_web;
pub mod suppliers_web;
pub mod users_web;
pub mod web;

use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
    Json, Router,
};
use sqlx::{Row as _, SqlitePool};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tower_http::services::ServeDir;

use crate::error::{AppError, AppResult};
use crate::localization::{load_context, LocalizationContext, MessageKey};
use crate::models::PriceRefusal;

use crate::repositories::{
    SqliteAccountRepository, SqliteBarcodeRepository, SqliteBusinessConfigurationRepository,
    SqliteCategoryRepository, SqliteCustomerReceiptRepository, SqliteCustomerRepository,
    SqliteDocSequenceRepository, SqlitePaymentMethodRepository, SqliteProductRepository,
    SqliteProductSupplierCostRepository, SqliteProductTaxRepository, SqlitePurchaseRepository,
    SqliteRoleRepository, SqliteSaleRepository, SqliteSessionRepository, SqliteSetupRepository,
    SqliteStockMovementRepository, SqliteSupplierRepository, SqliteTaxRepository,
    SqliteTaxSnapshotRepository, SqliteTransactionRepository, SqliteUserRepository,
};
use crate::routes::setup_web::setup_gate;
use crate::security::auth_middleware;
use crate::services::identity::{SystemClock, ThrottleConfig};
use crate::services::{
    AccountService, CustomerReceiptService, CustomerService, DocumentService, IdentityService,
    InventoryService, PaymentMethodService, PurchasesService, SalesService, SettingsService,
    SetupService, SupplierService, TaxService, TransactionService,
};

pub type InventorySvc = InventoryService<
    SqliteCategoryRepository,
    SqliteProductRepository,
    SqliteBarcodeRepository,
    SqliteStockMovementRepository,
>;

pub type SalesSvc = SalesService<
    SqliteSaleRepository,
    SqliteDocSequenceRepository,
    SqliteCategoryRepository,
    SqliteProductRepository,
    SqliteBarcodeRepository,
    SqliteStockMovementRepository,
    SqliteAccountRepository,
    SqliteTransactionRepository,
    SqlitePaymentMethodRepository,
    SqliteCustomerRepository,
    SqliteTaxSnapshotRepository,
>;

pub type CustomerSvc = CustomerService<SqliteCustomerRepository>;

/// The cross-department documents index: read-only composition over the four
/// repository families the `/documents` page reads.
pub type DocumentSvc = DocumentService<
    SqliteSaleRepository,
    SqlitePurchaseRepository,
    SqliteCustomerReceiptRepository,
    SqliteStockMovementRepository,
>;

/// Receipts: the grouped payments of one handover of money. It wraps the same
/// sales service the routes use, so every grouped payment reaches sales and
/// finance exactly like any other payment.
pub type ReceiptSvc = CustomerReceiptService<
    SqliteCustomerReceiptRepository,
    SqliteSaleRepository,
    SqliteDocSequenceRepository,
    SqliteCategoryRepository,
    SqliteProductRepository,
    SqliteBarcodeRepository,
    SqliteStockMovementRepository,
    SqliteAccountRepository,
    SqliteTransactionRepository,
    SqlitePaymentMethodRepository,
    SqliteCustomerRepository,
    SqliteTaxSnapshotRepository,
>;

pub type MethodSvc = PaymentMethodService<SqlitePaymentMethodRepository>;

pub type TaxSvc =
    TaxService<SqliteProductRepository, SqliteTaxRepository, SqliteProductTaxRepository>;

pub type SupplierSvc =
    SupplierService<SqliteSupplierRepository, SqliteProductSupplierCostRepository>;

pub type PurchasesSvc = PurchasesService<
    SqlitePurchaseRepository,
    SqliteDocSequenceRepository,
    SqliteSupplierRepository,
    SqliteProductSupplierCostRepository,
    SqliteCategoryRepository,
    SqliteProductRepository,
    SqliteBarcodeRepository,
    SqliteStockMovementRepository,
    SqliteAccountRepository,
    SqliteTransactionRepository,
    SqlitePaymentMethodRepository,
    SqliteTaxSnapshotRepository,
>;

/// The identity service the deny-by-default gate and the login/logout routes
/// resolve sessions through: SQLite repositories, wall-clock UTC, argon2id.
pub type IdentitySvc = IdentityService<
    SqliteUserRepository,
    SqliteSessionRepository,
    SqliteRoleRepository,
    SystemClock,
    crate::security::PasswordHasher,
>;

pub type SetupSvc = SetupService<SqliteSetupRepository, crate::security::PasswordHasher>;

pub type SettingsSvc = SettingsService<SqliteBusinessConfigurationRepository>;

#[derive(Clone)]
pub(crate) struct CurrencyOption {
    pub(crate) code: String,
    pub(crate) display_name: String,
}

pub(crate) const CURRENCY_CATALOG: [(&str, &str); 10] = [
    ("ARS", "Argentine Peso"),
    ("AUD", "Australian Dollar"),
    ("BRL", "Brazilian Real"),
    ("CAD", "Canadian Dollar"),
    ("EUR", "Euro"),
    ("GBP", "British Pound"),
    ("JPY", "Japanese Yen"),
    ("MXN", "Mexican Peso"),
    ("USD", "United States Dollar"),
    ("UYU", "Uruguayan Peso"),
];

pub(crate) fn currency_options(current_code: &str) -> Vec<CurrencyOption> {
    let mut options = CURRENCY_CATALOG
        .iter()
        .map(|(code, display_name)| CurrencyOption {
            code: (*code).to_owned(),
            display_name: (*display_name).to_owned(),
        })
        .collect::<Vec<_>>();
    if !CURRENCY_CATALOG
        .iter()
        .any(|(code, _)| *code == current_code)
    {
        options.push(CurrencyOption {
            code: current_code.to_owned(),
            display_name: current_code.to_owned(),
        });
    }
    options
}

// ---------------------------------------------------------------------------
// The ONE price-refusal renderer, shared by every surface that can answer one
// ---------------------------------------------------------------------------

/// The ONE mapping from a price refusal to a sentence. It lives here, in the
/// module every route shares, and not in the product module, because a price
/// refusal does not only reach the product form: the purchase record page's
/// "apply line cost" action writes a product's `cost_price` through
/// `InventoryService::update_product`, so the price rules run there too. Three
/// surfaces — the ladder preview, the product save routes and that purchase
/// action — reach this function, and they can only disagree in wording or in
/// language if someone edits it here.
///
/// Total by construction: the match has no fallback arm, so a new `PriceRefusal`
/// variant does not compile until it is translated, and the closed-catalog test
/// in `localization_tests` pins both catalogs to the same set.
pub(crate) fn price_refusal_key(refusal: &PriceRefusal) -> MessageKey {
    match refusal {
        PriceRefusal::MarkupNotAboveMinus100 => MessageKey::PriceRefusalMarkupNotAboveMinus100,
        PriceRefusal::MarkupNeedsPositiveCost => MessageKey::PriceRefusalMarkupNeedsPositiveCost,
        PriceRefusal::DerivationOverflow => MessageKey::PriceRefusalDerivationOverflow,
        PriceRefusal::SalePriceNotPositiveForProduct => {
            MessageKey::PriceRefusalSalePriceNotPositiveForProduct
        }
        PriceRefusal::SalePriceNegative => MessageKey::PriceRefusalSalePriceNegative,
        PriceRefusal::CostPriceNegative => MessageKey::PriceRefusalCostPriceNegative,
        PriceRefusal::SalePriceRequired => MessageKey::PriceRefusalSalePriceRequired,
    }
}

/// The refusal as the active locale words it. Every surface that states a price
/// refusal calls this, so one operator reading two of them reads the same
/// sentence twice.
pub(crate) fn price_refusal_message(
    refusal: &PriceRefusal,
    localization: &LocalizationContext,
) -> String {
    localization.tr(price_refusal_key(refusal)).to_owned()
}

/// A price refusal leaves a service as a typed `AppError`, and this is where a
/// surface that holds the operator's language turns it back into something they
/// can read. Everything else travels untouched: a conflict, a 404 or a database
/// fault must reach the operator exactly as it did before, and a caller that has
/// no `LocalizationContext` simply does not call this.
///
/// The localized sentence rides a `Validation`, which is the same 400 with the
/// same body shape the refusal always answered with — the global
/// `htmx:responseError` handler paints either one verbatim, and a plain browser
/// post reads the same JSON.
pub(crate) fn localized_refusal_error(
    error: AppError,
    localization: &LocalizationContext,
) -> AppError {
    match error {
        AppError::PriceRefused(refusal) => {
            AppError::Validation(price_refusal_message(&refusal, localization))
        }
        other => other,
    }
}

#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub account_service: AccountService<SqliteAccountRepository, SqliteTransactionRepository>,
    pub transaction_service:
        TransactionService<SqliteAccountRepository, SqliteTransactionRepository>,
    pub inventory_service: InventorySvc,
    pub sales_service: SalesSvc,
    pub customer_service: CustomerSvc,
    pub customer_receipt_service: ReceiptSvc,
    pub payment_method_service: MethodSvc,
    pub tax_service: TaxSvc,
    pub supplier_service: SupplierSvc,
    pub purchases_service: PurchasesSvc,
    /// Identity kernel service (S1b): the single session-validity opinion the
    /// guard and the login/logout routes share.
    pub identity_service: IdentitySvc,
    pub setup_service: SetupSvc,
    pub settings_service: SettingsSvc,
    setup_required: Arc<AtomicBool>,
    /// The documents index (`/documents`): the four families' read paths,
    /// every filter already permission-narrowed by the route. Wired exactly
    /// like the sibling services (it derives `Clone`, so no `Arc` wrapper).
    pub document_service: DocumentSvc,
    pub allow_negative: bool,
    pub allow_negative_stock: bool,
    /// `ENFORCE_CREDIT_LIMIT` (default true): the sales service rejects a credit
    /// confirm whose projected debt exceeds the customer's limit.
    pub enforce_credit_limit: bool,
}

impl AppState {
    /// Compatibility constructor: credit-limit enforcement defaults to true,
    /// exactly like `main` when the env var is absent.
    pub fn new(pool: SqlitePool, allow_negative: bool, allow_negative_stock: bool) -> Self {
        Self::new_with_credit_limit(pool, allow_negative, allow_negative_stock, true)
    }

    pub fn new_with_credit_limit(
        pool: SqlitePool,
        allow_negative: bool,
        allow_negative_stock: bool,
        enforce_credit_limit: bool,
    ) -> Self {
        // Production identity defaults: 12h absolute TTL, non-Secure cookie and
        // the shipped throttle shape. `main` overrides the policy and throttle
        // from the environment via `new_with_identity`; the hasher is always
        // the production argon2id (parameters pinned by a test in password.rs).
        Self::new_with_identity(
            pool,
            allow_negative,
            allow_negative_stock,
            enforce_credit_limit,
            crate::security::SessionPolicy::new(12, false),
            ThrottleConfig::default(),
        )
    }

    /// `main`'s constructor: the environment-configured session policy and
    /// login throttle, production hasher always.
    pub fn new_with_identity(
        pool: SqlitePool,
        allow_negative: bool,
        allow_negative_stock: bool,
        enforce_credit_limit: bool,
        policy: crate::security::SessionPolicy,
        throttle: ThrottleConfig,
    ) -> Self {
        let identity_service = IdentityService::new(
            SqliteUserRepository::new(pool.clone()),
            SqliteSessionRepository::new(pool.clone()),
            SqliteRoleRepository::new(pool.clone()),
            SystemClock,
            crate::security::PasswordHasher::production(),
            policy,
            throttle,
        );
        Self::with_identity_service(
            pool,
            allow_negative,
            allow_negative_stock,
            enforce_credit_limit,
            identity_service,
        )
    }

    /// Inject a fully-built identity service. The test suite uses this through
    /// `security/test_support` (light hasher); production constructors build
    /// the service themselves so the production hasher cannot be swapped out.
    pub fn with_identity_service(
        pool: SqlitePool,
        allow_negative: bool,
        allow_negative_stock: bool,
        enforce_credit_limit: bool,
        identity_service: IdentitySvc,
    ) -> Self {
        let acc_repo = SqliteAccountRepository::new(pool.clone());
        let tx_repo = SqliteTransactionRepository::new(pool.clone());
        let account_service = AccountService::new(acc_repo.clone(), tx_repo.clone());
        let transaction_service =
            TransactionService::new(acc_repo.clone(), tx_repo.clone(), allow_negative);
        let inventory_service = InventoryService::new(
            SqliteCategoryRepository::new(pool.clone()),
            SqliteProductRepository::new(pool.clone()),
            SqliteBarcodeRepository::new(pool.clone()),
            SqliteStockMovementRepository::new(pool.clone()),
            allow_negative_stock,
        );
        let tax_service = TaxService::new(
            SqliteProductRepository::new(pool.clone()),
            SqliteTaxRepository::new(pool.clone()),
            SqliteProductTaxRepository::new(pool.clone()),
        );
        let method_repo = SqlitePaymentMethodRepository::new(pool.clone());
        let payment_method_service = PaymentMethodService::new(method_repo.clone());
        let customer_service = CustomerService::new(SqliteCustomerRepository::new(pool.clone()));
        let sales_service = SalesService::new(
            SqliteSaleRepository::new(pool.clone()),
            SqliteDocSequenceRepository::new(pool.clone()),
            inventory_service.clone(),
            transaction_service.clone(),
            method_repo.clone(),
            customer_service.clone(),
            SqliteTaxSnapshotRepository::new(pool.clone()),
            enforce_credit_limit,
        );
        // M4: collections group the payments one handover of money produced; the
        // receipt service composes the same sales service and the finance-owned
        // (account, method) allowlist the rest of the app uses.
        let customer_receipt_service = CustomerReceiptService::new(
            SqliteCustomerReceiptRepository::new(pool.clone()),
            sales_service.clone(),
            payment_method_service.clone(),
        );
        // M3: suppliers + product/supplier cost satellite are consumed by the
        // purchases orchestrator; both share the same SQLite repos as the rest
        // of the app.
        let supplier_service = SupplierService::new(
            SqliteSupplierRepository::new(pool.clone()),
            SqliteProductSupplierCostRepository::new(pool.clone()),
        );
        let purchases_service = PurchasesService::new(
            SqlitePurchaseRepository::new(pool.clone()),
            SqliteDocSequenceRepository::new(pool.clone()),
            supplier_service.clone(),
            inventory_service.clone(),
            transaction_service.clone(),
            PaymentMethodService::new(method_repo),
            SqliteTaxSnapshotRepository::new(pool.clone()),
        );
        // The documents index composes the four families' read paths; it holds
        // only reads, so wiring it never moves write ownership.
        let document_service = DocumentService::new(
            SqliteSaleRepository::new(pool.clone()),
            SqlitePurchaseRepository::new(pool.clone()),
            SqliteCustomerReceiptRepository::new(pool.clone()),
            SqliteStockMovementRepository::new(pool.clone()),
        );
        let setup_service = SetupService::new(
            SqliteSetupRepository::new(pool.clone()),
            crate::security::PasswordHasher::production(),
        );
        let settings_service =
            SettingsService::new(SqliteBusinessConfigurationRepository::new(pool.clone()));
        Self {
            pool,
            account_service,
            transaction_service,
            inventory_service,
            sales_service,
            customer_service,
            customer_receipt_service,
            payment_method_service,
            tax_service,
            supplier_service,
            purchases_service,
            identity_service,
            setup_service,
            settings_service,
            setup_required: Arc::new(AtomicBool::new(false)),
            document_service,
            allow_negative,
            allow_negative_stock,
            enforce_credit_limit,
        }
    }

    /// Refresh the startup gate from the business configuration singleton.
    pub async fn refresh_setup_requirement(&self) -> AppResult<()> {
        self.setup_required
            .store(self.setup_service.is_required().await?, Ordering::Release);
        Ok(())
    }

    pub(crate) fn setup_required(&self) -> bool {
        self.setup_required.load(Ordering::Acquire)
    }

    pub(crate) fn mark_setup_complete(&self) {
        self.setup_required.store(false, Ordering::Release);
    }
}

/// Resolve the display names of the audit actors a finance view renders
/// (M5 Phase B). A department may not read identity tables and may not take the
/// identity service as a dependency (AC20 — the grep in `security/authz.rs`
/// scans the department routes and repositories), and the spec's interface rule
/// says the views show the actor as a name, never as an id. So the resolution
/// lives here in the wiring layer — this module is where `AppState` is
/// composed and the one place the boundary test explicitly allows to touch
/// identity — and the finance handlers receive display names alongside the
/// audit ids. An id that resolves to nothing cannot be rendered by the
/// validated data paths (every `created_by` is a live FK); the fallback exists
/// so a concurrent deactivation degrades to an explicit marker instead of a
/// blank row.
pub async fn audit_actor_names(
    pool: &SqlitePool,
    actor_ids: &[i64],
) -> AppResult<std::collections::BTreeMap<i64, String>> {
    // The callers collect ids straight from validated rows, so the list is
    // small and the ids are integers; dedupe keeps the statement small.
    let distinct: Vec<i64> = {
        let mut seen = std::collections::BTreeSet::new();
        actor_ids
            .iter()
            .copied()
            .filter(|id| seen.insert(*id))
            .collect()
    };
    let mut names: std::collections::BTreeMap<i64, String> = Default::default();
    if distinct.is_empty() {
        return Ok(names);
    }
    let mut sql = String::from("SELECT id, display_name FROM users WHERE id IN (");
    for (index, _) in distinct.iter().enumerate() {
        if index > 0 {
            sql.push(',');
        }
        sql.push('?');
    }
    sql.push(')');
    // The statement contains only `?` placeholders; every value arrives bound
    // as an integer collected from validated rows, nothing is interpolated.
    let mut query = sqlx::query(sqlx::AssertSqlSafe(sql));
    for id in &distinct {
        query = query.bind(*id);
    }
    let rows = query.fetch_all(pool).await?;
    for row in rows {
        let id: i64 = row.try_get(0)?;
        let display_name: String = row.try_get(1)?;
        names.insert(id, display_name);
    }
    Ok(names)
}

/// The ids of the users a typed actor filter matches, or `None` when the
/// filter is empty. `Some(empty)` means the typed name matched no user — a
/// filter that matches nothing, never an error and never a silent "all".
/// The match is the same normalized-substring rule `matching_customer_ids`
/// uses, over `display_name` OR `username`, run in Rust because the identity
/// tables are small by nature (if that ever stops being true this needs a
/// normalized index instead). Lives here, with `audit_actor_names`, because
/// this is the only layer allowed to read the identity tables (AC20), and the
/// statement below carries no interpolated text at all — the comparison runs
/// after the read, in Rust.
pub async fn audit_actor_ids(pool: &SqlitePool, needle: &str) -> AppResult<Option<Vec<i64>>> {
    let trimmed = needle.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let needle = crate::models::normalize_search(trimmed);
    let rows =
        sqlx::query_as::<_, (i64, String, String)>("SELECT id, display_name, username FROM users")
            .fetch_all(pool)
            .await?;
    Ok(Some(
        rows.into_iter()
            .filter(|(_, display_name, username)| {
                crate::models::normalize_search(display_name).contains(&needle)
                    || crate::models::normalize_search(username).contains(&needle)
            })
            .map(|(id, _, _)| id)
            .collect(),
    ))
}

/// Resolve presentation metadata once per request and make it available to
/// full pages, HTMX fragments, and the static picker boundary. The context is
/// an extension only: repositories and domain services remain locale-free.
async fn localization_middleware(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> AppResult<Response> {
    let context = load_context(&state.pool).await?;
    request.extensions_mut().insert(context);
    Ok(next.run(request).await)
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .merge(api::router())
        .merge(web::router())
        .merge(setup_web::router())
        .merge(identity_web::router())
        .merge(identity_api::router())
        .merge(customers_api::router())
        .merge(customers_web::router())
        .merge(inventory_api::router())
        .merge(inventory_web::router())
        .merge(sales_api::router())
        .merge(sales_web::router())
        .merge(purchases_api::router())
        .merge(purchases_web::router())
        .merge(documents_web::router())
        .merge(suppliers_web::router())
        .merge(users_web::router())
        .merge(roles_web::router())
        .merge(settings_web::router())
        .nest_service("/static", ServeDir::new("static"))
        .fallback(route_not_found)
        // Deny by default (S1b part 2): one gate in front of every route and
        // the fallback, so an unlisted path fails closed.
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            setup_gate,
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            localization_middleware,
        ))
        .with_state(state)
}

/// Distinctive body for the router-level 404 fallback. A routing miss must be
/// distinguishable from a handler-level 404 (which returns its own message),
/// so the smoke suite can use this marker as an oracle for the routing table.
async fn route_not_found() -> impl IntoResponse {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "error": "route not found" })),
    )
}

#[cfg(test)]
mod tests {
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::str::FromStr;
    use tower::ServiceExt;

    use super::{router, AppState};
    use crate::security::test_support;

    async fn test_state() -> AppState {
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")
            .unwrap()
            .create_if_missing(true)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        // S1b part 1: seed the fixed test session every request will authenticate with.
        test_support::seed_session(&pool).await.unwrap();
        AppState::new(pool, false, true)
    }

    #[tokio::test]
    async fn static_assets_are_served_from_disk() {
        let app = router(test_state().await);
        for uri in ["/static/htmx.min.js", "/static/tailwind.css"] {
            let req = Request::builder()
                .method("GET")
                .uri(uri)
                .header("cookie", test_support::TEST_COOKIE)
                .body(Body::empty())
                .unwrap();
            let resp = app.clone().oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "{uri} should be served");
        }
    }

    /// S1b part 2, FIX-4: the same assets must also load for a request that
    /// carries NO session at all. `/static/*` is the only route a browser hits
    /// before it has a cookie (the login page itself depends on both files), so
    /// a redirect here would leave every page unstyled and htmx-less — the
    /// allowlist entry has to be proven over HTTP, not only through the
    /// `is_public` predicate it is built from.
    #[tokio::test]
    async fn static_assets_load_without_a_session() {
        let app = router(test_state().await);
        for uri in ["/static/htmx.min.js", "/static/tailwind.css"] {
            let req = Request::builder()
                .method("GET")
                .uri(uri)
                .body(Body::empty())
                .unwrap();
            let resp = app.clone().oneshot(req).await.unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::OK,
                "{uri} must load anonymously: the login page needs it before any cookie exists"
            );
            let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
                .await
                .unwrap();
            assert!(
                !bytes.is_empty(),
                "{uri} must serve real bytes, not an empty body"
            );
        }
    }
}
