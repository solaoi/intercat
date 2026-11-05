"""Verify bounded SQLite faults against the native UI observations and saved DBs."""
import hashlib
import json
from pathlib import Path
import sqlite3
import sys


def verify(directory, previous):
    cases = {}
    turns = 0
    for mode in ['full', 'slow-write', 'crash-full', 'boundary-crash']:
        folder = directory / mode
        value = json.loads((folder / 'observations.json').read_text())
        assert value['passed'] and value['mode'] == mode
        restored = value['restored']
        queued = [word for word in restored['utterances'] if 'CHANGELOG' in word['text']]
        assert len(queued) == 1 and queued[0]['state'] == 'held'
        if mode == 'slow-write':
            assert value['stopBeforeWriterResumed'] and value['stopMs'] < 1800
            assert restored['dispatchPaused'] and restored['unloggedControls']
            assert any(word['text'] == 'This write was queued before Stop all.' and word['state'] == 'held'
                       for word in restored['utterances'])
        else:
            fault = value['fault']
            assert fault['errorCode'] == 'SQLITE_FULL' and fault['databasePagesUnchanged']
            assert fault['addedRows'] <= 128 and fault['addedBytes'] <= 65536
            assert all(word['text'] != 'This draft must remain not saved.' for word in restored['utterances'])
            if mode == 'boundary-crash':
                assert value['shellFinishedNormally'] and value['nativeTerminal'] == 'completed'
            else:
                assert value['stopMs'] < 5000
        if mode.endswith('crash') or mode == 'crash-full':
            assert restored['workers'][0]['phase'] == 'needs-recovery'
            assert value['editorExitedWithDaemon']
        else:
            assert restored['unloggedControls']
        with sqlite3.connect(f'file:{folder / "events.sqlite"}?mode=ro', uri=True) as db:
            count = db.execute("SELECT count(*) FROM operations WHERE kind='turn.start'").fetchone()[0]
            assert count == 1, (mode, count)
            turns += count
        cases[mode] = {key: value[key] for key in ['fault', 'stopMs', 'stopBeforeWriterResumed',
                                                  'shellFinishedNormally', 'nativeTerminal'] if key in value}
        cases[mode]['turnStarts'] = count
    root = Path(__file__).resolve().parents[1]
    sources = ['crates/intercat-daemon/src/execution.rs', 'crates/intercat-daemon/src/journal.rs',
               'crates/intercat-daemon/src/dispatch_control.rs', 'crates/intercat-daemon/src/fault_injection.rs',
               'crates/intercat-daemon/src/rpc.rs', 'crates/intercat-daemon/src/delivery.rs',
               'crates/intercat-daemon/src/editor.rs', 'services/editor/src/intercat_editor/server.py',
               'services/editor/src/intercat_editor/lifetime.py', 'src/main.tsx', 'src/style.css',
               'tests/storage-fault.spec.ts']
    return {'passed': True, 'requirements': [30, 37, 56], 'cases': cases,
            'newPaidTurns': turns, 'cumulativePaidTurns': previous + turns,
            'sourceSha256': {name: hashlib.sha256((root / name).read_bytes()).hexdigest() for name in sources}}


if __name__ == '__main__':
    directory = Path(sys.argv[1]).resolve()
    result = verify(directory, int(sys.argv[2]))
    (directory / 'verification.json').write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps({key: value for key, value in result.items() if key != 'sourceSha256'}))
