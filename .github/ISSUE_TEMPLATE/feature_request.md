---
name: Feature request
about: Suggest an addition or change to dira, dirad, the installer, or the contract
title: "[feat] "
labels: enhancement
---

**Problem**
What are you trying to do that dira doesn't support today?

**Proposed solution**

**Alternatives considered**

**Anything else**

Two things that usually need a heads-up:

- **Does this touch `/contract`?** The wire schema is the source of truth for the cloud
  verifier too, so a change there is a coordinated one — say so and we'll plan it.
- **Does this require capturing anything content-bearing?** Prompt text, file contents, and
  diffs are deliberately out of scope for capture (see the README). A proposal that needs
  them is not automatically a no, but it's a much bigger conversation than a normal feature.
