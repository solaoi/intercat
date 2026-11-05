import { createHash, createHmac, randomBytes } from 'node:crypto';
import fs from 'node:fs/promises';
import { AsyncEntry } from '@napi-rs/keyring';

export async function fingerprintKey() {
  const entry = new AsyncEntry('Intercat', 'secrets.hmac.v1');
  let key = await entry.getPassword();
  if (key === null) {
    key = randomBytes(32).toString('hex');
    await entry.setPassword(key);
    if (await entry.getPassword() !== key) throw new Error('Keychain fingerprint key could not be verified');
  }
  if (!/^[0-9a-f]{64}$/.test(key)) throw new Error('Invalid fingerprint key in Keychain');
  return key;
}

export function validSecretName(name) {
  return typeof name === 'string' && /^[A-Z][A-Z0-9_]{0,127}$/.test(name)
    && !['PATH', 'HOME', 'USER', 'LOGNAME', 'SHELL', 'TMPDIR', 'ENV', 'BASH_ENV', 'SHELLOPTS', 'SSH_AUTH_SOCK', 'NODE_OPTIONS', 'PYTHONPATH', 'PYTHONHOME'].includes(name)
    && !['XDG_', 'INTERCAT_', 'MUSE_', 'GIT_', 'DYLD_', 'LD_'].some(prefix => name.startsWith(prefix));
}

export function registration({ opId, trackId, sessionId, name, value }, key) {
  if (!validSecretName(name) || typeof value !== 'string' || !value || Buffer.byteLength(value) > 65536 || value.includes('\0')
    || !/^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(opId)
    || ![trackId, sessionId].every(id => typeof id === 'string' && id.length > 0 && id.length <= 128)
    || !/^[0-9a-f]{64}$/.test(key)) throw new Error('Invalid project secret.');
  const sign = content => createHmac('sha256', Buffer.from(key, 'hex')).update(content).digest('hex');
  return { opId, trackId, sessionId, name, byteLen: Buffer.byteLength(value), tag: sign(value), keyCheck: sign('intercat.secret-fingerprints.v1') };
}

export function validSeal(value) {
  return value && typeof value === 'object' && Object.keys(value).sort().join(',') === 'digest,revision'
    && Number.isSafeInteger(value.revision) && value.revision >= 0 && /^[0-9a-f]{64}$/.test(value.digest);
}
export function validProof(proof) {
  if (!proof || Object.keys(proof).sort().join(',') !== 'projects,sessions' || !Array.isArray(proof.sessions) || !Array.isArray(proof.projects)) return false;
  let previous = '';
  for (const value of proof.sessions) {
    if (!value || Object.keys(value).sort().join(',') !== 'digest,sessionId,trackId' || typeof value.sessionId !== 'string' || value.sessionId <= previous || value.sessionId.length > 128 || typeof value.trackId !== 'string' || !value.trackId || value.trackId.length > 128 || !/^[0-9a-f]{64}$/.test(value.digest)) return false;
    previous = value.sessionId;
  }
  previous = '';
  for (const value of proof.projects) {
    if (!value || Object.keys(value).sort().join(',') !== 'digest,projectId' || !/^[0-9a-f]{64}$/.test(value.projectId) || value.projectId <= previous || !/^[0-9a-f]{64}$/.test(value.digest)) return false;
    previous = value.projectId;
  }
  return true;
}
export function validReceipt(value) {
  return value && Object.keys(value).sort().join(',') === 'catalog,proof' && validSeal(value.catalog) && validProof(value.proof) && Buffer.byteLength(JSON.stringify(value)) <= 480 * 1024;
}
export function receiptMismatch(recovery = null) {
  return Object.assign(new Error('Worker history is paused because this database does not match its saved fingerprints. Restore the latest database, then reconnect.'), { code: 'FINGERPRINT_RECEIPT_MISMATCH', recovery });
}
function equalSeal(a, b) { return a === null ? b === null : b !== null && a.revision === b.revision && a.digest === b.digest; }
export function equalReceipt(a, b) {
  const listEqual = (left, right, fields) => left.length === right.length && left.every((value, i) => fields.every(field => value[field] === right[i][field]));
  return a === null ? b === null : b !== null && equalSeal(a.catalog, b.catalog)
    && listEqual(a.proof.sessions, b.proof.sessions, ['sessionId', 'trackId', 'digest'])
    && listEqual(a.proof.projects, b.proof.projects, ['projectId', 'digest']);
}
export function acceptCatalog(catalog, receipt) {
  if (!catalog || !validSeal(catalog.current) || (catalog.sealed !== null && !validSeal(catalog.sealed))
    || (catalog.sealed && (catalog.sealed.revision > catalog.current.revision
      || (catalog.sealed.revision === catalog.current.revision && catalog.sealed.digest !== catalog.current.digest)))
    || !validProof(catalog.proof) || (receipt !== null && !validReceipt(receipt))) throw new Error('Invalid fingerprint receipt.');
  const next = { catalog: catalog.current, proof: catalog.proof };
  if (equalReceipt(next, receipt) || (receipt !== null && !equalSeal(catalog.current, receipt.catalog) && equalSeal(catalog.sealed, receipt.catalog))
    || (receipt === null && catalog.sealed === null && catalog.current.revision === 0)) return next;
  throw receiptMismatch();
}

