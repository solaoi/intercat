import { randomBytes } from 'node:crypto';
import { spawn } from 'node:child_process';
import fs from 'node:fs/promises';
import path from 'node:path';
import { AsyncEntry } from '@napi-rs/keyring';
import { RpcClient } from './rpc-client.mjs';
import { createInterface } from 'node:readline';
import { synchronizeProjectSecrets } from './project-secrets.mjs';
import { fingerprintKey, fingerprintStore, synchronizeFingerprints, validReceipt, receiptMismatch } from './secrets.mjs';

export async function uiToken() {
  const entry = new AsyncEntry('Intercat', 'rpc.ui');
  let token = await entry.getPassword();
  if (token === null) {
    token = randomBytes(32).toString('hex');
    await entry.setPassword(token);
    if (await entry.getPassword() !== token) throw new Error('Keychain token could not be verified');
  }
  if (!/^[0-9a-f]{64}$/.test(token)) throw new Error('Invalid UI token in Keychain');
  return token;
}


export function operationId() {
  const bytes = randomBytes(16);
  bytes.writeUIntBE(Date.now(), 0, 6);
  bytes[6] = (bytes[6] & 0x0f) | 0x70;
  bytes[8] = (bytes[8] & 0x3f) | 0x80;
  const hex = bytes.toString('hex');
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}

export async function connectDaemon(binary, directory, token, editorSource, gitleaks, gitLfs) {
  await fs.mkdir(directory, { recursive: true, mode: 0o700 });
  const metadata = await fs.lstat(directory);
  if (!metadata.isDirectory() || metadata.uid !== process.getuid()) throw new Error('Unsafe daemon directory');
  await fs.chmod(directory, 0o700);
  const socket = path.join(directory, 'run/rpc.sock');
  const store = await fingerprintStore(directory);
  async function connect() {
    const client = await RpcClient.connect(socket, token);
    try { await synchronizeFingerprints(client, store); await synchronizeProjectSecrets(client, operationId()); return client; }
    catch (error) { client.close(); throw error; }
  }
  try { return await connect(); }
  catch (error) {
    if (!['ENOENT', 'ECONNREFUSED'].includes(error.code)) throw error;
  }
  await fs.access(binary, fs.constants.X_OK);
  await fs.access(gitLfs, fs.constants.X_OK);
  const secretKey = await fingerprintKey();
  const log = await fs.open(path.join(directory, 'daemon.log'), 'a', 0o600);
  const child = spawn(binary, ['serve', directory, editorSource], {
    detached: true, stdio: ['pipe', 'pipe', log.fd],
    env: { ...Object.fromEntries(['HOME', 'LANG', 'SHELL', 'SSH_AUTH_SOCK', 'XDG_CONFIG_HOME', 'GH_CONFIG_DIR'].filter(key => process.env[key]).map(key => [key, process.env[key]])), PATH: [path.dirname(gitLfs), process.env.PATH].filter(Boolean).join(path.delimiter), INTERCAT_GITLEAKS: gitleaks, INTERCAT_GIT_LFS: gitLfs },
  });
  child.stdin.on('error', () => {});
  const started = new Promise((resolve, reject) => { child.once('spawn', resolve); child.once('error', reject); });
  try {
    await started;
    await store.run(async () => {
      const receipt = await store.read();
      const lines = createInterface({ input: child.stdout });
      const hello = new Promise((resolve, reject) => {
        const timer = setTimeout(() => reject(new Error('Fingerprint confirmation timed out.')), 90000);
        const failed = () => { clearTimeout(timer); reject(new Error('The daemon rejected the saved fingerprint receipt.')); };
        child.once('close', failed); child.once('error', failed);
        lines.once('line', line => {
          clearTimeout(timer); child.removeListener('close', failed); child.removeListener('error', failed);
          if (Buffer.byteLength(line) > 1024 * 1024) { reject(new Error('Invalid fingerprint confirmation.')); return; }
          try { resolve(JSON.parse(line)); } catch { reject(new Error('Invalid fingerprint confirmation.')); }
        });
      });
      child.stdin.write(JSON.stringify({ token, secretKey, receipt }) + '\n');
      try {
        const value = await hello;
        if (value.error?.code === 'FINGERPRINT_RECEIPT_MISMATCH') throw receiptMismatch(value.recovery ?? null);
        if (!validReceipt(value.fingerprints)) throw new Error('Invalid fingerprint confirmation.');
        await store.publish(receipt, value.fingerprints);
        child.stdin.end(JSON.stringify(value.fingerprints.catalog) + '\n');
      } finally { lines.close(); }
    });
    child.unref(); child.stdout.unref();
  } catch (error) {
    child.stdin?.destroy();
    if (child.pid && child.exitCode === null && child.signalCode === null) {
      const stopped = new Promise(resolve => child.once('close', resolve));
      child.kill('SIGTERM');
      await stopped;
    }
    throw error;
  }
  finally { await log.close(); }
  const until = Date.now() + 5000;
  while (Date.now() < until) {
    try { return await connect(); }
    catch (error) {
      if (!['ENOENT', 'ECONNREFUSED'].includes(error.code)) throw error;
      await new Promise(resolve => setTimeout(resolve, 50));
    }
  }
  throw new Error('Daemon did not become ready');
}
