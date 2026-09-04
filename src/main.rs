mod api;
mod config;
mod error;
mod snap;
mod state;
mod validation;

use std::sync::Arc;

use tokio::net::TcpListener;
use tracing::info;
use tracing_subscriber::EnvFilter;

use crate::{config::Config, snap::SnapManager, state::StateStore};

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("telesnap: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| "telesnap=info".into()),
        )
        .init();

    config::ensure_root()?;
    let config = Arc::new(Config::from_env()?);
    config.prepare_directories()?;

    let store = StateStore::load(config.state_path.clone()).await?;
    let manager = Arc::new(SnapManager::new(config.clone(), store));
    if let Err(error) = manager.refresh_readiness().await {
        tracing::warn!(%error, "snapd is not ready; API will start unavailable");
    } else {
        manager.reconcile_state().await?;
    }
    let app = api::router(manager.clone(), config.api_token.clone());

    let listener = TcpListener::bind(config.bind).await?;
    info!(address = %config.bind, "telesnap API listening");
    let maintenance = tokio::spawn(manager.clone().run_maintenance_loop());
    let readiness = tokio::spawn(manager.run_readiness_loop());
    let server = axum::serve(listener, app).with_graceful_shutdown(shutdown_signal());
    tokio::select! {
        result = server => result?,
        result = maintenance => {
            return Err(format!("maintenance worker stopped unexpectedly: {result:?}").into());
        }
        result = readiness => {
            return Err(format!("readiness worker stopped unexpectedly: {result:?}").into());
        }
    }
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
}
