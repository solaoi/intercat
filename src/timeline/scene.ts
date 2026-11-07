import { Application, CanvasSource, Container, Graphics, NineSliceSprite, Sprite, Text, Texture } from 'pixi.js';
import type { TimelineMark } from '../bridge';
import { groupMarks, markLabel } from './inputMarkLayout';
import { formatTime, geometry, LABEL_WIDTH, TOP_HEIGHT, type TimelineView } from './model';
import type { Lane } from './viewport';

interface SceneState {
  rows: Lane[]; visible: Lane[]; width: number; height: number; scroll: number; total: number;
  selected: string; hovered: string | null; overview: boolean; view: TimelineView; marks: TimelineMark[];
}
interface ActiveBar { sprite: NineSliceSprite; start: number; end: number }
interface RowScene { container: Container; background: Graphics; content: Container; row: Lane; signature: string; active: ActiveBar[]; texture?: Texture }
function label(parent: Container, value: string, x: number, y: number, color: number, size = 10) {
  const text = new Text({ text: value, style: { fontFamily: 'Inter', fontSize: size, fill: color } });
  text.position.set(x, y); parent.addChild(text); return text;
}
function segment(graphic: Graphics, x: number, y: number, toX: number, toY: number, color: number, alpha = 1) {
  graphic.moveTo(x, y).lineTo(toX, toY).stroke({ color, width: 1, alpha });
}

