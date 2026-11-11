import type { IntegrationJob } from './types';

export function RepairOwnerChoices({job,disabled,choose}:{job:IntegrationJob;disabled:boolean;choose:(track:string)=>void}) {
  const owners=job.tracks.filter(track=>track.included&&job.candidate?.commits.some(commit=>commit.track===track.id));
  return <section className="repair-owners" aria-label="Worker to repair the integration">
    {owners.length>1&&<p>Choose the worker to fix the merged result.</p>}
    {!owners.length&&<p>No changed worker commit can own this repair.</p>}
    <div>{owners.map(owner=><button key={owner.id} disabled={disabled} onClick={()=>choose(owner.id)}>Ask Worker {owner.label} to fix</button>)}</div>
  </section>;
}
export function RepairAttribution({job}:{job:IntegrationJob}) {
  const repair=job.candidate?.repair;
  if(!repair||repair.requestedTrack===repair.appliedTrack)return null;
  const label=job.tracks.find(track=>track.id===repair.appliedTrack)?.label??'?';
  return <p className="commit-notice" role="status">The fix needed {label}’s changes · added to {label}’s commit</p>;
}
