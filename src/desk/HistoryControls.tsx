import { PrivacyText, usePrivacyLabels } from './PrivacyText';
import type { EditHistory } from '../bridge';
import type { useTimelineHistory } from './useTimelineHistory';

export function HistoryControls({ history, controller, disabled }: {
  history?: EditHistory;
  controller: ReturnType<typeof useTimelineHistory>;
  disabled: boolean;
}) {
  const label = usePrivacyLabels([history?.undo?.blocked ?? history?.undo?.text ?? '', history?.redo?.blocked ?? history?.redo?.text ?? '']);
  if (!history || !(history.undo || history.redo || history.takenBack.length || controller.error)) return null;
  const locked = disabled || controller.busy || controller.pending;
  return <div className="history-controls" aria-label="Timeline edit history">
    <div className="history-actions">
      <button disabled={locked || !history.undo} title={label(history.undo?.blocked || history.undo?.text || '')} onClick={() => controller.step('undo')}>Undo</button>
      <button disabled={locked || !history.redo} title={label(history.redo?.blocked || history.redo?.text || '')} onClick={() => controller.step('redo')}>Redo</button>
      {!!history.takenBack.length && <details className="taken-back"><summary>Taken back · {history.takenBack.length}</summary>
        <div className="taken-back-list">{history.takenBack.map(word => <div className="taken-back-word" key={word.id}>
          <p><PrivacyText text={word.text} /></p><div>
            <button disabled={locked} onClick={() => controller.restore(word, true)}>Restore to draft</button>
            <button disabled={locked} onClick={() => controller.restore(word, false)}>Restore held</button>
          </div>
        </div>)}</div>
      </details>}
    </div>

    {controller.error && <div className="history-error" role="alert"><span>{controller.error}</span>{controller.pending && <><button disabled={controller.busy} onClick={controller.retry}>Retry history change</button><button disabled={controller.busy} onClick={controller.reload}>Reload history</button></>}</div>}
  </div>;
}
