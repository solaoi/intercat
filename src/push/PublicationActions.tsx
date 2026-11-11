import React from 'react';
import type {DaemonStatus} from '../bridge';
import type {PullSelection} from '../pull/types';
import {pullDone} from '../pull/types';
import type {PushSelection} from './types';
import {pushDone} from './types';
import './push.css';
export default function PublicationActions({status,preview,disabled,commit,push,pull}:{status:DaemonStatus;preview:boolean;disabled:boolean;commit:(folder?:string)=>void;push:(selection:PushSelection)=>void;pull:(selection:PullSelection)=>void}) {
  const folders=status.project?.folders??[];
  const completed=(status.integrations??[]).filter(job=>job.status==='done');
  const committed=folders.filter(folder=>completed.some(job=>job.project.id===folder.id));
  const knownTrack=(id:string)=>completed.some(job=>job.tracks.some(track=>track.id===id&&track.included&&track.checkpoint));
  const pending=(status.integrations??[]).some(job=>!['done','discarded'].includes(job.status))
    ||(status.workers??[]).some(worker=>worker.activeTurn||worker.checkpointPending||worker.checkpoint&&!completed.some(job=>job.tracks.some(track=>track.id===worker.trackId&&track.included&&track.checkpoint?.sha===worker.checkpoint?.sha)))
    ||(status.workspaces??[]).some(workspace=>workspace.state==='ready'&&!knownTrack(workspace.trackId));
  const primaryPush=committed.length>0&&!pending;
  const latest=(id:string)=>[...completed].reverse().find(job=>job.project.id===id)?.candidate?.head;
  const pushed=committed.length>0&&committed.every(folder=>{const sha=latest(folder.id);return !!sha&&(status.pushes??[]).some(job=>{const member=job.review.members.find(member=>member.folder.id===folder.id&&member.target.sha===sha);return !!member&&member.target.destinations.every(destination=>job.recipients.some(recipient=>recipient.folderId===folder.id&&recipient.destination.fingerprint===destination.fingerprint&&recipient.phase==='done'&&recipient.receipt?.sha===sha));});});
  const primaryPull=primaryPush&&pushed;
  const count=preview?2:folders.length;
  const suffix=count>1?` · ${count} folders`:'';
  const selected=committed.map(folder=>folder.id);
  const saved=[...(status.pushes??[])].reverse();
  function hideMenu(event:React.MouseEvent<HTMLButtonElement>){event.currentTarget.closest('details')?.removeAttribute('open');}
  return <div className="commit-menu publication-actions"><button disabled={disabled} className="commit-button" onClick={()=>{if(primaryPush)push({folders:selected,...(primaryPull?{intent:'pull' as const}:{})});else commit();}}>{primaryPull?'Open PR':primaryPush?'Push':'Commit'}{suffix&&<span>{suffix}</span>}</button>
    <details><summary aria-label="More project actions" aria-disabled={disabled}>▾</summary><div>
      {primaryPush&&<button disabled={disabled} onClick={event=>{hideMenu(event);commit();}}>Commit…</button>}
      {committed.length>0&&<button disabled={disabled} onClick={event=>{hideMenu(event);push({folders:selected});}}>{committed.length>1?`Push both · ${committed.length} folders`:'Push…'}</button>}
      {committed.length>0&&<button disabled={disabled} onClick={event=>{hideMenu(event);push({folders:selected,intent:'pull'});}}>{committed.length>1?`Open ${committed.length} PRs`:'Open PR…'}</button>}
      {folders.length>1&&folders.map(folder=><React.Fragment key={folder.id}><button disabled={disabled} onClick={event=>{hideMenu(event);commit(folder.id);}}>Review {folder.name}</button>{committed.some(item=>item.id===folder.id)&&<button disabled={disabled} onClick={event=>{hideMenu(event);push({folders:[folder.id]});}}>Push {folder.name}…</button>}</React.Fragment>)}
      {[...(status.pullRequests??[])].reverse().map(job=><button key={job.id} disabled={disabled} onClick={event=>{hideMenu(event);pull({review:job.push,jobId:job.id});}}>{pullDone(job)?'PR ready':'Resume PR'} · {job.members.map(member=>member.name).join(', ')}</button>)}
      {saved.map(job=><button key={job.id} disabled={disabled} onClick={event=>{hideMenu(event);push({folders:job.review.members.map(member=>member.folder.id),jobId:job.id});}}>{pushDone(job)?'Pushed':'Resume Push'} · {job.review.members.map(member=>member.folder.name).join(', ')}</button>)}
    </div></details>
  </div>;
}
