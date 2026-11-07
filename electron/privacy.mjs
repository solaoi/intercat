import fs from 'node:fs/promises';
import { constants } from 'node:fs';
import path from 'node:path';

const limit = 2 * 1024 * 1024;
export function privacyText(encoded) {
  if (typeof encoded !== 'string' || encoded.length > Math.ceil(limit / 3) * 4) throw new Error('Invalid privacy text.');
  const bytes = Buffer.from(encoded, 'base64');
  if (bytes.length > limit || bytes.toString('base64') !== encoded) throw new Error('Invalid privacy text.');
  return new TextDecoder('utf-8', { fatal: true }).decode(bytes);
}
export async function privacyPreview({ directory, client, texts }) {
  if (!Array.isArray(texts) || texts.length > 64 || texts.some(text => typeof text !== 'string' || Buffer.byteLength(text) > limit)
      || texts.reduce((total,text) => total + Buffer.byteLength(text), 0) > limit) throw new Error('Invalid privacy preview.');
  const connection = await client();
  await fs.mkdir(directory, { recursive: true, mode: 0o700 });
  const metadata = await fs.lstat(directory);
  if (!metadata.isDirectory() || metadata.uid !== process.getuid() || metadata.mode & 0o077) throw new Error('Unsafe privacy preview directory.');
  const box = await fs.mkdtemp(path.join(directory, 'view-'));
  try {
    await fs.writeFile(path.join(box, 'input.json'), JSON.stringify(texts), { flag: constants.O_WRONLY | constants.O_CREAT | constants.O_EXCL | constants.O_NOFOLLOW, mode: 0o600 });
    const result = await connection.request('privacy.preview', { id: path.basename(box) });
    if (!Array.isArray(result) || result.length !== texts.length) throw new Error('Incomplete privacy preview.');
    return result.map(view => {
      if (!view || !Array.isArray(view.parts) || view.parts.length > 4097) throw new Error('Invalid privacy preview.');
      const parts = view.parts.map(part => {
        if (part?.kind === 'text') return { kind: 'text', text: privacyText(part.textBase64) };
        if (part?.kind !== 'redaction' || !Number.isSafeInteger(part.id) || part.id <= 0 || !['api-key','token','email','phone','password','name','address'].includes(part.category)) throw new Error('Invalid privacy replacement.');
        return { kind: 'redaction', id: part.id, category: part.category };
      });
      return { text: parts.map(part => part.kind === 'text' ? part.text : `<redacted:${part.category}#${part.id}>`).join(''), parts };
    });
  } finally { await fs.rm(box, { recursive: true, force: true }); }
}
