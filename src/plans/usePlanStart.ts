import { useEffect, useRef, useState } from 'react';
import type { DaemonStatus } from '../bridge';
import type { Plan } from './types';

export type PlanCountdowns = Record<string, { revision: number; seconds: number }>;

function visible(planId: string): boolean {
  return [...document.querySelectorAll<HTMLElement>(`[data-auto-plan="${CSS.escape(planId)}"]`)].some(tile => {
    const stage = tile.closest('.timeline');
    if (!stage) return false;
    if (!tile.checkVisibility({ checkOpacity: true, checkVisibilityCSS: true })) return false;
    const area = stage.getBoundingClientRect(), box = tile.getBoundingClientRect();
    return Math.min(box.right, area.right, innerWidth) - Math.max(box.left, area.left, 0) >= 12
      && Math.min(box.bottom, area.bottom, innerHeight) - Math.max(box.top, area.top, 0) >= 12;
  });
}

export function usePlanStart(status: DaemonStatus, updated: (plan: Plan) => void): PlanCountdowns {
  const live = useRef({ status, updated });
  live.current = { status, updated };
  const [countdowns, setCountdowns] = useState<PlanCountdowns>({});
  useEffect(() => {
    let disposed = false, busy = false, reset = false, pressed = false, sent = false;
    const clear = () => setCountdowns(old => Object.keys(old).length ? {} : old);
    async function pulse() {
      if (disposed || busy) return;
      const { status, updated } = live.current;
      const candidates = status.connected && status.mode !== 'preview' ? (status.plans ?? []).filter(plan =>
        plan.startMode === 'after-delay' && plan.executeAllowed && plan.state === 'draft' && plan.generation.state === 'ready' && !plan.rewrite) : [];
      if (!candidates.length && !sent) { clear(); return; }
      const idle = !reset && !pressed && document.visibilityState === 'visible' && document.hasFocus()
        && !status.dispatchPaused && !status.confirmation?.active
        && !document.querySelector('dialog[open], [popover]:popover-open')
        && !document.activeElement?.closest('input, textarea, [contenteditable="true"]');
      reset = false;
      const plans = candidates.filter(plan => visible(plan.id)).map(plan => ({ planId: plan.id, revision: plan.rev }));
      busy = true; sent = plans.length > 0;
      try {
        const result = await window.intercat.observePlans({ idle: !!idle, plans });
        if (disposed) return;
        result.started.forEach(updated);
        const next = Object.fromEntries(result.countdowns.map(item => [item.planId, { revision: item.revision, seconds: Math.max(1, Math.ceil(item.remainingMs / 1000)) }]));
        setCountdowns(old => JSON.stringify(old) === JSON.stringify(next) ? old : next);
      } catch { if (!disposed) clear(); }
      finally { busy = false; }
    }
    function activity() { reset = true; clear(); void pulse(); }
    function down() { pressed = true; activity(); }
    function up() { pressed = false; activity(); }
    const events = ['pointermove', 'keydown', 'wheel', 'scroll', 'focusin', 'focusout', 'visibilitychange', 'toggle'] as const;
    events.forEach(name => document.addEventListener(name, activity, true));
    document.addEventListener('pointerdown', down, true);
    document.addEventListener('pointerup', up, true);
    document.addEventListener('pointercancel', up, true);
    window.addEventListener('blur', activity);
    window.addEventListener('focus', activity);
    const timer = window.setInterval(() => { void pulse(); }, 250);
    void pulse();
    return () => {
      disposed = true; clearInterval(timer);
      events.forEach(name => document.removeEventListener(name, activity, true));
      document.removeEventListener('pointerdown', down, true);
      document.removeEventListener('pointerup', up, true);
      document.removeEventListener('pointercancel', up, true);
      window.removeEventListener('blur', activity); window.removeEventListener('focus', activity);
      if (sent) void window.intercat.observePlans({ idle: false, plans: [] }).catch(() => {});
    };
  }, []);
  return countdowns;
}
