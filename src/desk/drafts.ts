import { useEffect, useReducer, useRef } from 'react';
import type { ApprovalResponse, Attachment, EditAction, InputTarget, ReportQuestion, SecretOwner, Utterance, RephraseSubmission } from '../bridge';
import type { CommitContext } from '../commit/context';
import type { RepairFollowUp, RepairRequest } from '../commit/repair';

export type EditLease = NonNullable<Utterance['editHold']>;
export interface EditContext { kind: 'edit'; inputId: string; originalText: string; trackId: string | null; lease: EditLease | null; state: 'editing' | 'sent' }
export interface PasteDraft { id: string; role: 'request' | 'reference'; after: string; words: number; attachment: Attachment | null }
export interface ApprovalDraftContext { kind: 'approval'; owner: SecretOwner; trackId: string; sessionId: string; approvalId: string; requirement: unknown; mode: 'feedback' | 'restart'; label: string }
export interface ApprovalSubmission { opId: string; owner: SecretOwner; trackId: string; sessionId: string; approvalId: string; requirement: unknown; response: Extract<ApprovalResponse, { action: 'reply' }> }
export interface RephraseContext { kind: 'rephrase'; inputId: string; revision: number; touched: boolean; seeded: boolean }
export type DraftContext = RephraseContext | ApprovalDraftContext | EditContext | { kind: 'work' } | { kind: 'reply'; question: ReportQuestion } | { kind: 'repair'; id: string } | { kind: 'commit'; context: CommitContext };
export interface WorkSubmission { opId: string; text: string; attachmentIds: string[]; target: InputTarget; selectedTrackId: string | null; sources?: { attachmentId: string; role: 'request' | 'reference'; after: string }[]; replyTo?: { id: string; revision: number } }
export type DraftSubmission = { kind: 'rephrase'; request: RephraseSubmission } | { kind: 'approval'; request: ApprovalSubmission } | { kind: 'edit'; request: Extract<EditAction, { kind: 'end' }> } | { kind: 'work'; request: WorkSubmission } | { kind: 'repair'; request: RepairFollowUp } | { kind: 'commit'; method: 'start'; request: RepairRequest } | { kind: 'commit'; method: 'followup'; request: RepairFollowUp };
export interface DraftEntry { text: string; sources?: PasteDraft[]; target: InputTarget; attachments: Attachment[]; context: DraftContext; pending: DraftSubmission | null }
export interface DraftBook { active: string; stack: string[]; entries: Record<string, DraftEntry> }
export interface SavedDrafts { revision: number; opId: string | null; parentOpId: string | null; book: DraftBook }
export interface DraftSave { session: string; revision: number; opId: string; parentOpId: string | null; book: DraftBook }
export interface DraftSaveResult extends SavedDrafts { status: 'saved' | 'conflict' }
const emptyEntry = (context: DraftContext): DraftEntry => ({ text: '', target: { mode: 'none' }, attachments: [], context, pending: null });
const emptyBook = (): DraftBook => ({ active: 'work', stack: [], entries: { work: emptyEntry({ kind: 'work' }) } });
export function approvalDraftId(context: ApprovalDraftContext): string {
  return JSON.stringify(['approval', context.owner, context.trackId, context.sessionId, context.approvalId, context.requirement, context.mode]);
}
function key(context: DraftContext): string {
  switch (context.kind) {
    case 'approval': return approvalDraftId(context);
    case 'rephrase': return `rephrase:${context.inputId}`;
    case 'work': return 'work';
    case 'edit': return `edit:${context.inputId}`;
    case 'reply': return `reply:${context.question.id}:${context.question.revision}`;
    case 'repair': return `repair:${context.id}`;
    case 'commit': return JSON.stringify(['commit', context.context.jobId, context.context.track, context.context.selection]);
  }
}

class DraftSession {
  book = emptyBook();
  ready = false;
  issue: string | null = null;
  conflict = false;
  private revision = 0;
  private parentOpId: string | null = null;
  private baselineOpId: string | null = null;
  private version = 0;
  private savedVersion = 0;
  private operations = new Map<number, DraftSave>();
  private running = new Map<number, Promise<void>>();
  constructor(readonly session: string, private notify: () => void) {}
  get entry() { return this.book.entries[this.book.active]; }
  get unsaved() { return this.savedVersion < this.version; }

