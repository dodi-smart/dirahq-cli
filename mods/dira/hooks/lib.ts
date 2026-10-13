// Pure helpers: parsing dira/zavet output, matching, formatting and SVG.
// No `$` in here, so every function is testable without the engine.

import type { Audit, Decision, ElementKey, Elements, ElementState, GuardStats, Snapshot, Spec, Task } from '../types'

type Json = Record<string, any>

/** dira's brand palette (cli/dira/src/theme.rs), plus proba's blue from the landing site. */
export const C = {
  engaged: '#1fd6ae', // human time ●
  agent: '#9079ff', // agent time ◆, accent
  knowledge: '#e87ca0', // zavet ◇ ▪
  attributed: '#7fa6e0',
  ink: '#eceaf2',
  muted: '#8b8799',
  faint: '#5c586b',
  negative: '#c76b6b',
} as const

export const PROBA = '#5ba3f5'

// ---------------------------------------------------------------- parsing

export function parseJson(text: string): Json | null {
  try {
    const v = JSON.parse(text)
    return v && typeof v === 'object' ? v : null
  } catch {
    return null
  }
}

/** The JSON object inside a model's reply, prose or fences around it ignored. */
export function parseJsonIn(text: string): Json | null {
  return parseJson(text.slice(text.indexOf('{'), text.lastIndexOf('}') + 1))
}

/** `dira status --json` → the slice the mod draws, for this session; the cost is Claude Code's own. */
export function toSnapshot(status: Json, sessionId: string, costUsd: number | null): Snapshot {
  const mine = (status.active ?? []).find((s: Json) => s.session_id === sessionId)
  const billing = status.billing
  return {
    session: mine ? { human: mine.human_seconds ?? 0, agent: mine.agent_seconds ?? 0 } : null,
    costUsd,
    unbilled: billing && typeof billing.unbilled_amount === 'number' ? { amount: billing.unbilled_amount, currency: billing.currency ?? '' } : null,
    syncFailures: status.sync_health?.consecutive_failures ?? 0,
    isWedged: status.writer_health?.wedged === true,
  }
}

/** `zavet specs --json` (JSONL) → specs; unresolved units are dropped. */
export function parseSpecs(jsonl: string): Spec[] {
  const out: Spec[] = []
  for (const line of jsonl.split('\n')) {
    const r = parseJson(line)
    if (!r || typeof r.slug !== 'string' || r.unresolved) continue
    out.push({
      slug: r.slug,
      kind: r.kind ?? 'zavet',
      title: r.title ?? r.slug,
      state: r.state ?? 'unknown',
      primary: r.primary ?? null,
      files: (r.files ?? []).map((f: Json) => f.path).filter(Boolean),
      paths: r.paths ?? [],
      plannedPaths: r.plannedPaths ?? [],
      tasks: (r.tasks ?? []).map((t: Json): Task => ({ id: String(t.id), done: t.done === true })),
    })
  }
  return out
}

/** `zavet list` (id, status, title) + `zavet guards` (id, glob) → active decisions. */
export function parseDecisions(list: string, guards: string, files: string[]): Decision[] {
  const globs = new Map<string, string[]>()
  for (const line of guards.split('\n')) {
    const [id, glob] = line.split('\t')
    if (!id || !glob) continue
    globs.set(id, [...(globs.get(id) ?? []), glob])
  }
  const out: Decision[] = []
  for (const line of list.split('\n')) {
    const [id, status, title] = line.split('\t')
    if (!id || status !== 'active') continue
    const path = files.find(f => f.startsWith(id + '-')) ?? null
    out.push({ id, title: title ?? id, path: path ? `.zavet/decisions/${path}` : null, guards: globs.get(id) ?? [] })
  }
  return out
}

/** Guard-hit counts from `dira zavet decisions --json`, merged into the list. */
export function mergeDecisionStats(decisions: Decision[], json: Json | null): Decision[] {
  const rows: Json[] = json?.decisions ?? []
  return decisions.map(d => {
    const r = rows.find(x => x.id === d.id)
    if (!r) return d
    const total = (kind: string) =>
      (r.guard_stats ?? []).filter((g: Json) => String(g.kind).startsWith(kind)).reduce((a: number, g: Json) => a + (g.total ?? 0), 0)
    const stats: GuardStats = { shown: total('guard_shown'), blocked: total('guard_blocked'), complied: total('guard_complied') }
    return { ...d, stats }
  })
}

