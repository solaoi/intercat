// Keep LFS local to Intercat. Do not run git-lfs install or change Git config.
import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import * as fs from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const version = '3.8.1';
const releases = {
  arm64: ['arm64', '0ba10e897e0496412cd86eacaed4ed554c97ddc29bc5c709d28faa5eaaa2222c'],
  x64: ['amd64', '4292b3518e717e53c20a6c2c0696e2069a21e97500ad14b80b06bd52de8f3b72'],
};
if (process.platform !== 'darwin' || !releases[process.arch]) throw new Error('Intercat requires macOS arm64 or x64.');
const [architecture, archiveHash] = releases[process.arch];
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '.tools');
const destination = path.join(root, `git-lfs-${version}`);
const member = `git-lfs-${version}/git-lfs`;
const hash = bytes => createHash('sha256').update(bytes).digest('hex');
const extract = archive => execFileSync('/usr/bin/unzip', ['-p', archive, member], { maxBuffer: 64 * 1024 * 1024 });
const verifyArchive = bytes => {
  if (bytes.length > 32 * 1024 * 1024 || hash(bytes) !== archiveHash) throw new Error('Git LFS archive checksum mismatch.');
};
try {
  const archive = path.join(destination, 'release.zip');
  const executable = path.join(destination, 'git-lfs');
  if (!(await fs.lstat(archive)).isFile() || !(await fs.lstat(executable)).isFile()) throw new Error('Git LFS tool files must be regular files.');
  verifyArchive(await fs.readFile(archive));
  // Derive the binary digest from the pinned archive on both architectures.
  // Keeping the archive also lets later setup runs detect a replaced binary.
  if (hash(await fs.readFile(executable)) === hash(extract(archive))) process.exit(0);
} catch (error) { if (error.code !== 'ENOENT') throw error; }
await fs.mkdir(root, { recursive: true, mode: 0o700 });
const staging = await fs.mkdtemp(path.join(root, '.git-lfs-'));
try {
  const name = `git-lfs-darwin-${architecture}-v${version}.zip`;
  const response = await fetch(`https://github.com/git-lfs/git-lfs/releases/download/v${version}/${name}`);
  if (!response.ok) throw new Error(`Git LFS download failed: ${response.status}`);
  const bytes = Buffer.from(await response.arrayBuffer());
  verifyArchive(bytes);
  const archive = path.join(staging, 'release.zip');
  await fs.writeFile(archive, bytes, { mode: 0o400 });
  await fs.writeFile(path.join(staging, 'git-lfs'), extract(archive), { mode: 0o555 });
  const readme = execFileSync('/usr/bin/unzip', ['-p', archive, `git-lfs-${version}/README.md`], { maxBuffer: 1024 * 1024 });
  await fs.writeFile(path.join(staging, 'README.md'), readme, { mode: 0o444 });
  await fs.rm(destination, { recursive: true, force: true });
  await fs.rename(staging, destination);
  console.log(`Installed verified Git LFS ${version}.`);
} finally { await fs.rm(staging, { recursive: true, force: true }); }
