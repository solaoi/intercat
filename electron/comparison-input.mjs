// Select only saved Git identities. The renderer cannot supply worktree paths or commands.
const keys=(value,names)=>value&&typeof value==='object'&&!Array.isArray(value)&&Object.keys(value).sort().join(',')===names;
const uuid=value=>typeof value==='string'&&/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(value);
const operation=value=>uuid(value)&&value[14]==='7';
const point=value=>keys(value,'sha,trackId')&&uuid(value.trackId)&&typeof value.sha==='string'&&/^(?:[0-9a-f]{40}|[0-9a-f]{64})$/.test(value.sha);
export function validateComparison(method,value){
  let valid=false;
  if(['comparison.read','comparison.evidence'].includes(method))valid=keys(value,'id')&&operation(value.id);
  if(method==='comparison.open')valid=keys(value,'child,opId,parent')&&operation(value.opId)&&point(value.parent)&&point(value.child)&&value.parent.trackId!==value.child.trackId;
  if(method==='comparison.control')valid=keys(value,'action,attempt,id,opId,revision')&&operation(value.opId)&&operation(value.id)&&Number.isSafeInteger(value.revision)&&value.revision>0&&Number.isSafeInteger(value.attempt)&&value.attempt>0&&['hold','retry'].includes(value.action);
  if(method==='comparison.diff')valid=keys(value,'basis,id,path')&&operation(value.id)&&['parent','child','paths'].includes(value.basis)&&typeof value.path==='string'&&value.path.length>0&&value.path.length<=16384&&!value.path.startsWith('/')&&!value.path.includes('\0')&&!value.path.split('/').some(part=>part==='..'||part===''||part==='.');
  if(!valid)throw new Error('Invalid saved path comparison');
  return value;
}
