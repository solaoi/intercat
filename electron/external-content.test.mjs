import { test } from 'node:test';
import assert from 'node:assert/strict';
import { reviewExternal, confirmExternal } from './external-content.mjs';

test('review performs no open and confirmation consumes only the reviewed ID', async () => {
  const calls = [], opened = [];
  const client = { request: async (method, value) => {
    calls.push([method, value]);
    return method === 'external.review' ? { id: 'review', kind: 'link', url: value.url } : { url: 'https://example.invalid/reviewed' };
  } };
  await reviewExternal(client, { kind: 'link', url: 'https://example.invalid/reviewed' });
  assert.deepEqual(opened, []);
  await confirmExternal(client, { id: 'review', kind: 'link' }, async url => opened.push(url));
  assert.deepEqual(calls[1], ['external.link', { id: 'review' }]);
  assert.deepEqual(opened, ['https://example.invalid/reviewed']);
  await assert.rejects(confirmExternal(client, { id: 'review', kind: 'link', url: 'https://different.invalid' }, async url => opened.push(url)));
});

test('untrusted schemes never reach the operating system even from a malformed response', async () => {
  for (const url of ['javascript:alert(1)', 'file:///private/file', 'https://name:password@example.invalid/', 'https://example.invalid/\n']) {
    let opened = false;
    await assert.rejects(confirmExternal({ request: async () => ({ url }) }, { id: 'review', kind: 'link' }, async () => { opened = true; }));
    assert.equal(opened, false);
  }
});

test('images return only the daemon payload and never open a browser', async () => {
  let method;
  const image = { dataUrl: 'data:image/png;base64,aGVsbG8=' };
  const result = await confirmExternal({ request: async value => { method = value; return image; } }, { id: 'image', kind: 'image' }, async () => { throw new Error('Unexpected open'); });
  assert.equal(method, 'external.image'); assert.equal(result, image);
});
