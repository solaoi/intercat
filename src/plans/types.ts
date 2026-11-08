import type { Attachment } from '../bridge';

export interface PlanItem {
  planItemId: string; trackRef: string; folderId: string; order: number;
  title: string; instruction: string; constraints: string[]; doneWhen: string;
  after: { id: string; inherit: boolean }[];
  skipOrder: string[]; startRequested: boolean;
  state: 'planned' | 'editing' | 'running' | 'succeeded' | 'failed' | 'cut';
  utteranceId: string | null; detail: string | null;
}
export interface PlanAssumption { id: string; what: string; default: string; keptAt: number | null }
export interface PlanAssumptionCard { id: string; assumptionId: string; planId: string; planRevision: number; text: string; detail: string; label: string }
export interface Plan {
  id: string; rev: number; state: 'draft' | 'held' | 'running' | 'done' | 'single-job';
  sourceId: string; sourceRevision: number; executeAllowed: boolean; lockedTrack: string | null;
  startMode: 'manual' | 'after-delay';
  generation: { id: string; state: 'queued' | 'running' | 'ready' | 'timed-out' | 'invalid' | 'unlock-required' | 'failed'; attempt: number; detail: string | null };
  basedOn: { folderId: string; trackId: string | null; sha: string }[]; attachments: Attachment[];
  assumptions: PlanAssumption[]; summary: string; notices: string[]; items: PlanItem[]; seenRevision: number | null; rewrite: PlanRewrite | null;
}
export type PlanAction = { kind: 'keep-assumption'; assumptionId: string } | { kind: 'review' | 'start' | 'hold' | 'retry' | 'run-as-one' } | { kind: 'cut' | 'restore' | 'retry-item'; itemId: string }
  | { kind: 'resolve-lock'; unlock: boolean }
  | { kind: 'move'; itemId: string; direction: 'earlier' | 'later' }
  | { kind: 'start-without'; itemId: string; dependencyId: string }
  | { kind: 'retry-dependency'; itemId: string; dependencyId: string; clipId: string }
  | { kind: 'move-to' | 'unlock-and-move'; itemId: string; trackId: string };
export interface PlanCommand { opId: string; planId: string; revision: number; action: PlanAction }
export interface PlansStartSetting { revision: number; mode: Plan['startMode'] }
export interface PlansStartChange extends PlansStartSetting { opId: string }
export interface PlanObservation { idle: boolean; plans: { planId: string; revision: number }[] }
export interface PlanStartUpdate { countdowns: { planId: string; revision: number; remainingMs: number }[]; started: Plan[] }
export interface PlanRequest { id: string; itemId?: string; nonce: number; mode?: 'menu' | 'earlier' | 'later' | 'up' | 'down' | 'cut'; point?: { left: number; top: number } }

export interface PlanRewrite {
  owner: string; token: string; itemId: string; revision: number; before: PlanItem;
  renewed: { epochMs: number; monoMs: number; bootId: string; timelineMs: number };
  words: string | null; proposal: (Pick<PlanItem, 'trackRef' | 'folderId' | 'title' | 'instruction' | 'constraints' | 'doneWhen' | 'after'> & { id: string }) | null;
}
export type PlanRewriteAction = { kind: 'begin'; planId: string; itemId: string; token: string; revision: number }
  | { kind: 'renew' | 'cancel'; planId: string; token: string }
  | { kind: 'submit'; planId: string; token: string; revision: number; opId: string; text: string }
  | { kind: 'dependencies'; planId: string; token: string; revision: number; opId: string; after: PlanItem['after'] }
  | { kind: 'apply' | 'retry'; planId: string; token: string; revision: number; opId: string };
