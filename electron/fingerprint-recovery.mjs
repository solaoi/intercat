import { recoveryCommand } from './recovery-command.mjs';
import { fingerprintKey, fingerprintStore, receiptMismatch, validReceipt, equalReceipt } from './secrets.mjs';
import { projectRegistration, projectSecretStore } from './project-secrets.mjs';

export async function rebuildProjectFingerprints(binary, directory, request) {
  if (!request || Object.keys(request).sort().join(',') !== 'opId,projectId'
    || !/^[0-9a-f]{64}$/.test(request.projectId)
    || !/^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(request.opId)) throw new Error('Invalid fingerprint recovery request.');
  const store = await fingerprintStore(directory);
  return store.run(async () => {
    const receipt = await store.read();
    if (!receipt?.proof.projects.some(project => project.projectId === request.projectId)) throw new Error('The saved confirmation does not include this project.');
    const key = await fingerprintKey();
    const saved = await projectSecretStore(request.projectId).load();
    const { projectId, revision, fingerprints } = projectRegistration(saved, request.opId, key);
    // Raw project values stay in the app; only fingerprints use the pipe.
    const value = await recoveryCommand(binary, directory, 'rebuild-fingerprints', { secretKey: key, request: { opId: request.opId, receipt, manifest: { projectId, revision, fingerprints } } });
    if (!Array.isArray(value.recovery?.sessions) || !Array.isArray(value.recovery?.projects)) throw new Error('Invalid fingerprint reconstruction result.');
    const result = value.recovery;
    if (JSON.stringify(await store.read()) !== JSON.stringify(receipt)) throw receiptMismatch();
    return result;
  });
}

export async function reviewBackup(binary, directory, request) {
  const keys = request && Object.keys(request).sort().join(',');
  const operation = request && /^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(request.opId);
  const valid = request && (request.action === 'inspect' ? keys === 'action'
    : request.action === 'reviewFiles' ? keys === 'action,opId,trackId' && operation && typeof request.trackId === 'string' && request.trackId.length <= 128
    : request.action === 'cancelApply' ? keys === 'action,opId' && operation
    : request.action === 'chooseSession' ? keys === 'acknowledgeUnread,action,choice,opId,reviewId,sessionId' && operation
      && typeof request.sessionId === 'string' && request.sessionId.length > 0 && request.sessionId.length <= 128
      && ['pick-up', 'discard-history'].includes(request.choice) && typeof request.acknowledgeUnread === 'boolean'
      && (request.reviewId === null || /^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(request.reviewId))
    : request.action === 'stop' && keys === 'action,hostId,opId,root' && operation
      && typeof request.hostId === 'string' && request.hostId.length <= 300
      && request.root && Object.keys(request.root).sort().join(',') === 'pid,started_micros,started_seconds'
      && ['pid', 'started_seconds', 'started_micros'].every(key => Number.isSafeInteger(request.root[key]) && request.root[key] >= 0));
  if (!valid) throw new Error('Invalid backup review request.');
  const store = await fingerprintStore(directory);
  return store.run(async () => {
    const receipt = await store.read();
    if (!receipt) throw new Error('The saved fingerprint confirmation is missing.');
    const result = await recoveryCommand(binary, directory, 'review-backup', { secretKey: await fingerprintKey(), receipt, request });
    if (JSON.stringify(await store.read()) !== JSON.stringify(receipt)) throw receiptMismatch();
    if (!result.review || result.review.historyPaused !== true || !Array.isArray(result.review.hosts) || !Array.isArray(result.review.workspaces)) throw new Error('Invalid backup review result.');
    return result.review;
  });
}

export async function publishBackupRecovery(store, request, invoke) {
  return store.run(async () => {
    const current = await store.read();
    if (!current) throw new Error('The saved fingerprint confirmation is missing.');
    const prepared = await invoke(current, { action: 'apply', ...request });
    const publication = prepared.publication;
    if (!validReceipt(publication?.source) || !validReceipt(publication?.target)
      || publication.target.catalog.revision <= publication.source.catalog.revision
      || prepared.apply?.opId !== request.opId
      || !['awaiting-receipt', 'complete'].includes(prepared.apply?.phase)
      || (!equalReceipt(current, publication.source) && !equalReceipt(current, publication.target))) throw receiptMismatch();
    await store.publish(publication.source, publication.target);
    const confirmed = await invoke(publication.target, { action: 'confirmApply', opId: request.opId });
    if (confirmed.apply?.opId !== request.opId || confirmed.apply?.phase !== 'complete' || confirmed.historyPaused !== false
      || !equalReceipt(await store.read(), publication.target)) throw receiptMismatch();
    return { applied: true };
  });
}
export async function applyBackup(binary, directory, request) {
  if (!request || Object.keys(request).sort().join(',') !== 'acknowledgeUnverified,opId'
    || !/^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(request.opId)
    || typeof request.acknowledgeUnverified !== 'boolean') throw new Error('Invalid backup apply request.');
  const store = await fingerprintStore(directory);
  const key = await fingerprintKey();
  return publishBackupRecovery(store, request, async (receipt, action) => {
    const result = await recoveryCommand(binary, directory, 'review-backup', { secretKey: key, receipt, request: action });
    return result.review;
  });
}
