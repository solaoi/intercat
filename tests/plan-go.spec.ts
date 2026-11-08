import { _electron as electron, expect, test } from '@playwright/test';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { mockDrafts } from './support/drafts';
import { closeWindow } from './support/close';
import type { Plan, PlanCommand } from '../src/plans/types';
import type { InputResolution, Utterance, WaitingRequest } from '../src/bridge';

for (const zoom of [1, 2]) test(`A spoken plan waits for review and keeps its captured revision at ${zoom * 100}%`, async ({}, info) => {
  const directory = await mkdtemp('/private/tmp/ic-plan-go-');
  const app = await electron.launch({ args: ['.', '--preview', '--user-data-dir=' + directory] });
  await mockDrafts(app);
  try {
    await app.evaluate(({ ipcMain, BrowserWindow }, zoom) => {
      const plan: Plan = { id: 'dummy-plan', rev: 2, state: 'draft', sourceId: 'source', sourceRevision: 1, startMode: 'manual',
        executeAllowed: false, lockedTrack: null, generation: { id: 'generation', state: 'ready', attempt: 0, detail: null },
        basedOn: [], attachments: [], summary: 'Create a dummy blue badge.', assumptions: [], notices: [], seenRevision: null, rewrite: null,
        items: [{ planItemId: 'dummy-item', trackRef: 'new:1', folderId: 'dummy-folder', order: 0, title: 'Blue badge', instruction: 'Create a dummy blue badge.', constraints: [], doneWhen: 'The badge is blue.', after: [], state: 'planned', skipOrder:[],startRequested:false,utteranceId: null, detail: null }] };
      const word: Utterance = { id: 'dummy-go', revision: 1, text: 'Go ahead with the plan.', sourceText: 'Go ahead with the plan.', target: { mode: 'none' }, state: 'asked', createdAt: 0, attachments: [], reason: 'Review this version of the plan before starting it.',
        decision: { kind: 'command', trackId: null, boundary: null, command: 'start-waiting', confidence: 1, inferenceMs: 0, timedOut: false },
        startWaiting: { candidates: [{ kind: 'plan', planId: plan.id, revision: 2, text: plan.summary, label: null, available: false, reason: 'Review this version of the plan before starting it.' }], started: null } };
      const state = { plan, word, resolves: [] as InputResolution[], planCalls: [] as PlanCommand[] };
      Object.assign(globalThis, { planGoFixture: state });
      const snapshot = () => {
        const candidate = word.startWaiting!.candidates[0];
        candidate.available = candidate.revision === plan.rev && plan.seenRevision === plan.rev;
        candidate.reason = candidate.revision !== plan.rev ? 'The plan changed. Read this request again.' : candidate.available ? null : 'Review this version of the plan before starting it.';
        return { connected: true, sequence: plan.rev + word.revision, tracks: [], workers: [], utterances: [word], plans: [plan] };
      };
      ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', snapshot);
      ipcMain.removeHandler('intercat:timeline-window'); ipcMain.handle('intercat:timeline-window', () => ({ revision: 1, reset: true, clips: [], marks: [], removed: [], removedMarks: [] }));
      ipcMain.removeHandler('intercat:plan-command'); ipcMain.handle('intercat:plan-command', (_event, request: PlanCommand) => { state.planCalls.push(request); if (request.action.kind === 'review') plan.seenRevision = plan.rev; return structuredClone(plan); });
      ipcMain.removeHandler('intercat:resolve'); ipcMain.handle('intercat:resolve', (_event, request: { resolution: InputResolution }) => {
        state.resolves.push(request.resolution);
        if (request.resolution.action === 'retry') {
          word.revision++; word.startWaiting!.candidates[0].revision = plan.rev;
        } else if (request.resolution.action === 'startWaiting') {
          const choice: WaitingRequest = request.resolution.choice;
          if (choice.kind !== 'plan' || choice.planId !== plan.id || choice.revision !== plan.rev || plan.seenRevision !== plan.rev) throw new Error('The plan changed');
          plan.state = 'running'; plan.rev++; word.state = 'landed'; word.revision++; word.startWaiting!.started = { ...choice, revision: plan.rev };
        }
        return structuredClone(word);
      });
      BrowserWindow.getAllWindows()[0].setSize(1040, 760); BrowserWindow.getAllWindows()[0].webContents.setZoomFactor(zoom);
    }, zoom);
    const page = await app.firstWindow();
    const draft = page.getByRole('textbox', { name: 'Request draft' });
    await draft.fill('Keep this independent dummy draft.');
    const card = page.getByRole('group', { name: 'Start waiting work', exact: true });
    const start = card.getByRole('button', { name: /^Start plan/ });
    await expect(start).toBeDisabled();
    await card.getByRole('button', { name: 'Review plan', exact: true }).click();
    const review = page.getByRole('dialog', { name: 'Review plan' });
    await expect(review).toContainText('The badge is blue.');
    await review.getByRole('button', { name: 'Close plan review' }).click();
    await expect(start).toBeEnabled();
    if (zoom === 2) {
      await app.evaluate(() => { const { plan } = (globalThis as typeof globalThis & { planGoFixture: { plan: Plan } }).planGoFixture; plan.rev++; plan.seenRevision = null; });
      await expect(start).toBeDisabled();
      await expect(card).toContainText('The plan changed. Read this request again.');
      await card.getByRole('button', { name: 'Read again', exact: true }).click();
      await draft.focus(); await page.mouse.move(2, 2);
      await card.getByRole('button', { name: 'Review plan', exact: true }).click();
      await review.getByRole('button', { name: 'Close plan review' }).click();
      await expect(start).toBeEnabled();
    }
    await start.focus(); await expect(start).toBeInViewport();
    await writeFile(info.outputPath('spoken-plan-choice.png'), Buffer.from(await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64')), 'base64'));
    await start.click();
    await expect(card).toHaveCount(0); await expect(draft).toHaveValue('Keep this independent dummy draft.');
    const result = await app.evaluate(() => (globalThis as typeof globalThis & { planGoFixture: { plan: Plan; resolves: InputResolution[] } }).planGoFixture);
    expect(result.plan.state).toBe('running');
    expect(result.resolves.at(-1)).toEqual({ action: 'startWaiting', choice: { kind: 'plan', planId: 'dummy-plan', revision: zoom === 2 ? 3 : 2 } });
  } finally { await closeWindow(app); await rm(directory, { recursive: true, force: true }); }
});
