// Release config for the Dira CLI (dira + dirad + core/sources/contract).
//
// Composed from @dodi-smart/semantic-release-config rather than extended,
// because this repo needs a scope filter, a Rust version bump via
// `cargo set-version`, and github options the shared github() helper does
// not take. No `extends` line is needed for a composed config.
//
// Where this deviates from the shared default, and why:
//   - semantic-release-scope-filter runs first. It is not part of the shared
//     package (a third-party plugin this repo depends on directly): only
//     commits scoped to the CLI, daemon, contract, repo-wide, or deps count
//     toward a release.
//   - branches, releaseRules and release-notes types are identical to the
//     shared ones (checked literally), so commitAnalyzer() and releaseNotes()
//     run with no overrides.
//   - tagFormat is "v${version}" (Cargo convention), not the shared default.
//     That means the release commit message also carries the "v" prefix, so
//     git runs through plugin() directly instead of the git() helper, whose
//     message is hardcoded to the shared, unprefixed commitMessage.
//   - exec runs `cargo set-version` on the Rust workspace in place of npm;
//     npm is left out entirely since this is not a Node package.
//   - changelog only runs off the develop prerelease channel, same as the
//     behaviour this file replaces.
//   - github keeps this repo's muted options (no success comment, no release
//     labels, no assets) via plugin(), since github() only takes releasedLabels.
import { branches, commitAnalyzer, releaseNotes, changelog, exec, plugin } from "@dodi-smart/semantic-release-config";

const branch = process.env.GITHUB_REF_NAME || "";
const onDevelop = branch === "develop";

// Changelog + git commit only on release branches other than the develop
// prerelease channel. On develop we still bump versions but keep history
// clean of changelog churn.
const gitAssets = onDevelop ? ["Cargo.toml", "Cargo.lock"] : ["Cargo.toml", "Cargo.lock", "CHANGELOG.md"];

const config = {
  branches,
  tagFormat: "v${version}",
  plugins: [
    [
      "semantic-release-scope-filter",
      {
        scopes: ["cli", "daemon", "contract", "repo", "deps"],
        filterOutMissingScope: true,
      },
    ],
    commitAnalyzer(),
    releaseNotes(),
    ...(onDevelop ? [] : [changelog]),
    [
      exec,
      {
        // Bump the Rust workspace version + Cargo.lock before the commit/tag.
        prepareCmd: "cargo set-version --workspace ${nextRelease.version}",
      },
    ],
    [
      plugin("@semantic-release/git"),
      {
        assets: gitAssets,
        message: "chore(release): v${nextRelease.version} [skip ci]\n\n${nextRelease.notes}",
      },
    ],
    [plugin("@semantic-release/github"), { successComment: false, releasedLabels: false, assets: [] }],
  ],
};

export default config;
