import {expect,test} from '@playwright/test';
import {liveTimeline} from '../src/timeline/state';
import type {DaemonStatus} from '../src/bridge';
import type {RepairRun} from '../src/commit/repair';

test('the integration needs-you mark preserves the working track and follows only its repair',()=>{
  const run:RepairRun={id:'dummy-merge',revision:1,owner:{kind:'commit',id:'dummy-job',revision:1},track:'B',purpose:{kind:'merge'},phase:'held',approvals:[],prompts:[],output:null,failure:null,session:'merge-session',turn:null,secretInput:null,refresh:null,restoring:null,remaining:null};
  const status={connected:true,tracks:[{id:'B',label:'B',title:'Agent-created dummy',status:'working'}],workers:[{trackId:'B',phase:'working',activeTurn:'main-turn',sessionId:'main-session'}],repairRuns:[run],integrations:[{id:'dummy-job',status:'failed',candidate:{state:'conflict',failedTrack:'B'}}],utterances:[],workspaces:[]} as unknown as DaemonStatus;
  for(const phase of ['held','manual','running','captured'] as const){
    run.phase=phase;const track=liveTimeline(status,[])[0];
    expect(track.status).toBe('working');
    expect(track.integrationFailed).toBe(phase==='held'||phase==='manual');
    expect(status.workers![0]).toMatchObject({sessionId:'main-session',activeTurn:'main-turn'});
  }
  status.integrations![0]!.status='checked';run.phase='done';expect(liveTimeline(status,[])[0].integrationFailed).toBe(false);
});
