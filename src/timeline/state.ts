import type { DaemonStatus, TimelineClip } from '../bridge';
import type { Track, Clip, RepairLane } from './model';

interface Clips { work: Clip[]; repairs: Map<string, Clip[]> }
const indexed = new WeakMap<TimelineClip[], Map<string, Clips>>();
function groupClips(source: TimelineClip[]): Map<string, Clips> {
  const cached = indexed.get(source);
  if (cached) return cached;
  const result = new Map<string, Clips>();
  for (const item of source) {
    let group = result.get(item.trackId);
    if (!group) { group = { work: [], repairs: new Map() }; result.set(item.trackId, group); }
    const clip: Clip = { ...item, assumptionTimes: item.assumptionTimes?.map(at => at / 1000), start: item.start / 1000, end: item.end / 1000, generationEnd: item.generationEnd === null ? null : item.generationEnd / 1000 };
    if (!item.repairRunId) group.work.push(clip);
    else {
      const clips = group.repairs.get(item.repairRunId) ?? [];
      if (!group.repairs.has(item.repairRunId)) group.repairs.set(item.repairRunId, clips);
      clips.push(clip);
    }
  }
  indexed.set(source, result);
  return result;
}
const EMPTY_CLIPS: Clip[] = [];
export function liveTimeline(status: DaemonStatus, source: TimelineClip[]): Track[] {
  const colors = [0x819af5, 0x70b8ab, 0xb497cf, 0xd3ac72];
  const clips = groupClips(source);
  const workers = new Map(status.workers?.map(worker => [worker.trackId, worker]));
  const ready = new Set(status.workspaces?.filter(workspace => workspace.state === 'ready').map(workspace => workspace.trackId));
  const runs = Map.groupBy(status.repairRuns ?? [], run => run.track);
  const words = Map.groupBy(status.utterances?.filter(word => word.state === 'placed') ?? [], word => word.decision?.trackId);
  const questions = Map.groupBy(status.reportQuestions ?? [], question => question.trackId);
  return (status.tracks ?? []).map((track, index) => {
    const group = clips.get(track.id);
    const repairRuns = runs.get(track.id) ?? [];
    const worker = workers.get(track.id);
    const waitingForAnswer = questions.get(track.id) ?? [];
    const working = worker?.phase === 'working';
    const ids = new Set([...repairRuns.filter(run => run.phase !== 'discarded' || group?.repairs.has(run.id)).map(run => run.id), ...(group?.repairs.keys() ?? [])]);
    const repairs: RepairLane[] = [...ids].map(id => {
      const run = repairRuns.find(run => run.id === id);
      const working = run && ['queued', 'preparing', 'running', 'stopping', 'captured', 'applying'].includes(run.phase);
      const waiting = run && (['held', 'failed', 'needs-recovery'].includes(run.phase) || run.approvals.length > 0 || run.prompts.length > 0);
      return { id, status: waiting ? 'waiting' : working ? 'working' : 'done', canStop: !!run && ['queued', 'preparing', 'running'].includes(run.phase), clips: group?.repairs.get(id) ?? EMPTY_CLIPS };
    });
    return {
      id: track.id, label: track.label, title: track.title,
      completion: track.completion,
      folder: status.project ? `${status.project.name}${ready.has(track.id) ? '' : ' · preparing'}` : 'awaiting project',
      status: working ? 'working' : waitingForAnswer.length ? 'stopped' : track.status === 'done' ? 'done' : track.status === 'stopped' ? 'stopped' : 'waiting', color: colors[index % colors.length],
      nextWaitsForAnswer: working && !!worker?.activeTurn && waitingForAnswer.some(question => question.askedBeforeLastMessage),
      clips: group?.work ?? EMPTY_CLIPS, repairs,
      planned: (words.get(track.id) ?? []).map(word => ({ id: word.id, label: word.text, after: word.decision?.boundary === 'when-told' ? 'when you say start' : word.decision?.boundary === 'after-turn' || workers.get(track.id)?.activeTurn ? 'after this turn' : 'ready · waiting for worker' })),
    };
  });
}
