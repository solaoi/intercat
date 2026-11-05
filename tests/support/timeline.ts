import type { Page } from '@playwright/test';
export async function timelineClips(page: Page) {
  return page.evaluate(async () => {
    const state = await window.intercat.getStatus();
    return (await window.intercat.getTimelineWindow({ tracks: state.tracks!.map(track => track.id), start: 0, end: Number.MAX_SAFE_INTEGER, since: null })).clips;
  });
}
export async function timelineMarks(page: Page) {
  return page.evaluate(async () => {
    const state = await window.intercat.getStatus();
    return (await window.intercat.getTimelineWindow({ tracks: state.tracks!.map(track => track.id), start: 0, end: Number.MAX_SAFE_INTEGER, since: null })).marks;
  });
}
