//! `dirad` — the resident Dira daemon binary.
//!
//! All the wiring lives in the [`dirad`] library crate so integration tests can
//! stand up the same daemon; this binary just initializes tracing and calls
//! [`dirad::run`].
//!
//! On windows this is a windows-subsystem binary (DIRASH-0048). A console binary
//! launched by Task Scheduler or the Run key gets a console window of its own,
//! which stayed open and empty for as long as the daemon ran (every logon, and
//! after every `dira update`), and closing it shut the daemon down. A
//! windows-subsystem process never gets one. It logs to a file anyway.
#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() -> anyhow::Result<()> {
    // Children (git, plugin scripts, the background updater) must not hold
    // whatever handles this process was started with.
    dira_core::process::disinherit_std_handles();
    // The daemon takes no real CLI args, but answer `--version`/`-V` directly so
    // it can be inspected the same way as `dira` (e.g. by a service script).
    if std::env::args()
        .skip(1)
        .any(|a| a == "--version" || a == "-V")
    {
        // A windows-subsystem process starts with no console. Borrow the
        // terminal's, only on this path: attaching on a normal start would tie
        // the daemon to whatever console launched it, and closing that console
        // would then stop the daemon.
        #[cfg(windows)]
        unsafe {
            windows_sys::Win32::System::Console::AttachConsole(
                windows_sys::Win32::System::Console::ATTACH_PARENT_PROCESS,
            );
        }
        println!(
            "dirad {} (schema {})",
            env!("CARGO_PKG_VERSION"),
            dira_contract::SCHEMA_VERSION
        );
        return Ok(());
    }

    // On windows the daemon is spawned with all three stdio handles nulled and
    // no console, so anything written to stdout goes nowhere — which is why a
    // days-long capture outage left nothing to diagnose. Log to a size-capped
    // file there (and anywhere `DIRA_LOG_DIR` asks). macOS keeps stdout because
    // the launchd plist already redirects it; Linux keeps it for journald.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "dirad=info,warn".into());
    match dirad::logfile::log_dir().and_then(|d| dirad::logfile::RollingLog::open(&d).ok()) {
        Some(log) => tracing_subscriber::fmt()
            .with_env_filter(filter)
            // No SGR escapes in a file nobody is going to `cat` with a pager.
            .with_ansi(false)
            .with_writer(std::sync::Mutex::new(log))
            .init(),
        None => tracing_subscriber::fmt().with_env_filter(filter).init(),
    }

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
