import { acceptCatalog, verifiedFingerprintStore } from './secrets.mjs';

export async function pickupFromFiles(client, request) {
  const store = verifiedFingerprintStore(client);
  return store.run(async () => {
    const receipt = await store.read();
    const result = await client.request('worker.pickup.prepare', request);
    const current = acceptCatalog(result.catalog, receipt);
    await store.publish(receipt, current);
    await client.request('secret.confirm', current.catalog);
    return client.request('worker.pickup.activate', { opId: request.opId });
  });
}

export async function cancelFileRecovery(client, request) {
  // Prepare can hold this store's lock while waiting on a model. Send the
  // cancellation first; only serialize receipt publication after it stops.
  const result = await client.request('worker.pickup.cancel', request);
  const store = verifiedFingerprintStore(client);
  return store.run(async () => {
    const receipt = await store.read();
    const current = acceptCatalog(result.catalog, receipt);
    await store.publish(receipt, current);
    await client.request('secret.confirm', current.catalog);
    return result.recovery;
  });
}
