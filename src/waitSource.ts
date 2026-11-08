import type { WaitSource } from './bridge';

export function waitSourceLabel(source: WaitSource): string {
  switch (source.kind) {
    case 'turn': return `${source.value.label}’s recorded turn`;
    case 'clip': return `the turn containing ${source.value.turn.label}:${source.value.ordinal}`;
    case 'plan-item': return `${source.value.label} · ${source.value.title}`;
  }
}
