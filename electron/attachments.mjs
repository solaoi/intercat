// Renderer callers choose through a native dialog; they cannot supply paths.
export function referenceImporter({ choose, client, operationId }) {
  let pending = null;
  let running = false;
  return async function importReference(reselect = false) {
    if (running) throw new Error('A reference file is already being saved.');
    if (reselect) pending = null;
    running = true;
    try {
      if (!pending) {
        const file = await choose();
        if (!file) return null;
        pending = { opId: operationId(), path: file };
      }
      const result = await (await client()).request('attachment.import', pending);
      if (result?.id !== pending.opId || typeof result.name !== 'string' || !Number.isSafeInteger(result.byteLen) || result.byteLen < 0 || result.byteLen > 25 * 1024 * 1024 || !/^[a-f0-9]{64}$/.test(result.sha256)) throw new Error('The reference file could not be verified.');
      pending = null;
      return result;
    } finally { running = false; }
  };
}