/** Picks the highest `.../zavet/<semver>/bin/zavet` out of an `ls` listing. */
export function pickZavet(listing: string): string | null {
  const rows = listing
    .split('\n')
    .map(p => p.trim())
    .filter(p => /\/zavet\/\d+\.\d+\.\d+\/bin\/zavet$/.test(p))
  if (rows.length === 0) return null
  const ver = (p: string) => p.match(/\/zavet\/(\d+)\.(\d+)\.(\d+)\//)!.slice(1).map(Number)
  rows.sort((a, b) => {
    const [x, y] = [ver(a), ver(b)]
    return x[0]! - y[0]! || x[1]! - y[1]! || x[2]! - y[2]!
  })
  return rows[rows.length - 1] ?? null
}

/** `zavet audit` TSV → the stale specs and decisions the board draws. */
export function parseAudit(tsv: string, at: number): Audit {
  const staleSpecs: Audit['staleSpecs'] = []
  const staleDecisions: Audit['staleDecisions'] = []
  for (const line of tsv.split('\n')) {
    const [kind, id, n, since] = line.split('\t')
    if (!id || !n) continue
    if (kind === 'stale-spec') staleSpecs.push({ slug: id, commits: Number(n) || 0, since: since ?? '' })
    if (kind === 'stale-decision') staleDecisions.push({ id, commits: Number(n) || 0, since: since ?? '' })
  }
  staleSpecs.sort((a, b) => b.commits - a.commits)
  staleDecisions.sort((a, b) => b.commits - a.commits)
  return { staleSpecs, staleDecisions, at }
}

// ---------------------------------------------------------------- records

/** The "Agent directives" (or "Decision") section of a decision record. */
export function directives(markdown: string): string {
  const pick = (h: string) => {
    const m = markdown.match(new RegExp(`^##\\s+${h}\\s*\\n([\\s\\S]*?)(?=^##\\s|$(?![\\s\\S]))`, 'm'))
    return m?.[1]?.trim() ?? ''
  }
  return (pick('Agent directives') || pick('Decision')).slice(0, 1200)
}

/** First paragraph of a spec's Overview (or its first prose paragraph). */
export function overview(markdown: string): string {
  const body = markdown.replace(/^---\n[\s\S]*?\n---\n/, '')
  const sec = body.match(/^##\s+(Overview|Summary|Goal|Why)\s*\n+([\s\S]*?)(?:\n\s*\n|$)/im)
  const para = sec?.[2] ?? body.split(/\n\s*\n/).find(p => p.trim() && !p.trim().startsWith('#')) ?? ''
  return clip(para.replace(/\s+/g, ' ').replace(/`/g, '').trim(), 220)
}

export function frontmatter(md: string, key: string): string | null {
  const fm = md.match(/^---\n([\s\S]*?)\n---/)
  const m = fm?.[1]?.match(new RegExp(`^${key}:\\s*(.*)$`, 'm'))
  return m ? m[1]!.trim().replace(/^["']|["']$/g, '') : null
}

/** Unresolved bullets/paragraphs under `## Open questions` (none → 0). */
export function openQuestions(md: string): number {
  const m = md.match(/^##\s+Open questions\s*\n([\s\S]*?)(?=^##\s|$(?![\s\S]))/im)
  if (!m) return 0
  const body = m[1]!.trim()
  if (!body || /^(none|n\/a|—|-)\.?$/i.test(body)) return 0
  const bullets = body.split('\n').filter(l => /^\s*[-*]\s+\S/.test(l)).length
  return bullets || 1
}

/** Why a record needs vouching; empty when it is done. */
export function vouchReasons(md: string, kind: 'decision' | 'spec'): string[] {
  const out: string[] = []
  if (frontmatter(md, 'verified') === 'false') out.push('unverified')
  const q = openQuestions(md)
  if (q) out.push(`${q} open question${q > 1 ? 's' : ''}`)
  if (kind === 'spec' && frontmatter(md, 'confidence') === 'low') out.push('low confidence')
  return out
}

// ---------------------------------------------------------------- specs

function isActive(s: Spec): boolean {
  return !['done', 'archived', 'closed', 'superseded'].includes(s.state)
}

/** A zavet living spec (documentation kept current), as opposed to a work item. */
export function isLivingSpec(s: Spec): boolean {
  return s.kind === 'zavet'
}

/** A framework work unit (OpenSpec change, Spec Kit feature, BMAD epic…). */
export function isWorkItem(s: Spec): boolean {
  return !isLivingSpec(s) && isActive(s)
}

type Affected = { spec: Spec; files: string[]; isSpecEdited: boolean }

/** Living specs whose covered paths this session touched, most-touched first. */
export function affectedSpecs(touched: string[], specs: Spec[]): Affected[] {
  const out: Affected[] = []
  for (const spec of specs.filter(isLivingSpec)) {
    const files = touched.filter(p => spec.paths.includes(p))
    if (files.length === 0) continue
    out.push({ spec, files, isSpecEdited: touched.some(p => spec.files.includes(p)) })
  }
  return out.sort((a, b) => b.files.length - a.files.length)
}

/** Which framework unit the touched paths point at; null when none scores. */
export function inferFocus(touched: string[], specs: Spec[]): string | null {
  let best: { slug: string; score: number } | null = null
  for (const s of specs.filter(isWorkItem)) {
    const dir = s.primary ? s.primary.slice(0, s.primary.lastIndexOf('/') + 1) : null
    let score = 0
    for (const p of touched) {
      if (s.files.includes(p)) score += 3 // editing the unit's own task/plan file
      else if (dir && p.startsWith(dir)) score += 2
      if (s.plannedPaths.includes(p) || s.paths.includes(p)) score += 2
    }
    if (score > 0 && (!best || score > best.score)) best = { slug: s.slug, score }
  }
  return best?.slug ?? null
}

export function progress(tasks: Task[]): { done: number; total: number } {
  return { done: tasks.filter(t => t.done).length, total: tasks.length }
}

// ---------------------------------------------------------------- text

export function clip(s: string, n: number): string {
  return s.length > n ? s.slice(0, Math.max(0, n - 1)) + '…' : s
}

export function plural(n: number, word: string): string {
  return `${n} ${word}${n === 1 ? '' : 's'}`
}

export function bar(done: number, total: number, width = 5): string {
  if (total === 0) return ''
  const n = Math.round((done / total) * width)
  return '█'.repeat(n) + '░'.repeat(width - n)
}

export function dur(seconds: number): string {
  const m = Math.round(seconds / 60)
  if (m < 60) return `${m}m`
  return `${Math.floor(m / 60)}h${String(m % 60).padStart(2, '0')}`
}

/** `€1.8k`, `$52`, `$4.10`: money squeezed for the status line. */
export function moneyShort(amount: number, currency: string): string {
  const n = amount >= 1000 ? `${(amount / 1000).toFixed(amount >= 10000 ? 0 : 1)}k` : amount >= 20 ? String(Math.round(amount)) : amount.toFixed(2)
  return currency.length <= 1 ? `${currency}${n}` : `${n} ${currency}`
}

export function toRelative(path: string, root: string): string {
  const r = root.endsWith('/') ? root : root + '/'
  return path.startsWith(r) ? path.slice(r.length) : path
}

/** Repo-looking file paths named in free text (plans, subagent briefs). */
export function extractPaths(text: string): string[] {
  const found = text.match(/(?:^|[\s`'"(\[])((?:[\w.@-]+\/)+[\w.@-]+\.[A-Za-z0-9]{1,6})(?=[\s`'"),:\]]|$)/gm) ?? []
  const clean = found.map(f => f.replace(/^[\s`'"(\[]+/, '').replace(/^\.\//, ''))
  return [...new Set(clean.filter(p => !/^https?:/.test(p) && !p.startsWith('/')))]
}

// ---------------------------------------------------------------- matching

const escRe = (s: string) => s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')

// Compiled once per key set: the composer runs this on every keystroke and the
// reply footer on every render.
const wordRes = new Map<string, RegExp>()

/** Every whole-word occurrence of any key: `[start, end, key]`. */
function wordHits(text: string, keys: string[]): [number, number, string][] {
  if (keys.length === 0) return []
  const sig = keys.join('\u0000')
  let re = wordRes.get(sig)
  if (!re) {
    re = new RegExp(`(^|[^\\w:-])(${[...keys].sort((a, b) => b.length - a.length).map(escRe).join('|')})(?![\\w-])`, 'g')
    if (wordRes.size > 16) wordRes.clear()
    wordRes.set(sig, re)
  }
  const out: [number, number, string][] = []
  for (const m of text.matchAll(re)) {
    const start = m.index! + m[1]!.length
    out.push([start, start + m[2]!.length, m[2]!])
  }
  return out
}

/** Which of these ids or slugs the text names, as whole words, in key order. */
export function mentioned(text: string, keys: string[]): string[] {
  const seen = new Set(wordHits(text, keys).map(h => h[2]))
  return keys.filter(k => seen.has(k))
}

const globRes = new Map<string, RegExp>()

/** Minimal glob: `**` any depth, `*` within a segment, `?` one char. */
export function globMatch(glob: string, path: string): boolean {
  let re = globRes.get(glob)
  if (!re) {
    let src = ''
    for (let i = 0; i < glob.length; i++) {
      const c = glob[i]!
      if (c === '*' && glob[i + 1] === '*') {
        src += glob[i + 2] === '/' ? '(?:.*/)?' : '.*'
        i += glob[i + 2] === '/' ? 2 : 1
      } else if (c === '*') src += '[^/]*'
      else if (c === '?') src += '[^/]'
      else src += escRe(c)
    }
    re = new RegExp(`^${src}$`)
    globRes.set(glob, re)
  }
  return re.test(path)
}

/** Decision ids whose guard globs match a path. */
export function guardIds(path: string, decisions: Decision[]): string[] {
  return decisions.filter(d => d.guards.some(g => globMatch(g, path))).map(d => d.id)
}

export type DraftMark = { start: number; end: number; kind: 'guarded' | 'decision' | 'spec' }

/** What the composer lights up in a draft: guarded paths, decision ids, spec slugs. */
export function analyzeDraft(text: string, decisions: Decision[], specSlugs: string[]): { marks: DraftMark[]; ids: string[]; slugs: string[] } {
  const marks: DraftMark[] = []
  const ids = new Set<string>()
  for (const m of text.matchAll(/(?:[\w.@-]+\/)+[\w.@-]+(?:\.[A-Za-z0-9]{1,6})?/g)) {
    const hit = guardIds(m[0], decisions)
    if (hit.length === 0) continue
    hit.forEach(id => ids.add(id))
    marks.push({ start: m.index!, end: m.index! + m[0].length, kind: 'guarded' })
  }
  for (const [start, end, id] of wordHits(text, decisions.map(d => d.id))) {
    ids.add(id)
    marks.push({ start, end, kind: 'decision' })
  }
  const slugs = new Set<string>()
  for (const [start, end, slug] of wordHits(text, specSlugs)) {
    slugs.add(slug)
    marks.push({ start, end, kind: 'spec' })
  }
  return { marks, ids: [...ids], slugs: [...slugs] }
}

// ---------------------------------------------------------------- git

const COMMIT = /\bgit((?:\s+-[cC]\s+\S+)*)\s+commit\b/

export function isGitCommit(command: string): boolean {
  return COMMIT.test(command) && !/--amend\b.*--no-edit|\bcommit\s+--help/.test(command)
}

/** `Refs:` / `Spec:` values a commit command carries (as -m text or --trailer). */
export function commitTrailers(command: string): { key: 'Refs' | 'Spec'; value: string }[] {
  const out: { key: 'Refs' | 'Spec'; value: string }[] = []
  for (const m of command.matchAll(/\b(Refs|Spec):\s*([A-Za-z0-9:_.,\- ]+)/g)) {
    for (const v of m[2]!.split(/[ ,]+/).filter(Boolean)) out.push({ key: m[1] as 'Refs' | 'Spec', value: v })
  }
  return out.filter((t, i) => out.findIndex(x => x.key === t.key && x.value === t.value) === i)
}

// ---------------------------------------------------------------- status line

type Seg = [text: string, color: string]

/**
 * The status line as coloured segments, in dira's glyphs: ● you · ◆ agent ·
 * $ session cost · ▲ unbilled · ◇ decisions in play · ▪ specs to update.
 * Each segment drops out when its source is missing.
 */
function statusSegments(snap: Snapshot | null, guarded: number, staleSpecs: number): Seg[][] {
  const groups: Seg[][] = []
  if (snap?.session)
    groups.push([
      [`● ${dur(snap.session.human)}`, C.engaged],
      [`◆ ${dur(snap.session.agent)}`, C.agent],
    ])
  if (snap?.costUsd != null) groups.push([[moneyShort(snap.costUsd, '$'), '#e5a53b']])
  if (snap?.unbilled && snap.unbilled.amount > 0) groups.push([[`▲ ${moneyShort(snap.unbilled.amount, snap.unbilled.currency)}`, C.attributed]])
  const know: Seg[] = []
  if (guarded > 0) know.push([`◇${guarded}`, C.knowledge])
  if (staleSpecs > 0) know.push([`▪${staleSpecs}`, C.knowledge])
  if (know.length) groups.push(know)
  if (snap && (snap.syncFailures >= 3 || snap.isWedged)) groups.push([[snap.isWedged ? '⚠ writer' : `⚠ sync×${snap.syncFailures}`, C.negative]])
  return groups
}

export function statusLine(snap: Snapshot | null, guarded: number, staleSpecs = 0): string | undefined {
  const groups = statusSegments(snap, guarded, staleSpecs)
  return groups.length ? groups.map(g => g.map(([t]) => t).join(' ')).join(' · ') : undefined
}

const fg = (hex: string) => {
  const n = parseInt(hex.slice(1), 16)
  return `\x1b[38;2;${(n >> 16) & 255};${(n >> 8) & 255};${n & 255}m`
}

/** The same line in 24-bit ANSI for bin/statusline.sh; with nothing to say, the bare mark. */
export function statusAnsi(snap: Snapshot | null, guarded: number, staleSpecs = 0): string {
  const reset = '\x1b[0m'
  const mark = `${fg(C.agent)}◆${reset} ${fg(C.muted)}dira${reset}`
  const groups = statusSegments(snap, guarded, staleSpecs).map(g => g.map(([t, c]) => `${fg(c)}${t}${reset}`).join(' '))
  return groups.length ? `${mark}  ${groups.join(`${fg(C.faint)} · ${reset}`)}` : mark
}

// ---------------------------------------------------------------- proba

export const ELEMENT_KEYS: ElementKey[] = ['ci', 'review', 'authorship', 'supervision', 'intent']

const ELEMENT_LABELS: Record<ElementKey, string> = {
  ci: 'CI checks',
  review: 'review by someone other than the author',
  authorship: 'commits made in captured sessions',
  supervision: 'supervised minutes on the branch',
  intent: 'decision trailers on guarded paths',
}

/** proba's additive ladder (dirahq-cloud cloud/src/lib/proba/checks/fold.ts). */
export function gradeFor(el: Elements): 585 | 750 | 999 | null {
  if (el.ci !== 'ok' || el.review !== 'ok') return null
  if (el.authorship !== 'ok' || el.supervision !== 'ok') return 585
  if (el.intent !== 'ok') return 750
  return 999
}

/** The first rung not yet reached, and which elements stand in its way. */
export function nextRung(el: Elements): { target: 585 | 750 | 999; missing: ElementKey[] } | null {
  const ladder: [585 | 750 | 999, ElementKey[]][] = [
    [585, ['ci', 'review']],
    [750, ['authorship', 'supervision']],
    [999, ['intent']],
  ]
  for (const [target, keys] of ladder) {
    const missing = keys.filter(k => el[k] !== 'ok')
    if (missing.length) return { target, missing }
  }
  return null
}

/** `⬡ 750` and what stands before the next grade, as every surface says it. */
export function forecastText(grade: number | null, el: Elements): { label: string; next: string | null } {
  const rung = nextRung(el)
  return { label: `⬡ ${grade ?? 'unassayed'}`, next: rung ? `→ ${rung.target} needs ${rung.missing.join(', ')}` : null }
}

/** Plain-language line for one hallmark element, for tooltips. */
export function elementLine(key: ElementKey, state: ElementState): string {
  const mark = state === 'ok' ? '✓' : state === 'missing' ? '✕' : '…'
  return `${mark} ${ELEMENT_LABELS[key]}: ${state}`
}

// ---------------------------------------------------------------- svg

const esc = (s: string) => s.replace(/[&<>"']/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c]!)

/** An SVG document with the size it was drawn at, ready to spread onto `<Svg>`. */
export type SvgDoc = { source: string; width: number; height: number }

function svg(width: number, height: number, body: string): SvgDoc {
  return {
    source: `<svg xmlns="http://www.w3.org/2000/svg" width="${width}" height="${height}" viewBox="0 0 ${width} ${height}">${body}</svg>`,
    width,
    height,
  }
}

const CSS =
  `<style>text{font-family:ui-sans-serif,system-ui,-apple-system,sans-serif}` +
  `.k{font-size:9.5px;font-weight:600;fill:#8b8799;letter-spacing:.08em}` +
  `.t{font-size:12.5px;font-weight:600;fill:#24212e}.b{font-size:11.5px;fill:#5f5b6e}` +
  `.card{fill:#f7f6fb;stroke:#e6e3ef}.track{fill:#ebe8f3}` +
  `@media (prefers-color-scheme:dark){.t{fill:#eceaf2}.b{fill:#a29eb2}.card{fill:#1c1a24;stroke:#2c2938}.track{fill:#2c2938}}</style>`

/** The knowledge board: work items, living specs by health, decisions whose code moved. */
export function knowledgeSvg(
  width: number,
  current: { title: string; note: string }[],
  drifting: { title: string; note: string }[],
  decisions: { id: string; title: string; note: string; isLive: boolean }[],
  units: { title: string; note: string; frac: number }[],
): SvgDoc {
  const gap = 8
  const colW = (width - gap) / 2
  const cardH = 44
  let y = 0
  let body = ''
  const col = (x: number, glyph: string, label: string, color: string, list: { title: string; note: string }[], top: number) => {
    body += `<text x="${x}" y="${top + 12}" class="k"><tspan fill="${color}">${glyph}</tspan> ${esc(label)}</text>`
    list.forEach((c, i) => {
      const cy = top + 20 + i * (cardH + 6)
      body +=
        `<rect x="${x + 0.5}" y="${cy + 0.5}" width="${colW - 1}" height="${cardH}" rx="8" class="card"/>` +
        `<rect x="${x + 0.5}" y="${cy + 9}" width="3" height="${cardH - 17}" rx="1.5" fill="${color}"/>` +
        `<text x="${x + 12}" y="${cy + 19}" class="t">${esc(clip(c.title, Math.floor(colW / 7)))}</text>` +
        `<text x="${x + 12}" y="${cy + 35}" class="b">${esc(c.note)}</text>`
    })
    return top + 20 + list.length * (cardH + 6)
  }
  if (units.length) {
    body += `<text x="0" y="12" class="k"><tspan fill="${C.agent}">→</tspan> WORK ITEMS</text>`
    units.forEach((u, i) => {
      const uy = 20 + i * 30
      body +=
        `<text x="0" y="${uy + 12}" class="t">${esc(clip(u.title, 40))}</text>` +
        `<text x="${width}" y="${uy + 12}" class="b" text-anchor="end">${esc(u.note)}</text>` +
        `<rect x="0" y="${uy + 18}" width="${width}" height="4" rx="2" class="track"/>` +
        `<rect x="0" y="${uy + 18}" width="${Math.max(2, width * u.frac)}" height="4" rx="2" fill="${C.agent}"/>`
    })
    y = 20 + units.length * 30 + 12
  }
  const a = col(0, '◇', `DRIFTING · ${drifting.length}`, C.knowledge, drifting, y)
  const b = col(colW + gap, '✓', `CURRENT · ${current.length}`, C.engaged, current, y)
  y = Math.max(a, b) + 10
  if (decisions.length) {
    body += `<text x="0" y="${y + 12}" class="k"><tspan fill="${C.knowledge}">◆</tspan> DECISIONS WHOSE GUARDED CODE MOVED</text>`
    decisions.forEach((d, i) => {
      const dy = y + 22 + i * 20
      body +=
        `<text x="0" y="${dy + 10}" class="b"><tspan fill="${C.knowledge}" font-weight="600">${d.isLive ? '◆' : '◇'} ${esc(d.id)}</tspan>  ${esc(clip(d.title, Math.floor((width - 140) / 6.6)))}</text>` +
        `<text x="${width}" y="${dy + 10}" class="k" text-anchor="end">${esc(d.note)}</text>`
    })
    y += 22 + decisions.length * 20
  }
  return svg(width, y + 4, CSS + body)
}

/**
 * The relations map: touched files in one left-aligned column, the decisions
 * and specs they meet in another, one curve per relation, one row rhythm.
 */
export function relationsSvg(
  width: number,
  files: { name: string; isHot: boolean; isLinked: boolean }[],
  nodes: { key: string; label: string; kind: 'decision' | 'spec' }[],
  edges: [number, number][],
): SvgDoc {
  const row = 22
  const top = 4
  const fileCol = Math.round(width * 0.36)
  const nodeX = fileCol + Math.round(width * 0.14)
  const y = (i: number) => top + i * row + 15
  let body =
    `<style>text{font-family:ui-sans-serif,system-ui,-apple-system,sans-serif;font-size:12.5px}` +
    `.m{fill:#6f6b7d}.f{fill:#8b8799}.k{fill:#24212e}@media (prefers-color-scheme:dark){.k{fill:#eceaf2}.m{fill:#a29eb2}}</style>`
  for (const [a, b] of edges) {
    const x1 = fileCol + 8
    const x2 = nodeX - 8
    const mid = (x1 + x2) / 2
    body += `<path d="M${x1},${y(a) - 4} C${mid},${y(a) - 4} ${mid},${y(b) - 4} ${x2},${y(b) - 4}" fill="none" stroke="${C.knowledge}" stroke-opacity=".55" stroke-width="1.25"/>`
  }
  files.forEach((f, i) => {
    const room = Math.floor((fileCol - 14) / 6.9)
    const n = f.name.length > room ? '…' + f.name.slice(-(room - 1)) : f.name
    body +=
      `<text x="0" y="${y(i)}" class="${f.isLinked ? 'k' : 'f'}">${esc(n)}</text>` +
      `<circle cx="${fileCol + 4}" cy="${y(i) - 4}" r="3.2" fill="${f.isHot ? C.agent : f.isLinked ? C.knowledge : C.faint}">` +
      (f.isHot ? '<animate attributeName="opacity" values="1;.35;1" dur="1.8s" repeatCount="indefinite"/>' : '') +
      `</circle>`
  })
  nodes.forEach((n, i) => {
    const room = Math.floor((width - nodeX - 14) / 6.9) - n.key.length - 3
    body +=
      `<circle cx="${nodeX - 4}" cy="${y(i) - 4}" r="3.2" fill="none" stroke="${C.knowledge}" stroke-width="1.3"/>` +
      `<text x="${nodeX + 4}" y="${y(i)}"><tspan fill="${C.knowledge}" font-weight="600">${n.kind === 'decision' ? '◇' : '▪'} ${esc(n.key)}</tspan><tspan class="m">  ${esc(clip(n.label, room))}</tspan></text>`
  })
  return svg(width, top + Math.max(files.length, nodes.length, 1) * row + 4, body)
}

/**
 * The forecast badge: `⬡ 750` and five pips. Drawn interactive on desktop, so
 * hovering the badge or a pip shows what each element means and what stands
 * between this PR and the next grade.
 */
export function forecastBadgeSvg(grade: number | null, el: Elements, summary: string): SvgDoc {
  const font = 'font-family:ui-sans-serif,system-ui,-apple-system,sans-serif;font-size:13px;font-weight:650'
  const label = `⬡ ${grade ?? '—'}`
  const x0 = 12 + label.length * 7.2
  let body = `<title>${esc(summary)}</title><text x="0" y="14" style="${font}" fill="${PROBA}">${esc(label)}</text>`
  ELEMENT_KEYS.forEach((k, i) => {
    body +=
      `<g><title>${esc(elementLine(k, el[k]))}</title>` +
      `<circle cx="${x0 + i * 12}" cy="9.5" r="4.2" fill="${el[k] === 'ok' ? PROBA : 'none'}" stroke="${PROBA}" stroke-width="1.3" stroke-opacity="${el[k] === 'unknown' ? 0.4 : 1}"/>` +
      `<circle cx="${x0 + i * 12}" cy="9.5" r="7" fill="transparent"/></g>`
  })
  return svg(Math.ceil(x0 + ELEMENT_KEYS.length * 12), 19, body)
}
