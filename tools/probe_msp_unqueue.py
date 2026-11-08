"""Probe ten queued-turn reclaims with one generated question and owned MSP host."""
import argparse
import hashlib
import json
import queue
import time
from pathlib import Path

from verify_host_hooks import Host, command_id

PIN = '1c96114408acbeade9a8fb79e3b4425e5b64d50425938568aef01bdc04ce2e03'

def main():
    parser=argparse.ArgumentParser()
    parser.add_argument('--binary',type=Path,required=True)
    parser.add_argument('--auth',type=Path,required=True)
    parser.add_argument('--output',type=Path,required=True)
    args=parser.parse_args()
    binary=args.binary.resolve(strict=True)
    if hashlib.sha256(binary.read_bytes()).hexdigest()!=PIN:
        raise RuntimeError('The pinned Muse binary is required.')
    output=args.output.resolve();output.mkdir(parents=True,exist_ok=False,mode=0o700)
    (output/'probe-source.py').write_bytes(Path(__file__).read_bytes())
    root=output/'workspace';root.mkdir(mode=0o700)
    config=root/'config';muse=config/'muse';muse.mkdir(parents=True,mode=0o700)
    (muse/'auth.json').symlink_to(args.auth.resolve(strict=True))
    (muse/'settings.json').write_text(json.dumps({'schema_version':1,'provider':'meta','model':'muse-spark-1.3-contributor','contributor_mode_enabled':False,'agents':{'execution_capacity':1},'run':{'toolset':['request_user_input']},'permissions':{'schema_version':1,'default_profile':'intercat-worker','profiles':{'intercat-worker':{'extends':':ask-me'}}}}))
    host=Host(binary,root,config,output,False)
    seen=[];session=None;active=None
    result={'requestedCycles':10,'cycles':[],'passed':False,'startedTurns':0,'conservativeAllowance':12}
    def save():
        (output/'result.json').write_text(json.dumps(result,indent=2)+'\n')
    def buffered():
        frames=list(host.events);host.events.clear();seen.extend(frames);return frames
    def wait_for(predicate,seconds=25):
        for frame in buffered():
            if predicate(frame):return frame
        deadline=time.monotonic()+seconds
        while True:
            frame=host.receive(deadline);seen.append(frame)
            if predicate(frame):return frame
    try:
        host.request('initialize',{'clientInfo':{'name':'intercat_unqueue_probe','version':'0.1.0'},'capabilities':{'userInputDialogs':True}})
        host.send({'jsonrpc':'2.0','method':'initialized'})
        opened=host.request('session/start',{'commandId':command_id(),'workspaceRoot':str(root),'providerId':'meta','approvalMode':'promptUnmatched'})
        session=opened['session']['sessionId']
        result['startedTurns']=1;save()
        start=host.request('turn/start',{'commandId':command_id(),'sessionId':session,'reasoningEffort':'minimal','input':[{'type':'text','text':'This is a disposable protocol test. Use request_user_input now with one question, exactly "Which dummy colour?", and options "Red" and "Blue". Do not answer yourself or use any other tool. Wait for the human response. After it is settled, say only PROBE COMPLETE.'}]})
        active=start['turnId'];assert start['disposition']=='started',start
        request=wait_for(lambda f:f.get('method') in ('userInput/request','userInput/requested'),90)
        result['activeTurn']=active;result['questionId']=request['params']['userInputId'];save()
        print('Dummy question is waiting; testing ten queue reclaims.',flush=True)
        for number in range(10):
            command=command_id()
            queued=host.request('turn/start',{'commandId':command,'sessionId':session,'ifBusy':'queue','input':[{'type':'text','text':f'Dummy queue item {number+1}. This item must be reclaimed before execution.'}]})
            assert queued['disposition']=='queued',queued
            turn=queued['turnId'];reclaim=command_id()
            ack=host.request('turn/unqueue',{'commandId':reclaim,'sessionId':session,'turnId':turn})
            assert ack['status']=='accepted' and ack['turnId']==turn,ack
            event=wait_for(lambda f:f.get('method')=='turn/unqueued' and f['params'].get('turnId')==turn)
            assert event['params']['commandId']==command,event
            replay=host.request('turn/unqueue',{'commandId':reclaim,'sessionId':session,'turnId':turn})
            assert replay==ack,(replay,ack)
            result['cycles'].append({'number':number+1,'start':queued,'unqueue':ack,'event':event,'replayMatches':True});save()
            print(f'Reclaimed {number+1}/10.',flush=True)
        try:
            host.request('turn/unqueue',{'commandId':command_id(),'sessionId':session,'turnId':active})
        except RuntimeError as error:
            refused=json.loads(str(error))
            assert refused.get('code')==-32030,refused
            result['activeUnqueueRejected']=refused
        else:
            raise AssertionError('An already active turn was reclaimed.')
        steering=command_id()
        steered=host.request('turn/start',{'commandId':steering,'sessionId':session,'ifBusy':'steer','input':[{'type':'text','text':'This is an added dummy transport-test message. Keep the existing dummy question unanswered. Do not use another tool.'}]})
        assert steered['disposition']=='steered' and steered['turnId']==active,steered
        result['steered']=steered
        result['beforeStop']=host.request('session/read',{'sessionId':session,'excludeItems':True});buffered()
        host.request('turn/interrupt',{'commandId':command_id(),'sessionId':session,'turnId':active})
        terminal=wait_for(lambda f:f.get('method')=='turn/completed' and f['params'].get('turnId')==active)
        result['activeTerminal']=terminal
        buffered()
        reclaimed={cycle['start']['turnId'] for cycle in result['cycles']}
        launched=[f for f in seen if f.get('method') in ('turn/started','turn/completed') and f.get('params',{}).get('turnId') in reclaimed]
        assert not launched,launched
        result['reclaimedTurnsLaunched']=len(launched);result['passed']=True;save()
        print('Ten reclaims and replay checks passed; no reclaimed turn launched.',flush=True)
    except Exception as error:
        result['error']=str(error);save();raise
    finally:
        if session and active and not result['passed']:
            try:host.request('turn/interrupt',{'commandId':command_id(),'sessionId':session,'turnId':active})
            except Exception:pass
        host.close()

if __name__=='__main__':main()
