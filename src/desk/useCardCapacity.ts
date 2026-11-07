import { useLayoutEffect, useRef, useState, type RefObject } from 'react';
import { ROW_HEIGHT, TOP_HEIGHT } from '../timeline/model';

/** Use the room left by the composer and transcript, not a viewport breakpoint. */
export function useCardCapacity(region: RefObject<HTMLElement | null>, candidates: string[], revision: string, active: boolean) {
  const [capacity, setCapacity] = useState(2);
  const measured = useRef({ width: 0, revision: '', heights: new Map<string, number>() });
  useLayoutEffect(() => {
    const element = region.current;
    const scroll = element?.closest<HTMLElement>('.desk-scroll');
    if (element?.closest('.stage-cards')) { setCapacity(2); return; }
    if (!active || !element || !scroll || candidates.length < 2) return;

    function flowChildren(parent: HTMLElement): HTMLElement[] {
      return [...parent.children].flatMap(child => {
        if (!(child instanceof HTMLElement)) return [];
        const style = getComputedStyle(child);
        if (style.display === 'none') return [];
        return style.display === 'contents' ? flowChildren(child) : child.getBoundingClientRect().height ? [child] : [];
      });
    }
    function outerHeight(child: HTMLElement) {
      const style = getComputedStyle(child);
      return child.getBoundingClientRect().height + parseFloat(style.marginTop) + parseFloat(style.marginBottom);
    }
    const measure = () => {
      const width = element.getBoundingClientRect().width;
      if (!width) return;
      if (measured.current.width !== width || measured.current.revision !== revision) {
        measured.current = { width, revision, heights: new Map() };
      }
      const cards = [...element.querySelectorAll<HTMLElement>('[data-attention-card]')];
      for (const card of cards) {
        const height = card.getBoundingClientRect().height;
        if (height) measured.current.heights.set(card.dataset.attentionCard!, height);
      }
      // Newly arrived or reflowed cards must be measured before they can fold.
      if (candidates.some(id => !measured.current.heights.has(id))) { setCapacity(2); return; }

      const children = flowChildren(scroll);
      const scrollStyle = getComputedStyle(scroll);
      const gap = parseFloat(scrollStyle.rowGap) || 0;
      const other = children.filter(child => child !== element).reduce((sum, child) => sum + outerHeight(child), 0)
        + gap * Math.max(0, children.length - 1) + parseFloat(scrollStyle.paddingTop) + parseFloat(scrollStyle.paddingBottom);
      const stage = element.closest('.main')?.querySelector<HTMLElement>('.stage');
      const stageMinimum = TOP_HEIGHT + ROW_HEIGHT + (stage?.querySelector('.stage-footer')?.getBoundingClientRect().height ?? 0);
      const available = scroll.clientHeight + Math.max(0, (stage?.getBoundingClientRect().height ?? 0) - stageMinimum) - other;
      const style = getComputedStyle(element);
      const header = element.querySelector<HTMLElement>('.needs-you-heading')!;
      const more = element.querySelector<HTMLElement>('.needs-you-more');
      const hasMore = cards.length > candidates.length;
      const required = candidates.reduce((sum, id) => sum + measured.current.heights.get(id)!, 0)
        + outerHeight(header) + (hasMore && more ? outerHeight(more) : 0)
        + (parseFloat(style.rowGap) || 0) * (candidates.length + (hasMore ? 1 : 0))
        + parseFloat(style.marginTop) + parseFloat(style.marginBottom)
        + parseFloat(style.paddingTop) + parseFloat(style.paddingBottom);
      setCapacity(required <= available + 1 ? 2 : 1);
    };
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(element);
    observer.observe(scroll);
    for (const child of flowChildren(scroll)) observer.observe(child);
    const stage = element.closest('.main')?.querySelector<HTMLElement>('.stage');
    if (stage) observer.observe(stage);
    window.addEventListener('resize', measure);
    return () => { observer.disconnect(); window.removeEventListener('resize', measure); };
  });
  return capacity;
}
