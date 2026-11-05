import type { IntegrationJob, Change, Diff } from './types';
import type { RepairSelection } from './repair';

export interface DiffSelection { side: 'before' | 'after'; startLine: number; endLine: number; quote: string }
export interface CommitContext { jobId: string; revision: number; track: string; label: string; selection: RepairSelection; quote: string }

export function bindContext(job: IntegrationJob, track: string, change: Change, range: DiffSelection): CommitContext | null {
  const file = change[range.side];
  const owner = job.tracks.find(item => item.id === track);
  if (!file || !owner || !job.candidate || ![33188, 33261].includes(file.mode)) return null;
  return { jobId: job.id, revision: job.revision, track, label: owner.label, quote: range.quote,
    selection: { tree: job.candidate.blockedCommit?.tree ?? job.candidate.tree, path: change.path, object: file.object, side: range.side, startLine: range.startLine, endLine: range.endLine } };
}

export function contextState(context: CommitContext, job: IntegrationJob | null | undefined): 'current' | 'changed' | 'gone' {
  if (!job || job.id !== context.jobId || ['done', 'discarded'].includes(job.status)) return 'gone';
  return job.revision === context.revision && (job.candidate?.blockedCommit?.tree ?? job.candidate?.tree) === context.selection.tree ? 'current' : 'changed';
}

// Reattach only a unique complete-line match. An ambiguous or removed passage
// needs a new selection instead of guessing at its owner or line number.
export function reanchor(context: CommitContext, job: IntegrationJob, diff: Diff): CommitContext | null {
  const commit = job.candidate?.blockedCommit?.track === context.track ? job.candidate.blockedCommit : job.candidate?.commits.find(item => item.track === context.track);
  const change = commit?.changes.find(item => item.path === context.selection.path);
  if (!change || diff.binary || diff.tooLarge) return null;
  const content = `\n${diff[context.selection.side]}\n`;
  const needle = `\n${context.quote}\n`;
  const match = content.indexOf(needle);
  if (match < 0 || content.indexOf(needle, match + 1) >= 0) return null;
  const startLine = content.slice(0, match).split('\n').length;
  return bindContext(job, context.track, change, { side: context.selection.side, quote: context.quote, startLine, endLine: startLine + context.quote.split('\n').length - 1 });
}
