import type { Effect } from '../bridge';

const labels: Record<string, string> = { push: 'Push', send: 'Send', delete: 'Delete', migrate: 'Data change', deploy: 'Deploy', schedule: 'Scheduled job', 'external-write': 'External write' };
export function Effects({ effects }: { effects: Effect[] }) {
  if (!effects.length) return null;
  return <section className="effects" aria-label="Effects outside Git">
    <strong>⇗ Effects outside Git · {effects.length}</strong>
    <ul>{effects.map(effect => <li key={effect.id}><span>{labels[effect.kind] ?? effect.kind} · {effect.target}</span><b>{effect.running ? 'in progress' : effect.outcome === 'done' ? 'completed' : effect.outcome === 'failed' ? 'failed' : 'outcome unknown'}</b>{effect.reason && <small>{effect.reason}</small>}</li>)}</ul>
    <p>Restoring files does not undo these actions.</p>
  </section>;
}
