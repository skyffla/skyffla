use std::env;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use skyffla_rendezvous::app::{build_router, AppState, IpRateLimiter};
use skyffla_rendezvous::store::{SharedStreamStore, SqliteStreamStore};
use skyffla_rendezvous::unix_timestamp_seconds;
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::from_env()?;
    let store: SharedStreamStore = Arc::new(SqliteStreamStore::new(&config.database_path)?);
    spawn_cleanup_task(
        store.clone(),
        Duration::from_secs(config.cleanup_interval_seconds),
    );

    let app = build_router(AppState {
        store,
        rate_limiter: Arc::new(IpRateLimiter::new(
            config.rate_limit_requests,
            config.rate_limit_window_seconds,
        )),
        trust_proxy_headers: config.trust_proxy_headers,
    });

    let listener = TcpListener::bind(config.listen_addr).await?;
    println!(
        "skyffla-rendezvous listening on {} using database {}",
        config.listen_addr, config.database_path
    );
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}

fn spawn_cleanup_task(store: SharedStreamStore, interval: Duration) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        loop {
            ticker.tick().await;
            match unix_timestamp_seconds() {
                Ok(now_epoch_seconds) => {
                    if let Err(error) = store.purge_expired(now_epoch_seconds) {
                        eprintln!("failed to purge expired streams: {error}");
                    }
                }
                Err(error) => eprintln!("failed to read system clock for cleanup: {error}"),
            }
        }
    });
}

struct Config {
    listen_addr: SocketAddr,
    database_path: String,
    cleanup_interval_seconds: u64,
    rate_limit_requests: usize,
    rate_limit_window_seconds: u64,
    trust_proxy_headers: bool,
}

impl Config {
    fn from_env() -> Result<Self, Box<dyn std::error::Error>> {
        let listen_addr = env::var("SKYFFLA_RENDEZVOUS_ADDR")
            .unwrap_or_else(|_| "127.0.0.1:8080".into())
            .parse()?;
        let database_path = env::var("SKYFFLA_RENDEZVOUS_DB_PATH")
            .unwrap_or_else(|_| "skyffla-rendezvous.db".into());
        let cleanup_interval_seconds = env::var("SKYFFLA_RENDEZVOUS_CLEANUP_INTERVAL_SECONDS")
            .ok()
            .map(|value| value.parse())
            .transpose()?
            .unwrap_or(30);
        let rate_limit_requests = env::var("SKYFFLA_RENDEZVOUS_RATE_LIMIT")
            .ok()
            .map(|value| value.parse())
            .transpose()?
            .unwrap_or(120);
        let rate_limit_window_seconds = env::var("SKYFFLA_RENDEZVOUS_RATE_WINDOW_SECONDS")
            .ok()
            .map(|value| value.parse())
            .transpose()?
            .unwrap_or(60);
        let trust_proxy_headers = env::var("SKYFFLA_RENDEZVOUS_TRUST_PROXY_HEADERS")
            .ok()
            .map(|value| parse_bool_env(&value))
            .unwrap_or(false);

        Ok(Self {
            listen_addr,
            database_path,
            cleanup_interval_seconds,
            rate_limit_requests,
            rate_limit_window_seconds,
            trust_proxy_headers,
        })
    }
}

fn parse_bool_env(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}
