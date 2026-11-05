import { useEffect, useId, useLayoutEffect, useMemo, useRef, useState } from 'react';
import { Application } from 'pixi.js';
import 'pixi.js/unsafe-eval';
import type { TimelineMark } from '../bridge';
import { geometry, initialView, LABEL_WIDTH, TOP_HEIGHT, type Clip, type TimelineView, type Track } from './model';
import { lanes, revealLane, totalHeight, visibleLanes, type Lane } from './viewport';
import type { TimelineBounds } from './useWindow';
import { TimelineScene } from './scene';

interface Props {
  onViewport: (bounds: TimelineBounds) => void; liveNow: () => number | null; preview: boolean; advancing: boolean; tracks: Track[]; marks?: TimelineMark[]; selected: string; selectedRepair: string | null; view: TimelineView;
  onSelect: (id: string) => void; onClip: (track: Track, clip: Clip, repairId: string | null) => void;
  onRepairSelect: (track: Track, id: string) => void; onRepairStop: (id: string) => void; onCut: (id: string) => void; onInteraction: () => void;
}
const NO_MARKS: TimelineMark[] = [];
const PREVIEW_MARKS: TimelineMark[] = [
  { id: 'preview-1', trackId: 'A', repairRunId: null, at: (initialView.start + 75) * 1000, inputId: null, text: 'Add retry to login' },
  { id: 'preview-2', trackId: 'C', repairRunId: null, at: (initialView.start + 195) * 1000, inputId: null, text: 'Cover edge cases' },
  { id: 'preview-3', trackId: 'B', repairRunId: null, at: (initialView.start + 310) * 1000, inputId: null, text: 'Rate-limit it, separately' },
];
export function Timeline(props: Props) {
  const host = useRef<HTMLDivElement>(null), viewport = useRef<HTMLDivElement>(null), scroller = useRef<HTMLDivElement>(null);
  const application = useRef<Application | null>(null), scene = useRef<TimelineScene | null>(null);
  const activeButtons = useRef(new Map<string, { element: HTMLButtonElement; clip: Clip; width: number }>());
  const current = useRef(props); current.current = props;
  const anchor = useRef({ now: props.view.now, at: performance.now() });
  if (anchor.current.now !== props.view.now) anchor.current = { now: props.view.now, at: performance.now() };
  const [size, setSize] = useState({ width: 800, height: 300, resolution: window.devicePixelRatio });
  const [scroll, setScroll] = useState(0), [overview, setOverview] = useState(false), [ready, setReady] = useState(false), [error, setError] = useState('');
  const pendingReveal = useRef<string | null>(null);
  const rows = useMemo(() => lanes(props.tracks, overview), [props.tracks, overview]);
  const height = totalHeight(rows), visibleHeight = Math.max(0, size.height - TOP_HEIGHT);
  const visible = useMemo(() => visibleLanes(rows, scroll, visibleHeight), [rows, scroll, visibleHeight]);
  const requestedTracks = useMemo(() => [...new Set(visible.map(row => row.track.id))], [visible]);
  useEffect(() => {
    if (!props.preview && requestedTracks.length) props.onViewport({ tracks: requestedTracks, start: Math.max(0, Math.floor(props.view.start * 1000)), end: Math.ceil((props.view.start + props.view.duration) * 1000) });
  }, [props.preview, props.onViewport, requestedTracks, props.view.start, props.view.duration]);
  const selectedKey = props.selectedRepair ? `repair-${props.selectedRepair}` : `track-${props.selected}`;
  const navigation = useRef(selectedKey);
  useLayoutEffect(() => { navigation.current = selectedKey; }, [selectedKey]);
  const selected = rows.find(row => row.key === selectedKey);
  const mirror = selected && !visible.includes(selected) ? [...visible, selected] : visible;
  const layout = geometry(size.width);
  const scrollId = useId();
  const marks = props.preview ? PREVIEW_MARKS : props.marks ?? NO_MARKS;
  function moveTo(value: number) {
    if (scroller.current) { scroller.current.scrollTop = value; setScroll(scroller.current.scrollTop); }
  }
  function density(next: boolean, row = visible[0]) { pendingReveal.current = row?.key ?? null; setOverview(next); }
  function choose(row: Lane) {
    navigation.current = row.key;
    if (row.repairId) props.onRepairSelect(row.track, row.repairId); else props.onSelect(row.track.id);
    moveTo(revealLane(scroll, visibleHeight, row));
  }
  useLayoutEffect(() => {
    if (selected) moveTo(revealLane(scroller.current?.scrollTop ?? 0, visibleHeight, selected));
  }, [selectedKey, visibleHeight]);
  useLayoutEffect(() => {
    if (pendingReveal.current) {
      const row = rows.find(row => row.key === pendingReveal.current); pendingReveal.current = null;
      if (row) moveTo(row.top);
    }
  }, [rows]);
  useEffect(() => {
    let disposed = false;
    const instance = new Application();
    const measure = () => {
      const box = viewport.current!.getBoundingClientRect();
      const next = { width: Math.max(1, Math.floor(box.width)), height: Math.max(1, Math.floor(box.height)), resolution: window.devicePixelRatio };
      setSize(current => current.width === next.width && current.height === next.height && current.resolution === next.resolution ? current : next);
    };
    const observer = new ResizeObserver(measure);
    observer.observe(viewport.current!);
    window.addEventListener('resize', measure);
    void (async () => {
      await document.fonts.ready;
      await instance.init({ preference: 'webgl', backgroundAlpha: 0, antialias: true, autoDensity: true, resolution: window.devicePixelRatio });
      if (disposed) { instance.destroy(true, { children: true, context: true }); return; }
      application.current = instance; scene.current = new TimelineScene(instance);
      instance.canvas.setAttribute('aria-hidden', 'true'); host.current!.appendChild(instance.canvas);
      instance.ticker.add(() => {
        const props = current.current;
        if (!props.advancing) return;
        const now = props.preview ? props.view.now + Math.min(2, (performance.now() - anchor.current.at) / 1000) : props.liveNow() ?? props.view.now;
        scene.current?.tick(now);
        if (activeButtons.current.size) {
          const { timeWidth, futureWidth } = geometry(instance.screen.width);
          for (const button of activeButtons.current.values()) {
            const left = Math.max(LABEL_WIDTH, LABEL_WIDTH + (button.clip.start - props.view.start) / props.view.duration * timeWidth);
            const end = Math.min(instance.screen.width - futureWidth, LABEL_WIDTH + (Math.max(button.clip.end, now) - props.view.start) / props.view.duration * timeWidth);
            const width = Math.max(8, Math.round(end - left));
            if (button.width !== width) { button.width = width; button.element.style.width = `${width}px`; }
          }
        }
      });
      setReady(true);
    })().catch(e => { if (!disposed) setError(String(e)); });
    return () => { disposed = true; observer.disconnect(); window.removeEventListener('resize', measure); scene.current?.destroy(); scene.current = null; if (application.current === instance) { application.current = null; instance.destroy(true, { children: true, context: true }); } };
  }, []);
  useLayoutEffect(() => {
    scene.current?.sync({ rows, visible, width: size.width, height: size.height, scroll, total: height, selected: selectedKey, overview, view: props.view, marks });
  }, [ready, rows, visible, size, scroll, height, selectedKey, overview, props.view.start, props.view.duration, marks]);
  function overviewPosition(event: React.PointerEvent<HTMLDivElement>) {
    const box = event.currentTarget.getBoundingClientRect();
    moveTo((event.clientY - box.top) / Math.max(1, box.height) * height - visibleHeight / 2);
    props.onInteraction();
  }
  return <section ref={viewport} className="timeline" data-ready={ready} data-future-count={layout.futureCount} data-overview={overview} aria-label="Timeline" onPointerMove={props.onInteraction}>
    <div className="canvas-host" ref={host} />
    <button className="timeline-density" aria-pressed={overview} onClick={() => density(!overview)}>{overview ? 'Detail' : 'Overview'}</button>
    <div className="you-label">◆ <span>You</span></div>
    <div className="input-mark-actions" aria-label="Received inputs">
      {[...marks].sort((a, b) => a.at - b.at || a.id.localeCompare(b.id)).map(mark => {
        const left = LABEL_WIDTH + (mark.at / 1000 - props.view.start) / props.view.duration * layout.timeWidth;
        if (left < LABEL_WIDTH + 5 || left > size.width - layout.futureWidth - 5) return null;
        const track = props.tracks.find(track => track.id === mark.trackId);
        const description = `${mark.text} · to ${track?.label ?? '?'}${mark.repairRunId ? ' · for the commit' : ''}`;
        return <button key={mark.id} style={{ left: left - 6 }} title={description} aria-label={description} onClick={() => {
          if (!track) return;
          if (mark.repairRunId) props.onRepairSelect(track, mark.repairRunId); else props.onSelect(track.id);
        }} />;
      })}
    </div>
    <div id={scrollId} className="timeline-scroll" ref={scroller} onScroll={event => { setScroll(event.currentTarget.scrollTop); props.onInteraction(); }}>
      <div className="timeline-content" style={{ height }}>
        <div role="grid" aria-label="Tracks" aria-rowcount={rows.length} aria-colcount={1} tabIndex={0} className="track-grid" style={{ height }} aria-activedescendant={selected ? selected.key : undefined}
          onKeyDown={event => {
            if (!rows.length) return;
            let next = rows.findIndex(row => row.key === navigation.current);
            if (event.key === 'ArrowDown') next = Math.min(next + 1, rows.length - 1);
            else if (event.key === 'ArrowUp') next = Math.max(next - 1, 0);
            else if (event.key === 'Home') next = 0;
            else if (event.key === 'End') next = rows.length - 1;
            else if (event.key === 'Enter' && overview) { event.preventDefault(); density(false, selected); return; }
            else if (/^[1-9]$/.test(event.key)) next = rows.findIndex(row => !row.repairId && row.track === props.tracks[Math.min(Number(event.key) - 1, props.tracks.length - 1)]);
            else return;
            event.preventDefault(); props.onInteraction(); choose(rows[Math.max(0, next)]);
          }}>
          {mirror.map(row => <div role="row" id={row.key} key={row.key} aria-rowindex={row.index + 1} aria-selected={row.key === selectedKey} className={`${row.repairId ? 'repair-row' : 'track-row'}${overview ? ' strip-row' : ''}`} style={{ top: row.top, height: row.height }} onClick={() => { if (overview) density(false, row); else choose(row); }}>
            <div role="gridcell" aria-label={`${row.track.label} · ${row.repairId ? 'for the commit' : row.track.title} · ${row.status}`}>
              {overview ? <span className="sr-only">{row.track.label} · {row.track.title} · {row.status}</span> : row.repairId ? <><span className="repair-branch">└</span><span>for the commit</span><span className={`track-status ${row.status}`}>{row.status === 'stopped' ? '■ stopped' : row.status === 'done' ? '✓' : row.status === 'waiting' ? '◇' : '◌'}</span>{row.canStop && <button aria-label={`Stop ${row.track.label} repair`} onClick={event => { event.stopPropagation(); props.onRepairStop(row.repairId!); }}>■</button>}</> : <><span className="track-letter" style={{ color: `#${row.track.color.toString(16)}` }}>{row.track.label}</span><span className="track-name" title={row.track.title}>{row.track.title}<small>{row.track.folder}</small></span><span className={`track-status ${row.status}`}>{row.status === 'stopped' ? '■ stopped' : row.status === 'done' ? '✓' : row.status === 'waiting' ? '◇' : '◌'}</span></>}
            </div>
          </div>)}
        </div>
        {!overview && <div className="step-actions" aria-label="Recorded steps">{visible.flatMap(row => row.clips.map(clip => {
          const left = Math.max(LABEL_WIDTH, LABEL_WIDTH + (clip.start - props.view.start) / props.view.duration * layout.timeWidth);
          const end = Math.min(size.width - layout.futureWidth, LABEL_WIDTH + ((clip.running ? Math.max(clip.end, props.liveNow() ?? props.view.now) : clip.end) - props.view.start) / props.view.duration * layout.timeWidth);
          if (end < LABEL_WIDTH || left >= size.width - layout.futureWidth) return null;
          return <button key={clip.id} ref={clip.running ? element => { if (element) activeButtons.current.set(clip.id, { element, clip, width: -1 }); else activeButtons.current.delete(clip.id); } : undefined} style={{ left, top: row.top + (row.repairId ? 1 : 20), height: row.repairId ? 24 : 35, width: Math.max(8, end - left) }} aria-label={`Open ${row.repairId ? 'repair ' : ''}step ${row.track.label}:${clip.ordinal ?? clip.id}: ${clip.label}`} title={`${row.track.label}:${clip.ordinal ?? clip.id} · ${clip.label}${clip.approximate ? ' · approximate duration' : ''}${clip.effectCount ? ` · ${clip.effectCount} effects outside Git` : ''}`} onClick={() => { choose(row); props.onClip(row.track, clip, row.repairId); }}>{clip.ordinal ?? '•'}</button>;
        }))}</div>}
        {!overview && <div className="future-actions" style={{ left: size.width - layout.futureWidth }}>{visible.filter(row => !row.repairId).map(row => <div className="future-row" key={row.key} style={{ position: 'absolute', top: row.top, height: row.height }}>{row.track.planned.slice(0, layout.futureCount).map(planned => <button className="cut-turn" key={planned.id} aria-label={`Cut ${planned.label} from ${row.track.label}`} title={`Cut: ${planned.label} · ${planned.after}`} onClick={() => props.onCut(planned.id)} onKeyDown={event => { if (event.key === 'Delete' || event.key === 'Backspace') { event.preventDefault(); props.onCut(planned.id); } }}>×</button>)}</div>)}</div>}
      </div>
    </div>
    <div className="track-overview" role="scrollbar" tabIndex={0} aria-label="All tracks" aria-controls={scrollId} aria-orientation="vertical" aria-valuemin={0} aria-valuemax={Math.max(0, height - visibleHeight)} aria-valuenow={Math.round(scroll)} onPointerDown={event => { event.currentTarget.setPointerCapture(event.pointerId); overviewPosition(event); }} onPointerMove={event => { if (event.currentTarget.hasPointerCapture(event.pointerId)) overviewPosition(event); }} onPointerUp={event => event.currentTarget.releasePointerCapture(event.pointerId)} onKeyDown={event => {
      const delta = event.key === 'ArrowDown' ? 44 : event.key === 'ArrowUp' ? -44 : event.key === 'PageDown' ? visibleHeight : event.key === 'PageUp' ? -visibleHeight : 0;
      if (delta || event.key === 'Home' || event.key === 'End') { event.preventDefault(); props.onInteraction(); moveTo(event.key === 'Home' ? 0 : event.key === 'End' ? height : scroll + delta); }
    }} />
    {error && <p role="alert">Timeline could not start: {error}</p>}
  </section>;
}
