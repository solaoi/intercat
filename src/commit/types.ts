import type { Assumption, Folder } from '../bridge';

export interface Change { path: string; before: { object: string; mode: number } | null; after: { object: string; mode: number } | null }
export interface Commit { track: string; sha: string; parent: string; tree: string; message: string; author: string; changes: Change[] }
export interface SecretFinding { id: string; path: string; line: number; endLine: number; rule: string }
export const binaryReviewRule = 'binary-pattern-scan-incomplete';
export interface SecretScan { findings: SecretFinding[]; unscannedLfs: string[] }
export type IntegrationConflict = { kind: 'overlay'; base: string; incoming: string; paths: string[] } | { kind: 'baseline' | 'savedIndex'; paths: string[] };
export interface Candidate {
  revision: number; state: string; head: string; tree: string; settingsHash: string; buildRevision: number; workspace: string;
  commits: Commit[]; blockedCommit: Commit | null; secretScans: SecretScan[];
  undo: { sha:string;parent:string;tree:string;message:string;changes:Change[];scan:SecretScan } | null;
  undoConflict: {track:string;applied:string[];remaining:string[];base:string;target:string;incoming:string;paths:string[]} | null;
  check: { command: string | null; code: number | null; stdout: string; stderr: string; truncated: boolean } | null;
  reason: string | null; failedTrack: string | null; hooksSkipped: boolean; conflict: IntegrationConflict | null;
  dependencyRuns: { directory: string; forced: boolean; downloads: number; cacheHits: number }[];
  repair: {requestedTrack:string;appliedTrack:string} | null;
}
export type CommitAssumption = Assumption & { included: boolean };
export interface CommitOrderReview { overlaps: { path: string; tracks: string[] }[]; order: string[]; confirmed: boolean }
export interface IntegrationJob {
  exclusions: { track: string; dependents: string[]; omitted: boolean }[];
  orderReview?: CommitOrderReview | null;
  assumptions: CommitAssumption[]; assumptionsChanged: boolean; assumptionsPending: number; assumptionsFailed: number;
  id: string; revision: number; status: 'preparing' | 'choosing' | 'ordering' | 'open' | 'merge-conflict' | 'failed' | 'discarded' | 'building' | 'staging' | 'checked' | 'committing' | 'commit-blocked' | 'done';
  pathReview: import('./paths').PathReview;
  pathState: import('./paths').PathState | null;
  retirements: import('./paths').Retirement[];
  sessionName: string; slug: string; project: Folder; error: string | null; candidate: Candidate | null;
  tracks: { id: string; label: string; title: string; included: boolean; checkpointCount: number; lastClip: number | null; checkpoint: { sha: string } | null }[];
  plan: { undoConflict: Candidate['undoConflict']; targetRef: string; tree: string | null; rows: { track: string; state: string; changes: Change[]; conflicts: { path: string; withTracks: string[] }[] }[] } | null;
  messages: Record<string, string>; keptSecrets: string[]; finishedSinceOpen?: number;
  contributions: Record<string, unknown>; settingsChanged?: boolean;
}
export type IntegrationAction = { action: 'open'; tracks: string[] | null }
  | { action: 'order'; id: string; revision: number; tracks: string[] }
  | { action: 'refresh' | 'discard' | 'prepare' | 'recheck' | 'skipHooks' | 'retryCommit' | 'treatCheckAsFailure'; id: string; revision: number }
  | { action: 'select'; id: string; revision: number; track: string; included: boolean }
  | { action: 'choosePath'; id: string; revision: number; track: string }
  | { action: 'editMessage'; id: string; revision: number; track: string; message: string }
  | { action: 'retryAssumptions'; id: string; revision: number }
  | { action: 'keepAssumption'; id: string; revision: number; assumption: string }
  | { action: 'keepSecret'; id: string; revision: number; finding: string }
  | { action: 'commit'; id: string; revision: number; head: string; tree: string; settingsHash: string };
export interface IntegrationRequest { opId: string; action: IntegrationAction }
export interface DiffRequest { id: string; revision: number; owner: { kind: 'worker'; track: string } | { kind: 'undo' }; path: string }
export interface Diff { path: string; before: string; after: string; redacted: boolean; binary: boolean; tooLarge: boolean }
