import fs from 'node:fs/promises';
import path from 'node:path';
import { randomUUID } from 'node:crypto';

// Native visibility, rather than focus or keyboard activity, defines absence.
export function editorPresentation({ directory, now = Date.now }) {
  const file = path.join(directory, 'editor-character.json');
  const windows = new Map();
  let locked = false;
  let writes = Promise.resolve();
  let character = fs.readFile(file, 'utf8').then(text => {
    const value = JSON.parse(text);
    if (value !== 'speak' && value !== 'mute') throw new Error('Invalid Editor character preference');
    return value;
  }, error => { if (error.code === 'ENOENT') return 'speak'; throw error; });
  // The settings UI reports read errors; avoid an unhandled startup rejection.
  character.catch(() => {});
  function state(record) {
    return { hidden: record.hidden, returned: record.returns[0] ?? null };
  }
  async function publish(record) {
    try { record.changed({ character: await character, ...state(record) }); }
    catch { /* A failed preference read is surfaced by the explicit IPC read. */ }
  }
  function visibility(record) {
    const hidden = locked || record.window.isMinimized() || !record.window.isVisible();
    if (hidden === record.hidden) return;
    record.hidden = hidden;
    const at = now();
    if (hidden) record.since = at;
    else {
      if (record.since !== null && at - record.since > 600_000) {
        record.returns.push({ id: randomUUID(), from: record.since, to: at });
      }
      record.since = null;
    }
    void publish(record);
  }
  return {
    attach(window, changed) {
      const id = window.webContents.id;
      const hidden = locked || window.isMinimized() || !window.isVisible();
      const record = { window, changed, hidden, since: hidden ? now() : null, returns: [] };
      windows.set(id, record);
      const update = () => visibility(record);
      for (const event of ['show', 'hide', 'minimize', 'restore']) window.on(event, update);
      window.once('closed', () => windows.delete(id));
    },
    lock(value) {
      locked = value;
      for (const record of windows.values()) visibility(record);
    },
    async read(id) {
      const record = windows.get(id);
      if (!record) throw new Error('Editor window is unavailable');
      return { character: await character, ...state(record) };
    },
    async setCharacter(value) {
      if (value !== 'speak' && value !== 'mute') throw new Error('Invalid Editor character preference');
      const saved = writes.catch(() => {}).then(async () => {
        await fs.mkdir(directory, { recursive: true });
        await fs.writeFile(file + '.next', JSON.stringify(value) + '\n', { mode: 0o600 });
        await fs.rename(file + '.next', file);
        character = Promise.resolve(value);
        for (const record of windows.values()) void publish(record);
        return value;
      });
      writes = saved;
      return saved;
    },
    acknowledge(id, returnedId) {
      const record = windows.get(id);
      if (record?.returns[0]?.id === returnedId) {
        record.returns.shift();
        void publish(record);
      }
    },
  };
}
