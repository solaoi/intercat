function requestShape(request, keys) {
  if (!request || Object.keys(request).sort().join(',') !== keys || !['image', 'link'].includes(request.kind)) throw new Error('Invalid external URL request.');
}
export async function reviewExternal(client, request) {
  requestShape(request, 'kind,url');
  if (typeof request.url !== 'string' || request.url.length > 8192) throw new Error('Invalid external URL.');
  return client.request('external.review', request);
}
export async function confirmExternal(client, request, open) {
  requestShape(request, 'id,kind');
  if (typeof request.id !== 'string' || request.id.length > 64) throw new Error('Invalid external URL confirmation.');
  if (request.kind === 'image') return client.request('external.image', { id: request.id });
  const { url } = await client.request('external.link', { id: request.id });
  const target = new URL(url);
  if (!['http:', 'https:', 'mailto:'].includes(target.protocol) || target.username || target.password || /[\u0000-\u001f\u007f]/.test(url)) throw new Error('Invalid external link.');
  await open(url);
  return { opened: true };
}
