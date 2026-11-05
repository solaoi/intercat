import { app, BrowserWindow, dialog, ipcMain, Menu, MenuItem, net, protocol, session, shell } from 'electron';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { connectDaemon, operationId, uiToken } from './daemon.mjs';
import { registerFingerprint, validSecretName, receiptMismatch } from './secrets.mjs';
import { branchFromCheckpoint } from './branches.mjs';
import { pickupFromFiles, cancelFileRecovery } from './pickup.mjs';
import { referenceImporter } from './attachments.mjs';
import { rebuildProjectFingerprints, reviewBackup, applyBackup } from './fingerprint-recovery.mjs';
import { changeProjectSecret, synchronizeProjectSecrets, projectSecretSettings, projectSecretIssue } from './project-secrets.mjs';
import { reviewExternal, confirmExternal } from './external-content.mjs';
import { quitController } from './quit.mjs';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const assets = path.join(root, 'dist');
const csp = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; font-src 'self'; connect-src 'none'; worker-src 'self' blob:; object-src 'none'; base-uri 'self'; frame-src 'none'";

protocol.registerSchemesAsPrivileged([{ scheme: 'intercat', privileges: { standard: true, secure: true, supportFetchAPI: true } }]);
app.setName('Intercat');
const singleInstance = app.requestSingleInstanceLock();
if (!singleInstance) app.quit();
const preview = process.argv.includes('--preview');
let daemon;
let blockedConnection;
function connection() {
  if (blockedConnection) return Promise.reject(blockedConnection);
  daemon ??= uiToken().then(token => connectDaemon(path.join(root, 'target/debug/intercatd'), path.join(app.getPath('userData'), 'daemon'), token, path.join(root, 'services/editor/src'), path.join(root, '.tools/gitleaks-8.30.1/gitleaks')));
  return daemon;
}
const quitting = quitController({
  app, dialog, enabled: !preview,
  client: async () => daemon ? await daemon : null,
  connect: connection,
  binary: path.join(root, 'target/debug/intercatd'),
  data: path.join(app.getPath('userData'), 'daemon'),
  clearConnection: () => { daemon = undefined; blockedConnection = undefined; },
});

