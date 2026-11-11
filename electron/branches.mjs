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

export function validateBranchRequest(request) {
  const text = value => typeof value === 'string' && value.length > 0 && value.length <= 128;
  const point = request?.point;
  if (!request || Object.keys(request).sort().join(',') !== 'instructions,opId,parentTrackId,point'
    || !['current', 'checkpoint'].includes(request.instructions)
    || ![request.opId, request.parentTrackId].every(text)
    || !point || !(point.kind === 'turn' && Object.keys(point).sort().join(',') === 'kind,turnId' && text(point.turnId)
      || point.kind === 'initial' && Object.keys(point).sort().join(',') === 'kind,twoWaysId' && text(point.twoWaysId))) throw new Error('Invalid branch request');
}
