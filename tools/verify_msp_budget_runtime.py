"""Audit the native budget-downgrade worker run and its persisted evidence."""
import hashlib
import json
from pathlib import Path
import re
import sqlite3
import sys

from verify_msp_replay import tree


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def verify(directory):
    root = Path(__file__).resolve().parents[1]
    native = directory / 'native-8'
    settings = json.loads((directory / 'run-settings.json').read_text())
    assert settings['paidTurns'] == 0
    assert settings['binarySha256'] == sha(Path(settings['binary'])) == '1c96114408acbeade9a8fb79e3b4425e5b64d50425938568aef01bdc04ce2e03'
    result = json.loads((native / 'result.json').read_text())
    reopened = json.loads((native / 'reopened-view.json').read_text())
    assert result['provider'] == 'echo' and result['paidTurns'] == 0
    assert 64 <= result['seedTurns'] <= 96
    assert result['sameSession'] and result['restartMatches'] and result['sourceUnchanged']
    assert result['nextInputs'] == 1 and result['schema'] == 53
    assert result['beforeItems'] == 4 and result['restoredItems'] == 4 * result['seedTurns']
    assert result['persistedItems'] == len(reopened['items']) > result['restoredItems']
    resume = result['runtimeResume']
    assert resume['history']['mode'] == 'none'
    assert resume['history']['noneReason'] == 'historyBudget'
    assert resume['session']['providerId'] == 'echo'
    worker = result['worker']
    assert worker['sessionId'] == resume['session']['sessionId']
    assert worker['activeTurn'] is None and not worker['checkpointPending']
    assert 'BUDGET_RECOVERY_NEXT_INPUT' in worker['output']
    checkpoint = worker['checkpoint']['turnId']
    assert not worker['checkpoint']['liveProcesses']
    turns = {t['turnId']: t for t in reopened['turns']}
    assert len(turns) == result['seedTurns'] + 1
    assert all(t['terminal'] == 'completed' for t in turns.values())
    assert checkpoint in turns and reopened['activeTurnId'] is None
    with sqlite3.connect((native / 'events.sqlite').as_uri() + '?mode=ro', uri=True) as db:
        operations = list(db.execute("SELECT id,state,payload FROM operations WHERE kind='turn.start'"))
        assert len(operations) == result['seedTurns'] + 1
        assert all(state == 'complete' for _, state, _ in operations)
        submitted = [json.loads(payload) for _, _, payload in operations]
        next_inputs = [p for p in submitted if p['input'][0]['type'] == 'skill']
        assert len(next_inputs) == 1 and next_inputs[0]['commandId'] == checkpoint
        assert 'BUDGET_RECOVERY_NEXT_INPUT' in next_inputs[0]['input'][0]['arguments']
        opened = json.loads(db.execute("SELECT payload FROM events WHERE kind='worker.session'").fetchone()[0])
        assert opened['result'] == resume
        receipts = [json.loads(row[0]) for row in db.execute("SELECT payload FROM events WHERE kind='msp.received'")]
        pages = [f['result']['events'] for f in receipts if isinstance(f.get('result'), dict) and 'events' in f['result']]
        assert len(pages) > 1
        assert not any(f.get('error', {}).get('code') == -32603 for f in receipts)
        applied = [json.loads(row[0]) for row in db.execute("SELECT payload FROM events WHERE kind='msp.applied' ORDER BY seq")]
        terminal_ids = {f['params']['turnId'] for f in applied if f['method'] == 'turn/completed'}
        assert terminal_ids == set(turns)
        assert applied == json.loads((native / 'applied-frames.json').read_text())
    assert json.loads((native / 'sdk-view.json').read_text()) == reopened
    sdk_comparison = json.loads((native / 'sdk-comparison.json').read_text())
    assert sdk_comparison == {'frames': len(applied), 'items': len(reopened['items']),
                              'turns': len(turns), 'differentFields': []}
    sdk = root / 'research/verify/support/node_modules/@muse-code/sdk'
    oracle_baseline = json.loads((directory.parent / 'msp-unobserved-20261105/verification.json').read_text())
    oracle_hash = tree(sdk, (sdk / 'dist').rglob('*.js'))
    assert oracle_hash == oracle_baseline['oracleTreeSha256']
    prior = directory / 'native-4/events.sqlite'
    with sqlite3.connect(prior.as_uri() + '?mode=ro', uri=True) as db:
        errors = [json.loads(row[0]) for row in db.execute("SELECT payload FROM events WHERE kind='msp.received'")]
        assert any(f.get('error', {}).get('message') == 'response exceeds the outbound frame limit' for f in errors)
    assert '1 passed; 0 failed' in (directory / 'native-tests.log').read_text()
    log = (directory / 'rust-tests.log').read_text()
    assert 'FAILED' not in log and 'Doc-tests intercat_git' in log
    counts = re.findall(r'test result: ok\. (\d+) passed; 0 failed; (\d+) ignored', log)
    assert sum(int(n) for n, _ in counts) == 454
    assert sum(int(n) for _, n in counts) == 26
    clippy = (directory / 'clippy.log').read_text()
    assert 'Finished' in clippy and not re.search(r'(warning|error):', clippy)
    assert not (directory / 'format.log').read_text().strip()
    previous = json.loads((directory.parent / 'msp-retention-20261105/verification.json').read_text())
    frontend = tree(root, [p for d in ['src', 'electron'] for p in (root / d).rglob('*') if p.is_file()])
    assert frontend == previous['frontendTreeSha256']
    sources = ['crates/intercat-daemon/src/' + name + '.rs' for name in
               ['host', 'runtime', 'runtime_tests', 'reconcile', 'history', 'session_view']]
    sources += ['crates/intercat-daemon/tests/reconcile.rs', 'tools/verify_msp_budget_runtime.py',
                'tools/msp_budget_oracle.mjs']
    recorded_sources = json.loads((directory / 'run-sources.json').read_text())
    assert recorded_sources == {name: sha(root / name) for name in sources}
    artifacts = ['native-8/' + name for name in ['result.json', 'reopened-view.json', 'events.sqlite',
                 'observed.json', 'applied-frames.json', 'sdk-view.json', 'sdk-comparison.json']]
    artifacts += ['native-tests.log', 'rust-tests.log', 'clippy.log', 'format.log', 'native-4/events.sqlite', 'run-settings.json', 'run-sources.json']
    return {
        'requirement': 'M1.14', 'requirementComplete': False, 'schema': 53,
        'scope': 'real WorkerRunner cold resume after historyBudget, paged recovery, one next input, checkpoint and Journal reopening',
        'nativeProvider': 'echo', 'providerScope': 'test-build-only task-local host setting; production provider configuration unchanged',
        'seedTurns': result['seedTurns'], 'recoveredItems': result['restoredItems'],
        'nextInputs': 1, 'nativeTurnsAllCompleted': True, 'pages': len(pages),
        'journalRestartMatches': True, 'rustPassed': 454, 'rustIgnored': 26,
        'sdkComparedFrames': len(applied), 'sdkComparedItems': len(reopened['items']),
        'sdkComparedTurns': len(turns), 'sdkDifferentFields': [], 'oracleTreeSha256': oracle_hash,
        'paidTurnsThisStep': 0, 'cumulativePaidTurns': 156,
        'frontendTreeSha256': frontend, 'desktopTestsRerun': False,
        'remaining': ['native retained-history eviction and pruned-boundary reproduction',
                      'transient delta display and full initialization SDK comparison',
                      'fault-driven reconnect/resend decisions',
                      'actual VoiceOver and all 60 acceptance gates'],
        'sourceSha256': recorded_sources,
        'artifactSha256': {name: sha(directory / name) for name in artifacts},
    }


if __name__ == '__main__':
    directory = Path(sys.argv[1]).resolve()
    evidence = verify(directory)
    (directory / 'verification.json').write_text(json.dumps(evidence, ensure_ascii=False, indent=2) + '\n')
    print(json.dumps({key: evidence[key] for key in ['seedTurns', 'recoveredItems', 'pages', 'rustPassed', 'requirementComplete']}))
