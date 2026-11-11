// The renderer selects saved destinations; it cannot supply API hosts or tokens.
const id=value=>typeof value==='string'&&/^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(value);
const keys=(value,expected)=>value&&typeof value==='object'&&!Array.isArray(value)&&Object.keys(value).sort().join(',')===expected;
const text=(value,limit)=>typeof value==='string'&&value.length>0&&value.length<=limit&&!/[\x00-\x1f\x7f]/.test(value);
export function validatePull(method,request){
  let valid=false;
  if(method==='pull.read')valid=keys(request,'id')&&id(request.id);
  if(method==='pull.open')valid=keys(request,'choices,opId,pushReview')&&id(request.opId)&&id(request.pushReview)
    &&Array.isArray(request.choices)&&request.choices.length>0&&request.choices.length<=8
    &&new Set(request.choices.map(choice=>choice?.folderId)).size===request.choices.length
    &&request.choices.every(choice=>keys(choice,'base,destination,folderId')&&text(choice.folderId,128)&&text(choice.base,1024)&&typeof choice.destination==='string'&&/^[0-9a-f]{64}$/.test(choice.destination));
  if(method==='pull.command'&&keys(request,'action,opId')&&id(request.opId)){
    const action=request.action;
    const revision=action&&id(action.id)&&Number.isSafeInteger(action.revision)&&action.revision>0;
    valid=revision&&(keys(action,'action,id,revision')&&['confirm','retry','reviewAgain','discard','stop'].includes(action.action)
      ||keys(action,'action,drafts,id,revision')&&action.action==='edit'&&Array.isArray(action.drafts)&&action.drafts.length>0&&action.drafts.length<=8
      &&new Set(action.drafts.map(edit=>edit?.folderId)).size===action.drafts.length
      &&action.drafts.every(edit=>keys(edit,'draft,folderId')&&text(edit.folderId,128)&&keys(edit.draft,'body,title')&&text(edit.draft.title,512)&&[...edit.draft.title].length<=256&&typeof edit.draft.body==='string'&&Buffer.byteLength(edit.draft.body,'utf8')<=65536&&!edit.draft.body.includes('\0')));
  }
  if(!valid)throw new Error('Invalid PR request');
  return request;
}
export function githubLink(job,folderId,kind){
  const member=job?.members?.find(member=>member.folderId===folderId);
  const repository=member?.target?.repository;
  const component=value=>typeof value==='string'&&value.length>0&&value.length<=100&&!['.','..'].includes(value)&&/^[a-zA-Z0-9_.-]+$/.test(value);
  if(!repository||!component(repository.owner)||!component(repository.name))throw new Error('Invalid saved PR repository');
  const root=`https://github.com/${repository.owner}/${repository.name}`;
  const saved=member.result??member.existing;
  if(kind==='pull'&&Number.isSafeInteger(saved?.number)&&saved.number>0){const url=`${root}/pull/${saved.number}`;if(saved.url!==url)throw new Error('The saved PR URL differs from its repository');return url;}
  if(kind==='branch'&&typeof member.target.head==='string'&&member.target.head.length<=1024)return `${root}/tree/${member.target.head.split('/').map(encodeURIComponent).join('/')}`;
  throw new Error('No saved GitHub destination');
}
