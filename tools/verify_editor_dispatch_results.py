"""Match real Editor recovery and worker delivery to the saved database and Git objects."""
import hashlib
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile


def verify(directory):
    repository = Path(__file__).resolve().parents[1]
    previous = json.loads((directory.parent / 'editor-recovery-20261104/verification.json').read_text())
    for file, digest in previous['sourceSha256'].items():
        assert hashlib.sha256((repository / file).read_bytes()).hexdigest() == digest, file
    result = json.loads((directory / 'result.json').read_text())
    attempt = json.loads((directory / 'attempt.json').read_text())
    held = json.loads((directory / 'held.json').read_text())
    states = json.loads((directory / 'states.json').read_text())
    assert result['passed'] and attempt['passed'] and attempt['actualPaidTurns'] == 1
    assert attempt['previousPaidTurns'] == 151 and attempt['cumulativePaidTurns'] == 152
    assert result['sourceUnchanged'] and result['modelOriginalUnchanged']
    assert held['inputId'] == result['inputId'] and held['turnStarts'] == 0
    assert held['editor']['phase'] == 'unavailable' and held['editor']['restart'] == 3 and held['editor']['canRetry']
    waiting = [state for state in states if state['words'] and not state['editor']['modelLoaded']]
    assert waiting and any(state['editor']['phase'] == 'starting' for state in waiting)
    assert all(all(word['state'] == 'staged' for word in state['words']) and not state['workers'] for state in waiting)
    assert any(state['editor']['phase'] == 'ready' and state['words'][0]['state'] == 'landed' for state in states if state['words'])
    decision = result['input']['decision']
    assert decision['kind'] == 'fork' and decision['boundary'] == 'now'
    assert not decision['timedOut'] and 0 < decision['inferenceMs'] < 1200
    checkpoint = result['worker']['checkpoint']
    assert checkpoint and not checkpoint['unmergedPaths'] and not checkpoint['liveProcesses']
    assert result['worker']['phase'] == 'ready' and result['input']['state'] == 'landed'
    with sqlite3.connect(f'file:{directory / "evidence.sqlite"}?mode=ro', uri=True) as database:
        assert database.execute('PRAGMA integrity_check').fetchone()[0] == 'ok'
        assert database.execute('PRAGMA user_version').fetchone()[0] == 50
        assert database.execute("SELECT count(*) FROM operations WHERE kind='turn.start'").fetchone()[0] == 1
        assert database.execute("SELECT count(*) FROM operations WHERE kind='checkpoint.save' AND state='complete'").fetchone()[0] == 1
        row = database.execute('SELECT text,state FROM utterances WHERE id=?', (result['inputId'],)).fetchone()
        assert row == (result['input']['text'], 'landed')
        events = [(seq, kind, json.loads(payload)) for seq, kind, payload in database.execute('SELECT seq,kind,payload FROM events ORDER BY seq')]
        configured = [seq for seq, kind, _ in events if kind == 'editor.configured']
        assert len(configured) == 2
        assert database.execute('SELECT state FROM operations WHERE id=?', (result['repairOperation'],)).fetchone()[0] == 'complete'
        reading = next(seq for seq, kind, value in events if kind == 'utterance.reading' and value['id'] == result['inputId'])
        sent = next(seq for seq, kind, value in events if kind == 'utterance.execution' and value['id'] == result['inputId'] and value['state'] == 'sent')
        assert configured[1] < reading < sent
        items = {}
        messages = []
        for _, kind, frame in events:
            if kind != 'msp.received': continue
            item = frame.get('params', {}).get('item', {})
            if item.get('kind') == 'toolCall': items[item['itemId']] = item
            if item.get('kind') == 'userMessage': messages.append(item['text'])
        assert len(items) == 1
        item = next(iter(items.values()))
        assert item['tool'] == 'write_file' and item['status'] == 'completed'
        assert json.loads(item['args']) == {'path': 'after-recovery.txt', 'content': 'EDITOR_RECOVERY_DELIVERY_OK\n'}
        assert any(result['input']['text'] in message for message in messages)
    with tempfile.TemporaryDirectory(prefix='ic-editor-git-', dir='/private/tmp') as clone:
        env = {**os.environ, 'GIT_CONFIG_GLOBAL': '/dev/null', 'GIT_CONFIG_NOSYSTEM': '1'}
        subprocess.run(['git', 'clone', '-q', '--bare', str(directory / 'fixture.bundle'), clone], check=True, env=env)
        def git(*args):
            return subprocess.check_output(['git', '-C', clone, *args], env=env, text=True).strip()
        assert git('show', f'{checkpoint["sha"]}:after-recovery.txt') == 'EDITOR_RECOVERY_DELIVERY_OK'
        assert git('rev-parse', 'main') == result['workspace']['base']
        assert git('diff-tree', '--no-commit-id', '--name-only', '-r', checkpoint['sha']) == 'after-recovery.txt'
    return {'milestoneRequirement': 'M1.41', 'schema': 50, 'realGlimmerDecisionMs': decision['inferenceMs'],
            'heldUntilModelReady': True, 'automaticallyDeliveredToRealWorker': True,
            'checkpointVerifiedFromGitBundle': True, 'additionalPaidTurns': 1, 'cumulativePaidTurns': 152,
            'previousUIAndModelRecoveryVerified': True,
            'sourceSha256': {**previous['sourceSha256'], **{file: hashlib.sha256((repository / file).read_bytes()).hexdigest() for file in ('tools/verify_editor_dispatch.py', 'tools/verify_editor_dispatch_results.py')}}}


if __name__ == '__main__':
    directory = Path(sys.argv[1]).resolve()
    result = verify(directory)
    (directory / 'verification.json').write_text(json.dumps(result, ensure_ascii=False, indent=2) + '\n')
    print(json.dumps({key: value for key, value in result.items() if key != 'sourceSha256'}, ensure_ascii=False))