  async load() {
    try {
      const saved = await window.intercat.readDrafts(this.session);
      this.book = saved.book; this.revision = saved.revision;
      this.parentOpId = saved.opId; this.baselineOpId = saved.opId;
      this.ready = true; this.issue = null;
    } catch { this.issue = 'Saved drafts could not be loaded. Retry before writing.'; }
    this.notify();
  }
  change(edit: (book: DraftBook, entry: DraftEntry) => void) {
    if (!this.ready) return;
    const book = structuredClone(this.book);
    edit(book, book.entries[book.active]);
    this.book = book; this.version++;
    const request = { session: this.session, revision: this.revision, parentOpId: this.parentOpId, opId: crypto.randomUUID(), book: structuredClone(book) };
    this.revision++; this.parentOpId = request.opId;
    this.operations.set(this.version, request);
    void this.run(this.version, request);
    this.notify();
  }
  private run(version: number, request: DraftSave): Promise<void> {
    const existing = this.running.get(version);
    if (existing) return existing;
    const task = this.persist(version, request);
    this.running.set(version, task);
    void task.finally(() => { this.running.delete(version); });
    return task;
  }
  private async persist(version: number, request: DraftSave) {
    try {
      const response = await window.intercat.writeDrafts(request);
      if (version <= this.savedVersion) return;
      if (response.status === 'saved') this.savedVersion = Math.max(this.savedVersion, version);
      else {
        const known = [...this.operations].find(([, operation]) => operation.opId === response.opId);
        if (known && known[0] >= version) this.savedVersion = Math.max(this.savedVersion, known[0]);
        else if (response.opId !== this.baselineOpId && !known) this.conflict = true;
        if (this.savedVersion < this.version) this.issue = this.conflict ? 'This session’s drafts changed in another window.' : 'Drafts could not be saved. Your text is still here.';
      }
      if (this.savedVersion >= version) {
        this.baselineOpId = response.opId;
        for (const number of this.operations.keys()) if (number <= this.savedVersion) this.operations.delete(number);
      }
    } catch {
      if (this.savedVersion < this.version) this.issue = 'Drafts could not be saved. Your text is still here.';
    }
    if (this.savedVersion >= this.version) this.issue = null;
    this.notify();
  }
  async flush(): Promise<void> {
    await Promise.all([...this.running.values()]);
    if (this.conflict) throw new Error('Resolve the draft from the other window first.');
    while (this.savedVersion < this.version) {
      const entry = [...this.operations].find(([version]) => version > this.savedVersion);
      if (!entry) throw new Error('Draft save is unavailable.');
      const before = this.savedVersion;
      await this.run(...entry);
      if (this.savedVersion <= before) throw new Error(this.issue ?? 'Drafts could not be saved.');
    }
  }
  async resolve(keepLocal: boolean) {
    if (!this.ready) return;
    this.ready = false; this.notify();
    try {
      await Promise.all([...this.running.values()]);
      const saved = keepLocal ? await window.intercat.readDrafts(this.session) : await window.intercat.discardDrafts(this.session);
      if (!keepLocal) this.book = saved.book;
      this.revision = saved.revision; this.parentOpId = saved.opId; this.baselineOpId = saved.opId;
      this.operations.clear(); this.savedVersion = this.version;
      this.conflict = false; this.issue = null; this.ready = true;
      if (keepLocal) { this.change(() => {}); await this.flush(); }
    } finally { this.ready = true; this.notify(); }
  }
  setText(text: string) { this.change((_book, entry) => { entry.text = text; if (!text && !entry.sources?.length) entry.target = { mode: 'none' }; }); }
  insertPaste(id: string, words: number, segment: string | null, before: string, start: number, end: number, draftId: string) {
    const entry = this.book.entries[draftId];
    const index = segment ? entry?.sources?.findIndex(source => source.id === segment) ?? -1 : -1;
    const current = segment ? entry?.sources?.[index]?.after : entry?.text;
    if (current !== before) throw new Error('The draft changed while saving the paste. Paste it again.');
    this.change(book => {
      const entry = book.entries[draftId];
      const after = before.slice(end);
      if (segment) entry.sources![index].after = before.slice(0, start);
      else entry.text = before.slice(0, start);
      const typed = entry.text + (entry.sources ?? []).map(source => source.after).join('') + after;
      const source: PasteDraft = { id, words, role: typed.trim() ? 'reference' : 'request', after, attachment: null };
      entry.sources ??= [];
      entry.sources.splice(index + 1, 0, source);
    });
  }
  completePaste(id: string, attachment: Attachment, draftId: string) {
    const source = this.book.entries[draftId]?.sources?.find(source => source.id === id);
    if (!source || source.attachment) return;
    this.change(book => { book.entries[draftId].sources!.find(source => source.id === id)!.attachment = attachment; });
  }
  setPaste(id: string, update: Partial<Pick<PasteDraft, 'role' | 'after'>>) {
    this.change((_book, entry) => { const source = entry.sources?.find(source => source.id === id); if (source) Object.assign(source, update); });
  }
  removePaste(id: string) {
    this.change((_book, entry) => {
      const index = entry.sources?.findIndex(source => source.id === id) ?? -1;
      if (index < 0) return;
      const source = entry.sources!.splice(index, 1)[0];
      if (index === 0) entry.text += source.after;
      else entry.sources![index - 1].after += source.after;
      if (!entry.sources!.length) delete entry.sources;
      if (!entry.text && !entry.sources?.length) entry.target = { mode: 'none' };
    });
  }
  setTarget(target: InputTarget) { this.change((_book, entry) => { entry.target = target; }); }
  setAttachments(value: Attachment[] | ((files: Attachment[]) => Attachment[]), id = this.book.active) {
    this.change(book => { const entry = book.entries[id]; if (entry) entry.attachments = typeof value === 'function' ? value(entry.attachments) : value; });
  }
  open(context: DraftContext, carry = false) {
    this.change((book, entry) => {
      const id = key(context);
      book.entries[id] ??= { ...emptyEntry(context), text: carry ? entry.text : '' };
      book.entries[id].context = context;
      if (id === book.active) return;
      book.stack = [...book.stack.filter(previous => previous !== id && previous !== book.active), book.active];
      book.active = id;
    });
  }
  ensureRephrase(word: Utterance) {
    const id = `rephrase:${word.id}`;
    if (!this.ready) return id;
    const existing = this.book.entries[id];
    const proposal = word.rephrase?.proposal;
    if (!existing) {
      this.change(book => { book.entries[id] = { ...emptyEntry({ kind: 'rephrase', inputId: word.id, revision: word.revision, touched: false, seeded: !!proposal }), text: proposal ?? '' }; });
    } else if (existing.context.kind === 'rephrase' && existing.context.revision !== word.revision) {
      this.change(book => {
        const entry = book.entries[id];
        const touched = (entry.context as RephraseContext).touched;
        entry.context = { kind: 'rephrase', inputId: word.id, revision: word.revision, touched, seeded: !!proposal };
        entry.pending = null;
        if (!touched) entry.text = proposal ?? '';
      });
    } else if (proposal && existing.context.kind === 'rephrase' && !existing.context.touched && !existing.context.seeded && !existing.pending) {
      this.change(book => { const entry = book.entries[id]; entry.text = proposal; (entry.context as RephraseContext).seeded = true; });
    }
    return id;
  }
  setRephraseText(id: string, text: string) {
    this.change(book => { const entry = book.entries[id]; if (entry?.context.kind === 'rephrase' && !entry.pending) { entry.text = text; entry.context.touched = true; } });
  }
  openEdit(word: Utterance, lease: EditLease | null) {
    const context: EditContext = { kind: 'edit', inputId: word.id, originalText: word.text, trackId: word.decision?.trackId ?? null, lease, state: lease ? 'editing' : 'sent' };
    this.change((book, entry) => {
      const id = key(context);
      const sources = word.sources?.map(source => ({ id: source.attachmentId, role: source.role, after: source.after, words: 0, attachment: word.attachments.find(attachment => attachment.id === source.attachmentId) ?? null }));
      book.entries[id] ??= { ...emptyEntry(context), text: word.sourceText ?? word.text, target: word.target, attachments: word.attachments.filter(attachment => !sources?.some(source => source.id === attachment.id)), ...(sources?.length ? { sources } : {}) };
      if (!book.entries[id].pending) book.entries[id].context = context;
      if (id !== book.active) {
        book.stack = [...book.stack.filter(previous => previous !== id && previous !== book.active), book.active];
        book.active = id;
      }
    });
  }
  updateEdit(id: string, context: EditContext) {
    const entry = this.book.entries[id];
    if (!entry || entry.context.kind !== 'edit' || entry.pending || JSON.stringify(entry.context) === JSON.stringify(context)) return;
    this.change(book => { book.entries[id].context = context; });
  }
  remove(id: string) {
    this.change(book => {
      delete book.entries[id];
      book.stack = book.stack.filter(previous => previous !== id);
      if (book.active === id) book.active = book.stack.pop() ?? 'work';
    });
  }
  replaceContext(context: DraftContext) { this.change((_book, entry) => { entry.context = context; }); }
  resume(id: string) {
    this.change(book => {
      if (!book.entries[id]) return;
      book.stack = [...book.stack.filter(previous => previous !== id && previous !== book.active), book.active];
      book.active = id;
    });
  }
  back() { this.change(book => { book.active = book.stack.pop() ?? 'work'; }); }
  finish() {
    this.change((book, entry) => {
      const id = book.active;
      if (entry.context.kind === 'edit') delete book.entries[id];
      else book.entries[id] = emptyEntry(entry.context);
      book.stack = book.stack.filter(previous => previous !== id);
      book.active = book.stack.pop() ?? 'work';
    });
  }
  clearPending() { this.change((_book, entry) => { entry.pending = null; }); }
  async prepare(pending: DraftSubmission, id = this.book.active): Promise<DraftSubmission> {
    if (!this.book.entries[id].pending) this.change(book => { book.entries[id].pending = pending; });
    await this.flush();
    return this.book.entries[id].pending!;
  }
}

export function useDrafts(session: string | null) {
  const [, notify] = useReducer(value => value + 1, 0);
  const sessions = useRef(new Map<string | null, DraftSession>());
  let current = sessions.current.get(session);
  if (!current) { current = new DraftSession(session ?? 'home', notify); sessions.current.set(session, current); }
  useEffect(() => { if (session !== null && !current.ready) void current.load(); }, [current, session]);
  useEffect(() => {
    const protect = (event: BeforeUnloadEvent) => {
      if ([...sessions.current.values()].some(value => value.issue && value.unsaved)) { event.preventDefault(); event.returnValue = ''; }
    };
    window.addEventListener('beforeunload', protect);
    return () => window.removeEventListener('beforeunload', protect);
  }, []);
  return current;
}
