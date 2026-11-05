// Install the same pinned detector used by Commit and the Privacy gate.
import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import * as fs from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const version = '8.30.1';
const releases = {
  arm64: ['b40ab0ae55c505963e365f271a8d3846efbc170aa17f2607f13df610a9aeb6a5', 'ba52fb1bfabbcde42f032afad3d6e0b19dff8ed105229a16e7caa338bbc0e84f'],
  x64: ['dfe101a4db2255fc85120ac7f3d25e4342c3c20cf749f2c20a18081af1952709', 'cee01fea7173f1b779dff188e1c26ecbcb4027d394acc573b23aaf0be260e291'],
};
if (process.platform !== 'darwin' || !releases[process.arch]) throw new Error('Intercat requires macOS arm64 or x64.');
const [archiveHash, binaryHash] = releases[process.arch];
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '.tools');
const destination = path.join(root, `gitleaks-${version}`);
const hash = bytes => createHash('sha256').update(bytes).digest('hex');
try {
  if (hash(await fs.readFile(path.join(destination, 'gitleaks'))) === binaryHash) process.exit(0);
} catch (error) { if (error.code !== 'ENOENT') throw error; }
await fs.mkdir(root, { recursive: true, mode: 0o700 });
const staging = await fs.mkdtemp(path.join(root, '.gitleaks-'));
try {
  const name = `gitleaks_${version}_darwin_${process.arch}.tar.gz`;
  const response = await fetch(`https://github.com/gitleaks/gitleaks/releases/download/v${version}/${name}`);
  if (!response.ok) throw new Error(`Gitleaks download failed: ${response.status}`);
  const bytes = Buffer.from(await response.arrayBuffer());
  if (bytes.length > 16 * 1024 * 1024 || hash(bytes) !== archiveHash) throw new Error('Gitleaks archive checksum mismatch.');
  const archive = path.join(staging, 'release.tar.gz');
  await fs.writeFile(archive, bytes, { mode: 0o600 });
  execFileSync('/usr/bin/tar', ['-xzf', archive, '-C', staging, 'gitleaks', 'LICENSE']);
  if (hash(await fs.readFile(path.join(staging, 'gitleaks'))) !== binaryHash) throw new Error('Gitleaks executable checksum mismatch.');
  await fs.unlink(archive);
  await fs.chmod(path.join(staging, 'gitleaks'), 0o555);
  await fs.rm(destination, { recursive: true, force: true });
  await fs.rename(staging, destination);
  console.log(`Installed verified Gitleaks ${version}.`);
} finally { await fs.rm(staging, { recursive: true, force: true }); }
