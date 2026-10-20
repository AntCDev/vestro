use sqlx::postgres::PgPoolOptions;
use std::env;
use std::sync::Arc;

// Register our modules globally
mod networks;
mod tokens;
mod keys;
mod api;
mod orchestrator;
mod assets;
mod ledgerer;
mod routes;

#[derive(Clone)]
pub struct AppState {
    pub pool: sqlx::PgPool,
    pub networks: Arc<networks::NetworkRegistry>,
    pub registry: Arc<tokens::TokenRegistry>,
    pub orchestrator: Arc<orchestrator::PaymentOrchestrator>,
}

async fn initialize_database(pool: &sqlx::PgPool) -> Result<(), sqlx::Error> {
    sqlx::migrate!("./migrations").run(pool).await?;
    println!("Database tables initialized successfully.");
    Ok(())
}

#[tokio::main]
async fn main() {
    println!("==================================================");
    println!("🚀 Booting Payment Gateway...");
    println!("==================================================");

    dotenvy::dotenv().ok();

    let database_url = env::var("DATABASE_URL")
        .expect("DATABASE_URL environment variable must be set in .env");

    print!("🐘 Connecting to Database...");
    let pool = PgPoolOptions::new()
        .min_connections(10)
        .max_connections(100)
        .connect(&database_url)
        .await
        .expect("Failed to connect to PostgreSQL");
    println!(" Done.");

    print!("⚙️ Initializing Database Schema...");
    initialize_database(&pool)
        .await
        .expect("Failed to initialize database tables");
    println!(" Done.");

    // 1. Instantiate the networks ONCE on load
    let networks = Arc::new(networks::NetworkRegistry::from_env());

    // 2. Pass the singletons down to the token registry so handlers can clone the Arcs
    let registry = Arc::new(tokens::TokenRegistry::new(networks.clone()));

    // 3. Orchestrator
    let orchestrator = Arc::new(orchestrator::PaymentOrchestrator::new(
        pool.clone(),
        registry.clone(),
    ));

    // Code is authoritative: overwrites the assets rows on every boot.
    orchestrator.sync_assets().await.expect("Failed to sync assets");

    // DB is authoritative: never overwrites an operator's view mapping.
    registry.sync_checkout_views(&pool).await.expect("Failed to sync checkout views");

    // 4. Spawn payment watchers
    networks.spin_up_all(&pool);

    let state = AppState { pool, networks, registry, orchestrator };
    let app = routes::build(state);

    let port: u16 = env::var("PORT")
        .unwrap_or_else(|_| "3000".to_string())
        .parse()
        .expect("PORT must be a valid number");

    let host = env::var("HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let ip: std::net::IpAddr = host.parse().expect("HOST must be a valid IP address");
    let addr = std::net::SocketAddr::from((ip, port));

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();

    println!("\n==================================================");
    println!("⚡ Server booted up cleanly on http://{}", addr);
    if ip.is_unspecified() {
        println!("🔗 Local access: http://localhost:{}", port);
    }
    println!("==================================================\n");

    axum::serve(listener, app).await.unwrap();
}
