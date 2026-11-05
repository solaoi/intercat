import { createHash, createHmac } from 'node:crypto';
import { AsyncEntry } from '@napi-rs/keyring';
import { fingerprintKey, validSecretName, verifiedFingerprintStore, acceptCatalog } from './secrets.mjs';

const queues = new Map();
function serialized(id, operation) {
  const next = (queues.get(id) ?? Promise.resolve()).catch(() => {}).then(operation);
  queues.set(id, next);
  void next.finally(() => { if (queues.get(id) === next) queues.delete(id); }).catch(() => {});
  return next;
}
function validate(bundle, projectId) {
  if (!bundle || bundle.projectId !== projectId || !Number.isSafeInteger(bundle.revision) || bundle.revision < 0 || !Array.isArray(bundle.entries) || bundle.entries.length > 32) throw new Error('Invalid project secrets in Keychain.');
  let previous = ''; let size = 0;
  for (const entry of bundle.entries) {
    if (!validSecretName(entry.name) || entry.name <= previous || typeof entry.value !== 'string' || !entry.value || entry.value.includes('\0') || Buffer.byteLength(entry.value) > 8192) throw new Error('Invalid project secrets in Keychain.');
    previous = entry.name; size += Buffer.byteLength(entry.value);
  }
  if (size > 65536) throw new Error('Project secrets exceed 64 KiB.');
  return bundle;
}
export function projectSecretStore(projectId, entry = new AsyncEntry('Intercat', `project-secrets.${projectId}`)) {
  if (!/^[0-9a-f]{64}$/.test(projectId)) throw new Error('Invalid project identity.');
  async function read() {
    const stored = await entry.getPassword();
    if (stored === null) return { projectId, revision: 0, entries: [] };
    let bundle; try { bundle = JSON.parse(stored); } catch { throw new Error('Invalid project secrets in Keychain.'); }
    return validate(bundle, projectId);
  }
  return {
    async load() { const bundle = await read(); return { projectId, revision: bundle.revision, entries: bundle.entries }; },
    async change({ opId, name, value, expectedRevision, minimumRevision }, validateChange) {
      if (!/^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(opId) || !validSecretName(name) || !Number.isSafeInteger(minimumRevision) || minimumRevision < 0 || (expectedRevision !== null && (!Number.isSafeInteger(expectedRevision) || expectedRevision < 0)) || typeof validateChange !== 'function' || (value !== null && (typeof value !== 'string' || !value || value.includes('\0') || Buffer.byteLength(value) > 8192))) throw new Error('Invalid project secret.');
      return serialized(projectId, async () => {
        const previous = await read();
        const digest = createHash('sha256').update(JSON.stringify([name, value])).digest('hex');
        if (previous.operation?.id === opId) {
          if (previous.operation.digest !== digest || previous.revision < minimumRevision) throw new Error('Project secret operation reused.');
          const bundle = { projectId, revision: previous.revision, entries: previous.entries };
          await validateChange(bundle);
          return bundle;
        }
        if (expectedRevision !== null && expectedRevision !== previous.revision) throw new Error('Saved values changed. Reload before editing them.');
        const entries = previous.entries.filter(entry => entry.name !== name);
        if (value !== null) entries.push({ name, value });
        entries.sort((a, b) => a.name < b.name ? -1 : a.name > b.name ? 1 : 0);
        if (JSON.stringify(entries) === JSON.stringify(previous.entries) && previous.revision >= minimumRevision) {
          const bundle = { projectId, revision: previous.revision, entries };
          await validateChange(bundle);
          return bundle;
        }
        const next = validate({ projectId, revision: Math.max(previous.revision, minimumRevision) + 1, entries, operation: { id: opId, digest } }, projectId);
        const bundle = { projectId, revision: next.revision, entries };
        await validateChange(bundle);
        if (JSON.stringify(await read()) !== JSON.stringify(previous)) throw new Error('Saved values changed in another app. Reload before continuing.');
        const encoded = JSON.stringify(next);
        try {
          await entry.setPassword(encoded);
          if (await entry.getPassword() !== encoded) throw new Error('Readback did not match.');
        } catch { throw new Error('Keychain could not confirm the saved values.'); }
        return bundle;
      });
    },
  };
}
export function projectRegistration(bundle, opId, key) {
  validate(bundle, bundle.projectId);
  const sign = value => createHmac('sha256', Buffer.from(key, 'hex')).update(value).digest('hex');
  return { opId, projectId: bundle.projectId, revision: bundle.revision, keyCheck: sign('intercat.secret-fingerprints.v1'), fingerprints: bundle.entries.map(entry => ({ name: entry.name, byteLen: Buffer.byteLength(entry.value), tag: sign(entry.value) })) };
}
const issues = new WeakMap();
const issueText = 'Saved project secrets could not be applied. Review them in Settings.';
export function projectSecretIssue(client) { return issues.get(client) ?? null; }
async function holdForReview(client) {
  await client.request('secret.project.hold', {});
  issues.set(client, issueText);
}
async function deliver(client, store, bundle, opId, workerRequest) {
  const receipt = await store.read();
  const current = acceptCatalog(await client.request('secret.project.prepare', projectRegistration(bundle, opId, await fingerprintKey())), receipt);
  await store.publish(receipt, current);
  await client.request('secret.confirm', current.catalog);
  let workerError;
  try { if (workerRequest) await client.request('worker.secret', workerRequest); }
  catch (error) { workerError = error; }
  await client.request('secret.project.activate', bundle);
  issues.delete(client);
  if (workerError) throw Object.assign(new Error('Saved for this project, but delivery to this worker was not confirmed. Review the worker before retrying.'), { projectDelivered: true });
  return { accepted: true };
}
export async function deliverProjectSecrets(client, bundle, opId, workerRequest = null) {
  const store = verifiedFingerprintStore(client);
  return store.run(() => deliver(client, store, bundle, opId, workerRequest));
}
export async function changeProjectSecret(client, request, workerRequest = null) {
  const state = (await client.request('system.status', {})).projectSecrets;
  if (!state) throw new Error('Select a project before saving its secrets.');
  const store = verifiedFingerprintStore(client);
  return store.run(async () => {
    let held = false;
    try {
      const key = await fingerprintKey();
      const bundle = await projectSecretStore(state.projectId).change({ opId: request.opId, name: request.name, value: request.value, expectedRevision: request.expectedRevision, minimumRevision: state.revision }, async proposed => {
        await client.request('secret.project.check', projectRegistration(proposed, request.fingerprintOpId, key));
        await client.request('secret.project.hold', {});
        held = true;
      });
      return await deliver(client, store, bundle, request.fingerprintOpId, workerRequest);
    } catch (error) {
      if (held && !error.projectDelivered) {
        try { await holdForReview(client); }
        catch { client.close(); }
        if (error.code === 'FINGERPRINT_RECEIPT_MISMATCH') { client.close(); throw error; }
        throw new Error('Project secret delivery was not confirmed. Review saved values in Settings.');
      }
      throw error;
    }
  });
}
export async function synchronizeProjectSecrets(client, opId) {
  const state = (await client.request('system.status', {})).projectSecrets;
  if (!state) return;
  const bundle = await projectSecretStore(state.projectId).load();
  try { await deliverProjectSecrets(client, bundle, opId); }
  catch (error) {
    // A rejected saved bundle is recoverable in Settings. Receipt mismatches,
    // lost connections and Keychain errors still use the connection failure path.
    if (error.code !== -32002) throw error;
    await holdForReview(client);
  }
}
export async function projectSecretSettings(client) {
  const project = (await client.request('system.status', {})).projectSecrets;
  if (!project) throw new Error('No project is configured.');
  const saved = await projectSecretStore(project.projectId).load();
  return { project, savedRevision: saved.revision, savedNames: saved.entries.map(entry => entry.name), issue: projectSecretIssue(client) ?? (saved.revision !== project.revision || !project.ready ? issueText : null) };
}
