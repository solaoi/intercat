import fs from 'node:fs/promises';
import { constants } from 'node:fs';
import path from 'node:path';

const limit = 2 * 1024 * 1024;
function validId(id) {
  if (typeof id !== 'string' || !/^[a-f0-9]{8}-[a-f0-9]{4}-7[a-f0-9]{3}-[89ab][a-f0-9]{3}-[a-f0-9]{12}$/.test(id)) throw new Error('Invalid pasted text identifier.');
}

export function pastedTextStore({ directory, client }) {
  const pending = new Map();
  async function stage({ opId, text }) {
    validId(opId);
    if (typeof text !== 'string' || Buffer.byteLength(text) > limit) throw new Error('Pasted text can be up to 2 MiB.');
    await fs.mkdir(directory, { recursive: true, mode: 0o700 });
    const metadata = await fs.lstat(directory);
    if (!metadata.isDirectory() || metadata.uid !== process.getuid()) throw new Error('Unsafe pasted text directory.');
    await fs.chmod(directory, 0o700);
    const name = path.join(directory, opId);
    let file;
    try {
      file = await fs.open(name, constants.O_WRONLY | constants.O_CREAT | constants.O_EXCL | constants.O_NOFOLLOW, 0o600);
      await file.writeFile(text, 'utf8');
      await file.sync();
    } catch (error) {
      if (error.code !== 'EEXIST') {
        if (file) { await file.close(); file = null; await fs.unlink(name); }
        throw error;
      }
      const previous = await fs.open(name, constants.O_RDONLY | constants.O_NOFOLLOW);
      try {
        const stat = await previous.stat();
        if (!stat.isFile() || stat.uid !== process.getuid() || stat.nlink !== 1 || stat.size > limit || await previous.readFile('utf8') !== text) throw new Error('Pasted text operation reused.');
      } finally { await previous.close(); }
    } finally { await file?.close(); }
    const folder = await fs.open(directory, constants.O_RDONLY | constants.O_DIRECTORY | constants.O_NOFOLLOW);
    try { await folder.sync(); } finally { await folder.close(); }
    let words = 0;
    for (const segment of new Intl.Segmenter(undefined, { granularity: 'word' }).segment(text)) {
      if (segment.isWordLike) words++;
    }
    return { id: opId, words };
  }
  function finish(id) {
    validId(id);
    if (pending.has(id)) return pending.get(id);
    const task = (async () => {
      const result = await (await client()).request('attachment.paste', { opId: id, path: path.join(directory, id) });
      if (result?.id !== id || typeof result.name !== 'string' || !Number.isSafeInteger(result.byteLen) || result.byteLen < 0 || result.byteLen > 25 * 1024 * 1024 || !/^[a-f0-9]{64}$/.test(result.sha256)) throw new Error('The pasted reference could not be verified.');
      await fs.unlink(path.join(directory, id)).catch(error => { if (error.code !== 'ENOENT') throw error; });
      return result;
    })();
    pending.set(id, task);
    void task.finally(() => pending.delete(id)).catch(() => {});
    return task;
  }
  async function read(id) {
    validId(id);
    const value = await (await client()).request('attachment.paste.read', { id });
    if (typeof value?.textBase64 !== 'string' || value.textBase64.length > Math.ceil(limit / 3) * 4) throw new Error('Invalid pasted text response.');
    const bytes = Buffer.from(value.textBase64, 'base64');
    if (bytes.length > limit || bytes.toString('base64') !== value.textBase64) throw new Error('Invalid pasted text response.');
    return new TextDecoder('utf-8', { fatal: true }).decode(bytes);
  }
  async function refresh(request) {
    validId(request?.opId); validId(request?.attachmentId);
    if (Object.keys(request).sort().join(',') !== 'attachmentId,opId') throw new Error('Invalid reference screening request.');
    const result = await (await client()).request('attachment.paste.refresh', request);
    if (result?.attachmentId !== request.attachmentId || typeof result.changed !== 'boolean' || !Number.isSafeInteger(result.byteLen) || result.byteLen < 0 || result.byteLen > 25 * 1024 * 1024 || !/^[a-f0-9]{64}$/.test(result.sha256)) throw new Error('Invalid reference screening result.');
    return result;
  }
  async function references(request) {
    validId(request?.trackId);
    if (request.after !== null) validId(request.after);
    if (Object.keys(request).sort().join(',') !== 'after,trackId') throw new Error('Invalid reference list request.');
    const result = await (await client()).request('attachment.paste.list', request);
    if (!Array.isArray(result?.items) || result.items.length > 50) throw new Error('Invalid reference list response.');
    for (const item of result.items) {
      validId(item?.id);
      if (typeof item.name !== 'string' || !Number.isSafeInteger(item.byteLen) || item.byteLen < 0 || item.byteLen > 25 * 1024 * 1024 || !/^[a-f0-9]{64}$/.test(item.sha256)) throw new Error('Invalid pasted reference.');
    }
    if (result.next !== null) validId(result.next);
    return result;
  }
  async function flush() { await Promise.allSettled([...pending.values()]); }
  return { stage, finish, read, refresh, references, flush };
}
