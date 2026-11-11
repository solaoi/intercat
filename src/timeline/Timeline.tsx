import {GhostControls} from '../restore/GhostControls';
import { queueLabel } from '../scheduling';
import { useEffect, useId, useLayoutEffect, useMemo, useRef, useState } from 'react';
import { Application } from 'pixi.js';
import 'pixi.js/unsafe-eval';
import type { TimelineMark } from '../bridge';
import { geometry, initialView, LABEL_WIDTH, TOP_HEIGHT, type Clip, type PlannedTurn, type TimelineView, type Track } from './model';
import { lanes, revealLane, totalHeight, visibleLanes, type Lane } from './viewport';
import type { TimelineBounds } from './useWindow';
import { TimelineScene } from './scene';
import { TrackMenu } from './TrackMenu';
import { InputMarks } from './InputMarks';
import { groupMarks } from './inputMarkLayout';
import { completionLabel } from './completion';
import { dependencyLinks } from './futureLayout';
import { PlannedOverflow } from './PlannedOverflow';
import { groupBranchPoints, type BranchPreview } from './branchPointLayout';

interface Props {
  onRestoreAside:(id:string)=>void;
  onPushCommit?:(commit:NonNullable<TimelineMark['commit']>)=>void;
  branchPreview: BranchPreview | null; branchError: string; onBranchPoint: (trackId: string, at: number | null) => void;
  onViewport: (bounds: TimelineBounds) => void; liveNow: () => number | null; preview: boolean; advancing: boolean; tracks: Track[]; marks?: TimelineMark[]; selected: string; selectedRepair: string | null; view: TimelineView;
  onSelect: (id: string) => void; onClip: (track: Track, clip: Clip, repairId: string | null) => void;
  onOpenTrack: (id: string) => void;
  onInput: (id: string) => void;
  onReviewStop?: (jobs: NonNullable<Track['reviewJobs']>)=>void;
  onPlan: (id: string, itemId?: string, point?: { left: number; top: number }) => void;
  selectedPlanItem: string | null;
  focusPlanItem: { id: string; nonce: number } | null;
  onPlanControl: (id: string, itemId: string, mode: 'menu' | 'earlier' | 'later' | 'up' | 'down', point?: { left: number; top: number }) => void;
  onSayTo: (id: string) => void; targetDisabled: boolean;
  canDropFiles: boolean; onDropFiles: (files: File[], trackId: string) => void;
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
  const [dropTrack, setDropTrack] = useState<string | null>(null);
  const [hovered, setHovered] = useState<string | null>(null);
  const pointer = useRef<{ x: number; y: number } | null>(null);
  const pendingReveal = useRef<string | null>(null);
  const focusedPlanRequest = useRef<number | null>(null);
  const rows = useMemo(() => lanes(props.tracks, overview), [props.tracks, overview]);
  const height = totalHeight(rows), visibleHeight = Math.max(0, size.height - TOP_HEIGHT);
  const visible = useMemo(() => visibleLanes(rows, scroll, visibleHeight), [rows, scroll, visibleHeight]);
  const requestedTracks = useMemo(() => [...new Set(visible.map(row => row.track.id))], [visible]);
  useEffect(() => {
    if (!props.preview) props.onViewport({ tracks: requestedTracks, start: Math.max(0, Math.floor(props.view.start * 1000)), end: Math.ceil((props.view.start + props.view.duration) * 1000) });
  }, [props.preview, props.onViewport, requestedTracks, props.view.start, props.view.duration]);
  const selectedKey = props.selectedRepair ? `repair-${props.selectedRepair}` : `track-${props.selected}`;
  const navigation = useRef(selectedKey);
  useLayoutEffect(() => { navigation.current = selectedKey; }, [selectedKey]);
  const selected = rows.find(row => row.key === selectedKey);
  const mirror = selected && !visible.includes(selected) ? [...visible, selected] : visible;
  const layout = geometry(size.width);
  const branchRow = visible.find(row => !row.repairId && row.track.id === props.branchPreview?.trackId);
  const branchGroups = useMemo(() => props.branchPreview ? groupBranchPoints(props.branchPreview, props.view, size.width) : [], [props.branchPreview, props.view.start, props.view.duration, size.width]);
  const links = useMemo(() => overview ? [] : dependencyLinks(rows, size.width, props.view, props.selectedPlanItem), [rows, size.width, props.view, props.selectedPlanItem, overview]);
  const scrollId = useId();
  const marks = props.preview ? PREVIEW_MARKS : props.marks ?? NO_MARKS;
  const markGroups = useMemo(() => groupMarks(marks, props.view, size.width), [marks, props.view, size.width]);
  function rowAt(clientY: number) {
    const top = viewport.current?.getBoundingClientRect().top;
    if (top === undefined) return undefined;
    const position = clientY - top - TOP_HEIGHT + (scroller.current?.scrollTop ?? 0);
    return rows.find(row => row.top <= position && position < row.top + row.height);
  }
  function hoverAt(point: { x: number; y: number } | null) {
    const box = viewport.current?.getBoundingClientRect();
    setHovered(point && box && point.x >= box.left && point.x < box.right - 8
      && point.y >= box.top + TOP_HEIGHT && point.y < box.bottom ? rowAt(point.y)?.key ?? null : null);
  }
  function dropRow(clientY: number) {
    const row = rowAt(clientY);
    return row?.repairId || row?.track.planId || row?.track.reviewer || row?.track.ghost ? undefined : row;
  }
  function moveTo(value: number) {
    if (scroller.current) { scroller.current.scrollTop = value; setScroll(scroller.current.scrollTop); }
  }
  function density(next: boolean, row = visible[0]) { pendingReveal.current = row?.key ?? null; setOverview(next); }
  function choose(row: Lane) {
    navigation.current = row.key;
    if (row.repairId) props.onRepairSelect(row.track, row.repairId); else props.onSelect(row.track.id);
    moveTo(revealLane(scroll, visibleHeight, row));
  }
  function open(row: Lane) {
    if (overview) density(false, row);
    if(!row.repairId&&row.track.ghost){props.onRestoreAside(row.track.ghost.id);return;}
    if (row.track.planId) { props.onPlan(row.track.planId); return; }
    choose(row);
    if(row.track.reviewer) {const last=row.clips.at(-1);if(last)props.onClip(row.track,last,null);return;}
    if (!row.repairId) props.onOpenTrack(row.track.id);
  }
  useLayoutEffect(() => {
    if (selected) moveTo(revealLane(scroller.current?.scrollTop ?? 0, visibleHeight, selected));
  }, [selectedKey, visibleHeight]);
  useLayoutEffect(() => {
    const row = rows.find(row => row.track.planned.some(item => item.id === props.selectedPlanItem));
    if (row) moveTo(revealLane(scroller.current?.scrollTop ?? 0, visibleHeight, row));
  }, [props.selectedPlanItem]);
  useLayoutEffect(() => {
    const request = props.focusPlanItem;
    if (!request || focusedPlanRequest.current === request.nonce || overview) return;
    const row = rows.find(row => row.track.planned.some(item => item.id === request.id));
    if (!row) return;
    const next = revealLane(scroller.current?.scrollTop ?? 0, visibleHeight, row);
    if (scroller.current && Math.abs(next - scroller.current.scrollTop) > 1) {
      moveTo(next);
      return;
    }
    const item = viewport.current?.querySelector<HTMLButtonElement>(`[data-plan-item-id="${CSS.escape(request.id)}"] .planned-item-select`)
      ?? viewport.current?.querySelector<HTMLButtonElement>(`[data-plan-track-id="${CSS.escape(row.track.id)}"] .planned-more`);
    if (item) { item.focus({ preventScroll: true }); focusedPlanRequest.current = request.nonce; }
  }, [props.focusPlanItem, rows, scroll, visibleHeight, overview]);
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
    scene.current?.sync({ rows, visible, width: size.width, height: size.height, scroll, total: height, selected: selectedKey, selectedPlanItem: props.selectedPlanItem, hovered, overview, view: props.view, marks, branchPreview: props.branchPreview });
  }, [ready, rows, visible, size, scroll, height, selectedKey, props.selectedPlanItem, hovered, overview, props.view.start, props.view.duration, marks, props.branchPreview]);
  function planKey(event: React.KeyboardEvent<HTMLButtonElement>, planned: PlannedTurn) {
    if (event.nativeEvent.isComposing || !planned.planId) return;
    if (event.key === 'Delete' || event.key === 'Backspace') { event.preventDefault(); event.stopPropagation(); props.onCut(planned.id); return; }
    const mode = event.ctrlKey && event.key === 'Enter' || event.shiftKey && event.key === 'F10' ? 'menu' : event.altKey ? ({ ArrowLeft: 'earlier', ArrowRight: 'later', ArrowUp: 'up', ArrowDown: 'down' } as const)[event.key as 'ArrowLeft' | 'ArrowRight' | 'ArrowUp' | 'ArrowDown'] : null;
    if (!mode) return; event.preventDefault(); event.stopPropagation();
    const box = event.currentTarget.getBoundingClientRect(); props.onPlanControl(planned.planId, planned.id, mode, { left: box.left, top: box.bottom + 4 });
  }
  function overviewPosition(event: React.PointerEvent<HTMLDivElement>) {
    const box = event.currentTarget.getBoundingClientRect();
    moveTo((event.clientY - box.top) / Math.max(1, box.height) * height - visibleHeight / 2);
    props.onInteraction();
  }
  return <section ref={viewport} className="timeline" data-ready={ready} data-future-count={layout.futureCount} data-overview={overview} data-dependency-count={links.length} data-hovered-row={hovered ?? undefined} aria-label="Timeline"
    onPointerMove={event => {
      props.onInteraction();
      pointer.current = { x: event.clientX, y: event.clientY };
      hoverAt(pointer.current);
    }}
    onPointerLeave={() => { pointer.current = null; setHovered(null); }}
    onDragOver={event => {
      const row = props.canDropFiles && event.dataTransfer.types.includes('Files') ? dropRow(event.clientY) : undefined;
      setDropTrack(row?.track.id ?? null);
      if (row) { event.preventDefault(); event.dataTransfer.dropEffect = 'copy'; }
    }}
    onDragLeave={event => { if (!(event.relatedTarget instanceof Node) || !event.currentTarget.contains(event.relatedTarget)) setDropTrack(null); }}
    onDrop={event => {
      event.preventDefault(); setDropTrack(null);
      const row = dropRow(event.clientY);
      if (row && props.canDropFiles) props.onDropFiles(Array.from(event.dataTransfer.files), row.track.id);
    }}>
    {dropTrack && visible.filter(row => row.track.id === dropTrack && !row.repairId).map(row => <div key={row.key} className="lane-file-drop" style={{ top: TOP_HEIGHT + row.top - scroll, height: row.height }}>Drop to attach · to Worker {row.track.label}</div>)}
    <div className="canvas-host" ref={host} />
    {!!links.length && <ul className="sr-only" aria-label="Selected plan dependencies">{links.map(link => <li key={link.id}>{link.description}</li>)}</ul>}
    <button className="timeline-density" aria-pressed={overview} onClick={() => density(!overview)}>{overview ? 'Detail' : 'Overview'}</button>
    <div className="you-label">◆ <span>You</span></div>
    <InputMarks groups={markGroups} tracks={props.tracks} open={mark => {
      if(mark.commit){props.onPushCommit?.(mark.commit);return;}
      if (mark.inputId) { props.onInput(mark.inputId); return; }
      const track = props.tracks.find(track => track.id === mark.trackId);
      if (!track) return;
      if (mark.repairRunId) props.onRepairSelect(track, mark.repairRunId); else props.onSelect(track.id);
    }} />
    <div id={scrollId} className="timeline-scroll" ref={scroller} onScroll={event => { setScroll(event.currentTarget.scrollTop); hoverAt(pointer.current); props.onInteraction(); }}>
      <div className="timeline-content" style={{ height }}>
        {!overview && branchRow && <div className="branch-point-actions" role="group" aria-label={`Saved turns for ${branchRow.track.label}`}>
          {branchGroups.map(group => <button key={group.points.map(point => point.turnId).join(':')} style={{ left: group.x - 7, top: branchRow.top + 43 }}
            aria-label={group.points.length === 1 ? `Branch at end of ${branchRow.track.label}’s turn ${group.points[0].turnNumber}` : `${group.points.length} saved turns in ${branchRow.track.label} · choose a branch point`}
            aria-pressed={group.selected} title={group.points.length === 1 ? `End of turn ${group.points[0].turnNumber}` : `${group.points.length} saved turns`}
            onClick={() => props.onBranchPoint(branchRow.track.id, group.points.length === 1 ? group.points[0].timelineAt : null)} />)}
        </div>}
        <div className="lane-hit-areas" aria-hidden="true">{visible.map(row => <div key={row.key} className="lane-hit-area" style={{ top: row.top, height: row.height }}
          onClick={() => open(row)} />)}</div>
        <div role="grid" aria-label="Tracks" aria-rowcount={rows.length} aria-colcount={1} tabIndex={0} className="track-grid" style={{ height }} aria-activedescendant={selected ? selected.key : undefined}
          onKeyDown={event => {
            if (event.target !== event.currentTarget || !rows.length) return;
            let next = rows.findIndex(row => row.key === navigation.current);
            if (event.key === 'ArrowDown') next = Math.min(next + 1, rows.length - 1);
            else if (event.key === 'ArrowUp') next = Math.max(next - 1, 0);
            else if (event.key === 'Home') next = 0;
            else if (event.key === 'End') next = rows.length - 1;
            else if (event.key === 'Enter' && selected) { event.preventDefault(); open(selected); return; }
            else if (/^[1-9]$/.test(event.key)) next = rows.findIndex(row => !row.repairId && row.track === props.tracks[Math.min(Number(event.key) - 1, props.tracks.length - 1)]);
            else return;
            event.preventDefault(); props.onInteraction(); choose(rows[Math.max(0, next)]);
          }}>
          {mirror.map(row => { const result = row.repairId ? null : completionLabel(row.track); const status = !row.repairId&&row.track.ghost ? row.track.ghost.phase==='aside'?'held':'stopping' : !row.repairId && row.track.waitingForMemory ? 'queued · waiting for memory' : !row.repairId && row.track.queuePosition ? queueLabel(row.track.queuePosition) : result?.text ?? row.status; const warnings = row.clips.filter(clip => clip.warnings?.length).length; return <div role="row" id={row.key} key={row.key} aria-rowindex={row.index + 1} aria-selected={row.key === selectedKey} className={`${row.repairId ? 'repair-row' : 'track-row'}${row.track.ghost&&!row.repairId?' ghost-row':''}${overview ? ' strip-row' : ''}`} style={{ top: row.top, height: row.height }} onClick={() => open(row)}>
            <div role="gridcell" aria-label={`${row.track.label} · ${row.track.ghost&&!row.repairId?'set aside':row.repairId ? 'for the commit' : row.track.title} · ${status}${!row.repairId && row.track.integrationFailed ? ' · integration failed · needs you' : ''}${!row.repairId && row.track.branchContext ? ` · ${row.track.branchContext}` : ''}${!row.repairId && row.track.nextWaitsForAnswer ? ' · next waits for your answer' : ''}${warnings ? ` · ${warnings} step warnings` : ''}`}>
              {overview ? <span className="sr-only">{row.track.label} · {row.track.title} · {status}{row.track.nextWaitsForAnswer ? ' · next waits for your answer' : ''}</span> : row.track.ghost&&!row.repairId ? <><span className="track-letter">{row.track.label}</span><span className="ghost-name">Worker {row.track.label}</span></> : row.repairId ? <><span className="repair-branch">└</span><span>for the commit</span><span className={`track-status ${row.status}`}>{row.status === 'stopped' ? '■ stopped' : row.status === 'done' ? '✓' : row.status === 'waiting' ? '◇' : '◌'}</span>{row.canStop && <button aria-label={`Stop ${row.track.label} repair`} onClick={event => { event.stopPropagation(); props.onRepairStop(row.repairId!); }}>■</button>}</> : <>{!row.track.reviewer && <span className={`track-letter${row.track.planId ? ' planned-letter' : ''}`} style={{ color: `#${row.track.color.toString(16)}` }}>{row.track.label}</span>}<span className="track-name" title={row.track.title}>{row.track.reviewer ? row.track.label : row.track.title}<small>{row.track.folder}</small>{row.track.integrationFailed && <small className="track-integration-failure">integration failed · needs you</small>}{row.track.branchContext && <small>{row.track.branchContext}</small>}{result && <small className={`track-result ${result.tone}`}>{result.text}</small>}{row.track.waitingForMemory && <small>queued · waiting for memory</small>}{!row.track.waitingForMemory && row.track.queuePosition && <small>{queueLabel(row.track.queuePosition)}</small>}{row.track.nextWaitsForAnswer && <small className="track-question-wait">next waits for your answer</small>}</span><span className={`track-status ${result?.tone ?? row.status}`}>{result?.mark ?? (row.status === 'stopped' ? '■ stopped' : row.status === 'waiting' ? '◇' : '◌')}</span>{row.track.reviewer && !!row.track.reviewJobs?.length && <button aria-label="Stop reviews" onClick={event=>{event.stopPropagation();props.onReviewStop?.(row.track.reviewJobs!);}}>■</button>}{!row.track.reviewer && <TrackMenu track={row.track} disabled={props.targetDisabled || !!row.track.planId} sayTo={props.onSayTo} />}</>}
            </div>
          </div>; })}
        </div>
        {!overview && <div className="assumption-mark-actions" aria-label="Recorded assumptions">{visible.filter(row=>!row.track.ghost).flatMap(row => row.clips.flatMap(clip => (clip.assumptionTimes ?? []).map(at => {
          const x = LABEL_WIDTH + (at - props.view.start) / props.view.duration * layout.timeWidth;
          if (x < LABEL_WIDTH + 5 || x > size.width - layout.futureWidth - 5) return null;
          const description = `Open assumptions for ${row.track.label}:${clip.ordinal ?? clip.id}`;
          return <button key={`${clip.id}-${at}`} style={{ left: x - 9, top: row.top + (row.repairId ? -14 : 4) }} aria-label={description} title={description}
            onClick={() => { choose(row); props.onClip(row.track, clip, row.repairId); }} />;
        })))}</div>}
        {!overview && <div className="step-actions" aria-label="Recorded steps">{visible.flatMap(row => row.clips.map(clip => {
          const left = Math.max(LABEL_WIDTH, LABEL_WIDTH + (clip.start - props.view.start) / props.view.duration * layout.timeWidth);
          const end = Math.min(size.width - layout.futureWidth, LABEL_WIDTH + ((clip.running ? Math.max(clip.end, props.liveNow() ?? props.view.now) : clip.end) - props.view.start) / props.view.duration * layout.timeWidth);
          if (end < LABEL_WIDTH || left >= size.width - layout.futureWidth) return null;
          const warningText = clip.warnings?.map(warning => warning.text).join(' ') ?? '';
          return <button key={clip.id} data-warning={warningText ? 'true' : undefined} ref={clip.running&&!row.track.ghost ? element => { if (element) activeButtons.current.set(clip.id, { element, clip, width: -1 }); else activeButtons.current.delete(clip.id); } : undefined} style={{ left, top: row.top + (row.repairId||row.track.ghost ? 1 : 20) + (clip.reviewOffset ?? 0), height: row.repairId||row.track.ghost ? 24 : 35, width: Math.max(8, end - left) }} aria-label={`Open ${row.repairId ? 'repair ' : ''}step ${row.track.label}:${clip.ordinal ?? clip.id}: ${clip.label}${clip.assumptionCount ? `, ${clip.assumptionCount} assumptions` : ''}${warningText ? ` · Warning: ${warningText}` : ''}`} title={`${row.track.label}:${clip.ordinal ?? clip.id} · ${clip.label}${clip.approximate ? ' · approximate duration' : ''}${clip.effectCount ? ` · ${clip.effectCount} effects outside Git` : ''}${warningText ? ` · ${warningText}` : ''}`} onClick={() => { choose(row); props.onClip(row.track, clip, row.repairId); }}>{clip.ordinal ?? '•'}</button>;
        }))}</div>}
        {!overview&&visible.filter(row=>!row.repairId&&row.track.ghost).map(row=><div key={`ghost-${row.key}`} style={{position:'absolute',top:row.top,left:LABEL_WIDTH,right:0,height:row.height,pointerEvents:'none',zIndex:4}}><GhostControls track={row.track} hovered={hovered===row.key} disabled={props.targetDisabled} open={props.onRestoreAside}/></div>)}
        {!overview && <div className="future-actions" style={{ left: size.width - layout.futureWidth, width: layout.futureWidth }}>{visible.filter(row => !row.repairId&&!row.track.ghost).map(row => <div className="future-row" data-plan-track-id={row.track.id} key={row.key} style={{ position: 'absolute', top: row.top, height: row.height, width: layout.futureWidth }}>
          {row.track.planned.slice(0, layout.futureCount).map((planned, index) => planned.reviewFix
            ? <div key={planned.id} data-review-fix={planned.id} className="planned-item-hit" style={{ left: 8 + index * (layout.tileWidth + 8), width: layout.tileWidth }}>
                <button className="planned-item-select" aria-haspopup="dialog" aria-label={`Open ${planned.label} · ${planned.after}`} onClick={() => props.onInput(planned.id)}><span className="sr-only">Review {planned.label}</span></button>
                <button className="planned-item-cut" aria-label={`Cut ${planned.label} from ${row.track.label}`} onClick={() => props.onCut(planned.id)}>×</button>
              </div>
            : planned.planId
            ? <div key={planned.id} data-plan-item-id={planned.id} data-auto-plan={planned.planId} className="planned-item-hit" style={{ left: 8 + index * (layout.tileWidth + 8), width: layout.tileWidth }}>
                <button className="planned-item-select" aria-haspopup={planned.blocked ? 'menu' : 'dialog'} aria-pressed={planned.id === props.selectedPlanItem} title={`${planned.label}\n${planned.after}`} aria-label={`${planned.label} · ${planned.after}`} onClick={event => { const box = event.currentTarget.getBoundingClientRect(); props.onPlan(planned.planId!, planned.id, { left: box.left, top: box.bottom + 4 }); }} onContextMenu={event => { event.preventDefault(); props.onPlanControl(planned.planId!, planned.id, 'menu', { left: event.clientX, top: event.clientY }); }} onKeyDown={event => planKey(event, planned)}><span className="sr-only">Review {planned.label}</span></button>
                <button className="planned-item-cut" aria-label={`Cut ${planned.label} from ${row.track.label}`} onClick={() => props.onCut(planned.id)}>×</button>
              </div>
            : <button className="cut-turn" key={planned.id} style={{ marginLeft: layout.tileWidth - (index ? 24 : 20) }} aria-label={`Cut ${planned.label} from ${row.track.label}`} title={`Cut: ${planned.label} · ${planned.after}`} onClick={() => props.onCut(planned.id)} onKeyDown={event => { if (event.key === 'Delete' || event.key === 'Backspace') { event.preventDefault(); props.onCut(planned.id); } }}>×</button>)}
          {row.track.planned.length > layout.futureCount && layout.futureCount > 0 && <PlannedOverflow items={row.track.planned.slice(layout.futureCount)} track={row.track.label} selected={props.selectedPlanItem} control={planKey}
            openItem={item => { if (item.planId) props.onPlan(item.planId, item.id); else props.onInput(item.id); }} />}
        </div>)}</div>}
      </div>
    </div>
    <div className="track-overview" role="scrollbar" tabIndex={0} aria-label="All tracks" aria-controls={scrollId} aria-orientation="vertical" aria-valuemin={0} aria-valuemax={Math.max(0, height - visibleHeight)} aria-valuenow={Math.round(scroll)} onPointerDown={event => { event.currentTarget.setPointerCapture(event.pointerId); overviewPosition(event); }} onPointerMove={event => { if (event.currentTarget.hasPointerCapture(event.pointerId)) overviewPosition(event); }} onPointerUp={event => event.currentTarget.releasePointerCapture(event.pointerId)} onKeyDown={event => {
      const delta = event.key === 'ArrowDown' ? 44 : event.key === 'ArrowUp' ? -44 : event.key === 'PageDown' ? visibleHeight : event.key === 'PageUp' ? -visibleHeight : 0;
      if (delta || event.key === 'Home' || event.key === 'End') { event.preventDefault(); props.onInteraction(); moveTo(event.key === 'Home' ? 0 : event.key === 'End' ? height : scroll + delta); }
    }} />
    {error && <p role="alert">Timeline could not start: {error}</p>}
    {props.branchError && <p className="branch-points-error" role="status">Saved branch points are unavailable. Open branch preparation to retry.</p>}
  </section>;
}
