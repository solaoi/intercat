import fs from 'node:fs/promises';
import path from 'node:path';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { operationId } from './daemon.mjs';

const execute = promisify(execFile);
const choices = new Set(['ask', 'background', 'stop']);
const alive = pid => { try { process.kill(pid, 0); return true; } catch { return false; } };

export function quitController({ app, dialog, client, connect, binary, data, clearConnection, enabled }) {
  const settings = path.join(app.getPath('userData'), 'quit-preference.json');
  let allowed = false, busy = false;
  async function preference() {
    try {
      const value = JSON.parse(await fs.readFile(settings, 'utf8'));
      return choices.has(value) ? value : 'ask';
    } catch (error) {
      if (error.code === 'ENOENT' || error instanceof SyntaxError) return 'ask';
      throw error;
    }
  }
  async function setPreference(value) {
    if (!choices.has(value)) throw new Error('Invalid quit preference');
    const temporary = settings + '.next';
    await fs.writeFile(temporary, JSON.stringify(value) + '\n', { mode: 0o600 });
    await fs.rename(temporary, settings);
    return value;
  }
  async function stopEverything() {
    const connection = await connect();
    let pid;
    if (connection) {
      const status = await connection.request('system.status');
      pid = status.daemonPid;
      if (!Number.isSafeInteger(pid) || pid <= 0) throw new Error('The daemon process could not be identified.');
      try { await connection.request('system.shutdown', { opId: operationId(), paused: true }); }
      catch (error) { if (alive(pid)) throw error; }
      const deadline = Date.now() + 20000;
      while (alive(pid) && Date.now() < deadline) await new Promise(resolve => setTimeout(resolve, 50));
      if (alive(pid)) throw new Error('The daemon is still finishing work. Wait and try again.');
    }
    const { stdout } = await execute(binary, ['stop-processes', data], { timeout: 30000, maxBuffer: 1024 * 1024 });
    const audit = JSON.parse(stdout);
    if (audit.hosts || audit.processes || audit.groups || audit.issues.length) {
      throw new Error([
        'Some owned processes or groups are still running.',
        ...audit.live.map(item => 'PID ' + item.pid),
        ...audit.issues,
      ].join('\n'));
    }
    clearConnection();
  }
  async function stopWithReview(quitting) {
    for (;;) {
      try { await stopEverything(); return true; }
      catch (error) {
        const result = await dialog.showMessageBox({
          type: 'warning', message: 'Stopping is not complete', detail: error.message,
          buttons: quitting ? ['Try again', 'Quit anyway', 'Cancel'] : ['Try again', 'Cancel'],
          defaultId: 0, cancelId: quitting ? 2 : 1, noLink: true,
        });
        if (result.response === 0) continue;
        if (quitting && result.response === 1) return true;
        return false;
      }
    }
  }
  async function requestQuit() {
    if (busy) return;
    busy = true;
    try {
      let action = await preference();
      if (action === 'ask') {
        let connection;
        try { connection = await client(); } catch { connection = null; }
        let running = 0;
        if (connection) {
          try {
            const status = await connection.request('system.status');
            running = (status.workers ?? []).filter(worker => worker.activeTurn).length;
            running += (status.repairRuns ?? []).filter(run => ['queued', 'preparing', 'running'].includes(run.phase)).length;
          } catch { running = null; }
        }
        const result = await dialog.showMessageBox({
          type: 'question', message: 'Quit Intercat?',
          detail: running === null ? 'The worker connection is unavailable.' : running + (running === 1 ? ' track is working.' : ' tracks are working.'),
          buttons: ['Keep working in the background', 'Stop everything', 'Cancel'],
          defaultId: 0, cancelId: 2, checkboxLabel: 'Remember', noLink: true,
        });
        if (result.response === 2) return;
        action = result.response === 1 ? 'stop' : 'background';
        if (result.checkboxChecked) await setPreference(action);
      }
      if (action === 'stop' && !await stopWithReview(true)) return;
      allowed = true;
      app.quit();
    } finally { busy = false; }
  }
  app.on('before-quit', event => {
    if (!enabled || allowed) return;
    event.preventDefault();
    void requestQuit().catch(error => dialog.showErrorBox('Could not quit Intercat', error.message));
  });
  app.on('window-all-closed', () => { allowed = true; app.quit(); });
  return {
    preference, setPreference,
    async stopProcesses() {
      if (busy) return;
      busy = true;
      try {
        const result = await dialog.showMessageBox({
          type: 'question', message: 'Stop all turns and processes?',
          detail: 'All worker hosts and their recorded processes will stop. Intercat will stay open.',
          buttons: ['Stop', 'Cancel'], defaultId: 0, cancelId: 1, noLink: true,
        });
        if (result.response === 0) await stopWithReview(false);
      } finally { busy = false; }
    },
  };
}