const queues = new Map();
const clients = new WeakMap();
export function verifiedFingerprintStore(client) {
  const store = clients.get(client);
  if (!store) throw new Error('Fingerprint receipts have not been verified.');
  return store;
}
function serialized(id, operation) {
  const next = (queues.get(id) ?? Promise.resolve()).catch(() => {}).then(operation);
  queues.set(id, next);
  void next.finally(() => { if (queues.get(id) === next) queues.delete(id); }).catch(() => {});
  return next;
}
export async function fingerprintStore(directory) {
  const id = createHash('sha256').update(await fs.realpath(directory)).digest('hex');
  const entry = new AsyncEntry('Intercat', `fingerprints.${id}`);
  async function read() {
    const stored = await entry.getPassword();
    if (stored === null) return null;
    let value;
    try { value = JSON.parse(stored); } catch { throw receiptMismatch(); }
    if (validSeal(value)) throw Object.assign(new Error('This fingerprint confirmation uses an unsupported format. This development build does not migrate saved confirmations. Keep the existing database and use its matching build.'), { code: 'FINGERPRINT_RECEIPT_MISMATCH' });
    if (!validReceipt(value)) throw receiptMismatch();
    return value;
  }
  return {
    id, read,
    async publish(expected, next) {
      if (!validReceipt(next)) throw new Error('Invalid fingerprint confirmation.');
      const current = await read();
      if (equalReceipt(current, next)) return;
      if (!equalReceipt(current, expected)) throw receiptMismatch();
      await entry.setPassword(JSON.stringify(next));
      if (!equalReceipt(await read(), next)) throw new Error('The fingerprint receipt could not be verified.');
    },
    run(operation) { return serialized(id, operation); },
  };
}

export async function synchronizeFingerprints(client, store) {
  await store.run(async () => {
    const receipt = await store.read();
    const current = acceptCatalog(await client.request('secret.catalog'), receipt);
    await store.publish(receipt, current);
    await client.request('secret.confirm', current.catalog);
  });
  clients.set(client, store);
}

// The caller may expose the value to a host only after the external receipt and
// the daemon confirmation both complete. Reconnection repairs either lost ack.
export async function registerFingerprint(client, request) {
  const store = clients.get(client);
  if (!store) throw new Error('Fingerprint receipts have not been verified.');
  return store.run(async () => {
    const receipt = await store.read();
    const fingerprint = registration(request, await fingerprintKey());
    const current = acceptCatalog(await client.request('secret.registerFingerprint', fingerprint), receipt);
    await store.publish(receipt, current);
    await client.request('secret.confirm', current.catalog);
    return { registered: true };
  });
}
