## [0.2.3](https://github.com/dodi-smart/dirahq-cli/compare/v0.2.2...v0.2.3) (2026-08-05)

### 🐛 Bug Fixes

* **daemon:** keep token capture alive across a corrupt transcript tail ([c0662ad](https://github.com/dodi-smart/dirahq-cli/commit/c0662ad8e1093cea5a55c359d042b34727f06504)), closes [#91](https://github.com/dodi-smart/dirahq-cli/issues/91)
* **daemon:** report the store path instead of silently using a throwaway one ([077c54e](https://github.com/dodi-smart/dirahq-cli/commit/077c54efba53efdffb66c0ef57e69b548a4c9ff8)), closes [#76](https://github.com/dodi-smart/dirahq-cli/issues/76) [#92](https://github.com/dodi-smart/dirahq-cli/issues/92)

### 📚 Documentation

* **repo:** strike D-0018's directive that D-0020 already corrected ([4927e5a](https://github.com/dodi-smart/dirahq-cli/commit/4927e5a371f7251a6b84db392c4fcccfc5383ca4))

### ✅ Tests

* **cli:** isolate the update e2e suite and restore its exec-staging lock ([3902da7](https://github.com/dodi-smart/dirahq-cli/commit/3902da7ca2aca7644eee84ad900141f0ddf9c299)), closes [#80](https://github.com/dodi-smart/dirahq-cli/issues/80) [#80](https://github.com/dodi-smart/dirahq-cli/issues/80)

## [0.2.2](https://github.com/dodi-smart/dirahq-cli/compare/v0.2.1...v0.2.2) (2026-08-04)

### 🐛 Bug Fixes

* **daemon:** advance sync watermarks per acked chunk and pace long drains ([44b7ac3](https://github.com/dodi-smart/dirahq-cli/commit/44b7ac3ef74248d0d1fd8cbb027d3ddf5e567c75)), closes [#88](https://github.com/dodi-smart/dirahq-cli/issues/88)

## [0.2.1](https://github.com/dodi-smart/dirahq-cli/compare/v0.2.0...v0.2.1) (2026-08-04)

### 🐛 Bug Fixes

* **cli:** confirm process exit before starting a replacement daemon ([d7443b7](https://github.com/dodi-smart/dirahq-cli/commit/d7443b765f96e16cda5a93748b6e96080432125e))
* **cli:** price models from a generated table, refreshed monthly ([7c700b5](https://github.com/dodi-smart/dirahq-cli/commit/7c700b5234e03b9762c7b5f6f83153bdad3a8d50))
* **daemon:** capture subagent transcripts and stop coalescing the span opener ([1d14064](https://github.com/dodi-smart/dirahq-cli/commit/1d1406404cd4a863760c929dfc6a755f41717460))
* **daemon:** give token usage its own sync cursor ([c3fd5f5](https://github.com/dodi-smart/dirahq-cli/commit/c3fd5f5c95491b3bb696d2ea7041760fde18c304))

## [0.2.0](https://github.com/dodi-smart/dirahq-cli/compare/v0.1.2...v0.2.0) (2026-08-02)

### ✨ Features

* **cli:** capture how a record is verified, and let one correct another ([891606f](https://github.com/dodi-smart/dirahq-cli/commit/891606fd787399f71fd472aea2dc9e571d7e2e63))
* **cli:** fix agent time accounting and the windows control channel ([8ae49e1](https://github.com/dodi-smart/dirahq-cli/commit/8ae49e156b6570a40e7d8d5d8fd5a95cec2fb8d7)), closes [#1](https://github.com/dodi-smart/dirahq-cli/issues/1)

### 🐛 Bug Fixes

* **daemon:** count every spec toward coverage, not only verified ones ([81cb1ed](https://github.com/dodi-smart/dirahq-cli/commit/81cb1ed6328ec1a19da95af3b3c79ed953b527b8))

### ✅ Tests

* **cli:** pin that why leads with a correction and lists checks ([1ec2946](https://github.com/dodi-smart/dirahq-cli/commit/1ec29462f170bd6f3645ec8691fd4763b489b9a5))
* **cli:** serialise executable staging against subprocess forks ([6d29a12](https://github.com/dodi-smart/dirahq-cli/commit/6d29a1280dd259f818791e2a6326168f61151744)), closes [#80](https://github.com/dodi-smart/dirahq-cli/issues/80)

## [0.1.2](https://github.com/dodi-smart/dirahq-cli/compare/v0.1.1...v0.1.2) (2026-07-31)

### 🐛 Bug Fixes

* **cli:** session hygiene, sync batching, ack telemetry and the knowledge window ([1320fcf](https://github.com/dodi-smart/dirahq-cli/commit/1320fcf5984a20e36a066ab78546f861fdaec683)), closes [#74](https://github.com/dodi-smart/dirahq-cli/issues/74) [#72](https://github.com/dodi-smart/dirahq-cli/issues/72) [#71](https://github.com/dodi-smart/dirahq-cli/issues/71) [#67](https://github.com/dodi-smart/dirahq-cli/issues/67) [#74](https://github.com/dodi-smart/dirahq-cli/issues/74) [#72](https://github.com/dodi-smart/dirahq-cli/issues/72) [#71](https://github.com/dodi-smart/dirahq-cli/issues/71) [#67](https://github.com/dodi-smart/dirahq-cli/issues/67) [#24](https://github.com/dodi-smart/dirahq-cli/issues/24) [#74](https://github.com/dodi-smart/dirahq-cli/issues/74) [#72](https://github.com/dodi-smart/dirahq-cli/issues/72) [#71](https://github.com/dodi-smart/dirahq-cli/issues/71) [#67](https://github.com/dodi-smart/dirahq-cli/issues/67) [#24](https://github.com/dodi-smart/dirahq-cli/issues/24)

## [0.1.1](https://github.com/dodi-smart/dirahq-cli/compare/v0.1.0...v0.1.1) (2026-07-30)

### 🐛 Bug Fixes

* **cli:** compare release versions by semver, never by string equality ([f7ea468](https://github.com/dodi-smart/dirahq-cli/commit/f7ea4686c2d5f1d0a8dd8b8469f85181796d0e16))
* **cli:** fall back to anonymous in dira update when the token is rejected ([5149d79](https://github.com/dodi-smart/dirahq-cli/commit/5149d7958a44af6a6e39410161c6386479b78996))
* **cli:** fall back to anonymous when GitHub rejects GH_TOKEN/GITHUB_TOKEN ([34a4a2a](https://github.com/dodi-smart/dirahq-cli/commit/34a4a2a588f528a3ed978570e5ed89e1a784d440))
* **cli:** link the MSVC CRT statically so windows needs no VC++ redist ([b63b039](https://github.com/dodi-smart/dirahq-cli/commit/b63b0398675f1bd334243f490ade477b87863fda))
* **cli:** normalize $LASTEXITCODE in install.ps1, and guard the smoke assertions ([cad2b69](https://github.com/dodi-smart/dirahq-cli/commit/cad2b6956dd1616032cab9eb72d6243b26637035)), closes [#58](https://github.com/dodi-smart/dirahq-cli/issues/58)
* **cli:** retry the post-swap version probe on ETXTBSY ([ba1800a](https://github.com/dodi-smart/dirahq-cli/commit/ba1800a6800b3a154bba4b6ef07a71409b1fe87a))
* **cli:** stop install.ps1 leaking $LASTEXITCODE from best-effort daemon calls ([47f0f00](https://github.com/dodi-smart/dirahq-cli/commit/47f0f00562a67e6da2e9da7decf916a70b4eeb9a))

## [0.1.0](https://github.com/dodi-smart/dirahq-cli/compare/v0.0.0...v0.1.0) (2026-07-28)

### ✨ Features

* **cli:** add a curl | sh installer for dira + dirad ([1385d4f](https://github.com/dodi-smart/dirahq-cli/commit/1385d4f18b27c88c1e85b26c377c0966428be02b))
* **cli:** add dira update, daemon supervision, and dira zavet install ([8aeb8fa](https://github.com/dodi-smart/dirahq-cli/commit/8aeb8fae27763207cf1880ac4d7d9b2023ff89d1))
* **cli:** add Gemini CLI and Cursor harness sources ([97a4c56](https://github.com/dodi-smart/dirahq-cli/commit/97a4c56920926638a2f1ef539437e6495513744a))
* **cli:** add grok-build harness source and dira init grok ([62666ef](https://github.com/dodi-smart/dirahq-cli/commit/62666efa64865b9369ec46f67b027de657f248c3))
* **cli:** add native windows support and wsl-aware install ([ff02e74](https://github.com/dodi-smart/dirahq-cli/commit/ff02e74f76460106e494f9c4832c030acf8f1c55))
* **cli:** bullet-proof the sync loop — typed 401/429 handling, health, self-healing writer ([0ac3f13](https://github.com/dodi-smart/dirahq-cli/commit/0ac3f13ea2d37a5843f05ff15a836253b0ac92ce))
* **cli:** capture grok token usage from updates.jsonl turn_completed records ([d84ced9](https://github.com/dodi-smart/dirahq-cli/commit/d84ced9c51468bef41b49524509b77b3e5cbe1c9)), closes [#42](https://github.com/dodi-smart/dirahq-cli/issues/42)
* **cli:** crash-safe two-phase device-key rotation ([7c84e01](https://github.com/dodi-smart/dirahq-cli/commit/7c84e0184ca8c253d319b6e4e3646b8ee250b180))
* **cli:** dira zavet command surface in the brand triad ([cbe9f3f](https://github.com/dodi-smart/dirahq-cli/commit/cbe9f3f25859ce691c7284a8032f04079b13a64b)), closes [#e87ca0](https://github.com/dodi-smart/dirahq-cli/issues/e87ca0)
* **cli:** manual-session note/label + activity, dira invoice ([#14](https://github.com/dodi-smart/dirahq-cli/issues/14)) ([9c43869](https://github.com/dodi-smart/dirahq-cli/commit/9c43869951faa9b7e1e0f580e47802f3015a243f))
* **cli:** point the default cloud_url at the hosted app.dirahq.sh ([ee0158b](https://github.com/dodi-smart/dirahq-cli/commit/ee0158b6e30938e54e1627e47278c2d9ea800596))
* **cli:** redesign dira status around the concept summary block ([c1c3565](https://github.com/dodi-smart/dirahq-cli/commit/c1c3565abe6a9dbeadc31f0cc2215d5a0482062e))
* **cli:** rich help text, styled --help, and completions install guidance ([21a6180](https://github.com/dodi-smart/dirahq-cli/commit/21a61803dbb6cba0f72d8d7f012ed47e821978d6))
* **cli:** SPECS in the wiki, spec answers and mixed hits in zavet why ([c134457](https://github.com/dodi-smart/dirahq-cli/commit/c134457bc082ed0b24959292f8e2884f9fc9ce59))
* **cli:** theme the TUI to the Dira brand palette ([7abb309](https://github.com/dodi-smart/dirahq-cli/commit/7abb3093d152b64b5175c9549c4c0608e185c6f3)), closes [#1fd6ae](https://github.com/dodi-smart/dirahq-cli/issues/1fd6ae) [#9079ff](https://github.com/dodi-smart/dirahq-cli/issues/9079ff) [#e5a53b](https://github.com/dodi-smart/dirahq-cli/issues/e5a53b)
* **contract:** add grok harness to the wire schema ([e795eae](https://github.com/dodi-smart/dirahq-cli/commit/e795eae5ded28a8d2e0330565626ba37905b538d))
* **contract:** add KnowledgeEnvelope — a consent-tiered knowledge channel beside attestations ([8dbdbdc](https://github.com/dodi-smart/dirahq-cli/commit/8dbdbdc667dbb73f9b46e3fed880ea39246b498f))
* **contract:** add signed billing-summary request envelope (schema 1.1.0) ([239c7ae](https://github.com/dodi-smart/dirahq-cli/commit/239c7aeaece3af500ee488d3e09b5b5866847ce0))
* **contract:** carry schemaVersion on the rotate-key envelope ([13e6a21](https://github.com/dodi-smart/dirahq-cli/commit/13e6a21733d38ce5697ffc1a55eec1160dade162))
* **daemon:** capture living specs (.zavet/specs) with staleness, decision links, and cost attribution ([9605e0c](https://github.com/dodi-smart/dirahq-cli/commit/9605e0c7301c322e9a1c864fea27a8675c759eac))
* **daemon:** fetch cloud billing summary and expose compute + billing on status ([3058654](https://github.com/dodi-smart/dirahq-cli/commit/30586549b83d8ca9555c054a83159af7fca7af83))
* **daemon:** idempotent chunked sync + epoch handshake + dira device resync ([74cd94b](https://github.com/dodi-smart/dirahq-cli/commit/74cd94bb89ee7534f28228edc1adb263c8491c13))
* **daemon:** sync zavet knowledge to the cloud behind an explicit [sync] knowledge knob ([be07341](https://github.com/dodi-smart/dirahq-cli/commit/be073418bc451eade9b5645324b01846defb1758))
* **daemon:** zavet knowledge module — capture, storage, attribution, query ([f4e1c7a](https://github.com/dodi-smart/dirahq-cli/commit/f4e1c7ab47b13011c0515e7410277e61527d2c84))

### 🐛 Bug Fixes

* **cli:** adapt device keychain to keyring 4's v1-compat API ([75fcaa5](https://github.com/dodi-smart/dirahq-cli/commit/75fcaa5e89ddbb62187f5ab68dcc59b20d4ee31e))
* **cli:** correct live-view accounting to match the cloud's deduped, idle-trimmed measures ([314cd95](https://github.com/dodi-smart/dirahq-cli/commit/314cd95318b98245124b9415d1f8584b9b2420a4))
* **cli:** lead with GitHub's anonymous rate limit in release-resolution errors ([2843562](https://github.com/dodi-smart/dirahq-cli/commit/2843562772dadda315db28bd05da55a6c484dc7d))
* **cli:** make the local daemon reliably reachable and single-instance safe ([8758eb3](https://github.com/dodi-smart/dirahq-cli/commit/8758eb391fc4f24aec80c5fbfcdb977999221236))
* **cli:** per-gap engaged intervals + at-band seed so the cloud stops under-billing ~80% (dirahq-cloud[#21](https://github.com/dodi-smart/dirahq-cli/issues/21)) ([6cf4f39](https://github.com/dodi-smart/dirahq-cli/commit/6cf4f39b9720df3bde430aefb4da05584efc47b6))
* **cli:** rebuild ended session rollups from full history and carry prompts/branch on partials ([277eaf6](https://github.com/dodi-smart/dirahq-cli/commit/277eaf6e26a3a7a2a1e321b274377e288335d64d)), closes [#40](https://github.com/dodi-smart/dirahq-cli/issues/40) [#40](https://github.com/dodi-smart/dirahq-cli/issues/40)
* **contract:** use an RFC 2606 reserved address in the signing vector ([75b04b6](https://github.com/dodi-smart/dirahq-cli/commit/75b04b6a3a7185ee35d929b2052489adfb95a289))
* **daemon:** attribute engaged_seconds by opening-signal so per-session sums to the deduped total ([54ceac7](https://github.com/dodi-smart/dirahq-cli/commit/54ceac773933bb602a0a1257f8a24ebc68acf3aa))
* **daemon:** handle SIGTERM the same as Ctrl-C for orderly shutdown ([25adb6f](https://github.com/dodi-smart/dirahq-cli/commit/25adb6fb7752ae49ffa45b99c6de49b82929fb35))

### ⚡ Performance Improvements

* **cli:** daemon efficiency — shared client, timer jitter, deep idle, quantized presence ([8f329fd](https://github.com/dodi-smart/dirahq-cli/commit/8f329fde1f3d9ceaace6f69fad8dd65701c4537d))

### 📚 Documentation

* **repo:** add security policy, code of conduct, and issue/PR templates ([55ab654](https://github.com/dodi-smart/dirahq-cli/commit/55ab6548a2d16b2b417a452e5954dead875da7ff))
* **repo:** document installation and releasing, and record the decisions ([e7b24fd](https://github.com/dodi-smart/dirahq-cli/commit/e7b24fde46c3752ddc104f00bf0620d5e2f82849))
* **repo:** spec-layer contract in the zavet guide, and this repo's own capture-pipeline spec ([99c2192](https://github.com/dodi-smart/dirahq-cli/commit/99c21925ed32e669a0cf1585daa1912196890139))
* **repo:** zavet guide, readme cross-link, and the repo's own knowledge layer ([47805dd](https://github.com/dodi-smart/dirahq-cli/commit/47805dd1fdff3bef7b3899184a32fceb6a248ad0))

### 💄 Styles

* **cli:** use is_multiple_of for the three modulo checks clippy now flags ([5af33d9](https://github.com/dodi-smart/dirahq-cli/commit/5af33d94ea18a17ba85b00025cdb9a6f3c936364))
* **repo:** apply rustfmt --all under current stable ([f7cc24b](https://github.com/dodi-smart/dirahq-cli/commit/f7cc24bb1ef3cc4d9522603e046478fdc2f1c775))

### ♻️ Code Refactoring

* **cli:** bulk-join child rows in zavet knowledge sync windows ([a26b1b3](https://github.com/dodi-smart/dirahq-cli/commit/a26b1b38e3a08dbebabd2d88b9bc02ac32bfb59a))
* **cli:** share the billable footer, cloud-link gate, and timestamp helpers ([0c7cfdd](https://github.com/dodi-smart/dirahq-cli/commit/0c7cfdd70ad94887ce4d4796d0212de8d2b1a7de))
* **cli:** shared zavet render panels — body, commits, unified spec badges ([54cfef1](https://github.com/dodi-smart/dirahq-cli/commit/54cfef119cb1dab2766248a4e1bb342a8269b58b))
* **daemon:** shared sync-channel plumbing and typed content-downgrade ([daedc14](https://github.com/dodi-smart/dirahq-cli/commit/daedc140fddaab9567a2a2a5d60a45591d0a942c))
* **daemon:** simplify spec-layer internals after review ([4a97b87](https://github.com/dodi-smart/dirahq-cli/commit/4a97b871d4c06e0f1c4f1cb4955575a8f878669b))

### ✅ Tests

* **cli:** mock-cloud harnesses and daemon e2e coverage for the hardening branch ([6772cd1](https://github.com/dodi-smart/dirahq-cli/commit/6772cd1a9f6328c705c6b25780b1c6d2cbe7ae55)), closes [#21](https://github.com/dodi-smart/dirahq-cli/issues/21)
* **cli:** move test fixtures off real registered domains ([137e56c](https://github.com/dodi-smart/dirahq-cli/commit/137e56c85b26f7bb707b9722e46d422688b727b0))
* **cli:** sync the vendored zavet dialect README with dirahq-zavet ([a9b0d86](https://github.com/dodi-smart/dirahq-cli/commit/a9b0d865701246cb33a26409c0552e4f1a01886d))
* **daemon:** vendor canonical zavet dialect fixtures + golden walker ([a103081](https://github.com/dodi-smart/dirahq-cli/commit/a1030815722bbc6ed0062816f8810f0d1c2118e8))

### 👷 Continuous Integration

* **repo:** set up rust toolchain directly instead of via mise ([6696c25](https://github.com/dodi-smart/dirahq-cli/commit/6696c2565d6c7a9f7ab578fe4d5de44290ba4345))