function authorize(event) {
  const url = new URL(event.senderFrame?.url ?? 'about:blank');
  if (event.senderFrame !== event.sender.mainFrame || url.protocol !== 'intercat:' || url.host !== 'app') throw new Error('Untrusted IPC sender');
}
async function status() {
  if (preview) return { connected: false, mode: 'preview' };
  try { const client = await connection(); return { connected: true, ...(await client.request('system.status')), projectSecretIssue: projectSecretIssue(client) }; }
  catch (error) {
    daemon = undefined;
    if (error.code === 'FINGERPRINT_RECEIPT_MISMATCH') blockedConnection = error;
    return { connected: false, mode: 'unavailable', message: blockedConnection?.message ?? 'The daemon or Keychain is unavailable.', fingerprintRecovery: blockedConnection?.recovery ?? null };
  }
}
ipcMain.handle('intercat:status', async event => {
  authorize(event);
  return status();
});
ipcMain.handle('intercat:external-review', async (event, request) => {
  authorize(event);
  if (preview) throw new Error('External content is unavailable in preview.');
  return reviewExternal(await connection(), request);
});
ipcMain.handle('intercat:external-confirm', async (event, request) => {
  authorize(event);
  if (preview) throw new Error('External content is unavailable in preview.');
  return confirmExternal(await connection(), request, url => shell.openExternal(url));
});
ipcMain.handle('intercat:external-cancel', async (event, id) => {
  authorize(event);
  if (preview || typeof id !== 'string') throw new Error('Invalid external URL cancellation.');
  return (await connection()).request('external.cancel', { id });
});
ipcMain.handle('intercat:retry-connection', async event => {
  authorize(event);
  blockedConnection = undefined; daemon = undefined;
  return status();
});
ipcMain.handle('intercat:rebuild-fingerprints', async (event, request) => {
  authorize(event);
  if (preview || !blockedConnection) throw new Error('Fingerprint recovery is not active.');
  const recovery = await rebuildProjectFingerprints(path.join(root, 'target/debug/intercatd'), path.join(app.getPath('userData'), 'daemon'), request);
  blockedConnection = receiptMismatch(recovery);
  daemon = undefined;
  return status();
});
ipcMain.handle('intercat:backup-review', async (event, request) => {
  authorize(event);
  if (preview || !blockedConnection) throw new Error('Restored history is not awaiting review.');
  return reviewBackup(path.join(root, 'target/debug/intercatd'), path.join(app.getPath('userData'), 'daemon'), request);
});
ipcMain.handle('intercat:backup-apply', async (event, request) => {
  authorize(event);
  if (preview || !blockedConnection) throw new Error('Restored history is not awaiting recovery.');
  await applyBackup(path.join(root, 'target/debug/intercatd'), path.join(app.getPath('userData'), 'daemon'), request);
  blockedConnection = undefined; daemon = undefined;
  return status();
});
ipcMain.handle('intercat:backup-cancel', async (event, request) => {
  authorize(event);
  if (preview || !blockedConnection || !request || Object.keys(request).join(',') !== 'opId') throw new Error('Backup recovery is not awaiting review.');
  await reviewBackup(path.join(root, 'target/debug/intercatd'), path.join(app.getPath('userData'), 'daemon'), { action: 'cancelApply', opId: request.opId });
  blockedConnection = undefined; daemon = undefined;
  return status();
});
ipcMain.handle('intercat:pickup', async (event, request) => {
  authorize(event);
  if (preview || !request || Object.keys(request).sort().join(',') !== 'opId,sessionId,trackId'
    || ![request.opId, request.trackId, request.sessionId].every(value => typeof value === 'string' && value.length <= 128)) throw new Error('Invalid recovery request');
  return pickupFromFiles(await connection(), request);
});
ipcMain.handle('intercat:pickup-cancel', async (event, request) => {
  authorize(event);
  if (preview || !request || Object.keys(request).sort().join(',') !== 'opId,recoveryId,sessionId,trackId'
    || ![request.opId, request.recoveryId, request.trackId, request.sessionId].every(value => typeof value === 'string' && value.length <= 128)) throw new Error('Invalid cancellation request');
  return cancelFileRecovery(await connection(), request);
});
ipcMain.handle('intercat:branch', async (event, request) => {
  authorize(event);
  if (preview || !request || Object.keys(request).sort().join(',') !== 'opId,parentTrackId,turnId'
    || ![request.opId, request.parentTrackId, request.turnId].every(value => typeof value === 'string' && value.length <= 128)) throw new Error('Invalid branch request');
  return branchFromCheckpoint(await connection(), request);
});
ipcMain.handle('intercat:integration-command', async (event, request) => {
  authorize(event);
  if (preview || !request || Object.keys(request).sort().join(',') !== 'action,opId' || typeof request.opId !== 'string' || !request.action || typeof request.action !== 'object') throw new Error('Invalid Commit request');
  return (await connection()).request('integration.command', request);
});
ipcMain.handle('intercat:integration-diff', async (event, request) => {
  authorize(event);
  if (preview || !request || Object.keys(request).sort().join(',') !== 'id,path,revision,track' || typeof request.path !== 'string' || request.path.length > 16384 || typeof request.id !== 'string' || typeof request.track !== 'string' || !Number.isSafeInteger(request.revision)) throw new Error('Invalid diff request');
  return (await connection()).request('integration.diff', request);
});
for (const [channel, method, keys] of [
  ['repair-start', 'repair.start', 'jobId,opId,purpose,revision,track'],
  ['repair-followup', 'repair.followup', 'id,opId,revision,selection,text'],
  ['repair-refresh', 'repair.refresh', 'jobId,opId,repairId,revision'],
  ['repair-control', 'repair.control', 'action,id,opId,revision'],
  ['repair-stop', 'repair.stop', 'id,opId'],
  ['repair-answer', 'repair.answer', 'allow,approvalId,id,opId,requirement'],
  ['repair-respond', 'repair.respond', 'id,opId,promptId,response,revision'],
]) ipcMain.handle(`intercat:${channel}`, async (event, request) => {
  authorize(event);
  if (preview || !request || Object.keys(request).sort().join(',') !== keys || typeof request.opId !== 'string') throw new Error('Invalid repair request');
  return (await connection()).request(method, request);
});
ipcMain.handle('intercat:timeline-window', async (event, request) => {
  authorize(event);
  if (!request || Object.keys(request).sort().join(',') !== 'end,since,start,tracks' || !Array.isArray(request.tracks)) throw new Error('Invalid timeline window');
  return preview ? { revision: 0, reset: true, clips: [], removed: [] } : (await connection()).request('timeline.window', request);
});
ipcMain.handle('intercat:output-flows', async event => {
  authorize(event);
  return preview ? [] : (await connection()).request('system.outputFlows', {});
});
ipcMain.handle('intercat:worker-secret', async (event, request) => {
  authorize(event);
  if (preview || !request || Object.keys(request).sort().join(',') !== 'fingerprintOpId,name,opId,owner,question,scope,sessionId,trackId,turnId,value'
    || !['once', 'project'].includes(request.scope)
    || !validSecretName(request.name) || typeof request.value !== 'string' || !request.value || Buffer.byteLength(request.value) > 8192 || request.value.includes('\0')) throw new Error('Invalid worker secret.');
  const client = await connection();
  const { fingerprintOpId, scope, ...install } = request;
  if (scope === 'project') {
    return changeProjectSecret(client, { opId: request.opId, fingerprintOpId, name: request.name, value: request.value, expectedRevision: null }, install);
  }
  await registerFingerprint(client, { opId: fingerprintOpId, trackId: request.trackId, sessionId: request.sessionId, name: request.name, value: request.value });
  return client.request('worker.secret', install);
});
ipcMain.handle('intercat:operation-id', event => { authorize(event); return operationId(); });
ipcMain.handle('intercat:project-secret-usage', async event => { authorize(event); return preview ? [] : (await connection()).request('secret.project.usage', {}); });
ipcMain.handle('intercat:project-secret-settings', async event => { authorize(event); return projectSecretSettings(await connection()); });
ipcMain.handle('intercat:project-secret-retry', async event => {
  authorize(event); const client = await connection();
  await synchronizeProjectSecrets(client, operationId());
  return projectSecretSettings(client);
});
ipcMain.handle('intercat:project-secret-remove', async (event, request) => {
  authorize(event);
  if (preview || !request || Object.keys(request).sort().join(',') !== 'name,opId,revision' || !validSecretName(request.name) || !Number.isSafeInteger(request.revision)) throw new Error('Invalid project secret removal.');
  const client = await connection();
  await changeProjectSecret(client, { opId: request.opId, fingerprintOpId: request.opId, name: request.name, value: null, expectedRevision: request.revision });
  return { removed: true };
});
ipcMain.handle('intercat:project-secret-apply', async (event, revision) => { authorize(event); if (preview || !Number.isSafeInteger(revision)) throw new Error('Invalid project revision.'); return (await connection()).request('secret.project.apply', { revision }); });
ipcMain.handle('intercat:project-secret-process', async (event, request) => { authorize(event); if (preview || !request || Object.keys(request).sort().join(',') !== 'identity,keep,nonce' || typeof request.keep !== 'boolean' || typeof request.nonce !== 'string') throw new Error('Invalid process decision.'); return (await connection()).request('secret.project.process', request); });
ipcMain.handle('intercat:restore-deletion', async (event, request) => {
  authorize(event);
  if (preview || !request || Object.keys(request).sort().join(',') !== 'opId,repairRunId,snapshotId,trackId' || (request.repairRunId !== null && typeof request.repairRunId !== 'string') || typeof request.trackId !== 'string' || typeof request.snapshotId !== 'string') throw new Error('Invalid restore request');
  return (await connection()).request('deletion.restore', request);
});
ipcMain.handle('intercat:dispatch-pause', async (event, request) => {
  authorize(event);
  if (preview || !request || Object.keys(request).sort().join(',') !== 'opId,paused' || typeof request.opId !== 'string' || typeof request.paused !== 'boolean') throw new Error('Invalid pause request');
  return (await connection()).request('system.pause', request);
});
ipcMain.handle('intercat:quit-preference', async (event, value) => {
  authorize(event);
  return value === undefined ? quitting.preference() : quitting.setPreference(value);
});
ipcMain.handle('intercat:stops', async (event, request) => {
  authorize(event);
  if (preview || !request || Object.keys(request).sort().join(',') !== 'opId,stops' || !['irreversible-only','every-approval'].includes(request.stops)) throw new Error('Invalid stops setting');
  return (await connection()).request('policy.stops', request);
});
ipcMain.handle('intercat:clip-detail', async (event, clipId) => {
  authorize(event);
  if (preview || typeof clipId !== 'string' || !/^[0-9a-f-]{36}$/.test(clipId)) throw new Error('Invalid step');
  return (await connection()).request('timeline.detail', { clipId });
});
ipcMain.handle('intercat:report-response', async (event, request) => {
  authorize(event);
  if (preview || !request || Object.keys(request).sort().join(',') !== 'answer,opId,questionId,revision' || typeof request.questionId !== 'string' || !Number.isSafeInteger(request.revision)) throw new Error('Invalid request');
  return (await connection()).request('report.respond', request);
});
ipcMain.handle('intercat:editor-install', async event => {
  authorize(event);
  if (preview) return null;
  const window = BrowserWindow.fromWebContents(event.sender);
  const python = await dialog.showOpenDialog(window, { title: 'Choose Python in the MLX environment', properties: ['openFile'] });
  if (python.canceled) return null;
  return (await connection()).request('editor.download', { opId: operationId(), python: python.filePaths[0] });
});
ipcMain.handle('intercat:editor-retry', async (event, opId) => { authorize(event); if (preview) throw new Error('Editor control is unavailable in preview.'); return (await connection()).request('editor.retry', { opId }); });
ipcMain.handle('intercat:editor-download', async (event, opId) => { authorize(event); if (preview) throw new Error('Editor control is unavailable in preview.'); return (await connection()).request('editor.download', { opId }); });
ipcMain.handle('intercat:editor-pause', async event => { authorize(event); if (preview) throw new Error('Editor control is unavailable in preview.'); return (await connection()).request('editor.pauseDownload', {}); });
ipcMain.handle('intercat:project-setup', async event => {
  authorize(event);
  if (preview) return { project: null };
  const window = BrowserWindow.fromWebContents(event.sender);
  const selected = await dialog.showOpenDialog(window, { title: 'Choose a clean project root folder', properties: ['openDirectory'] });
  if (selected.canceled) return { project: null };
  try {
    const client = await connection();
    const project = await client.request('project.configure', { opId: operationId(), path: selected.filePaths[0] });
    await synchronizeProjectSecrets(client, operationId());
    return { project };
  } catch (error) { return { project: null, error: error.message }; }
});
ipcMain.handle('intercat:worker-setup', async event => {
  authorize(event);
  if (preview) return { configured: false };
  const window = BrowserWindow.fromWebContents(event.sender);
  const selected = await dialog.showOpenDialog(window, { title: 'Choose the Muse Code executable', properties: ['openFile'] });
  if (selected.canceled) return { configured: false };
  try {
    return await (await connection()).request('worker.configure', { opId: operationId(), source: selected.filePaths[0], credentials: path.join(app.getPath('home'), '.config/muse/auth.json') });
  } catch (error) { return { configured: false, error: error.message }; }
});
ipcMain.handle('intercat:worker-retry', async (event, request) => {
  authorize(event);
  if (preview || !request || Object.keys(request).sort().join(',') !== 'opId,trackId' || typeof request.trackId !== 'string' || typeof request.opId !== 'string') throw new Error('Invalid request');
  return (await connection()).request('worker.retry', request);
});
for (const action of ['start', 'stop', 'hold', 'answer', 'respond']) {
  ipcMain.handle(`intercat:worker-${action}`, async (event, request) => {
    authorize(event);
    const keys = action === 'answer' ? 'allow,approvalId,opId,requirement,trackId' : action === 'respond' ? 'opId,promptId,response,revision,trackId' : action === 'stop' ? 'opId,trackId,turnId' : 'opId,trackId';
    if (preview || !request || Object.keys(request).sort().join(',') !== keys || typeof request.trackId !== 'string' || typeof request.opId !== 'string') throw new Error('Invalid request');
    if (action === 'stop' && request.turnId !== null && (typeof request.turnId !== 'string' || !/^[a-f0-9-]{36}$/.test(request.turnId))) throw new Error('Invalid turn');
    return (await connection()).request(`worker.${action}`, request);
  });
}
const importReference = referenceImporter({
  operationId, client: connection,
  choose: async () => {
    const result = await dialog.showOpenDialog(window, { title: 'Attach a reference file', properties: ['openFile'], filters: [{ name: 'Text and source files', extensions: ['txt','md','json','csv','log','js','ts','tsx','jsx','py','rs','go','c','h','cpp','java','swift','kt','html','css','yml','yaml','toml','xml','sql','sh'] }] });
    return result.canceled ? null : result.filePaths[0];
  },
});
ipcMain.handle('intercat:attach-reference', async (event, reselect) => {
  authorize(event);
  if (preview || typeof reselect !== 'boolean') throw new Error('Connect a project to attach a reference.');
  return importReference(reselect);
});
ipcMain.handle('intercat:stage', async (event, request) => {
  authorize(event);
  if (preview || !request || Object.keys(request).sort().join(',') !== 'attachmentIds,opId,selectedTrackId,target,text' || !Array.isArray(request.attachmentIds) || request.attachmentIds.length > 128 || request.attachmentIds.some(id => typeof id !== 'string' || !/^[a-f0-9-]{36}$/.test(id)) || typeof request.text !== 'string' || !request.text.trim() || Buffer.byteLength(request.text) > 32 * 1024 || typeof request.opId !== 'string' || !/^[a-f0-9]{8}-[a-f0-9]{4}-7[a-f0-9]{3}-[89ab][a-f0-9]{3}-[a-f0-9]{12}$/.test(request.opId)) throw new Error('Invalid request');
  return (await connection()).request('utterance.create', request);
});
ipcMain.handle('intercat:cut', async (event, id) => {
  authorize(event);
  if (preview || typeof id !== 'string' || id.length > 64) throw new Error('Invalid request');
  return (await connection()).request('utterance.cut', { opId: operationId(), utteranceId: id });
});
ipcMain.handle('intercat:start-held', async (event, request) => {
  authorize(event);
  if (preview || !request || Object.keys(request).sort().join(',') !== 'opId,revision,utteranceId' || typeof request.opId !== 'string' || typeof request.utteranceId !== 'string' || !Number.isSafeInteger(request.revision) || request.revision < 1) throw new Error('Invalid held request');
  return (await connection()).request('utterance.start', request);
});
ipcMain.handle('intercat:resolve', async (event, request) => {
  authorize(event);
  if (preview || !request || Object.keys(request).sort().join(',') !== 'opId,resolution,revision,utteranceId' || typeof request.utteranceId !== 'string' || request.utteranceId.length > 64 || !Number.isSafeInteger(request.revision) || request.revision < 1) throw new Error('Invalid request');
  return (await connection()).request('utterance.resolve', request);
});
ipcMain.handle('intercat:preparation-retry', async (event, opId) => {
  authorize(event);
  if (preview || typeof opId !== 'string') throw new Error('Invalid request');
  return (await connection()).request('preparation.retry', { opId });
});

