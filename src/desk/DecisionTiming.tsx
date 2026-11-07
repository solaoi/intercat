import type { InputDecision } from '../bridge';

function recorded(value: unknown): value is number {
  return typeof value === 'number' && Number.isFinite(value) && value >= 0;
}

export function DecisionTiming({ decision }: { decision: InputDecision | null | undefined }) {
  const timings = decision?.timings?.filter(timing => recorded(timing.roundTripMs) && recorded(timing.inferenceMs)) ?? [];
  if (!timings.length) return null;
  return <details className="decision-timing">
    <summary>Decision timing</summary>
    <p>Round trip includes the Editor service queue and transfer. Inference is model time.</p>
    {timings.map((timing, index) => <dl key={index} aria-label={`Decision request ${index + 1}`}>
      <dt>Request {index + 1} round trip</dt><dd>{Math.round(timing.roundTripMs).toLocaleString()} ms</dd>
      <dt>Request {index + 1} inference</dt><dd>{Math.round(timing.inferenceMs).toLocaleString()} ms</dd>
    </dl>)}
  </details>;
}
