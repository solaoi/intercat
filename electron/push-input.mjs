// Validate the renderer boundary; the daemon owns all destination resolution.
const id = value => typeof value === 'string' && /^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(value);
const text = (value, limit = 1024) => typeof value === 'string' && value.length > 0 && value.length <= limit && !/[\x00-\x1f\x7f]/.test(value);
const keys = (value, expected) => value && typeof value === 'object' && !Array.isArray(value) && Object.keys(value).sort().join(',') === expected;
export function validatePush(method, request) {
  let valid = false;
  if (method === 'push.remotes') valid = keys(request, 'folderId') && text(request.folderId, 128);
  if (method === 'push.presentation') valid = keys(request, 'id') && id(request.id);
  if (method === 'push.review') valid = keys(request, 'choices,opId') && id(request.opId)
    && Array.isArray(request.choices) && request.choices.length > 0 && request.choices.length <= 8
    && new Set(request.choices.map(choice => choice?.folderId)).size === request.choices.length
    && request.choices.every(choice => keys(choice, 'folderId,remote,targetRef,through') && text(choice.folderId, 128) && text(choice.remote)
      && (choice.targetRef === null || text(choice.targetRef))
      && (choice.through === null || typeof choice.through === 'string' && /^(?:[0-9a-f]{40}|[0-9a-f]{64})$/.test(choice.through)));
  if (method === 'push.command' && keys(request, 'action,opId') && id(request.opId)) {
    const action = request.action;
    valid = keys(action, 'action,review') && action.action === 'confirm' && id(action.review)
      || keys(action, 'action,id,revision') && ['retry', 'cancel'].includes(action.action) && id(action.id) && Number.isSafeInteger(action.revision) && action.revision > 0;
  }
  if (!valid) throw new Error('Invalid Push request');
  return request;
}
