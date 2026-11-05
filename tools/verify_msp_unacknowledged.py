"""Verify same-ID intake recovery and its Stop/capacity boundaries."""
import hashlib
import json
from pathlib import Path
import re
import sys

from probe_msp_unacknowledged import build
from verify_msp_replay import tree


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def verify(directory):
    root = Path(__file__).resolve().parents[1]
    fixture = json.loads((directory / 'fixture.json').read_text())
    assert fixture == build()
    rust = json.loads((directory / 'rust.json').read_text())['cases']
    sdk = json.loads((directory / 'ts.json').read_text())['cases']
    assert len(fixture['cases']) == len(rust) == len(sdk) == 9
    states = {}
    for case, actual, expected in zip(fixture['cases'], rust, sdk):
        assert case['id'] == actual['id'] == expected['id']
        assert actual['restartVerified']
        assert actual['logicalState'] == expected['logicalState']
        replayed = [frame['params'] for frame in actual['sent'] if frame['method'] == 'turn/start']
        assert replayed == [frame['params'] for frame in expected['sent'][1:]]
        assert replayed == (case['operations'] if case['allowReplay'] else [])
        assert [op['payload'] for op in actual['operations']] == case['operations']
        assert actual['input']['text'] == case['operations'][0]['input'][0]['text']
        assert actual['input']['state'] == ('held' if actual['logicalState'] == 'held' else 'starting')
        states[case['id']] = actual['logicalState']
    assert states == {
        'materialized-after-lost-ack': 'running', 'abandoned': 'held',
        'unknown-rejection-reason': 'held', 'code-only-durable-rejection': 'held',
        'kind-only-durable-rejection': 'held', 'transient-replay-failure': 'pending',
        'second-disconnect': 'pending', 'ack-without-materialization': 'pending',
        'stopped-before-replay': 'pending',
    }
    old = json.loads((directory / 'rust-before.json').read_text())['cases'][0]
    old_sdk = json.loads((directory / 'ts-before.json').read_text())['cases'][0]
    assert old['operations'][0]['state'] == 'superseded'
    assert not any(f['method'] == 'turn/start' for f in old['sent'])
    assert len(old_sdk['sent']) == 2 and len(old_sdk['pending']) == 1
    log = (directory / 'rust-tests.log').read_text()
    assert 'FAILED' not in log and 'Doc-tests intercat_git' in log
    totals = re.findall(r'test result: ok\. (\d+) passed; 0 failed; (\d+) ignored', log)
    assert sum(int(n) for n, _ in totals) == 468
    assert sum(int(n) for _, n in totals) == 28
    for test in ['a_stop_cancels_queued_starts_but_keeps_the_connection_usable',
                 'a_cancelled_recovery_query_wakes_without_a_fabricated_host_frame',
                 'recovery_reserves_its_execution_slot_and_stop_interrupts_the_wait',
                 'an_internal_hold_cancels_old_starts_without_invalidating_its_resume',
                 'only_an_owned_queued_turn_is_reclaimed_and_the_start_is_held',
                 'reclaim_reuses_its_saved_identity_and_waits_for_removal_before_completion']:
        assert test + ' ... ok' in log
    clippy = (directory / 'clippy.log').read_text()
    assert 'Finished' in clippy and not re.search(r'(warning|error):', clippy)
    assert not (directory / 'format.log').read_text().strip()
    previous = json.loads((directory.parent / 'msp-handshake-20261105/verification.json').read_text())
    sdk_path = root / 'research/verify/support/node_modules/@muse-code/sdk'
    assert tree(sdk_path, (sdk_path / 'dist').rglob('*.js')) == previous['oracleTreeSha256']
    frontend = tree(root, [p for d in ['src', 'electron'] for p in (root / d).rglob('*') if p.is_file()])
    assert frontend == previous['frontendTreeSha256']
    sources = ['crates/intercat-daemon/src/' + name + '.rs' for name in
               ['msp', 'history', 'reconcile', 'reconcile_store', 'execution', 'runtime', 'runtime_tests']]
    sources += ['crates/intercat-daemon/tests/reconcile.rs',
                'crates/intercat-daemon/examples/msp_reconnect.rs',
                'crates/intercat-daemon/examples/msp_snapshot_probe.rs',
                'tools/probe_msp_unacknowledged.py', 'tools/msp_unacknowledged_oracle.mjs',
                'tools/verify_msp_unacknowledged.py']
    frozen = json.loads((directory / 'run-sources.json').read_text())
    assert frozen == {name: sha(root / name) for name in sources}
    artifacts = ['fixture.json', 'before-fixture.json', 'rust.json', 'ts.json',
                 'rust-before.json', 'ts-before.json', 'comparison-before.json',
                 'rust-tests.log', 'clippy.log', 'format.log', 'run-sources.json']
    return {
        'requirement': 'M1.14', 'requirementComplete': False,
        'scope': 'Unacknowledged turn intake: exact saved command replay, durable rejection, unresolved outcomes, and Stop/capacity boundaries',
        'cases': 9, 'states': states, 'exactReplayMatches': 8,
        'rustPassed': 468, 'rustIgnored': 28,
        'oracleTreeSha256': previous['oracleTreeSha256'], 'frontendTreeSha256': frontend,
        'paidTurnsThisStep': 0, 'cumulativePaidTurns': 156,
        'remaining': ['broader fault/reconnect/resend coverage', 'native retained-history eviction',
                      'actual VoiceOver and all 60 acceptance gates'],
        'sourceSha256': frozen, 'artifactSha256': {name: sha(directory / name) for name in artifacts},
    }


if __name__ == '__main__':
    output = Path(sys.argv[1]).resolve()
    result = verify(output)
    (output / 'verification.json').write_text(json.dumps(result, ensure_ascii=False, indent=2) + '\n')
    print(json.dumps({key: result[key] for key in ['cases', 'exactReplayMatches', 'rustPassed', 'requirementComplete']}))
