import { acceptCatalog, verifiedFingerprintStore } from './secrets.mjs';
const exact=(value,names)=>value&&typeof value==='object'&&!Array.isArray(value)&&Object.keys(value).sort().join(',')===names;
const op=value=>typeof value==='string'&&/^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(value);
const rev=value=>Number.isSafeInteger(value)&&value>0;
const label=value=>typeof value==='string'&&value.length>0&&value.length<=512&&!value.includes('\0');
const efforts=['none','low','medium','high','xhigh','max','ultra'];
const selection=value=>exact(value,'sessionId,trackId,variation')&&label(value.trackId)&&label(value.sessionId)&&exact(value.variation,'alternate,modelId,primary,providerId')&&label(value.variation.modelId)&&label(value.variation.providerId)&&value.variation.primary==='minimal'&&efforts.includes(value.variation.alternate);
export function validateTwoWays(method,value){
  const valid=method==='two-ways.options'?exact(value,'inputId,revision')&&op(value.inputId)&&rev(value.revision):method==='two-ways.read'?exact(value,'id')&&op(value.id):method==='two-ways.command'&&exact(value,'action,id,opId,revision')&&op(value.opId)&&op(value.id)&&rev(value.revision)&&['cancel','retry','confirm-usage'].includes(value.action);
  if(!valid)throw new Error('Invalid paired request');return value;
}
export function validateTwoWaysEdit(action){
  if(!exact(action,'edit,kind,revision,token')||action.kind!=='end'||!op(action.token)||!rev(action.revision)||!exact(action.edit,'kind,opId,selection')||action.edit.kind!=='tryTwoWays'||!op(action.edit.opId)||!selection(action.edit.selection))throw new Error('Invalid paired instruction');
  return action;
}
export async function finishTwoWays(client,request){
  validateTwoWays('two-ways.read',request);
  const store=verifiedFingerprintStore(client);
  return store.run(async()=>{
    const job=await client.request('two-ways.read',request);
    if(job.request?.opId!==request.id)throw new Error('The paired confirmation belongs to another request.');
    if(['queued','cancelled'].includes(job.phase))return job;
    if(!['awaiting-receipt','cancelling'].includes(job.phase))throw new Error('This paired request is not waiting for confirmation.');
    const receipt=await store.read();
    const catalog=await client.request('secret.catalog');
    const current=acceptCatalog(catalog,receipt);
    await store.publish(receipt,current);
    await client.request('secret.confirm',current.catalog);
    return client.request('two-ways.read',request);
  });
}
