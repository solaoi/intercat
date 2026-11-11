import { validateComparison } from './comparison-input.mjs';

// Saved-path IPC accepts journal identities, never caller-supplied execution paths.
export function registerComparison({ ipcMain, authorize, preview, connection }) {
  for (const [channel, method] of [
    ['comparison-open', 'comparison.open'],
    ['comparison-control', 'comparison.control'],
    ['comparison-read', 'comparison.read'],
    ['comparison-evidence', 'comparison.evidence'],
    ['comparison-diff', 'comparison.diff'],
  ]) {
    ipcMain.handle(`intercat:${channel}`, async (event, request) => {
      authorize(event);
      if (preview) throw new Error('Connect to the local daemon before checking saved paths.');
      return (await connection()).request(method, validateComparison(method, request));
    });
  }
}
