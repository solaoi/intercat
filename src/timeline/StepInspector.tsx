import { useEffect, useState } from 'react';
import type { OutputResult, StepDetail, StepItem } from '../bridge';
import { DeletionRecovery } from './DeletionRecovery';
import { Effects } from '../desk/Effects';
import { MarkdownMessage } from '../markdown/MarkdownMessage';

interface Patch { files: { path: string; hunks: { oldStart: number; oldLines: number; newStart: number; newLines: number; lines: string[] }[] }[] }
function patchText(output: OutputResult): string | null {
  if (output.encoding !== 'utf8' || !output.content) return null;
  try {
    const patch = JSON.parse(output.content) as Patch;
    if (!Array.isArray(patch.files)) return null;
    return patch.files.map(file => `--- ${file.path}\n+++ ${file.path}\n${file.hunks.map(hunk => `@@ -${hunk.oldStart},${hunk.oldLines} +${hunk.newStart},${hunk.newLines} @@\n${hunk.lines.join('\n')}`).join('\n')}`).join('\n');
  } catch { return null; }
}
function Artifact({ name, output }: { name: string; output?: OutputResult }) {
  if (!output) return <p>{name} is being retrieved.</p>;
  if (output.unavailable) return <p>{name} could not be retrieved from this worker.</p>;
  const text = name === 'Patch' ? patchText(output) : output.encoding === 'utf8' ? output.content : null;
  return <div className="step-artifact"><strong>{name}</strong>{output.eof === false || output._intercatTruncated?.length ? <p>Partial output — the stored preview is truncated.</p> : null}{text ? <pre>{text}</pre> : <p>No text preview is available.</p>}</div>;
}
function Item({ item }: { item: StepItem }) {
  return <article className="step-item">
    <header><strong>{item.tool ?? item.kind}</strong><span>{item.status} · revision {item.revision}</span></header>
    {item.recordedAt && <small>{item.recordedAt}</small>}
    {item.retracted && <p>This item was retracted by the worker.</p>}
    {item.truncated || item._intercatTruncated?.length ? <p>Partial output — the stored preview is truncated.</p> : null}
    {item.args && <details><summary>Arguments</summary><pre>{item.args}</pre></details>}
    {item.commandText && <pre>{item.commandText}</pre>}
    {item.text && (item.kind === 'agentMessage' ? <MarkdownMessage text={item.text} /> : <details><summary>Message</summary><pre>{item.text}</pre></details>)}
    {item.visibleOutput && <details><summary>Tool output</summary><pre>{item.visibleOutput}</pre></details>}
    {item.failureReason && <p role="note">{item.failureReason}</p>}
    {item.outputRef && <Artifact name="Full output" output={item.outputs.outputRef} />}
  </article>;
}
export default function StepInspector({ clipId, sequence }: { clipId: string; sequence?: number }) {
  const [detail, setDetail] = useState<StepDetail | null>(null);
  const [error, setError] = useState('');
  useEffect(() => { setDetail(null); setError(''); }, [clipId]);
  useEffect(() => {
    let disposed = false;
    void window.intercat.getClipDetail(clipId).then(value => { if (!disposed) { setDetail(value); setError(''); } }).catch(() => { if (!disposed) setError('This step could not be loaded.'); });
    return () => { disposed = true; };
  }, [clipId, sequence]);
  return <div className="step-detail" aria-label="Step details">{error && <p role="alert">{error}</p>}{!detail && !error && <p>Loading step…</p>}{detail && <>
    <div className="step-metadata"><span>{detail.clip.category === 'snapshot' ? 'Recovered history · timing unavailable' : <>{detail.clip.running ? 'In progress' : 'Finished'} · {((detail.clip.end - detail.clip.start) / 1000).toFixed(1)} s{detail.clip.approximate ? ' · approximate duration' : ''}</>}</span><span>{detail.clip.turnId ? `Turn ${detail.clip.turnId}` : 'Session history'}</span></div>
    <Effects effects={detail.effects ?? []} />
    {detail.deletionSnapshots?.map(snapshot => <DeletionRecovery key={snapshot.id} snapshot={snapshot} />)}
    {detail.items.filter(item => item.patchRef).map(item => <Artifact key={item.itemId} name="Patch" output={item.outputs.patchRef} />)}
    {detail.items.length ? detail.items.map(item => <Item item={item} key={item.itemId} />) : <p>Waiting for the next worker item.</p>}
  </>}</div>;
}
