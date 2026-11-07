import type { ElectronApplication } from '@playwright/test';

// Presentation tests replace the private daemon with a deterministic draft RPC.
// Persistence, revision races and restart recovery are exercised in Rust.
export async function mockDrafts(app: ElectronApplication) {
  await app.evaluate(({ ipcMain }) => {
    ipcMain.removeHandler('intercat:privacy-preview'); ipcMain.handle('intercat:privacy-preview', (_event, texts: string[]) => texts.map(text => ({ text, parts: text ? [{ kind: 'text', text }] : [] })));
    const sessions = new Map<string, { revision: number; opId: string | null; parentOpId: string | null; book: unknown }>();
    const storage = { failWrites: false };
    Object.assign(globalThis, { draftStorageFixture: storage });
    ipcMain.removeHandler('intercat:utterance-origin'); ipcMain.handle('intercat:utterance-origin', () => null);
    function read(session: string) {
      return structuredClone(sessions.get(session) ?? { revision: 0, opId: null, parentOpId: null, book: { active: 'work', stack: [], entries: { work: { text: '', target: { mode: 'none' }, attachments: [], context: { kind: 'work' }, pending: null } } } });
    }
    ipcMain.removeHandler('intercat:drafts-read'); ipcMain.handle('intercat:drafts-read', (_event, session) => read(session));
    ipcMain.removeHandler('intercat:drafts-discard'); ipcMain.handle('intercat:drafts-discard', (_event, session) => read(session));
    ipcMain.removeHandler('intercat:drafts-write'); ipcMain.handle('intercat:drafts-write', (_event, request) => {
      if (storage.failWrites) throw new Error('Simulated draft storage failure');
      const current = read(request.session);
      if (current.opId === request.opId) return { status: 'saved', ...current };
      if (current.revision !== request.revision || current.opId !== request.parentOpId) return { status: 'conflict', ...current };
      const saved = { revision: current.revision + 1, opId: request.opId, parentOpId: request.parentOpId, book: structuredClone(request.book) };
      sessions.set(request.session, saved);
      return { status: 'saved', ...saved };
    });
  });
  await (await app.firstWindow()).reload();
}

export async function mockPastes(app: ElectronApplication) {
  await app.evaluate(({ ipcMain }) => {
    const fixture = { originals: new Map<string, string>(), finishes: [] as string[], fail: false };
    Object.assign(globalThis, { pasteFixture: fixture });
    ipcMain.removeHandler('intercat:paste-stage'); ipcMain.handle('intercat:paste-stage', (_event, request) => {
      fixture.originals.set(request.opId, request.text);
      return { id: request.opId, words: request.text.split(/\s+/).length };
    });
    ipcMain.removeHandler('intercat:paste-finish'); ipcMain.handle('intercat:paste-finish', (_event, id) => {
      fixture.finishes.push(id);
      if (fixture.fail) { fixture.fail = false; throw new Error('Lost import acknowledgement'); }
      return { id, name: 'pasted.txt', byteLen: Buffer.byteLength(fixture.originals.get(id)!), sha256: 'a'.repeat(64) };
    });
    ipcMain.removeHandler('intercat:paste-read'); ipcMain.handle('intercat:paste-read', (_event, id) => fixture.originals.get(id));
  });
}
