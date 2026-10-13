import { expect, test } from 'claude-code/testing'

import {
  extractPaths,
  inferFocus,
  mentioned,
  commitTrailers,
  forecastText,
  isGitCommit,
  parseDecisions,
  parseSpecs,
  pickZavet,
  statusLine,
  toSnapshot,
} from './lib'

const SPEC = JSON.stringify({
  slug: 'openspec:add-csv-import', kind: 'openspec', title: 'Add CSV import', state: 'active',
  primary: 'openspec/changes/add-csv-import/proposal.md',
  files: [{ path: 'openspec/changes/add-csv-import/tasks.md' }],
  paths: [], plannedPaths: ['src/import/reader.ts'],
  requirements: [{ id: 'R1' }],
  tasks: [{ id: '1.1', done: true, status: null }, { id: '1.2', done: false, status: null }],
})

test('parses specs and drops unresolved units', async () => {
  const specs = parseSpecs(`${SPEC}\n{"slug":"x:y","unresolved":"no file"}\n`)
  expect(specs.length).toBe(1)
  expect(specs[0]!.tasks.length).toBe(2)
})

test('infers focus from the unit files and planned paths', async () => {
  const specs = parseSpecs(SPEC)
  expect(inferFocus(['src/import/reader.ts'], specs)).toBe('openspec:add-csv-import')
  expect(inferFocus(['README.md'], specs)).toBe(null)
})

test('status line drops absent segments', async () => {
  expect(statusLine(null, 0)).toBe(undefined)
  const snap = toSnapshot(
    { active: [{ session_id: 's', human_seconds: 600, agent_seconds: 4000 }],
      billing: { unbilled_amount: 380, currency: '€' }, sync_health: { consecutive_failures: 0 } },
    's',
    1.5,
  )
  expect(statusLine(snap, 2, 1)).toBe(
    '● 10m ◆ 1h07 · $1.50 · ▲ €380 · ◇2 ▪1',
  )
})

test('commit trailers are read from -m text and --trailer flags', async () => {
  const cmd = `git add -A && git commit -s --trailer 'Spec: attestation-sync' -m "feat(cli): x\n\nRefs: D-0001, D-0008"`
  expect(isGitCommit(cmd)).toBe(true)
  expect(isGitCommit('git log')).toBe(false)
  expect(commitTrailers(cmd).map(t => `${t.key}: ${t.value}`).sort()).toEqual(['Refs: D-0001', 'Refs: D-0008', 'Spec: attestation-sync'])
})

test('forecast text names the next grade and what stands before it', async () => {
  const el = { ci: 'ok', review: 'ok', authorship: 'ok', supervision: 'ok', intent: 'missing' } as const
  expect(forecastText(750, el)).toEqual({ label: '⬡ 750', next: '→ 999 needs intent' })
  expect(forecastText(999, { ...el, intent: 'ok' })).toEqual({ label: '⬡ 999', next: null })
})

test('decisions and zavet binary discovery', async () => {
  const d = parseDecisions('D-0001\tactive\tWire\nD-0002\tsuperseded\tOld\n', 'D-0001\tcontract/**\n', ['D-0001-wire.md'])
  expect(d).toEqual([{ id: 'D-0001', title: 'Wire', path: '.zavet/decisions/D-0001-wire.md', guards: ['contract/**'] }])
  expect(pickZavet('/h/.claude/plugins/cache/dirahq/zavet/1.10.0/bin/zavet\n/h/.claude/plugins/cache/dirahq/zavet/1.8.0/bin/zavet\n')).toBe(
    '/h/.claude/plugins/cache/dirahq/zavet/1.10.0/bin/zavet',
  )
})

test('extracts repo paths from plan prose', async () => {
  expect(extractPaths('Edit `cli/core/src/config.rs` and [x](cli/dira/src/main.rs), see https://a.b/c.md')).toEqual([
    'cli/core/src/config.rs',
    'cli/dira/src/main.rs',
  ])
})

test('finds mentioned ids as whole words', async () => {
  expect(mentioned('kept D-0009 intact, not D-00090; see daemon-lifecycle.', ['D-0009', 'D-0008', 'daemon-lifecycle'])).toEqual([
    'D-0009',
    'daemon-lifecycle',
  ])
})

import { analyzeDraft, globMatch, gradeFor, nextRung, openQuestions, vouchReasons } from './lib'

test('globs: ** spans dirs, * stays in a segment', async () => {
  expect(globMatch('cli/core/src/sync/**', 'cli/core/src/sync/batch.rs')).toBe(true)
  expect(globMatch('contract/**', 'contract/src/lib.rs')).toBe(true)
  expect(globMatch('cli/*.rs', 'cli/core/x.rs')).toBe(false)
  expect(globMatch('install.sh', 'install.sh')).toBe(true)
})

test('composer marks guarded paths, ids and slugs', async () => {
  const d = [{ id: 'D-0008', title: 't', path: null, guards: ['cli/core/src/config.rs'] }]
  const r = analyzeDraft('refactor cli/core/src/config.rs per D-0008 and daemon-lifecycle', d, ['daemon-lifecycle'])
  expect(r.ids).toEqual(['D-0008'])
  expect(r.slugs).toEqual(['daemon-lifecycle'])
  expect(r.marks.map(m => m.kind).sort()).toEqual(['decision', 'guarded', 'spec'])
})

test('proba ladder is additive', async () => {
  const all = { ci: 'ok', review: 'ok', authorship: 'ok', supervision: 'ok', intent: 'ok' } as const
  expect(gradeFor(all)).toBe(999)
  expect(gradeFor({ ...all, intent: 'missing' })).toBe(750)
  expect(gradeFor({ ...all, supervision: 'missing' })).toBe(585)
  expect(gradeFor({ ...all, review: 'pending' })).toBe(null)
  expect(nextRung({ ...all, intent: 'missing' })).toEqual({ target: 999, missing: ['intent'] })
})

test('vouch reasons from frontmatter and open questions', async () => {
  const md = '---\nid: D-1\nverified: false\n---\n## Decision\nx\n## Open questions\n- a?\n- b?\n## Why\ny\n'
  expect(openQuestions(md)).toBe(2)
  expect(vouchReasons(md, 'decision')).toEqual(['unverified', '2 open questions'])
  expect(vouchReasons('---\nverified: true\n---\n## Open questions\nNone.\n', 'decision')).toEqual([])
})
