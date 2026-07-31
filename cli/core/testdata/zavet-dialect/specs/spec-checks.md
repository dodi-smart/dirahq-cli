---
title: Spec checks are scenarios
origin: designed
confidence: high
date: 2026-08-01
paths:
  - src/ui/**
checks:
  - narrow viewport has no overflow :: run-the-sweep
  - run-the-scenario-suite
---

## Overview

A spec's checks are feature-level scenarios, parsed by the exact same rule as a
decision's invariant checks — one key, one grammar, two subjects.
