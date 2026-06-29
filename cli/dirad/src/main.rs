//! `dirad` — the resident Dira daemon binary.
//!
//! All the wiring lives in the [`dirad`] library crate so integration tests can
//! stand up the same daemon; this binary just initializes tracing and calls
//! [`dirad::run`].

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // The daemon takes no real CLI args, but answer `--version`/`-V` directly so
    // it can be inspected the same way as `dira` (e.g. by a service script).
    if std::env::args()
        .skip(1)
        .any(|a| a == "--version" || a == "-V")
    {
        println!(
            "dirad {} (schema {})",
            env!("CARGO_PKG_VERSION"),
            dira_contract::SCHEMA_VERSION
        );
        return Ok(());
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "dirad=info,warn".into()),
        )
        .init();

    dirad::run().await
}
