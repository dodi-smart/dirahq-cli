//! `dirad` — the resident Dira daemon binary.
//!
//! All the wiring lives in the [`dirad`] library crate so integration tests can
//! stand up the same daemon; this binary just initializes tracing and calls
//! [`dirad::run`].

fn main() -> anyhow::Result<()> {
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

    // Resolve the system local UTC offset while the process is still single-threaded
    // — before building the multithreaded tokio runtime. `current_local_offset()`
    // errors once other threads exist, so this MUST run first for `report_local_day`
    // to resolve a real local day boundary instead of always falling back to UTC.
    dirad::init_local_offset();

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(dirad::run())
}
