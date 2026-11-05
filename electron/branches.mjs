import { acceptCatalog, verifiedFingerprintStore } from './secrets.mjs';

export async function branchFromCheckpoint(client, request) {
  const store = verifiedFingerprintStore(client);
  return store.run(async () => {
    const receipt = await store.read();
    const result = await client.request('branch.prepare', request);
    const current = acceptCatalog(result.catalog, receipt);
    await store.publish(receipt, current);
    await client.request('secret.confirm', current.catalog);
    return client.request('branch.activate', { opId: request.opId });
  });
}
