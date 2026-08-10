---
name: Bug report
about: Something in dira, dirad, the installer, or the contract is broken
title: "[bug] "
labels: bug
---

**What happened**

**What you expected**

**Repro**
- `dira --version` (and `dirad --version` if they differ):
- Install method (`install.sh` / `install.ps1` / `just install` / built from source):
- OS + arch:
- Harness and version (Claude Code, Codex, Gemini, Cursor, OpenCode, Grok Build):
- Steps to reproduce:

**Output**
(`dira status`, `dira doctor --json`, daemon logs, the failing command's stderr —
paste as text, not a screenshot. If the bug is about capture not working, run
`dira doctor --probe` and include that too.)

**Does this involve captured data?**
Please don't paste prompt text, file contents, or diffs. If the bug is that dira captured
something it shouldn't have, describe the *shape* of what leaked and report it privately —
see [SECURITY.md](../../SECURITY.md).

**Is this a security issue?**
If this could expose the device key, let something reach the daemon that shouldn't, or put
content-bearing data into the store or onto the wire, please don't file it here — see
[SECURITY.md](../../SECURITY.md) for private reporting.
