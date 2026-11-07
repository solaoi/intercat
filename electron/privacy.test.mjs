import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import path from 'node:path';
import { privacyPreview, privacyText } from './privacy.mjs';

test('large previews pass only a private file identifier over RPC and clean up', async () => {
  const directory = await fs.mkdtemp('/private/tmp/ic-privacy-transport-');
  try {
    const input = '日本語\n'.repeat(60000);
    const result = await privacyPreview({ directory, texts: [input], client: async () => ({ request: async (method, params) => {
      assert.equal(method, 'privacy.preview'); assert.deepEqual(Object.keys(params), ['id']);
      assert.ok(JSON.stringify(params).length < 100);
      const folder = path.join(directory, params.id);
      assert.equal((await fs.stat(folder)).mode & 0o777, 0o700);
      const file = path.join(folder, 'input.json');
      assert.equal((await fs.stat(file)).mode & 0o777, 0o600);
      const saved = JSON.parse(await fs.readFile(file, 'utf8'));
      assert.deepEqual(saved, [input]); assert.ok((await fs.stat(file)).size > 512 * 1024);
      return [{ parts: [{ kind: 'text', textBase64: Buffer.from(input).toString('base64') }] }];
    } }) });
    assert.equal(result[0].text, input); assert.deepEqual(await fs.readdir(directory), []);
  } finally { await fs.rm(directory, { recursive: true, force: true }); }
});

test('failed and malformed previews also delete their private input', async () => {
  const directory = await fs.mkdtemp('/private/tmp/ic-privacy-transport-');
  try {
    await assert.rejects(privacyPreview({ directory, texts: ['dummy'], client: async () => ({ request: async () => { throw new Error('dummy failure'); } }) }));
    assert.deepEqual(await fs.readdir(directory), []);
    await assert.rejects(privacyPreview({ directory, texts: ['dummy'], client: async () => ({ request: async () => [{ parts: [{ kind: 'text', textBase64: 'not base64' }] }] }) }));
    assert.deepEqual(await fs.readdir(directory), []);
  } finally { await fs.rm(directory, { recursive: true, force: true }); }
});

test('Full findings retain the category and stable identity from the daemon', async () => {
  const directory = await fs.mkdtemp('/private/tmp/ic-full-privacy-transport-');
  try {
    const parts = ['name', 'password', 'address'].map((category, index) => ({ kind: 'redaction', id: index + 1, category }));
    const result = await privacyPreview({ directory, texts: ['dummy private text'], client: async () => ({ request: async () => [{ parts }] }) });
    assert.deepEqual(result[0].parts, parts);
    assert.equal(result[0].text, '<redacted:name#1><redacted:password#2><redacted:address#3>');
    assert.deepEqual(await fs.readdir(directory), []);
  } finally { await fs.rm(directory, { recursive: true, force: true }); }
});

test('the transport rejects a symlink directory and invalid UTF-8', async () => {
  const root = await fs.mkdtemp('/private/tmp/ic-privacy-transport-');
  try {
    await fs.mkdir(path.join(root, 'outside')); await fs.symlink(path.join(root, 'outside'), path.join(root, 'link'));
    await assert.rejects(privacyPreview({ directory: path.join(root, 'link'), texts: ['dummy'], client: async () => ({ request: async () => [] }) }));
    assert.deepEqual(await fs.readdir(path.join(root, 'outside')), []);
    assert.throws(() => privacyText(Buffer.from([0xff]).toString('base64')));
  } finally { await fs.rm(root, { recursive: true, force: true }); }
});
