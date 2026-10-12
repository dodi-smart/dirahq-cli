---
id: DIRASH-0038
title: dira update refreshes the cwd repo's cloud pin through the new binary, never installs, never sweeps
status: active
guards:
  - cli/dira/src/update/mod.rs
  - cli/dira/src/cloud_init.rs
checks:
  - the post-update steps honour --no-zavet and --no-cloud :: cargo test -p dira --bin dira -- update::tests::post_update_steps_honour_no_zavet_and_no_cloud
  - refresh never creates artifacts and never lowers a pin :: cargo test -p dira --bin dira -- cloud_init::tests::refresh_never_creates_artifacts_and_never_downgrades
  - cloud refresh bumps and lists, never creates, never lowers :: cargo test -p dira --test cloud_init_e2e cloud_refresh_
  - a hung child is killed at the budget and a chatty one never deadlocks :: cargo test -p dira --bin dira -- update::tests::a_ch
  - the stale listing is read-only and excludes the repo just refreshed :: cargo test -p dira --bin dira -- cloud_init::tests::stale_known_repos_lists_each_older_repo_once_and_writes_nothing
origin: recorded
verified: true
---

## Decision

After a successful `dira update` (binaries swapped and the daemon back up), the
old process spawns the freshly installed binary as `dira cloud refresh
--after-update`. That command refreshes the `.dira/` cloud wiring of the repo
the update was run from, only when the wiring already exists, and only to a
newer version. It lists other repos the local store has seen events from that
still pin an older dira, and writes nothing there. `dira update --no-cloud`
skips the step. The step never changes the exit code.

## Why

`.dira/bootstrap.sh` pins the dira version a cloud VM installs. Without this
step every cloud session keeps running the version the laptop had when the
repo was wired, silently, after each local upgrade.

The running process cannot do the work. The swap replaces the file by rename
(D-0003), so the process that ran `dira update` still carries the old template
and the old `CARGO_PKG_VERSION`. Only the new binary can render the new pin.

The distribution spec used to say no cwd is resolved anywhere in the update
path, following DIRASH-0024. That rule exists so a machine-scope command never
rewrites committed files a user never asked for. The `.dira/` files are
dira's own generated output, and their presence is the repo owner's opt-in,
so refreshing them is inside that consent. Creating them is not, and stays
with `dira onboard` and `dira cloud init`.

## Rejected

- Refresh every repo the store knows about. DIRASH-0027 already rejected
  treating stored paths as live; they can be stale, unmounted, or deleted,
  and a dozen surprise diffs is worse than one list.
- Do the refresh in the old process. It would pin the old version.
- Auto-commit the refreshed files. The user reviews and commits.
- Opt-in flag only. Nobody learns a flag exists for a problem they cannot see.

## Agent directives

- Run the cloud refresh only in the success arm after `daemon::restart`, never
  on `--no-restart`, `--check`, or after a rollback.
- Spawn `bin_dir/dira cloud refresh --after-update`; never call
  `cloud_init::apply` from `update::run_update`.
- `cloud refresh` never creates `.dira/` and never lowers a pin. Compare with
  `RepoCloudStatus::pin_relation`.
- The list of other repos is read-only: `Store::open_readonly`, existence and
  pin checks only, no writes.
- A failure in this step is one printed line naming `dira cloud refresh`,
  never a non-zero exit.

## Verification

The `checks:` cover the decision logic, the never-create and
never-downgrade rules, the no-op paths, the spawn's budget and pipe
draining against a scripted `dira`, and the read-only listing. The spawn from the update arm is
not exercised end to end: the e2e suite always passes `--no-restart` (no
daemon restarts in tests, D-0021), so a human checks it by running `dira
update` inside a wired repo and reading the refresh line.
