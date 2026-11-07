import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import { EventEmitter } from 'node:events';
import { editorPresentation } from './editor-presentation.mjs';

async function fixture() {
  const directory = await fs.mkdtemp('/private/tmp/ic-presentation-');
  let at = 1_800_000_000_000;
  const controller = editorPresentation({ directory, now: () => at });
  const window = Object.assign(new EventEmitter(), { webContents: { id: 1 }, visible: true, minimized: false,
    isVisible() { return this.visible; }, isMinimized() { return this.minimized; } });
  controller.attach(window, () => {});
  return { directory, controller, window, advance: milliseconds => { at += milliseconds; } };
}

test('default Speak and immediate Mute persist across controller restart', async () => {
  const f = await fixture();
  try {
    assert.equal((await f.controller.read(1)).character, 'speak');
    await f.controller.setCharacter('mute');
    assert.equal((await f.controller.read(1)).character, 'mute');
    const restarted = editorPresentation({ directory: f.directory });
    restarted.attach(f.window, () => {});
    assert.equal((await restarted.read(1)).character, 'mute');
    assert.equal((await fs.stat(f.directory + '/editor-character.json')).mode & 0o777, 0o600);
    await assert.rejects(f.controller.setCharacter('off'));
    assert.equal((await f.controller.read(1)).character, 'mute');
  } finally { await fs.rm(f.directory, { recursive: true, force: true }); }
});

test('focus changes never count as away and ten minutes exactly does not qualify', async () => {
  const f = await fixture();
  try {
    f.window.emit('blur'); f.advance(900_000); f.window.emit('focus');
    assert.deepEqual(await f.controller.read(1), { character: 'speak', hidden: false, returned: null });
    f.window.visible = false; f.window.emit('hide'); f.advance(600_000);
    f.window.visible = true; f.window.emit('show');
    assert.equal((await f.controller.read(1)).returned, null);
    f.window.minimized = true; f.window.emit('minimize'); f.advance(600_001);
    f.window.minimized = false; f.window.emit('restore');
    const returned = (await f.controller.read(1)).returned;
    assert.equal(returned.to - returned.from, 600_001);
    f.window.emit('focus'); f.window.emit('show');
    assert.deepEqual((await f.controller.read(1)).returned, returned);
    f.controller.acknowledge(1, 'wrong');
    assert.deepEqual((await f.controller.read(1)).returned, returned);
    f.controller.acknowledge(1, returned.id);
    assert.equal((await f.controller.read(1)).returned, null);
  } finally { await fs.rm(f.directory, { recursive: true, force: true }); }
});

test('overlapping lock and hidden periods produce one whole interval', async () => {
  const f = await fixture();
  try {
    f.window.visible = false; f.window.emit('hide'); f.advance(300_000);
    f.controller.lock(true); f.window.visible = true; f.window.emit('show'); f.advance(400_000);
    assert.equal((await f.controller.read(1)).hidden, true);
    assert.equal((await f.controller.read(1)).returned, null);
    f.controller.lock(false);
    assert.equal((await f.controller.read(1)).returned.to - (await f.controller.read(1)).returned.from, 700_000);
    const old = (await f.controller.read(1)).returned.id;
    f.controller.lock(true); f.advance(700_000); f.controller.lock(false);
    f.controller.acknowledge(1, old);
    assert.notEqual((await f.controller.read(1)).returned.id, old);
  } finally { await fs.rm(f.directory, { recursive: true, force: true }); }
});

test('preference write failure preserves the selected value and a later write can recover', async () => {
  const f = await fixture();
  try {
    await f.controller.setCharacter('mute');
    await fs.mkdir(f.directory + '/editor-character.json.next');
    await assert.rejects(f.controller.setCharacter('speak'));
    assert.equal((await f.controller.read(1)).character, 'mute');
    await fs.rmdir(f.directory + '/editor-character.json.next');
    await Promise.all([f.controller.setCharacter('speak'), f.controller.setCharacter('mute')]);
    assert.equal((await f.controller.read(1)).character, 'mute');
  } finally { await fs.rm(f.directory, { recursive: true, force: true }); }
});

test('corrupt preferences surface an error until explicitly corrected', async () => {
  const f = await fixture();
  try {
    await fs.writeFile(f.directory + '/editor-character.json', 'invalid');
    const restarted = editorPresentation({ directory: f.directory }); restarted.attach(f.window, () => {});
    await assert.rejects(restarted.read(1));
    await restarted.setCharacter('mute'); assert.equal((await restarted.read(1)).character, 'mute');
    f.window.emit('closed'); await assert.rejects(f.controller.read(1));
  } finally { await fs.rm(f.directory, { recursive: true, force: true }); }
});

test('unread returns are queued per window and the character choice is shared', async () => {
  const f = await fixture();
  try {
    const second = Object.assign(new EventEmitter(), { webContents: { id: 2 }, isVisible: () => true, isMinimized: () => false });
    f.controller.attach(second, () => {});
    f.controller.lock(true); f.advance(700_000); f.controller.lock(false);
    const first = (await f.controller.read(1)).returned;
    f.controller.lock(true); f.advance(800_000); f.controller.lock(false);
    assert.deepEqual((await f.controller.read(1)).returned, first);
    f.controller.acknowledge(1, first.id);
    const next = (await f.controller.read(1)).returned;
    assert.equal(next.to - next.from, 800_000);
    assert.equal((await f.controller.read(2)).returned.to - (await f.controller.read(2)).returned.from, 700_000);
    await f.controller.setCharacter('mute');
    assert.equal((await f.controller.read(2)).character, 'mute');
    f.controller.acknowledge(1, first.id);
    assert.deepEqual((await f.controller.read(1)).returned, next);
  } finally { await fs.rm(f.directory, { recursive: true, force: true }); }
});
