import { registerComparison } from './comparison.mjs';
import {validateFresh} from './fresh-input.mjs';
import { validatePush } from './push-input.mjs';
import { validatePull,githubLink } from './pull-input.mjs';
import { privacyPreview, privacyText } from './privacy.mjs';
import { app, BrowserWindow, dialog, ipcMain, Menu, MenuItem, net, powerMonitor, protocol, session, shell } from 'electron';
import path from 'node:path';
import { execFile } from 'node:child_process';
import { applyMergeInput } from './merge-input.mjs';
import { promisify } from 'node:util';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { connectDaemon, operationId, uiToken } from './daemon.mjs';
import { registerFingerprint, validSecretName, receiptMismatch } from './secrets.mjs';
import { branchFromCheckpoint, validateBranchRequest } from './branches.mjs';
import { validateTwoWays,validateTwoWaysEdit,finishTwoWays } from './two-ways.mjs';
import { pickupFromFiles, cancelFileRecovery } from './pickup.mjs';
import { referenceImporter } from './attachments.mjs';
import { rebuildProjectFingerprints, reviewBackup, applyBackup } from './fingerprint-recovery.mjs';
import { changeProjectSecret, synchronizeProjectSecrets, projectSecretSettings, projectSecretIssue } from './project-secrets.mjs';
import { reviewExternal, confirmExternal } from './external-content.mjs';
import { quitController } from './quit.mjs';
import { draftStore } from './drafts.mjs';
import { pastedTextStore } from './pasted-text.mjs';
import { editorPresentation } from './editor-presentation.mjs';
import { commandPlan, observePlans, planWindowFocused, replyPlan } from './plans.mjs';
import { ProjectSetup } from './project-setup.mjs';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const assets = path.join(root, 'dist');
const csp = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; font-src 'self'; connect-src 'none'; worker-src 'self' blob:; object-src 'none'; base-uri 'self'; frame-src 'none'";

