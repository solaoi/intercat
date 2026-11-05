import type { ElectronApplication } from '@playwright/test';
import path from 'node:path';

/** Configure a deterministic/local model for an opt-in integration test. */
export async function configureEditor(app: ElectronApplication, data: string, model = process.env.INTERCAT_TEST_EDITOR_MODEL!, python = process.env.INTERCAT_TEST_EDITOR_PYTHON!) {
  await app.evaluate(async (_electron, input) => {
    const load = process.getBuiltinModule('module').createRequire(input.rpc);
    const { RpcClient } = load(input.rpc);
    const { AsyncEntry } = load('@napi-rs/keyring');
    const { operationId } = load(input.daemon);
    const token = await new AsyncEntry('Intercat', 'rpc.ui').getPassword();
    const client = await RpcClient.connect(input.socket, token);
    try { await client.request('editor.configure', { opId: operationId(), config: { model: input.model, python: input.python } }); }
    finally { client.close(); }
  }, { model, python, rpc: path.resolve('electron/rpc-client.mjs'), daemon: path.resolve('electron/daemon.mjs'), socket: path.join(data, 'daemon/run/rpc.sock') });
}
