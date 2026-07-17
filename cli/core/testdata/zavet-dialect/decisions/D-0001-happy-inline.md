---
id: D-0001
title: Auth session tokens are opaque
status: active
guards: [src/auth/**, "docs/adr/"]
---
Session tokens are random opaque ids, never JWTs.
