import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { pastedTextStore } from './pasted-text.mjs';

const id = '01960000-0000-7000-8000-000000000001';
const attachment = { id, name: 'pasted.txt', byteLen: 8, sha256: 'a'.repeat(64) };
async function fixture(t, request) {
  const directory = await fs.mkdtemp(path.join(os.tmpdir(), 'intercat-paste-'));
  t.after(() => fs.rm(directory, { recursive: true, force: true }));
  const store = pastedTextStore({ directory, client: async () => ({ request }) });
  return { directory, store };
}
test('stages an exact private original and rejects reuse with different text', async t => {
  const { store, directory } = await fixture(t);
  const text = '日本語の依頼\nKeep the complete tail.';
  const staged = await store.stage({ opId: id, text });
  assert.equal(staged.id, id); assert.ok(staged.words > 0);
  assert.equal(await fs.readFile(path.join(directory, id), 'utf8'), text);
  assert.equal((await fs.stat(directory)).mode & 0o777, 0o700);
  assert.equal((await fs.stat(path.join(directory, id))).mode & 0o777, 0o600);
  assert.deepEqual(await store.stage({ opId: id, text }), staged);
  await assert.rejects(store.stage({ opId: id, text: text + 'different' }), /reused/);
});
test('enforces UTF-8 byte limits and refuses linked originals', async t => {
  const { store, directory } = await fixture(t);
  const limit = 2 * 1024 * 1024;
  const text = '字'.repeat(Math.floor(limit / 3)) + 'a'.repeat(limit % 3);
  await store.stage({ opId: id, text });
  await assert.rejects(store.stage({ opId: id, text: text + 'a' }), /2 MiB/);
  await fs.unlink(path.join(directory, id));
  const other = path.join(directory, 'other'); await fs.writeFile(other, 'target');
  await fs.symlink(other, path.join(directory, id));
  await assert.rejects(store.stage({ opId: id, text: 'target' }));
});
test('lost acknowledgement retains the original and retry preserves its operation', async t => {
  const calls = []; let lost = true;
  const { store, directory } = await fixture(t, async (method, params) => {
    calls.push({ method, params });
    if (lost) { lost = false; throw new Error('Lost acknowledgement'); }
    return attachment;
  });
  await store.stage({ opId: id, text: 'Original' });
  await assert.rejects(store.finish(id), /Lost/);
  assert.equal(await fs.readFile(path.join(directory, id), 'utf8'), 'Original');
  assert.deepEqual(await store.finish(id), attachment);
  assert.deepEqual(calls[0], calls[1]);
  await assert.rejects(fs.stat(path.join(directory, id)), { code: 'ENOENT' });
  // The daemon replays its sealed original even after the inbox was removed.
  assert.deepEqual(await store.finish(id), attachment);
});
test('concurrent finishing shares one RPC and close waits for it', async t => {
  let release; let count = 0;
  const gate = new Promise(resolve => { release = resolve; });
  const { store } = await fixture(t, async () => { count++; await gate; return attachment; });
  await store.stage({ opId: id, text: 'Original' });
  const one = store.finish(id); const two = store.finish(id);
  let flushed = false; const closing = store.flush().then(() => { flushed = true; });
  await new Promise(resolve => setTimeout(resolve, 5));
  assert.equal(count, 1); assert.equal(flushed, false);
  release(); await Promise.all([one, two, closing]); assert.equal(flushed, true);
});
test('invalid attachment acknowledgement retains original and original reads validate encoding', async t => {
  let result = { ...attachment, byteLen: 30 * 1024 * 1024 };
  const { store, directory } = await fixture(t, async () => result);
  await store.stage({ opId: id, text: 'Original' });
  await assert.rejects(store.finish(id), /verified/);
  assert.equal(await fs.readFile(path.join(directory, id), 'utf8'), 'Original');
  for (const textBase64 of ['?', '/w==', Buffer.alloc(2 * 1024 * 1024 + 1).toString('base64')]) {
    result = { textBase64 }; await assert.rejects(store.read(id));
  }
  result = { textBase64: Buffer.from('原文\nComplete').toString('base64') };
  assert.equal(await store.read(id), '原文\nComplete');
});
test('reference screening validates identity and sends the same retry request', async t => {
  const request = { opId: id, attachmentId: id };
  const calls = [];
  let result = { attachmentId: id, sha256: 'b'.repeat(64), byteLen: 20, changed: true };
  const { store } = await fixture(t, async (method, params) => { calls.push({ method, params }); return result; });
  assert.deepEqual(await store.refresh(request), result);
  assert.deepEqual(await store.refresh(request), result);
  assert.deepEqual(calls[0], { method: 'attachment.paste.refresh', params: request });
  assert.deepEqual(calls[0], calls[1]);
  result = { ...result, attachmentId: 'wrong' };
  await assert.rejects(store.refresh(request), /Invalid reference screening result/);
  await assert.rejects(store.refresh({ ...request, extra: true }), /Invalid reference screening request/);
});
test('reference listing preserves pagination and rejects unverified metadata', async t => {
  let result = { items: [attachment], next: id };
  const { store } = await fixture(t, async (method, params) => {
    assert.equal(method, 'attachment.paste.list');
    assert.equal(params.trackId, id);
    return result;
  });
  assert.deepEqual(await store.references({ trackId: id, after: null }), result);
  result = { items: [{ ...attachment, sha256: 'invalid' }], next: null };
  await assert.rejects(store.references({ trackId: id, after: id }), /Invalid pasted reference/);
});
