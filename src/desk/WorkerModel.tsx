import type { SessionModel } from '../bridge';

export function WorkerModel({ model, prefix }: { model?: SessionModel | null; prefix?: string }) {
  const id = model?.modelId;
  const known = id?.match(/^muse-spark-(\d+(?:\.\d+)*)(?:-contributor)?$/);
  const name = known ? `Spark ${known[1]}` : id === 'muse-wonder' || id === 'muse-wonder-contributor' ? 'Wonder' : id;
  const text = [prefix, name || 'Model —', model?.reasoningEffort || 'effort —'].filter(Boolean).join(' · ');
  return <span className="worker-model" title={[id, model?.providerId, model?.reasoningEffort].filter(Boolean).join(' · ') || 'The worker has not reported its model or effort.'}>{text}</span>;
}
