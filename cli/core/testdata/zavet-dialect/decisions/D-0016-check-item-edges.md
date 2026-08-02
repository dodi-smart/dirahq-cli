---
id: D-0016
title: Check items follow the list dialect
status: active
checks:
  - "keeps a hash :: runner -c 'a # b'"
  - has-no-command ::
  - keeps later separators :: runner --grep 'A::b'
---

## Decision

Quoting is the escape hatch for a command containing ` #`, which the shared
decomment rule would otherwise truncate. An item with a label and no command
verifies nothing and drops. Only the FIRST `::` splits, so a command keeps its
own separators.
