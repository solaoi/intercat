import type { SessionModel } from '../bridge';
import type { PairedInstruction } from '../twoWays/types';
import './workerModel.css';

export function WorkerModel({ model, prefix, defaultEffort = false }: { model?: SessionModel | null; prefix?: string; defaultEffort?: boolean }) {
  const id = model?.modelId;
  const known = id?.match(/^muse-spark-(\d+(?:\.\d+)*)(?:-contributor)?$/);
  const name = known ? `Spark ${known[1]}` : id === 'muse-wonder' || id === 'muse-wonder-contributor' ? 'Wonder' : id;
  const effort = model?.reasoningEffort ? `${defaultEffort ? 'default ' : ''}${model.reasoningEffort}` : 'effort —';
  const text = [prefix, name || 'Model —', effort].filter(Boolean).join(' · ');
  return <span className="worker-model" title={[id, model?.providerId, model?.reasoningEffort].filter(Boolean).join(' · ') || 'The worker has not reported its model or effort.'}>{text}</span>;
}

export function PairedInstructionEffort({instruction}:{instruction:PairedInstruction}) {
  const phase = instruction.phase === 'pending' ? ' · queued' : instruction.phase === 'finished' ? ' · finished' : '';
  return <span className="worker-paired-effort">Two-way instruction: {instruction.effort}{phase}</span>;
}
