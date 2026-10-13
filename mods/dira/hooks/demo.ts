// Seed data for /dira-demo: a freelance project with zavet living specs,
// framework units from all three frameworks, decisions, an open PR's forecast
// and a session's numbers. Pure data: the demo draws every surface without
// touching the repo.

import type { Audit, Decision, Forecast, Snapshot, Spec } from '../types'

/** The demo file guarded by DEMO-0008, relative to the demo root (~/.cache/dira/demo). */
export const DEMO_FILE = 'dira-demo/src/socket.rs'

const spec = (slug: string, kind: string, title: string, state: string, done: number, total: number, paths: string[] = []): Spec => ({
  slug,
  kind,
  title,
  state,
  primary: kind === 'zavet' ? `.zavet/specs/${slug}.md` : `${kind}/${slug.split(':')[1]}/tasks.md`,
  files: [],
  paths,
  plannedPaths: [],
  tasks: Array.from({ length: total }, (_, i) => ({ id: `${Math.floor(i / 3) + 1}.${(i % 3) + 1}`, done: i < done })),
})

export const DEMO_SPECS: Spec[] = [
  spec('daemon-lifecycle', 'zavet', 'Daemon startup and ingress lifecycle', 'active', 0, 0, [DEMO_FILE]),
  spec('checkout-flow', 'zavet', 'Checkout and payment capture', 'active', 0, 0),
  spec('invoicing', 'zavet', 'Invoices, credit notes and VAT', 'active', 0, 0),
  spec('auth-sessions', 'zavet', 'Sign-in, sessions and device trust', 'active', 0, 0),
  spec('search', 'zavet', 'Catalogue search and ranking', 'active', 0, 0),
  spec('openspec:add-csv-import', 'openspec', 'Add CSV import', 'active', 3, 5),
  spec('speckit:002-photo-albums', 'speckit', 'Photo albums', 'in-progress', 2, 7),
  spec('bmad:epic-cart', 'bmad', 'Cart', 'in-progress', 1, 3),
]

export const DEMO_DECISIONS: Decision[] = [
  { id: 'DEMO-0008', title: 'The control socket is anchored to a fixed per-user path, never $TMPDIR', path: null, guards: ['dira-demo/src/**'], stats: { shown: 9, blocked: 3, complied: 14 } },
  { id: 'DEMO-0003', title: 'Replace running binaries by rename only, never by opening the target', path: null, guards: [], stats: { shown: 4, blocked: 1, complied: 6 } },
  { id: 'DEMO-0011', title: 'TLS trust anchors ship inside the binary, never the host trust store', path: null, guards: [], stats: { shown: 2, blocked: 0, complied: 2 } },
  { id: 'DEMO-0019', title: 'Prices are stored in minor units; rounding happens once, at invoice time', path: null, guards: [], stats: { shown: 6, blocked: 2, complied: 5 } },
]

/** Seeded into the overview cache so blurbs need no record files. */
export const DEMO_OVERVIEWS: Record<string, string> = {
  'daemon-lifecycle': 'How the daemon starts, binds its control socket and accepts hook traffic; a second daemon never starts while the first is alive.',
  'DEMO-0008': 'Keep socket_path() independent of TMPDIR. Bind first; a failed bind means another daemon owns the socket.',
}

export const DEMO_AUDIT: Audit = {
  staleSpecs: [
    { slug: 'checkout-flow', commits: 6, since: 'a0a481b' },
    { slug: 'auth-sessions', commits: 2, since: 'e776cab' },
  ],
  staleDecisions: [
    { id: 'DEMO-0019', commits: 4, since: 'a0a481b' },
    { id: 'DEMO-0003', commits: 1, since: 'e776cab' },
  ],
  at: 0,
}

export const DEMO_SNAP: Snapshot = {
  session: { human: 47 * 60, agent: 2 * 3600 + 12 * 60 },
  costUsd: 18.4,
  unbilled: { amount: 2340, currency: '€' },
  syncFailures: 0,
  isWedged: false,
}

export const DEMO_FORECAST: Forecast = {
  branch: 'feat/csv-import',
  pr: 214,
  elements: { ci: 'ok', review: 'ok', authorship: 'ok', supervision: 'ok', intent: 'missing' },
  grade: 750,
  notes: ['2 commits touch dira-demo/src/** without Refs: DEMO-0008'],
  at: 0,
}

export const DEMO_SOURCE = `//! Control socket for the daemon.

use std::path::PathBuf;

pub fn socket_path(runtime_dir: Option<PathBuf>, data_dir: PathBuf) -> PathBuf {
    runtime_dir.unwrap_or(data_dir).join("dira.sock")
}
`
