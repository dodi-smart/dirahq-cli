// Release config for the Dira CLI (dira + dirad + core/sources/contract).
// Scope-filtered: only commits scoped to the CLI (or shared `contract`/repo-wide
// scopes) count toward a release. The version source of truth is
// `[workspace.package] version` in the root Cargo.toml; `cargo set-version`
// bumps it (and Cargo.lock). Tags are `v${version}`.
const branch = process.env.GITHUB_REF_NAME || "";

const config = {
  branches: ["main", { name: "develop", prerelease: true, channel: "develop" }],
  tagFormat: "v${version}",
  plugins: [
    [
      "semantic-release-scope-filter",
      {
        scopes: ["cli", "daemon", "contract", "repo", "deps"],
        filterOutMissingScope: true,
      },
    ],
    [
      "@semantic-release/commit-analyzer",
      {
        preset: "conventionalcommits",
        releaseRules: [
          { type: "feat", release: "minor" },
          { type: "fix", release: "patch" },
          { type: "perf", release: "patch" },
          { type: "revert", release: "patch" },
          { type: "docs", release: false },
          { type: "style", release: false },
          { type: "refactor", release: "patch" },
          { type: "test", release: false },
          { type: "build", release: false },
          { type: "ci", release: false },
          { breaking: true, release: "major" },
        ],
      },
    ],
    [
      "@semantic-release/release-notes-generator",
      {
        preset: "conventionalcommits",
        presetConfig: {
          types: [
            { type: "feat", section: "✨ Features", hidden: false },
            { type: "fix", section: "🐛 Bug Fixes", hidden: false },
            { type: "perf", section: "⚡ Performance Improvements", hidden: false },
            { type: "revert", section: "⏪ Reverts", hidden: false },
            { type: "docs", section: "📚 Documentation", hidden: false },
            { type: "style", section: "💄 Styles", hidden: false },
            { type: "refactor", section: "♻️ Code Refactoring", hidden: false },
            { type: "test", section: "✅ Tests", hidden: false },
            { type: "build", section: "📦 Build System", hidden: false },
            { type: "ci", section: "👷 Continuous Integration", hidden: false },
            { type: "chore", section: "🔧 Miscellaneous Chores", hidden: true },
          ],
        },
        writerOpts: { commitsSort: ["scope", "subject"] },
      },
    ],
    [
      "@semantic-release/exec",
      {
        // Bump the Rust workspace version + Cargo.lock before the commit/tag.
        prepareCmd: "cargo set-version --workspace ${nextRelease.version}",
      },
    ],
    [
      "@semantic-release/github",
      { successComment: false, releasedLabels: false, assets: [] },
    ],
  ],
};

// Changelog + git commit only on release branches other than the develop
// prerelease channel. On develop we still bump versions but keep history clean
// of changelog churn.
if (branch !== "develop") {
  config.plugins.splice(3, 0, [
    "@semantic-release/changelog",
    { changelogFile: "CHANGELOG.md" },
  ]);
  config.plugins.splice(-1, 0, [
    "@semantic-release/git",
    {
      assets: ["Cargo.toml", "Cargo.lock", "CHANGELOG.md"],
      message:
        "chore(release): v${nextRelease.version} [skip ci]\n\n${nextRelease.notes}",
    },
  ]);
} else {
  config.plugins.splice(-1, 0, [
    "@semantic-release/git",
    {
      assets: ["Cargo.toml", "Cargo.lock"],
      message:
        "chore(release): v${nextRelease.version} [skip ci]\n\n${nextRelease.notes}",
    },
  ]);
}

module.exports = config;
