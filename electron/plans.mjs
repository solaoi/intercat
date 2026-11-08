const uuid = /^[a-f0-9]{8}-[a-f0-9]{4}-[1-8][a-f0-9]{3}-[89ab][a-f0-9]{3}-[a-f0-9]{12}$/;
const operation = /^[a-f0-9]{8}-[a-f0-9]{4}-7[a-f0-9]{3}-[89ab][a-f0-9]{3}-[a-f0-9]{12}$/;
const id = value => typeof value === 'string' && uuid.test(value);
const keys = (value, expected) => value && typeof value === 'object' && !Array.isArray(value) && Object.keys(value).sort().join(',') === expected;

export function planWindowFocused(window, sender, currentOwner, expectedOwner) {
  return !!window && !window.isDestroyed() && window.isVisible() && window.isFocused() && !window.isMinimized()
    && !sender.isDestroyed() && sender.isFocused() && currentOwner === expectedOwner;
}

export async function commandPlan(connect, request) {
  const action = request?.action;
  let valid = false;
  switch (action?.kind) {
    case 'review': case 'start': case 'hold': case 'retry': case 'run-as-one':
      valid = keys(action, 'kind'); break;
    case 'keep-assumption':
      valid = keys(action, 'assumptionId,kind') && id(action.assumptionId); break;
    case 'resolve-lock':
      valid = keys(action, 'kind,unlock') && typeof action.unlock === 'boolean'; break;
    case 'cut': case 'restore': case 'retry-item':
      valid = keys(action, 'itemId,kind') && id(action.itemId); break;
    case 'move':
      valid = keys(action, 'direction,itemId,kind') && id(action.itemId) && ['earlier', 'later'].includes(action.direction); break;
    case 'move-to': case 'unlock-and-move':
      valid = keys(action, 'itemId,kind,trackId') && id(action.itemId) && id(action.trackId); break;
    case 'start-without':
      valid = keys(action, 'dependencyId,itemId,kind') && id(action.itemId) && id(action.dependencyId); break;
    case 'retry-dependency':
      valid = keys(action, 'clipId,dependencyId,itemId,kind') && id(action.itemId) && id(action.dependencyId) && id(action.clipId); break;
  }
  if (!keys(request, 'action,opId,planId,revision') || typeof request.opId !== 'string' || !operation.test(request.opId)
      || !id(request.planId) || !Number.isSafeInteger(request.revision) || request.revision < 1 || !valid) {
    throw new Error('Invalid plan action');
  }
  return (await connect()).request('plan.command', request);
}

export async function observePlans(connect, request, { owner, sequence, focused }) {
  if (!keys(request, 'idle,plans') || typeof request.idle !== 'boolean' || !Array.isArray(request.plans) || request.plans.length > 1000
      || request.plans.some(plan => !keys(plan, 'planId,revision') || !id(plan.planId) || !Number.isSafeInteger(plan.revision) || plan.revision < 1)) {
    throw new Error('Invalid plan observation');
  }
  const client = await connect();
  return client.request('plan.observe', { owner, sequence, idle: request.idle && focused(), plans: request.plans });
}

export async function replyPlan(connect, request) {
  if (!keys(request, 'opId,planId,question,questionId,revision,text') || !operation.test(request.opId)
      || !id(request.planId) || !id(request.questionId) || !Number.isSafeInteger(request.revision) || request.revision < 1
      || typeof request.text !== 'string' || !request.text.trim() || Buffer.byteLength(request.text, 'utf8') > 32768
      || typeof request.question !== 'string' || Buffer.byteLength(request.question, 'utf8') > 8192) throw new Error('Invalid plan reply');
  return (await connect()).request('plan.reply', request);
}
