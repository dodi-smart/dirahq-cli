// Commitlint config (ESM .mjs so the commitlint-github-action accepts it and we
// can express a function-based `ignores`).
//
// Conventional Commits, enforced by the `commit-lint` CI job. The rules mirror
// the documented policy in CLAUDE.md: a fixed type list, a mandatory scope from
// the release-mapped set, and ≤180-char lines.
//
// `ignores` exempts integration *merge* commits whose subject uses a
// `merge(<scope>): …` prefix. Commitlint's built-in `defaultIgnores` only skips
// subjects starting with `Merge ` (capital, GitHub's default), so these custom
// merge subjects would otherwise be linted as real commits and rejected on
// their `merge` "type". Merge commits are conventionally exempt from the type
// rules, so we skip them explicitly while keeping `defaultIgnores` on.
export default {
  extends: ["@commitlint/config-conventional"],
  defaultIgnores: true,
  ignores: [(message) => /^merge\(/i.test(message)],
  rules: {
    "body-max-line-length": [1, "always", 180],
    "footer-max-line-length": [1, "always", 180],
    "header-max-length": [1, "always", 180],
    "subject-case": [2, "never", ["upper-case", "pascal-case"]],
    "type-enum": [
      2,
      "always",
      [
        "feat",
        "fix",
        "docs",
        "style",
        "refactor",
        "perf",
        "test",
        "build",
        "ci",
        "chore",
        "revert",
      ],
    ],
    "scope-enum": [
      2,
      "always",
      ["cli", "daemon", "contract", "ci", "release", "repo", "deps"],
    ],
    "scope-empty": [2, "never"],
  },
};
