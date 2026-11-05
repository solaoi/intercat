import type { DaemonStatus } from './bridge';

/** Keep unchanged projections stable; the renderer reads the clock separately. */
export function mergeStatus(current: DaemonStatus, value: DaemonStatus): DaemonStatus {
  if (current.daemonPid === value.daemonPid && current.sequence !== undefined && value.sequence !== undefined && value.sequence < current.sequence) return current;
  const result = { ...value };
  let changed = Object.keys(current).length !== Object.keys(value).length;
  for (const key of Object.keys(value) as (keyof DaemonStatus)[]) {
    const before = current[key], after = value[key];
    if (key === 'timeline' && current.timeline && value.timeline) {
      const { clock: _beforeClock, ...beforeProjection } = current.timeline;
      const { clock: _afterClock, ...afterProjection } = value.timeline;
      if (JSON.stringify(beforeProjection) === JSON.stringify(afterProjection)) result.timeline = current.timeline;
      else changed = true;
    } else if (before === after || JSON.stringify(before) === JSON.stringify(after)) {
      Object.assign(result, { [key]: before });
    } else changed = true;
  }
  return changed ? result : current;
}
