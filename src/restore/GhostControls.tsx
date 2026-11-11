import {useEffect,useRef,useState} from 'react';
import type {Track} from '../timeline/model';
import type {AsideInfo,RestoreRequest} from './types';
import './restore.css';
export function GhostControls({track,hovered,disabled,open}:{track:Track;hovered:boolean;disabled:boolean;open:(id:string)=>void}) {
  const ghost=track.ghost!;
  const [info,setInfo]=useState<AsideInfo|null>(null),[checking,setChecking]=useState(false),[failed,setFailed]=useState(false),[busy,setBusy]=useState(false);
  const pending=useRef<RestoreRequest|null>(null);
  useEffect(()=>{
    if(ghost.phase!=='aside')return;
    let disposed=false,running=false;
    const read=async()=>{
      if(running)return;running=true;setChecking(true);
      try{const value=await window.intercat.getAsideProcesses({retirementId:ghost.id});if(!disposed){setInfo(value);setFailed(false);}}
      catch{if(!disposed)setFailed(true);}
      finally{running=false;if(!disposed)setChecking(false);}
    };
    void read();const timer=setInterval(()=>{void read();},2000);
    return()=>{disposed=true;clearInterval(timer);};
  },[ghost.id,ghost.phase]);
  async function stop(){
    if(disabled||busy)return;setBusy(true);
    try{
      pending.current??=info?.stop?.record.request??{opId:await window.intercat.createOperationId(),retirementId:ghost.id};
      const stop=await window.intercat.stopAsideProcesses(pending.current);pending.current=null;
      setInfo(current=>({retirementId:ghost.id,groups:current?.groups??null,error:current?.error??null,stop}));
    }catch{setFailed(true);}finally{setBusy(false);}
  }
  const waiting=ghost.phase!=='aside',stopping=busy||!!info?.stop&&info.stop.state!=='complete'&&!info.stop.record.error;
  const issue=failed||!!pending.current||!!info?.error||!!info?.stop?.record.error||!!ghost.error;
  return <div className="ghost-controls" data-hovered={hovered} onClick={event=>event.stopPropagation()}>
    {waiting?<span role="status">stopping…</span>:stopping?<span role="status">Stopping processes…</span>:issue?<button onClick={()=>open(ghost.id)}>Processes need checking</button>:info?.groups?<><span>{info.groups} {info.groups===1?'group':'groups'} still running</span><button disabled={disabled||busy} aria-label={`Stop ${track.label}’s remaining processes`} onClick={()=>{void stop();}}>Stop</button></>:!info&&checking?<span>Checking processes…</span>:null}
    {issue&&(info?.stop||pending.current)&&<button disabled={disabled||busy} onClick={()=>{void stop();}}>Retry Stop</button>}
    <button className="ghost-restore" disabled={disabled||waiting||stopping} aria-label={`Restore Worker ${track.label}`} onClick={()=>open(ghost.id)}>Restore</button>
  </div>;
}
