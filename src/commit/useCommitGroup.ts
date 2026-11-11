import {useEffect,useRef,useState} from 'react';
import type {IntegrationJob} from './types';
import {groupRequest,type CommitGroup,type CommitGroupRequest} from './group';

type Pending={kind:'open';request:{opId:string;tracks:string[]|null}}
  |{kind:'commit';request:CommitGroupRequest}
  |{kind:'discard';request:{opId:string;id:string;revision:number}};

export function useCommitGroup({enabled,initialOperation,tracks,groups,jobs,update,select,close}:{
  enabled:boolean;initialOperation?:string;tracks:string[]|null;groups:CommitGroup[];jobs:IntegrationJob[];
  update:()=>Promise<void>;select:(id:string|undefined)=>void;close:()=>void;
}) {
  const [id,setId]=useState<string>(),[folder,setFolder]=useState<string>();
  const [busy,setBusy]=useState(enabled),[error,setError]=useState('');
  const pending=useRef<Pending|null>(null),opening=useRef(false);
  const group=groups.find(group=>group.id===id);
  async function perform(operation:Pending) {
    pending.current=operation;setBusy(true);setError('');
    try {
      const result=operation.kind==='open'?await window.intercat.openCommitGroup(operation.request)
        :operation.kind==='commit'?await window.intercat.commitGroup(operation.request)
        :await window.intercat.discardCommitGroup(operation.request);
      await update();setId(result.id);pending.current=null;
      if(operation.kind==='open') {
        const chosen=result.members.find(member=>member.folderId===folder)??result.members.find(member=>member.jobId)??result.members[0];
        setFolder(chosen?.folderId);select(chosen?.jobId??undefined);
      }
      if(operation.kind==='discard')close();
    } catch(failure) {setError(failure instanceof Error?failure.message:'The all-folder choice could not be confirmed.');}
    finally {setBusy(false);}
  }
  async function open() {
    const operation=pending.current??{kind:'open' as const,request:{opId:initialOperation??await window.intercat.createOperationId(),tracks}};
    await perform(operation);
  }
  useEffect(()=>{if(enabled&&!opening.current){opening.current=true;void open();}},[enabled]);
  function choose(folderId:string) {
    const member=group?.members.find(member=>member.folderId===folderId);
    if(member){setFolder(folderId);select(member.jobId??undefined);}
  }
  async function commit() {
    if(!group)return;setError('');
    try {await perform(pending.current??{kind:'commit',request:groupRequest(group,jobs,await window.intercat.createOperationId())});}
    catch(failure){setError(failure instanceof Error?failure.message:'Review all folders before committing.');}
  }
  async function discard() {
    if(group)await perform(pending.current??{kind:'discard',request:{opId:await window.intercat.createOperationId(),id:group.id,revision:group.revision}});
  }
  async function refresh() {
    if(pending.current)return;
    await perform({kind:'open',request:{opId:await window.intercat.createOperationId(),tracks:group?.tracks??tracks}});
  }
  async function reviewCurrent() {
    if(pending.current?.kind!=='commit')return;
    setBusy(true);setError('');
    try {await update();pending.current=null;}
    catch(failure){setError(failure instanceof Error?failure.message:'The current group could not be read.');}
    finally{setBusy(false);}
  }
  return {group,folder,busy,error,pending:pending.current,choose,commit,discard,refresh,reviewCurrent,
    retry:()=>pending.current?perform(pending.current):open()};
}
