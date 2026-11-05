import type { ElectronApplication } from '@playwright/test';

/** Close the window; this intentionally leaves background work to the test owner. */
export async function closeWindow(app: ElectronApplication) {
  if (app.context().isClosed()) return;
  const closed=app.waitForEvent('close');
  await app.evaluate(({BrowserWindow}) => {
    for (const window of BrowserWindow.getAllWindows()) window.close();
  });
  await closed;
}
