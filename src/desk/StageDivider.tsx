import { useLayoutEffect, useRef, useState, type RefObject } from 'react';
import { TOP_HEIGHT, ROW_HEIGHT } from '../timeline/model';

export interface DeskLayout {
  mode: 'normal' | 'transcript' | 'stage';
  height: number | null;
}

interface Props {
  stage: RefObject<HTMLElement | null>;
  desk: RefObject<HTMLElement | null>;
  layout: DeskLayout;
  change: (layout: DeskLayout) => void;
}

/** The input, decision line and request strip reserve space before the stage grows. */
export function StageDivider({ stage, desk, layout, change }: Props) {
  const drag = useRef<{ y: number; height: number; maximum: number } | null>(null);
  const [size, setSize] = useState({ height: 0, maximum: 0 });
  const height = size.height;
  const maximum = () => {
    if (!stage.current || !desk.current) return 0;
    const top = stage.current.getBoundingClientRect().top;
    const bottom = stage.current.parentElement!.getBoundingClientRect().bottom;
    const reserved = [...desk.current.querySelectorAll<HTMLElement>('.composer,.model-status,.status-bar,.editor-line,.request-strip,.history-controls,.confirmation-notice,.application-command-error')]
      .reduce((total, element) => {
        const style = getComputedStyle(element);
        return total + element.getBoundingClientRect().height + parseFloat(style.marginTop) + parseFloat(style.marginBottom);
      }, 0);
    // Reserve the collapsed controls, divider and desk spacing as well.
    return Math.max(0, bottom - top - reserved - 42);
  };
  useLayoutEffect(() => {
    if (!stage.current) return;
    const measure = () => {
      const next = { height: Math.round(stage.current!.getBoundingClientRect().height), maximum: Math.round(maximum()) };
      setSize(current => current.height === next.height && current.maximum === next.maximum ? current : next);
    };
    const observer = new ResizeObserver(measure);
    observer.observe(stage.current);
    if (desk.current) observer.observe(desk.current);
    measure();
    return () => observer.disconnect();
  }, [stage, desk]);

  function resize(next: number, limit = maximum()) {
    const normalLimit = Math.min(limit - 96, innerHeight * 0.4);
    const minimum = Math.min(TOP_HEIGHT + ROW_HEIGHT, Math.max(0, normalLimit));
    if (layout.mode === 'normal' && next < (stage.current?.getBoundingClientRect().height ?? height)) {
      change({ mode: 'normal', height: Math.max(minimum, next) });
    } else if (next >= limit - 8) {
      change({ mode: 'stage', height: null });
    } else {
      change({ mode: next >= normalLimit ? 'transcript' : 'normal', height: Math.max(minimum, next) });
    }
  }

  return <div className="stage-divider" role="separator" tabIndex={0} aria-label="Resize stage and desk" aria-orientation="horizontal"
    aria-valuemin={0} aria-valuemax={Math.max(height, size.maximum)} aria-valuenow={height}
    aria-valuetext={layout.mode === 'stage' ? 'Stage mode' : layout.mode === 'transcript' ? 'Transcript collapsed' : `${height} pixels`}
    title="Drag to resize · double-click to reset · ⇧⌘T for Stage mode"
    onDoubleClick={() => change({ mode: 'normal', height: null })}
    onPointerDown={event => {
      if (event.button !== 0 || !stage.current) return;
      drag.current = { y: event.clientY, height: stage.current.getBoundingClientRect().height, maximum: maximum() };
      event.currentTarget.setPointerCapture(event.pointerId);
      event.preventDefault();
      event.currentTarget.focus();
    }}
    onPointerMove={event => {
      if (drag.current) resize(drag.current.height + event.clientY - drag.current.y, drag.current.maximum);
    }}
    onPointerUp={event => { drag.current = null; if (event.currentTarget.hasPointerCapture(event.pointerId)) event.currentTarget.releasePointerCapture(event.pointerId); }}
    onPointerCancel={() => { drag.current = null; }}
    onKeyDown={event => {
      if (event.key === 'ArrowUp' || event.key === 'ArrowDown') {
        event.preventDefault(); resize(height + (event.key === 'ArrowUp' ? -24 : 24));
      } else if (event.key === 'Home') {
        event.preventDefault(); change({ mode: 'normal', height: Math.min(TOP_HEIGHT + ROW_HEIGHT, maximum()) });
      } else if (event.key === 'End' || event.key === 'Enter') {
        event.preventDefault(); change({ mode: event.key === 'Enter' && layout.mode === 'stage' ? 'normal' : 'stage', height: null });
      }
    }}><span aria-hidden="true">▴ ┃ ▾</span></div>;
}
