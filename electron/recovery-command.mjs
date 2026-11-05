import { spawn } from 'node:child_process';
import fs from 'node:fs/promises';

// Private recovery commands never open the daemon RPC or launch a worker.
export async function recoveryCommand(binary, directory, command, input) {
  const metadata = await fs.lstat(directory);
  if (!metadata.isDirectory() || metadata.uid !== process.getuid() || (metadata.mode & 0o777) !== 0o700) throw new Error('Unsafe daemon directory.');
  const frame = Buffer.from(JSON.stringify(input) + '\n');
  const child = spawn(binary, [command, directory], {
    stdio: ['pipe', 'pipe', 'pipe'],
    env: Object.fromEntries(['HOME', 'PATH', 'LANG', 'SHELL'].filter(name => process.env[name]).map(name => [name, process.env[name]])),
  });
  return new Promise((resolve, reject) => {
    let output = ''; let stopped = false; let force;
    const stop = () => { if (stopped) return; stopped = true; child.kill('SIGTERM'); force = setTimeout(() => child.kill('SIGKILL'), 2000); };
    const timer = setTimeout(stop, command === 'review-backup' && input.request?.action === 'apply' ? 90000 : 30000);
    child.stdin.on('error', () => {}); child.stderr.resume(); child.stdout.setEncoding('utf8');
    child.stdout.on('data', chunk => { if (stopped) return; output += chunk; if (Buffer.byteLength(output) > 1024 * 1024) stop(); });
    child.once('error', () => { clearTimeout(timer); clearTimeout(force); frame.fill(0); reject(new Error('The recovery command could not start.')); });
    child.once('close', code => {
      clearTimeout(timer); clearTimeout(force); frame.fill(0);
      if (stopped || code !== 0) { reject(new Error('The recovery command did not finish.')); return; }
      let value; try { value = JSON.parse(output); } catch { reject(new Error('Invalid recovery command result.')); return; }
      if (value.error) { reject(new Error(value.error.message)); return; }
      resolve(value);
    });
    child.stdin.end(frame, () => frame.fill(0));
  });
}
