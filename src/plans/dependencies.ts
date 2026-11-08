import type { PlanDependencyLocation } from '../bridge';
import type { Plan, PlanItem } from './types';

export function orderedPredecessors(plan: Plan, item: PlanItem, plans: Plan[]): PlanItem[] {
  const before = item.trackRef.startsWith('new:') ? [] : plans.slice(0, Math.max(0, plans.findIndex(candidate => candidate.id === plan.id))).flatMap(candidate => [...candidate.items].sort((a, b) => a.order - b.order));
  return [...before, ...plan.items.filter(parent => parent.order < item.order).sort((a, b) => a.order - b.order)]
    .filter(parent => parent.trackRef === item.trackRef && parent.state !== 'cut' && !item.skipOrder.includes(parent.planItemId));
}

export function dependencyIds(plan: Plan, item: PlanItem, plans: Plan[]): string[] {
  return [...new Set([...orderedPredecessors(plan, item, plans).map(parent => parent.planItemId), ...item.after.map(parent => parent.id)])];
}

export function blockedDependencies(plan: Plan, item: PlanItem, plans: Plan[], locations: Record<string, PlanDependencyLocation>): PlanDependencyLocation[] {
  if (item.state !== 'planned') return [];
  return dependencyIds(plan, item, plans).flatMap(id => {
    const location = locations[id];
    return location && (location.state === 'failed' || location.state === 'cut') ? [location] : [];
  });
}