protocol.registerSchemesAsPrivileged([{ scheme: 'intercat', privileges: { standard: true, secure: true, supportFetchAPI: true } }]);
app.setName('Intercat');
const singleInstance = app.requestSingleInstanceLock();
if (!singleInstance) app.quit();
const preview = process.argv.includes('--preview');
const presentation = editorPresentation({ directory: app.getPath('userData') });
const drafts = draftStore(connection);
const projectSetup = new ProjectSetup(connection, operationId);
const pastedText = pastedTextStore({ directory: path.join(app.getPath('userData'), 'pasted-text'), client: connection });
let daemon;
let blockedConnection;
function connection() {
  if (blockedConnection) return Promise.reject(blockedConnection);
  daemon ??= uiToken().then(token => connectDaemon(path.join(root, 'target/debug/intercatd'), path.join(app.getPath('userData'), 'daemon'), token, path.join(root, 'services/editor/src'), path.join(root, '.tools/gitleaks-8.30.1/gitleaks'), path.join(root, '.tools/git-lfs-3.8.1/git-lfs')));
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
const windowOwners = new Map();
const viewRevisions = new Map();
const planObservations = new Map();
async function disconnectWindow(contents) {
  const owner = windowOwners.get(contents.id);
  if (!owner) return;
  windowOwners.delete(contents.id);
  const revision = (viewRevisions.get(owner) ?? 0) + 1;
  viewRevisions.delete(owner);
  const planSequence = (planObservations.get(owner) ?? 0) + 1;
  planObservations.delete(owner);
  try {
    const client = daemon ? await daemon : null;
    if (!client) return;
    await Promise.allSettled([
      client.request('utterance.edit', { owner, action: { kind: 'disconnect' } }),
      client.request('plan.edit', { owner, action: { kind: 'disconnect' } }),
      client.request('timeline.observe', { owner, revision, view: null }),
      client.request('plan.observe', { owner, sequence: planSequence, idle: false, plans: [] }),
    ]);
  }
  catch { /* Observations expire, and interrupted edits become held. */ }
}
function windowOwner(contents) {
  if (windowOwners.has(contents.id)) return windowOwners.get(contents.id);
  const owner = operationId();
  windowOwners.set(contents.id, owner);
  const disconnected = () => {
    if (windowOwners.get(contents.id) !== owner) return;
    contents.off('destroyed', disconnected);
    contents.off('render-process-gone', disconnected);
    contents.off('did-start-navigation', disconnected);
    void disconnectWindow(contents);
  };
  contents.once('destroyed', disconnected);
  contents.once('render-process-gone', disconnected);
  contents.once('did-start-navigation', disconnected);
  return owner;
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
  return { ...(await status()), projectSelection: projectSetup.current(event.sender.id)?.selection ?? null };
});
ipcMain.handle('intercat:drafts-read', async (event, session) => {
  authorize(event);
  if (preview) throw new Error('Draft storage is unavailable in preview.');
  return drafts.read(session);
});
ipcMain.handle('intercat:drafts-write', async (event, request) => {
  authorize(event);
  if (preview) throw new Error('Draft storage is unavailable in preview.');
  return drafts.write(request, event.sender.id);
});
ipcMain.handle('intercat:drafts-discard', async (event, session) => {
  authorize(event);
  if (preview) throw new Error('Draft storage is unavailable in preview.');
  return drafts.discard(session, event.sender.id);
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
for(const [channel,method] of [['fresh-prepare','worker.fresh.prepare'],['fresh-command','worker.fresh.command'],['fresh-read','worker.fresh.read']])ipcMain.handle(`intercat:${channel}`,async(event,request)=>{
  authorize(event);
  if(preview)throw new Error('Connect workers before starting a fresh conversation.');
  return (await connection()).request(method,validateFresh(method,request));
});
ipcMain.handle('intercat:pickup-cancel', async (event, request) => {
  authorize(event);
  if (preview || !request || Object.keys(request).sort().join(',') !== 'opId,recoveryId,sessionId,trackId'
    || ![request.opId, request.recoveryId, request.trackId, request.sessionId].every(value => typeof value === 'string' && value.length <= 128)) throw new Error('Invalid cancellation request');
  return cancelFileRecovery(await connection(), request);
});
for(const [channel,method] of [['two-ways-read','two-ways.read'],['two-ways-command','two-ways.command'],['two-ways-options','two-ways.options']])ipcMain.handle(`intercat:${channel}`,async(event,request)=>{
  authorize(event);if(preview)throw new Error('Connect to the daemon before preparing two paths.');
  return (await connection()).request(method,validateTwoWays(method,request));
});
ipcMain.handle('intercat:two-ways-finish',async(event,request)=>{
  authorize(event);if(preview)throw new Error('Connect to the daemon before confirming two paths.');
  return finishTwoWays(await connection(),validateTwoWays('two-ways.read',request));
});
ipcMain.handle('intercat:branch-points', async (event, request) => {
  authorize(event);
  if (preview || !request || Object.keys(request).sort().join(',') !== 'before,range,trackId'
    || typeof request.trackId !== 'string' || !request.trackId.length || request.trackId.length > 128
    || request.before !== null && (!Number.isSafeInteger(request.before) || request.before <= 0)
    || request.range !== null && (!request.range || Object.keys(request.range).sort().join(',') !== 'end,start' || !Number.isSafeInteger(request.range.start) || !Number.isSafeInteger(request.range.end) || request.range.start < 0 || request.range.end < request.range.start)) throw new Error('Invalid branch point query');
  return (await connection()).request('branch.points', request);
});
ipcMain.handle('intercat:branch', async (event, request) => {
  authorize(event);
  if (preview) throw new Error('Branching needs a connected daemon');
  validateBranchRequest(request);
  return branchFromCheckpoint(await connection(), request);
});
ipcMain.handle('intercat:pull-github',async(event,request)=>{
  authorize(event);
  if(preview||!request||Object.keys(request).sort().join(',')!=='folderId,id,kind'||typeof request.folderId!=='string'||request.folderId.length>128||!['pull','branch'].includes(request.kind))throw new Error('Invalid saved GitHub link');
  const job=await(await connection()).request('pull.read',validatePull('pull.read',{id:request.id}));
  await shell.openExternal(githubLink(job,request.folderId,request.kind));
});
registerComparison({ ipcMain, authorize, preview, connection });
for(const [channel,method] of [['pull-open','pull.open'],['pull-command','pull.command'],['pull-read','pull.read']])ipcMain.handle(`intercat:${channel}`,async(event,request)=>{
  authorize(event);
  if(preview)throw new Error('Connect to the local daemon before opening PR.');
  return (await connection()).request(method,validatePull(method,request));
});
for (const [channel,method] of [['push-remotes','push.remotes'],['push-review','push.review'],['push-presentation','push.presentation'],['push-command','push.command']]) ipcMain.handle(`intercat:${channel}`,async(event,request)=>{
  authorize(event);
  if(preview)throw new Error('Connect to the local daemon before Push.');
  return (await connection()).request(method,validatePush(method,request));
});
ipcMain.handle('intercat:integration-command', async (event, request) => {
  authorize(event);
  if (preview || !request || Object.keys(request).sort().join(',') !== 'action,opId' || typeof request.opId !== 'string' || !request.action || typeof request.action !== 'object') throw new Error('Invalid Commit request');
  return (await connection()).request('integration.command', request);
});
ipcMain.handle('intercat:integration-diff', async (event, request) => {
  authorize(event);
  if (preview || !request || Object.keys(request).sort().join(',') !== 'id,owner,path,revision' || typeof request.path !== 'string' || request.path.length > 16384 || typeof request.id !== 'string' || !Number.isSafeInteger(request.revision) || request.revision<1 || !request.owner || (request.owner.kind==='worker' ? Object.keys(request.owner).sort().join(',')!=='kind,track'||typeof request.owner.track!=='string' : request.owner.kind!=='undo'||Object.keys(request.owner).join(',')!=='kind')) throw new Error('Invalid diff request');
  return (await connection()).request('integration.diff', request);
});
for (const [channel, method, keys] of [
  ['repair-start', 'repair.start', 'opId,owner,purpose,track'],
  ['repair-followup', 'repair.followup', 'id,opId,revision,selection,text'],
  ['repair-refresh', 'repair.refresh', 'jobId,opId,repairId,revision'],
  ['repair-control', 'repair.control', 'action,id,opId,revision'],
  ['repair-processes', 'repair.processes', 'action,id,nonce,opId,revision'],
  ['repair-stop', 'repair.stop', 'id,opId'],
  ['repair-answer', 'repair.answer', 'approvalId,id,opId,requirement,response'],
  ['repair-respond', 'repair.respond', 'id,opId,promptId,response,revision'],
  ['merge-begin', 'merge.begin', 'id,opId,revision'],
  ['merge-start', 'merge.start', 'opId,owner,purpose,track'],
  ['merge-return', 'merge.return', 'id,opId,revision'],
  ['realign-request', 'realign.request', 'opId,source,trackId'],
  ['realign-control', 'realign.control', 'action,id,opId,revision'],
  ['path-restore','path.restore','opId,retirementId'],
  ['path-restore-control','path.restore-control','action,id,opId,revision'],
  ['path-restore-access','path.restore-access','fingerprint,opId,request'],
  ['aside-process-stop','path.stop-processes','opId,retirementId'],
  ['commit-group-open','commit.group.open','opId,tracks'],
  ['commit-group-commit','commit.group.commit','id,opId,revision,targets'],
  ['commit-group-discard','commit.group.discard','id,opId,revision'],
]) ipcMain.handle(`intercat:${channel}`, async (event, request) => {
  authorize(event);
  if (preview || !request || Object.keys(request).sort().join(',') !== keys || typeof request.opId !== 'string') throw new Error('Invalid repair request');
  if(['repair-start','merge-start'].includes(channel)&&(!request.owner||Object.keys(request.owner).sort().join(',')!=='id,kind,revision'||!['commit','realign','undo','shared','restore'].includes(request.owner.kind)||typeof request.owner.id!=='string'||request.owner.id.length>128||!Number.isSafeInteger(request.owner.revision)||request.owner.revision<1))throw new Error('Invalid integration Run owner');
  if (channel === 'repair-processes' && (typeof request.id !== 'string' || typeof request.nonce !== 'string' || !Number.isSafeInteger(request.revision) || request.revision < 0 || !['keep', 'stop', 'retry-cleanup'].includes(request.action))) throw new Error('Invalid Run process request');
  if (channel === 'realign-request' && (typeof request.trackId !== 'string' || request.trackId.length > 128 || !request.source || !['commit','reparent'].includes(request.source.kind) || Object.keys(request.source).sort().join(',') !== (request.source.kind==='commit'?'jobId,kind,revision':'jobId,kind,parent,revision') || typeof request.source.jobId !== 'string' || request.source.jobId.length>128 || !Number.isSafeInteger(request.source.revision) || request.source.revision<1 || request.source.kind==='reparent' && (typeof request.source.parent!=='string'||request.source.parent.length>128))) throw new Error('Invalid realignment source');
  if (channel === 'realign-control' && (!Number.isSafeInteger(request.revision)||request.revision<1||typeof request.id!=='string'||!request.action||typeof request.action!=='object')) throw new Error('Invalid realignment control');
  if(channel==='commit-group-open'&&request.tracks!==null&&(!Array.isArray(request.tracks)||request.tracks.length>256||request.tracks.some(track=>typeof track!=='string'||track.length>128)))throw new Error('Invalid group tracks');
  if(channel.startsWith('commit-group-')&&channel!=='commit-group-open'&&(typeof request.id!=='string'||!Number.isSafeInteger(request.revision)||request.revision<1))throw new Error('Invalid all-folder review');
  if(channel==='commit-group-commit'&&(!Array.isArray(request.targets)||request.targets.length>8||request.targets.some(target=>!target||Object.keys(target).sort().join(',')!=='head,jobId,revision,settingsHash,tree'||!['head','jobId','settingsHash','tree'].every(key=>typeof target[key]==='string'&&target[key].length<=128)||!Number.isSafeInteger(target.revision)||target.revision<1)))throw new Error('Invalid all-folder candidate');
  return (await connection()).request(method, request);
});
ipcMain.handle('intercat:aside-processes',async(event,request)=>{
  authorize(event);
  if(preview||!request||Object.keys(request).join(',')!=='retirementId'||typeof request.retirementId!=='string'||request.retirementId.length>128)throw new Error('Invalid retired path');
  return (await connection()).request('path.processes',request);
});
ipcMain.handle('intercat:merge-apply',async(event,request)=>{
  authorize(event);
  if(preview||!request||Object.keys(request).sort().join(',')!=='files,id,opId,revision')throw new Error('Invalid manual resolution');
  return applyMergeInput({directory:path.join(app.getPath('userData'),'daemon','merge-inputs'),client:connection,request});
});
ipcMain.handle('intercat:merge-read', async (event, request) => {
  authorize(event);
  if (preview || !request || Object.keys(request).sort().join(',') !== 'id,path,revision' || typeof request.id !== 'string' || !Number.isSafeInteger(request.revision) || request.path !== null && typeof request.path !== 'string') throw new Error('Invalid conflict query');
  return (await connection()).request('merge.read',request);
});
ipcMain.handle('intercat:merge-open', async (event, request) => {
  authorize(event);
  if (preview || !request || Object.keys(request).sort().join(',') !== 'id,opId,path,revision' || typeof request.path !== 'string' || typeof request.opId !== 'string') throw new Error('Invalid conflict editor request');
  const file = await (await connection()).request('merge.open', request);
  if (typeof file !== 'string' || !path.isAbsolute(file)) throw new Error('Invalid conflict editor path');
  await promisify(execFile)('/usr/bin/open',['-t',file],{timeout:10000});
});
ipcMain.handle('intercat:editor-log', async (event, request) => {
  authorize(event);
  if (preview || !request || Object.keys(request).sort().join(',') !== 'before,trackId'
    || typeof request.trackId !== 'string' || !request.trackId.length || request.trackId.length > 128
    || request.before !== null && (!Number.isSafeInteger(request.before) || request.before <= 0)) throw new Error('Invalid Editor log query');
  return (await connection()).request('editor.log', request);
});
ipcMain.handle('intercat:editor-presentation', event => {
  authorize(event);
  return presentation.read(event.sender.id);
});
ipcMain.handle('intercat:editor-character', (event, value) => {
  authorize(event);
  return presentation.setCharacter(value);
});
ipcMain.handle('intercat:editor-return-read', (event, id) => {
  authorize(event);
  if (typeof id !== 'string' || id.length > 128) throw new Error('Invalid Editor return');
  presentation.acknowledge(event.sender.id, id);
});
ipcMain.handle('intercat:editor-away', async (event, request) => {
  authorize(event);
  if (preview || !request || Object.keys(request).sort().join(',') !== 'before,from,to'
    || !Number.isSafeInteger(request.from) || !Number.isSafeInteger(request.to) || request.from < 0 || request.to <= request.from
    || request.before !== null && (!Number.isSafeInteger(request.before) || request.before <= 0)) throw new Error('Invalid Editor away query');
  return (await connection()).request('editor.away', request);
});
ipcMain.handle('intercat:timeline-window', async (event, request) => {
  authorize(event);
  if (!request || Object.keys(request).sort().join(',') !== 'end,since,start,tracks' || !Array.isArray(request.tracks)) throw new Error('Invalid timeline window');
  return preview ? { revision: 0, reset: true, clips: [], removed: [] } : (await connection()).request('timeline.window', request);
});
ipcMain.handle('intercat:timeline-observe', async (event, view) => {
  authorize(event);
  if (view !== null && (!view || Object.keys(view).sort().join(',') !== 'repairRunId,trackId'
      || typeof view.trackId !== 'string' || (view.repairRunId !== null && typeof view.repairRunId !== 'string'))) throw new Error('Invalid timeline observation');
  if (preview) return null;
  const owner = windowOwner(event.sender);
  const revision = (viewRevisions.get(owner) ?? 0) + 1;
  viewRevisions.set(owner, revision);
  return (await connection()).request('timeline.observe', { owner, revision, view });
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
ipcMain.handle('intercat:execution-limits', async event => {
  authorize(event);
  if (preview) return { revision: 0, parallelWorkers: 4, plannerReviewerTurns: 2 };
  return (await connection()).request('execution-limits.get', {});
});
ipcMain.handle('intercat:plan-reply', async (event, request) => {
  authorize(event);
  if (preview) throw new Error('Plan replies are unavailable in preview.');
  return replyPlan(connection, request);
});
ipcMain.handle('intercat:plan-command', async (event, request) => {
  authorize(event);
  if (preview) throw new Error('Plans are unavailable in preview.');
  return commandPlan(connection, request);
});
ipcMain.handle('intercat:review-stop', async (event, request) => { authorize(event); if (preview) throw new Error('Reviews are unavailable in preview.'); return (await connection()).request('review.stop', request); });
ipcMain.handle('intercat:review-retry', async (event, request) => {
  authorize(event);
  if (preview) throw new Error('Reviews are unavailable in preview.');
  return (await connection()).request('review.retry', request);
});
ipcMain.handle('intercat:review-fix', async (event, request) => {
  authorize(event);
  if (preview) throw new Error('Review fixes are unavailable in preview.');
  return (await connection()).request('review.fix', request);
});
ipcMain.handle('intercat:plans-start-get', async event => {
  authorize(event);
  return preview ? { revision: 0, mode: 'manual' } : (await connection()).request('plans-start.get', {});
});
ipcMain.handle('intercat:plans-start-change', async (event, request) => {
  authorize(event);
  if (preview) throw new Error('Plans start is unavailable in preview.');
  return (await connection()).request('plans-start.change', request);
});
ipcMain.handle('intercat:plan-observe', async (event, request) => {
  authorize(event);
  if (preview) return { countdowns: [], started: [] };
  const owner = windowOwner(event.sender);
  const sequence = (planObservations.get(owner) ?? 0) + 1;
  planObservations.set(owner, sequence);
  return observePlans(connection, request, { owner, sequence, focused: () => {
    const win = BrowserWindow.fromWebContents(event.sender);
    return planWindowFocused(win, event.sender, windowOwners.get(event.sender.id), owner);
  } });
});
ipcMain.handle('intercat:execution-limits-change', async (event, request) => {
  authorize(event);
  if (preview) return { revision: request.revision + 1, parallelWorkers: request.parallelWorkers, plannerReviewerTurns: request.plannerReviewerTurns };
  return (await connection()).request('execution-limits.change', request);
});
ipcMain.handle('intercat:checks', async event => {
  authorize(event);
  if (preview) return { setting: { revision: 0, folders: {} }, folders: [] };
  return (await connection()).request('checks.get', {});
});
ipcMain.handle('intercat:checks-change', async (event, request) => {
  authorize(event);
  if (preview || !request || Object.keys(request).sort().join(',') !== 'command,folder,history,opId,revision' || typeof request.folder !== 'string' || typeof request.history !== 'boolean' || !Number.isSafeInteger(request.revision)) throw new Error('Invalid Check settings change.');
  return (await connection()).request('checks.change', request);
});
ipcMain.handle('intercat:check-environment', async event => {
  authorize(event);
  if (preview) return { selection: { revision: 0, file: null }, candidates: [], available: false };
  return (await connection()).request('check-environment.get', {});
});
ipcMain.handle('intercat:check-environment-choose', async event => {
  authorize(event);
  const window = BrowserWindow.fromWebContents(event.sender);
  const selected = await dialog.showOpenDialog(window, { title: 'Choose an untracked Check environment file', properties: ['openFile', 'showHiddenFiles'] });
  return selected.canceled ? null : selected.filePaths[0] ?? null;
});
ipcMain.handle('intercat:check-environment-change', async (event, request) => {
  authorize(event);
  if (preview || !request || Object.keys(request).sort().join(',') !== 'file,opId,revision' || (request.file !== null && typeof request.file !== 'string') || !Number.isSafeInteger(request.revision)) throw new Error('Invalid Check environment change.');
  return (await connection()).request('check-environment.change', request);
});
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
ipcMain.handle('intercat:confirmation-change', async (event, request) => {
  authorize(event);
  if (preview || !request || Object.keys(request).sort().join(',') !== 'action,opId,revision' || typeof request.opId !== 'string' || !Number.isSafeInteger(request.revision) || request.revision < 0) throw new Error('Invalid confirmation preference');
  return (await connection()).request('confirmation.change', request);
});
ipcMain.handle('intercat:utterance-confirm', async (event, request) => {
  authorize(event);
  if (preview || !request || Object.keys(request).sort().join(',') !== 'decisionId,opId,revision,utteranceId' || !['decisionId', 'opId', 'utteranceId'].every(key => typeof request[key] === 'string' && /^[0-9a-f-]{36}$/.test(request[key])) || !Number.isSafeInteger(request.revision) || request.revision < 1) throw new Error('Invalid interpretation confirmation');
  return (await connection()).request('utterance.confirm', request);
});
function validOrigin(identity) {
  return identity && Object.keys(identity).sort().join(',') === 'sessionId,turnId,utteranceId'
    && ['turnId', 'utteranceId'].every(key => typeof identity[key] === 'string' && /^[0-9a-f-]{36}$/.test(identity[key]))
    && typeof identity.sessionId === 'string' && identity.sessionId.length > 0 && identity.sessionId.length <= 512;
}
ipcMain.handle('intercat:utterance-origin', async (event, utteranceId) => {
  authorize(event);
  if (preview || typeof utteranceId !== 'string' || !/^[0-9a-f-]{36}$/.test(utteranceId)) throw new Error('Invalid request');
  return (await connection()).request('utterance.origin', { utteranceId });
});
ipcMain.handle('intercat:utterance-changes', async (event, identity) => {
  authorize(event);
  if (preview || !validOrigin(identity)) throw new Error('Invalid request turn');
  return (await connection()).request('utterance.changes', identity);
});
ipcMain.handle('intercat:utterance-diff', async (event, request) => {
  authorize(event);
  if (preview || !request || Object.keys(request).sort().join(',') !== 'checkpoint,identity,path' || !validOrigin(request.identity)
    || typeof request.checkpoint !== 'string' || typeof request.path !== 'string') throw new Error('Invalid saved changes');
  return (await connection()).request('utterance.diff', request);
});
ipcMain.handle('intercat:utterance-stop', async (event, request) => {
  authorize(event);
  if (preview || !request || Object.keys(request).sort().join(',') !== 'identity,opId,trackId' || !validOrigin(request.identity)
    || typeof request.opId !== 'string' || typeof request.trackId !== 'string') throw new Error('Invalid request stop');
  return (await connection()).request('utterance.stop', request);
});
ipcMain.handle('intercat:utterance-misread', async (event, request) => {
  authorize(event);
  if (preview || !request || Object.keys(request).sort().join(',') !== 'opId,revision,utteranceId'
    || typeof request.opId !== 'string' || typeof request.utteranceId !== 'string' || !Number.isSafeInteger(request.revision)) throw new Error('Invalid correction');
  return (await connection()).request('utterance.misread', request);
});
ipcMain.handle('intercat:utterance-read', async (event, utteranceId) => {
  authorize(event);
  if (preview || typeof utteranceId !== 'string' || !/^[0-9a-f-]{36}$/.test(utteranceId)) throw new Error('Invalid request');
  return (await connection()).request('utterance.read', { utteranceId });
});
ipcMain.handle('intercat:clip-detail', async (event, clipId) => {
  authorize(event);
  if (preview || typeof clipId !== 'string' || !/^[0-9a-f-]{36}$/.test(clipId)) throw new Error('Invalid step');
  return (await connection()).request('timeline.detail', { clipId });
});
ipcMain.handle('intercat:proposal-answer', async (event, request) => {
  authorize(event);
  if (preview) throw new Error('Proposals are unavailable in preview.');
  return (await connection()).request('proposal.respond', request);
});
ipcMain.handle('intercat:assumption-answer', async (event, request) => {
  authorize(event);
  return (await connection()).request('assumption.respond', request);
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
ipcMain.handle('intercat:folder-assign', async (event, request) => { authorize(event); if (preview) throw new Error('Folder selection is unavailable in preview.'); return (await connection()).request('folder.assign', request); });
ipcMain.handle('intercat:project-setup', async event => {
  authorize(event);
  if (preview) return { project: null };
  const current = projectSetup.current(event.sender.id);
  if (current) return current;
  const window = BrowserWindow.fromWebContents(event.sender);
  const selected = await dialog.showOpenDialog(window, { title: 'Choose project folders (up to 8)', properties: ['openDirectory', 'multiSelections'] });
  if (selected.canceled) return { project: null };
  return projectSetup.begin(event.sender.id, selected.filePaths);
});
ipcMain.handle('intercat:project-selection', async (event, request) => {
  authorize(event);
  if (preview) throw new Error('Folder selection is unavailable in preview.');
  return projectSetup.resolve(event.sender.id, request);
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
ipcMain.handle('intercat:inheritance-finish', async (event, request) => {
  authorize(event);
  if (preview || !request || Object.keys(request).sort().join(',') !== 'applicationId,opId,trackId' || !['applicationId','opId','trackId'].every(key => typeof request[key] === 'string')) throw new Error('Invalid request');
  return (await connection()).request('inheritance.finish', request);
});
ipcMain.handle('intercat:checkpoint-exclude', async (event, request) => {
  authorize(event);
  if (preview || !request || Object.keys(request).sort().join(',') !== 'issueId,opId,revision' || typeof request.issueId !== 'string' || typeof request.opId !== 'string' || !Number.isSafeInteger(request.revision) || request.revision < 1) throw new Error('Invalid request');
  return (await connection()).request('checkpoint.exclude', request);
});
ipcMain.handle('intercat:inheritance-issue', async (event, request) => {
  authorize(event);
  if (preview || !request || Object.keys(request).sort().join(',') !== 'action,issueId,opId' || typeof request.issueId !== 'string' || typeof request.opId !== 'string' || !['keep','stop'].includes(request.action)) throw new Error('Invalid request');
  return (await connection()).request('inheritance.issue', request);
});
for (const action of ['start', 'stop', 'hold', 'answer', 'respond']) {
  ipcMain.handle(`intercat:worker-${action}`, async (event, request) => {
    authorize(event);
    const keys = action === 'answer' ? 'approvalId,opId,requirement,response,trackId' : action === 'respond' ? 'opId,promptId,response,revision,trackId' : action === 'stop' ? 'opId,trackId,turnId' : 'opId,trackId';
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
ipcMain.handle('intercat:attach-dropped-reference', async (event, selectedPath) => {
  authorize(event);
  if (preview || typeof selectedPath !== 'string' || selectedPath.length > 4096 || !path.isAbsolute(selectedPath)) throw new Error('Drop a file from your computer.');
  return importReference(false, selectedPath);
});
ipcMain.handle('intercat:privacy-change', async (event, request) => {
  authorize(event);
  if (preview || !request || Object.keys(request).sort().join(',') !== 'mode,opId,revision' || !['basic','full'].includes(request.mode) || typeof request.opId !== 'string' || !Number.isSafeInteger(request.revision) || request.revision < 0) throw new Error('Invalid privacy setting');
  return (await connection()).request('privacy.change', request);
});
ipcMain.handle('intercat:privacy-preview', async (event, texts) => {
  authorize(event);
  if (preview) throw new Error('Privacy preview is unavailable in preview mode.');
  return privacyPreview({ directory: path.join(app.getPath('userData'), 'daemon', 'privacy-previews'), client: connection, texts });
});
ipcMain.handle('intercat:privacy-original', async (event, id) => {
  authorize(event);
  if (preview || !Number.isSafeInteger(id) || id <= 0) throw new Error('Invalid privacy value.');
  return privacyText((await (await connection()).request('privacy.original', { id })).textBase64);
});
ipcMain.handle('intercat:paste-stage', async (event, request) => {
  authorize(event);
  if (preview) throw new Error('Connect a project to paste a reference.');
  return pastedText.stage(request);
});
ipcMain.handle('intercat:paste-finish', async (event, id) => {
  authorize(event);
  if (preview) throw new Error('Connect a project to paste a reference.');
  return pastedText.finish(id);
});
ipcMain.handle('intercat:paste-read', async (event, id) => {
  authorize(event);
  if (preview) throw new Error('Connect a project to read a reference.');
  return pastedText.read(id);
});
ipcMain.handle('intercat:paste-refresh', async (event, request) => {
  authorize(event);
  if (preview) throw new Error('Connect a project to screen a reference.');
  return pastedText.refresh(request);
});
ipcMain.handle('intercat:paste-list', async (event, request) => {
  authorize(event);
  if (preview) throw new Error('Connect a project to list references.');
  return pastedText.references(request);
});
ipcMain.handle('intercat:stage', async (event, request) => {
  authorize(event);
  const keys = request && Object.keys(request).filter(key => !['sources', 'executeAllowed', 'planItem'].includes(key)).sort().join(',');
  if (request?.executeAllowed !== undefined && typeof request.executeAllowed !== 'boolean') throw new Error('Invalid execution permission');
  if (request?.planItem !== undefined && (!request.planItem || Object.keys(request.planItem).sort().join(',') !== 'itemId,planId,revision'
    || ![request.planItem.planId, request.planItem.itemId].every(id => typeof id === 'string' && /^[a-f0-9-]{36}$/.test(id))
    || !Number.isSafeInteger(request.planItem.revision) || request.planItem.revision < 1)) throw new Error('Invalid plan item context');
  if (preview || !request || keys !== 'attachmentIds,opId,selectedTrackId,target,text' || (request.sources !== undefined && (!Array.isArray(request.sources) || request.sources.length > 128)) || !Array.isArray(request.attachmentIds) || request.attachmentIds.length > 128 || request.attachmentIds.some(id => typeof id !== 'string' || !/^[a-f0-9-]{36}$/.test(id)) || typeof request.text !== 'string' || (!request.text.trim() && !request.sources?.length) || Buffer.byteLength(request.text) > 32 * 1024 || typeof request.opId !== 'string' || !/^[a-f0-9]{8}-[a-f0-9]{4}-7[a-f0-9]{3}-[89ab][a-f0-9]{3}-[a-f0-9]{12}$/.test(request.opId)) throw new Error('Invalid request');
  return (await connection()).request('utterance.create', request);
});
ipcMain.handle('intercat:history', async (event, request) => {
  authorize(event);
  if (preview || !request || Object.keys(request).sort().join(',') !== 'action,opId') throw new Error('Invalid history action');
  return { connected: true, ...await (await connection()).request('timeline.history', request) };
});
ipcMain.handle('intercat:native-history', (event, direction) => {
  authorize(event);
  if (direction === 'undo') event.sender.undo();
  else if (direction === 'redo') event.sender.redo();
  else throw new Error('Invalid text edit');
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
ipcMain.handle('intercat:rephrase', async (event, request) => {
  authorize(event);
  if (preview || !request || Object.keys(request).sort().join(',') !== 'opId,revision,text,utteranceId') throw new Error('Invalid rephrased request');
  return (await connection()).request('utterance.rephrase', request);
});
ipcMain.handle('intercat:resolve', async (event, request) => {
  authorize(event);
  if (preview || !request || Object.keys(request).sort().join(',') !== 'opId,resolution,revision,utteranceId' || typeof request.utteranceId !== 'string' || request.utteranceId.length > 64 || !Number.isSafeInteger(request.revision) || request.revision < 1) throw new Error('Invalid request');
  return (await connection()).request('utterance.resolve', request);
});
ipcMain.handle('intercat:plan-edit', async (event, action) => {
  authorize(event);
  if (preview || !action || !['begin', 'renew', 'cancel', 'submit', 'apply', 'retry', 'dependencies'].includes(action.kind)) throw new Error('Invalid plan edit');
  const contents = event.sender;
  const owner = windowOwner(contents);
  const client = await connection();
  if (contents.isDestroyed() || windowOwners.get(contents.id) !== owner) throw new Error('The editing window closed.');
  return client.request('plan.edit', { owner, action });
});
ipcMain.handle('intercat:edit-request', async (event, action) => {
  authorize(event);
  if (preview || !action || !['begin', 'renew', 'apply', 'end'].includes(action.kind)) throw new Error('Invalid request edit');
  if(action.edit?.kind==='tryTwoWays')validateTwoWaysEdit(action);
  const contents = event.sender;
  const owner = windowOwner(contents);
  const client = await connection();
  if (contents.isDestroyed() || windowOwners.get(contents.id) !== owner) throw new Error('The editing window closed.');
  return client.request('utterance.edit', { owner, action });
});
ipcMain.handle('intercat:preparation-retry', async (event, opId) => {
  authorize(event);
  if (preview || typeof opId !== 'string') throw new Error('Invalid request');
  return (await connection()).request('preparation.retry', { opId });
});

app.whenReady().then(() => {
  if (!singleInstance) return;
  powerMonitor.on('lock-screen', () => presentation.lock(true));
  powerMonitor.on('unlock-screen', () => presentation.lock(false));
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
  const standardEdit = Menu.buildFromTemplate([{ role: 'editMenu' }]).items[0].submenu.items;
  const editItems = standardEdit.map(item => item.role === 'undo' || item.role === 'redo'
    ? { id: `timeline-${item.role}`, label: item.label, accelerator: item.role === 'undo' ? 'CommandOrControl+Z' : 'Shift+CommandOrControl+Z', click: (_clicked, window) => { if (window instanceof BrowserWindow) window.webContents.send('intercat:history-shortcut', item.role); } }
    : item);
  const menu = Menu.buildFromTemplate([{ role: 'appMenu' }, { label: 'Edit', submenu: editItems }, { role: 'viewMenu' }, { role: 'windowMenu' }]);
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
  const projectOwner = window.webContents.id;
  window.webContents.once('destroyed', () => projectSetup.forget(projectOwner));
  presentation.attach(window, state => {
    if (!window.webContents.isDestroyed()) window.webContents.send('intercat:editor-presentation-changed', state);
  });
  let closing = false;
  let checkingDrafts = false;
  window.on('close', event => {
    if (closing) return;
    event.preventDefault();
    if (checkingDrafts) return;
    checkingDrafts = true;
    void Promise.all([drafts.flush(), pastedText.flush()]).then(() => disconnectWindow(window.webContents)).then(() => { closing = true; window.close(); })
      .catch(error => { checkingDrafts = false; dialog.showErrorBox('Could not save drafts', error.message); });
  });
  window.webContents.setWindowOpenHandler(() => ({ action: 'deny' }));
  window.webContents.on('will-navigate', (event, url) => { if (!url.startsWith('intercat://app/')) event.preventDefault(); });
  void window.loadURL('intercat://app/');
  return window;
}

