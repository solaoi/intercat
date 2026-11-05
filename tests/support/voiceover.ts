import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { expect, type ElectronApplication } from '@playwright/test';

const run = promisify(execFile);

async function requireFocus(app: ElectronApplication) {
  if (!await app.evaluate(({ BrowserWindow }) => BrowserWindow.getAllWindows()[0].isFocused())) {
    throw new Error('The verification window lost focus; VoiceOver capture stopped.');
  }
}

export async function nextVoiceOverTrack(app: ElectronApplication) {
  await requireFocus(app);
  // HIToolbox Events.h: kVK_DownArrow = 0x7D. Exercise the actual macOS
  // keyboard route, including VoiceOver, rather than injecting a DOM event.
  await run('/usr/bin/osascript', ['-e', 'tell application "System Events" to key code 125'], { timeout: 2500 });
}

export async function requireVoiceOver() {
  await run('/usr/bin/pgrep', ['-x', 'VoiceOver']);
  // The user enables scripting in VoiceOver Utility. Read its real output;
  // do not synthesize speech from the DOM accessibility name.
  await run('/usr/bin/osascript', ['-e', 'tell application "VoiceOver" to get content of last phrase'], { timeout: 2500 });
}

export async function spokenTrack(app: ElectronApplication, index: number) {
  const title = `利用者認証と変更履歴を確認するトラック ${index}`;
  const status = index <= 100 ? 'working' : 'done';
  let spoken = '';
  await expect.poll(async () => {
    await requireFocus(app);
    const result = await run('/usr/bin/osascript', ['-e', 'tell application "VoiceOver" to get content of last phrase'], { timeout: 2500 });
    const phrase = result.stdout.trim();
    if (new RegExp(`${title}(?!\\d)`).test(phrase) && phrase.includes(status)) {
      spoken = phrase;
      return true;
    }
    return false;
  }, { timeout: 4000, intervals: [50, 100, 200] }).toBe(true);
  return spoken;
}