app.whenReady().then(() => {
  if (!singleInstance) return;
  session.defaultSession.setPermissionRequestHandler((_contents, _permission, callback) => callback(false));
  session.defaultSession.setPermissionCheckHandler(() => false);
  protocol.handle('intercat', async (request) => {
  const url = new URL(request.url);
  if (url.host !== 'app' || request.method !== 'GET') return new Response(null, { status: 403 });
  let requested;
  try { requested = decodeURIComponent(url.pathname); } catch { return new Response(null, { status: 400 }); }
  const target = path.resolve(assets, `.${requested === '/' ? '/index.html' : requested}`);
  if (!target.startsWith(assets + path.sep)) return new Response(null, { status: 403 });
  try {
    const response = await net.fetch(pathToFileURL(target).href);
    const headers = new Headers(response.headers);
    headers.set('Content-Security-Policy', csp);
    headers.set('X-Content-Type-Options', 'nosniff');
    return new Response(response.body, { status: response.status, headers });
  } catch { return new Response(null, { status: 404 }); }
  });
  createWindow();
  const menu = Menu.getApplicationMenu() ?? Menu.buildFromTemplate([{ role: 'appMenu' }, { role: 'editMenu' }, { role: 'viewMenu' }, { role: 'windowMenu' }]);
  menu.append(new MenuItem({ label: 'Agents', submenu: [
    { label: 'Stop all turns', enabled: !preview, accelerator: 'Alt+Command+.', click: () => { void connection().then(client => client.request('system.pause', { opId: operationId(), paused: true })).catch(error => dialog.showErrorBox('Could not stop all turns', error.message)); } },
    { label: 'Stop all turns and processes…', enabled: !preview, click: () => { void quitting.stopProcesses().catch(error => dialog.showErrorBox('Could not stop processes', error.message)); } },
  ] }));
  Menu.setApplicationMenu(menu);
  app.on('activate', () => { if (BrowserWindow.getAllWindows().length === 0) createWindow(); });
  app.on('second-instance', () => { const window = BrowserWindow.getAllWindows()[0] ?? createWindow(); window.show(); window.focus(); });
});

function createWindow() {
  const window = new BrowserWindow({
    width: 1440, height: 900, minWidth: 1040, minHeight: 720,
    backgroundColor: '#0c0d10', titleBarStyle: 'hiddenInset',
    webPreferences: { preload: path.join(root, 'electron/preload.cjs'), sandbox: true, contextIsolation: true, nodeIntegration: false, webSecurity: true, spellcheck: false },
  });
  window.webContents.setWindowOpenHandler(() => ({ action: 'deny' }));
  window.webContents.on('will-navigate', (event, url) => { if (!url.startsWith('intercat://app/')) event.preventDefault(); });
  void window.loadURL('intercat://app/');
  return window;
}

