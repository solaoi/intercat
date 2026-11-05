import type { Page } from '@playwright/test';

interface TraceEvent { name: string; ph: string; ts: number; dur?: number; pid: number; tid: number; args?: { name?: string } }
export async function benchmarkFrames(page: Page, frames = 240) {
  const session = await page.context().newCDPSession(page);
  const complete = new Promise<{ stream?: string }>(resolve => session.once('Tracing.tracingComplete', resolve));
  await session.send('Tracing.start', { categories: 'devtools.timeline,v8,blink.user_timing', transferMode: 'ReturnAsStream' });
  const intervals = await page.evaluate(async count => {
    const times: number[] = [];
    await new Promise<void>(resolve => {
      function frame(now: number) {
        performance.mark('intercat-bench-frame'); times.push(now);
        if (times.length <= count) requestAnimationFrame(frame); else resolve();
      }
      requestAnimationFrame(frame);
    });
    return times.slice(1).map((now, i) => now - times[i]);
  }, frames);
  await session.send('Tracing.end');
  const { stream } = await complete;
  if (!stream) throw new Error('The browser did not return a performance trace.');
  let body = '';
  for (;;) {
    const read = await session.send('IO.read', { handle: stream });
    body += read.base64Encoded ? Buffer.from(read.data, 'base64').toString('utf8') : read.data;
    if (read.eof) break;
  }
  await session.send('IO.close', { handle: stream }); await session.detach();
  const trace = JSON.parse(body) as { traceEvents: TraceEvent[] };
  const markers = trace.traceEvents.filter(event => event.name === 'intercat-bench-frame').sort((a, b) => a.ts - b.ts);
  if (markers.length < frames) throw new Error('The browser did not record frame markers.');
  const thread = markers[0];
  const calls = trace.traceEvents.filter(event => event.pid === thread.pid && event.tid === thread.tid && event.ph === 'X' && event.dur && ['FireAnimationFrame', 'FunctionCall', 'EvaluateScript', 'RunMicrotasks', 'EventDispatch', 'MinorGC', 'MajorGC'].includes(event.name))
    .map(event => [event.ts, event.ts + event.dur!] as const).sort((a, b) => a[0] - b[0]);
  // Merge nested trace intervals so a callback and its FunctionCall count once.
  const merged: number[][] = [];
  for (const call of calls) {
    const last = merged.at(-1);
    if (last && call[0] <= last[1]) last[1] = Math.max(last[1], call[1]); else merged.push([...call]);
  }
  const script = markers.slice(1).map((marker, i) => merged.reduce((sum, [start, end]) => sum + Math.max(0, Math.min(end, marker.ts) - Math.max(start, markers[i].ts)), 0) / 1000);
  const percentile = (values: number[], q: number) => [...values].sort((a, b) => a - b)[Math.min(values.length - 1, Math.floor(values.length * q))];
  return { frames: intervals.length, fps: intervals.length * 1000 / intervals.reduce((a, b) => a + b, 0), frameP95Ms: percentile(intervals, .95), frameMaxMs: Math.max(...intervals), jsMeanMs: script.reduce((a, b) => a + b, 0) / script.length, jsP95Ms: percentile(script, .95), jsMaxMs: Math.max(...script), scriptSamples: script.length, trace };
}
