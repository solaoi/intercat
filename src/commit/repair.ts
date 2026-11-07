import type { SessionModel, Approval, Prompt } from '../bridge';

export interface RepairSelection { tree: string; path: string; side: 'before' | 'after'; object: string; startLine: number; endLine: number }
export type RepairPurpose = { kind: 'check' } | { kind: 'removeSecret'; finding: string } | { kind: 'assumption'; id: string; revision: number; text: string } | { kind: 'edit'; selection: RepairSelection; text: string };
export interface RepairRequest { opId: string; jobId: string; revision: number; track: string; purpose: RepairPurpose }
export interface RepairRefreshRequest { opId: string; jobId: string; revision: number; repairId: string }
export type RepairResponse = { status: 'accepted'; run: RepairRun } | { status: 'rejected'; reason: string };
export interface RepairFollowUp { opId: string; id: string; revision: number; text: string; selection?: RepairSelection }
export interface RepairRun {
  model?: SessionModel | null;
  id: string; revision: number; jobId: string; jobRevision: number; track: string; purpose: RepairPurpose;
  phase: 'queued' | 'preparing' | 'running' | 'stopping' | 'held' | 'captured' | 'applying' | 'done' | 'failed' | 'needs-recovery' | 'discarded';
  approvals: Approval[]; prompts: Prompt[]; output: string | null; failure: string | null; session: string | null;
  refresh: { operation: string; error: string | null } | null;
  restoring: string | null;
  turn: string | null;
  secretInput: string | null;
}
