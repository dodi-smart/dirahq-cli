---
id: DIRASH-0048
title: The Windows daemon is a windowless binary, registered per user by an XML logon task or a Run key, and started at once
status: active
guards:
  - cli/dirad/src/main.rs
  - cli/core/src/process.rs
checks:
  - the task XML names the user, survives battery, has no time limit and restarts on failure :: cargo test -p dira --bin dira -- windows_task_tests
  - Run-key supervision is its own variant and restarts by spawning, not schtasks /Run :: cargo test -p dira --bin dira -- daemon::tests
origin: session
verified: false
---

## Decision

`dirad` is built with `windows_subsystem = "windows"`, so no console window is ever created for
it. Every process it spawns goes through `dira_core::process::command`/`hidden`, which sets
`CREATE_NO_WINDOW`. `dira daemon install` registers the `DiraDaemon` task from an XML
definition (`schtasks /Create /XML`). That definition names the current user in both the logon
trigger and the principal (`InteractiveToken`, `LeastPrivilege`). It allows starting on battery
and keeps running when unplugged, sets `ExecutionTimeLimit` to `PT0S` (no limit), restarts on
failure every minute, and is `Hidden`. When the task cannot be registered, install writes the
HKCU Run key and **starts `dirad` immediately**. The Run key is its own `Supervision::RunKey`,
and a restart under it stops the process (with confirmed exit, D-0019) and spawns the
replacement directly. `dirad` writes its own pidfile once it holds the single-instance guard,
so every daemon, supervised or not, can be stopped by pid.

## Why

Windows users reported an empty console window that stayed open after every update, a daemon
that was not running after install or reboot, and a large backlog of unhandled events. All
three traced back to these causes:

- A console-subsystem `dirad` launched by Task Scheduler or Explorer (the Run key) gets a
  console window of its own. Logging goes to a file, so the window stays empty. Closing it
  stops the daemon (`CTRL_CLOSE_EVENT`).
- `schtasks /SC ONLOGON` with no user registers an "any user" trigger, which needs admin. On a
  standard account it always failed, and the Run-key fallback returned success without
  starting anything, so nothing ran until the next logon. When the task did register, its flat
  form inherited the defaults: no start on battery, stop when unplugged, killed after 72 h,
  never restarted.
- Detection mapped the Run key to `ScheduledTask`, so `dira update`'s restart ran
  `schtasks /Run` against a task that did not exist. The update then rolled back and left the
  daemon stopped.

## Rejected

- **Keep a console `dirad` and launch it with `CREATE_NO_WINDOW` everywhere.** Task Scheduler
  and Explorer launch it, not us, so there is no flag to pass on those paths.
- **A Windows service.** Installing a service needs admin, and it runs in session 0 under
  another account, so it cannot see the user's per-user pipe ACL (D-0016) or their keychain.
- **Only the Run key.** It is reliably admin-free, but it never restarts a crashed daemon. The
  XML task does, so the Run key stays the fallback.
- **A Startup-folder shortcut.** Same launch-once limit as the Run key, and users and
  "startup cleaner" tools delete it more easily.

## Agent directives

- Never spawn a process from code `dirad` runs without `dira_core::process::command`/`hidden`.
  A console child of a windowless parent opens a visible window.
- Never `AttachConsole` in `dirad` outside `--version`. Attaching ties the daemon's lifetime to
  whichever terminal started it.
- Keep `UserId` in both the trigger and the principal of `logon_task_xml`, and keep the battery,
  time-limit and restart settings. They are the fix, not decoration.
- Run-key installs must start the daemon now (`Launch::NeedsStart`). A Run-key restart spawns
  the replacement and never calls `schtasks`.
- Start every long-lived child (`dirad`, the background updater) with
  `dira_core::process::spawn_isolated`. On windows a child otherwise inherits the parent's own
  stdout pipe and holds it open for its life. Whoever reads that pipe then hangs: `install.ps1`
  piping `dira daemon install`, or a harness reading `dira hook`.
- Validated on 2026-11-10 in a Windows 11 ARM64 VM (x64 build, filtered medium-integrity token):
  - task registration without elevation;
  - no console window on install, update, restart or a Run-key logon launch;
  - Run-key restart, stop and uninstall;
  - background update and rollback.

  Not exercised: a real reboot and logon (the trigger was simulated with `schtasks /Run` and an
  Explorer-style launch), and a non-admin account. So this stays `verified: false`.
