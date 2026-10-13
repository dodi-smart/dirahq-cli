export type Snapshot = {
  session: { human: number; agent: number } | null
  costUsd: number | null
  unbilled: { amount: number; currency: string } | null
  syncFailures: number
  isWedged: boolean
}

export type Task = { id: string; done: boolean }

export type Spec = {
  slug: string
  kind: string
  title: string
  state: string
  primary: string | null
  files: string[]
  paths: string[]
  plannedPaths: string[]
  tasks: Task[]
}

export type GuardStats = { shown: number; blocked: number; complied: number }

export type Decision = { id: string; title: string; path: string | null; guards: string[]; stats?: GuardStats }

export type Focus = { slug: string; source: 'declared' | 'inferred' | 'user' }

export type Env = { dira: string | null; zavet: string | null; hasZavetDir: boolean; root: string }

/** A knowledge chip drawn under one tool row of the transcript. */
export type Chip =
  | { kind: 'decision' | 'spec'; id: string; text: string; isOk?: boolean }
  | { kind: 'trailer'; id: string }
  | { kind: 'hallmark'; id: string; text: string; isOk: boolean }

/** The knowledge check drawn under an ExitPlanMode row. */
export type PlanCheck = {
  isChecking: boolean
  decisions: { id: string; title: string }[]
  specs: { slug: string; title: string; isInPlan: boolean }[]
  conflicts: { id: string; issue: string }[]
}

/** `zavet audit`, the parts the board draws. */
export type Audit = {
  staleSpecs: { slug: string; commits: number; since: string }[]
  staleDecisions: { id: string; commits: number; since: string }[]
  at: number
}

export type ElementKey = 'ci' | 'review' | 'authorship' | 'supervision' | 'intent'
export type ElementState = 'ok' | 'missing' | 'pending' | 'unknown'
export type Elements = Record<ElementKey, ElementState>

/** A local forecast of the branch's proba hallmark. */
export type Forecast = {
  branch: string
  pr: number | null
  elements: Elements
  grade: 585 | 750 | 999 | null
  notes: string[]
  at: number
}

/** What the composer knows about the draft being typed. */
export type Draft = {
  text: string
  ids: string[]
  slugs: string[]
  conflicts: { phrase: string; id: string; why: string }[]
}

/** A record that still needs a human's word. */
export type VouchItem = { key: string; kind: 'decision' | 'spec'; title: string; path: string; reasons: string[] }

/** The parts of the mod a person can switch off with /dira-mods. */
export type Part = 'status' | 'composer' | 'transcript' | 'band' | 'steward'
export type Parts = Record<Part, boolean>

declare module 'claude-code' {
  interface PluginState {
    dira: {
      env: Env | null
      snap: Snapshot | null
      specs: Spec[]
      decisions: Decision[]
      touched: string[]
      guarded: string[]
      focus: Focus | null
      announced: string[]
      overviews: Record<string, string>
      chips: Record<string, Chip[]>
      audit: Audit | null
      isDemo: boolean
      planChecks: Record<string, PlanCheck>
      forecast: Forecast | null
      narration: string | null
      turnTouched: string[]
      isBandOpen: boolean
      draft: Draft | null
      vouch: { items: VouchItem[]; total: number; current: string | null }
      parts: Parts
    }
  }
}