/** Scene objects cover only the viewport. Their geometry survives clock ticks. */
export class TimelineScene {
  private state?: SceneState;
  private grid = new Graphics();
  private headings = new Container();
  private marks = new Container();
  private layer = new Container();
  private playhead = new Container();
  private shade = new Sprite(Texture.WHITE);
  private head = new Sprite(Texture.WHITE);
  private timeWidth = 1;
  private right = 1;
  private overview = new Graphics();
  private position = new Graphics();
  private barTextures = new Map<boolean, Texture>();
  private rowScenes = new Map<string, RowScene>();
  private gridKey = '';
  private overviewKey = '';
  private previousMarks?: TimelineMark[];
  private markTexture?: Texture;
  constructor(private app: Application) {
    app.stage.eventMode = 'none';
    this.shade.alpha = 0.018; this.head.alpha = 0.75;
    this.playhead.addChild(this.shade, this.head);
    app.stage.addChild(this.grid, this.layer, this.headings, this.marks, this.playhead, this.overview, this.position);
  }
  destroy() {
    this.markTexture?.destroy(true);
    for (const texture of this.barTextures.values()) texture.destroy(true);
    for (const scene of this.rowScenes.values()) scene.texture?.destroy(true);
  }
  private barTexture(thinking: boolean) {
    let texture = this.barTextures.get(thinking);
    if (!texture) {
      const canvas = document.createElement('canvas'); canvas.width = canvas.height = 3;
      const context = canvas.getContext('2d')!;
      context.fillStyle = 'rgba(255,255,255,.6)'; context.fillRect(0, 0, 3, 3);
      context.clearRect(1, 1, 1, 1);
      context.fillStyle = `rgba(255,255,255,${thinking ? .10 : .22})`; context.fillRect(1, 1, 1, 1);
      texture = new Texture({ source: new CanvasSource({ resource: canvas, scaleMode: 'nearest' }) });
      this.barTextures.set(thinking, texture);
    }
    return texture;
  }
  sync(state: SceneState) {
    const previous = this.state;
    this.state = state;
    if (this.app.screen.width !== state.width || this.app.screen.height !== state.height || this.app.renderer.resolution !== window.devicePixelRatio) this.app.renderer.resize(state.width, state.height, window.devicePixelRatio);
    const { timeWidth, futureWidth } = geometry(state.width);
    const right = state.width - futureWidth;
    this.timeWidth = timeWidth; this.right = right;
    const key = `${state.width}/${state.height}/${this.app.renderer.resolution}/${state.view.start}/${state.view.duration}`;
    const gridChanged = key !== this.gridKey;
    if (gridChanged) {
      this.gridKey = key;
      this.grid.clear(); this.headings.removeChildren().forEach(child => child.destroy());
      segment(this.grid, right, 27, right, state.height, 0x25272f);
      segment(this.grid, LABEL_WIDTH, 67, right, 67, 0x272932);
      let timeLabelEnd = LABEL_WIDTH - 12;
      for (let t = Math.ceil(state.view.start / 120) * 120; t < state.view.start + state.view.duration; t += 120) {
        const x = LABEL_WIDTH + (t - state.view.start) / state.view.duration * timeWidth;
        segment(this.grid, x, 28, x, state.height, 0x17191f);
        if (t + 120 < state.view.start + state.view.duration) {
          const tick = label(this.headings, formatTime(t).slice(0, 5), x + 5, 6, 0x82858f);
          if (tick.x < timeLabelEnd + 12 || tick.x + tick.width > right - 8) tick.destroy();
          else timeLabelEnd = tick.x + tick.width;
        }
      }
      label(this.headings, 'WAITING TO RUN', right + 10, 7, 0x747984, 9);
    }
    if (gridChanged || this.previousMarks !== state.marks || previous?.rows !== state.rows || previous?.scroll !== state.scroll) {
      this.previousMarks = state.marks;
      this.markTexture?.destroy(true); this.markTexture = undefined;
      this.marks.removeChildren().forEach(child => child.destroy());
      const drawing = new Container();
      const tracks = state.rows.map(row => row.track);
      let labelEnd = LABEL_WIDTH;
      for (const group of groupMarks(state.marks, state.view, state.width)) {
        const x = group.x;
        const mark = group.marks[0];
        const color = mark.operation === null ? 0x9299a6 : 0xd9a24a;
        const glyph = new Graphics();
        if (mark.attached) glyph.poly([x - 4, 61, x + 2, 61, x + 5, 64, x + 5, 73, x - 4, 73]).stroke({ color, width: 1.5 });
        else if (mark.operation === 'park') glyph.poly([x, 62, x + 5, 67, x, 72, x - 5, 67]).stroke({ color, width: 1.5 });
        else glyph.poly([x, 62, x + 5, 67, x, 72, x - 5, 67]).fill(color);
        drawing.addChild(glyph);
        const row = state.visible.find(row => row.track.id === mark.trackId && row.repairId === mark.repairRunId);
        if (group.marks.length === 1 && row) {
          const end = TOP_HEIGHT + row.top - state.scroll + 7;
          if (end >= TOP_HEIGHT) segment(glyph, x, 96, x, end, color, 0.3);
        }
        if (x - 8 < labelEnd) continue;
        const text = new Text({ text: group.marks.length > 1 ? `${group.marks.length} requests` : Array.from(mark.text).slice(0, 25).join(''), style: { fontFamily: 'Inter', fontSize: 10, fill: 0xb6b8c1 } });
        const target = new Text({ text: group.marks.length > 1 ? 'Choose a request' : markLabel(mark, tracks), style: { fontFamily: 'Inter', fontSize: 10, fontStyle: mark.operation === 'park' ? 'italic' : 'normal', fill: 0x9299a6 } });
        const width = Math.max(text.width, target.width);
        if (x - 8 + width > right - 5) { text.destroy(); target.destroy(); continue; }
        text.position.set(x - 8, 37); target.position.set(x - 8, 82);
        drawing.addChild(text, target); labelEnd = x - 8 + width + 12;
      }
      const bounds = drawing.getLocalBounds();
      if (bounds.width > 0 && bounds.height > 0) {
        const x = bounds.minX, y = bounds.minY;
        this.markTexture = this.app.renderer.generateTexture({ target: drawing, resolution: this.app.renderer.resolution, antialias: true });
        const image = new Sprite(this.markTexture); image.position.set(x, y); this.marks.addChild(image);
      }
      drawing.destroy({ children: true, context: true });
    }
    const keep = new Set(state.visible.map(row => row.key));
    for (const [key, scene] of this.rowScenes) if (!keep.has(key)) { scene.texture?.destroy(true); scene.container.destroy({ children: true, context: true }); this.rowScenes.delete(key); }
    for (const row of state.visible) {
      let scene = this.rowScenes.get(row.key);
      if (!scene) {
        const container = new Container(), background = new Graphics(), content = new Container();
        container.addChild(background, content); this.layer.addChild(container);
        scene = { container, background, content, row, signature: '', active: [] }; this.rowScenes.set(row.key, scene);
      }
      scene.container.y = TOP_HEIGHT + row.top - state.scroll;
      const signature = `${key}/${row.height}/${row.track.color}/${row.status}/${row.track.planned.map(item => item.id + item.label + item.after).join('/')}`;
      if (scene.row.clips !== row.clips || scene.signature !== signature) {
        scene.row = row; scene.signature = signature; this.drawRow(scene);
      }
      scene.background.clear();
      if (row.key === state.selected || row.key === state.hovered) {
        const alpha = row.key === state.hovered ? row.key === state.selected ? 0.11 : 0.045 : 0.07;
        scene.background.rect(0, 0, state.width - 8, row.height - 1).fill({ color: row.track.color, alpha });
      }
      segment(scene.background, 0, 0, state.width - 8, 0, 0x202229);
    }
    // Clip scrollable rows without allocating an off-screen render texture.
    this.layer.mask = this.app.stage.getChildByLabel('rows-mask') as Graphics | null;
    let mask = this.layer.mask as Graphics | null;
    if (!mask) { mask = new Graphics(); mask.label = 'rows-mask'; this.app.stage.addChild(mask); this.layer.mask = mask; }
    mask.clear().rect(0, TOP_HEIGHT, state.width - 8, Math.max(0, state.height - TOP_HEIGHT)).fill(0xffffff);
    const overviewKey = `${state.width}/${state.height}/${state.total}/${state.rows.map(row => `${row.track.color}:${row.status}:${row.height}`).join('/')}`;
    if (overviewKey !== this.overviewKey) {
      this.overviewKey = overviewKey; this.overview.clear();
      const height = Math.max(1, state.height - TOP_HEIGHT);
      this.overview.rect(state.width - 8, TOP_HEIGHT, 8, height).fill(0x16191f);
      for (const row of state.rows) {
        const y = TOP_HEIGHT + row.top / Math.max(1, state.total) * height;
        const h = Math.max(1, row.height / Math.max(1, state.total) * height);
        this.overview.rect(state.width - 7, y, row.status === 'waiting' ? 6 : 4, h).fill({ color: row.status === 'waiting' ? 0xd9a24a : row.status === 'stopped' ? 0x8c95a8 : row.track.color, alpha: 0.7 });
      }
    }
    const overviewHeight = Math.max(1, state.height - TOP_HEIGHT);
    this.position.clear().rect(state.width - 8, TOP_HEIGHT + state.scroll / Math.max(1, state.total) * overviewHeight, 7, Math.max(4, Math.min(overviewHeight, overviewHeight * overviewHeight / Math.max(overviewHeight, state.total)))).stroke({ color: 0xe0e2eb, width: 1 });
    this.tick(state.view.now);
  }
  private drawRow(scene: RowScene) {
    const state = this.state!, row = scene.row;
    const { timeWidth, futureWidth, futureCount, tileWidth } = geometry(state.width);
    const right = state.width - futureWidth;
    const toX = (t: number) => LABEL_WIDTH + (t - state.view.start) / state.view.duration * timeWidth;
    scene.texture?.destroy(true); scene.texture = undefined;
    scene.content.removeChildren().forEach(child => child.destroy({ children: true, context: true })); scene.active = [];
    const statics = new Container(), graphics = new Graphics();
    statics.addChild(graphics);
    const compact = row.repairId !== null;
    const top = state.overview ? 2 : compact ? 4 : 22;
    const height = state.overview ? 8 : compact ? 18 : 31;
    if (!state.overview) for (const clip of row.clips) {
      for (const at of clip.assumptionTimes ?? []) {
        const x = toX(at), y = top - 9;
        if (x >= LABEL_WIDTH + 5 && x <= right - 5) {
          graphics.poly([x, y - 5, x + 5, y, x, y + 5, x - 5, y]).fill(0xd9a24a);
        }
      }
    }
    for (const clip of row.clips) {
      const left = Math.max(LABEL_WIDTH, toX(clip.start)), end = Math.min(right, toX(clip.running ? Math.max(clip.end, state.view.now) : clip.end));
      if (end < LABEL_WIDTH || left >= right) continue;
      const width = Math.max(3, end - left - 2);
      if (clip.running) {
        const sprite = new NineSliceSprite({ texture: this.barTexture(clip.kind === 'think'), leftWidth: 1, rightWidth: 1, topHeight: 1, bottomHeight: 1, width, height });
        sprite.tint = row.track.color; sprite.position.set(left, top);
        scene.content.addChild(sprite);
        scene.active.push({ sprite, start: clip.start, end: clip.end });
      } else {
        graphics.roundRect(left, top, width, height, state.overview ? 0 : 3).fill({ color: row.track.color, alpha: clip.kind === 'think' ? 0.10 : 0.22 }).stroke({ color: row.track.color, alpha: 0.55, width: 1 });
      }
      if (clip.warnings?.length) {
        const size = Math.min(state.overview ? 4 : 6, width);
        graphics.poly([left + width - size, top, left + width, top, left + width, top + size]).fill(0xd9a24a);
      }
      if (state.overview) continue;
      if (clip.kind === 'edit') segment(graphics, left + 2, top + 1, left + Math.min(12, width - 2), top + 1, 0xd9a24a);
      const generation = clip.generationEnd == null ? (clip.kind === 'think' ? width : 0) : Math.min(width, Math.max(0, toX(clip.generationEnd) - left));
      for (let x = 2; x < generation - 2; x += 8) segment(graphics, left + x, top + height - 3, left + Math.min(x + 16, generation - 2), top + 3, row.track.color, 0.15);
      (clip.toolTicks ?? []).forEach((tool, i, all) => { const x = left + Math.max(2, generation + (width - generation) * i / all.length); segment(graphics, x, top + height - 7, x, top + height - 2, tool.status === 'inProgress' ? 0xd9a24a : row.track.color, 0.8); });
      if (clip.effectCount) label(statics, '⇗', left + Math.max(1, width - 13), top - 12, 0xd9a24a, 11);
      if (width > 25) label(statics, clip.label.slice(0, Math.max(0, Math.floor((width - 20) / 5.5))) + (clip.label.length * 5.5 > width - 12 ? '…' : ''), left + 6, top + 3, row.track.color);
    }
    if (!row.repairId && !state.overview) row.track.planned.slice(0, futureCount).forEach((planned, col) => {
      const x = right + 8 + col * (tileWidth + 8);
      for (let p = 0; p < tileWidth; p += 8) { segment(graphics, x + p, 15, x + Math.min(p + 4, tileWidth), 15, row.track.color, 0.6); segment(graphics, x + p, 60, x + Math.min(p + 4, tileWidth), 60, row.track.color, 0.6); }
      label(statics, planned.label.slice(0, 23), x + 9, 23, row.track.color);
      label(statics, planned.after.slice(0, 23), x + 9, 40, 0x747984, 9);
    });
    const bounds = statics.getLocalBounds();
    if (bounds.width > 0 && bounds.height > 0) {
      const x = bounds.minX, y = bounds.minY;
      scene.texture = this.app.renderer.generateTexture({ target: statics, resolution: this.app.renderer.resolution, antialias: true });
      const image = new Sprite(scene.texture); image.position.set(x, y); scene.content.addChildAt(image, 0);
    }
    // These immutable shapes are now pixels. Keep neither their command arrays
    // nor their GPU contexts alive while the running bars continue to change.
    statics.destroy({ children: true, context: true });
  }
  tick(now: number) {
    const state = this.state; if (!state) return;
    const right = this.right;
    const toX = (t: number) => Math.min(right - 1, LABEL_WIDTH + (t - state.view.start) / state.view.duration * this.timeWidth);
    const x = Math.max(LABEL_WIDTH, toX(now));
    this.shade.position.set(x, 27); this.shade.width = Math.max(0, right - x); this.shade.height = Math.max(0, state.height - 27);
    this.head.position.set(x, 28); this.head.width = 1; this.head.height = Math.max(0, state.height - 28);
    for (const scene of this.rowScenes.values()) for (const bar of scene.active) {
      const left = Math.max(LABEL_WIDTH, toX(bar.start)), end = toX(Math.max(bar.end, now));
      // A subpixel width change rebuilds the nine-slice mesh even when it
      // cannot add a displayed pixel. Keep its edge on the renderer's grid.
      const resolution = this.app.renderer.resolution;
      const width = Math.max(3, Math.round((end - left - 2) * resolution) / resolution);
      bar.sprite.x = left;
      if (bar.sprite.width !== width) bar.sprite.width = width;
    }
  }
}
