const keys=(value,names)=>value&&typeof value==='object'&&!Array.isArray(value)&&Object.keys(value).sort().join(',')===names;
const uuid=value=>typeof value==='string'&&/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(value);
const operation=value=>uuid(value)&&value[14]==='7';
export function validateFresh(method,value) {
  let valid=false;
  if(method==='worker.fresh.prepare')valid=keys(value,'opId,sessionId,trackId')&&operation(value.opId)&&uuid(value.trackId)&&uuid(value.sessionId);
  if(method==='worker.fresh.read')valid=keys(value,'id')&&operation(value.id);
  if(method==='worker.fresh.command') {
    const action=value?.action;
    valid=keys(value,'action,id,opId,revision')&&operation(value.opId)&&operation(value.id)&&Number.isSafeInteger(value.revision)&&value.revision>0
      && (keys(action,'kind')&&['cancel','retry'].includes(action.kind)
        ||keys(action,'fingerprint,kind')&&action.kind==='allow-access'&&typeof action.fingerprint==='string'&&/^[0-9a-f]{64}$/.test(action.fingerprint));
  }
  if(!valid)throw new Error('Invalid Start fresh request');
  return value;
}
