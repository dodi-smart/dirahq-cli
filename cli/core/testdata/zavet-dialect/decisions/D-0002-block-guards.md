---
id: D-0002
title: Payments idempotency keys are session-scoped
guards:
  - 'src/payments/**'
  - src/billing/   # the billing surface too
status: active
---
Idempotency keys derive from the session, not the request.
