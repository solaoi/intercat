import type { SessionModel, Approval, Prompt } from '../bridge';

export interface RepairSelection { tree: string; path: string; side: 'before' | 'after'; object: string; startLine: number; endLine: number }
export type RepairPurpose = { kind: 'check' } | { kind: 'merge' } | { kind: 'removeSecret'; finding: string } | { kind: 'assumption'; id: string; revision: number; text: string } | { kind: 'edit'; selection: RepairSelection; text: string };
export interface RepairOwner {kind:'commit'|'realign'|'undo'|'shared'|'restore';id:string;revision:number}
export interface RepairRequest { opId: string; owner:RepairOwner; track: string; purpose: RepairPurpose }
export interface RepairRefreshRequest { opId: string; jobId: string; revision: number; repairId: string }
export type RepairResponse = { status: 'accepted'; run: RepairRun } | { status: 'rejected'; reason: string };
export interface RepairFollowUp { opId: string; id: string; revision: number; text: string; selection?: RepairSelection }
export interface RemainingRunProcesses {
  nonce: string; label: string; sessionName: string; createdAt: number; revision: number;
  state: 'waiting' | 'kept' | 'stopping' | 'cleaning'; error: string | null;
}
export interface RunProcessCommand { opId: string; id: string; nonce: string; revision: number; action: 'keep' | 'stop' | 'retry-cleanup' }
export interface RepairRun {
  model?: SessionModel | null;
  id: string; revision: number; owner:RepairOwner; track: string; purpose: RepairPurpose;
  phase: 'queued' | 'preparing' | 'running' | 'stopping' | 'held' | 'manual' | 'captured' | 'applying' | 'done' | 'failed' | 'needs-recovery' | 'discarded';
  externalEditor?: boolean;
  approvals: Approval[]; prompts: Prompt[]; output: string | null; failure: string | null; session: string | null;
  refresh: { operation: string; error: string | null } | null;
  restoring: string | null;
  remaining: RemainingRunProcesses | null;
  turn: string | null;
  secretInput: string | null;
}
