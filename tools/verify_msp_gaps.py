"""Check concrete gap schedules, preserving the remaining M1.14 obligations."""
import hashlib
import json
from pathlib import Path
import re
import sys

from build_msp_gap_fixture import build
from compare_msp_replay import compare
from verify_msp_replay import tree


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def calls(case, rust):
    ids = set()
    out = []
    for frame in case['sent']:
        assert frame['method'] == 'view/page'
        assert isinstance(frame['id'], int) and frame['id'] > 0 and frame['id'] not in ids
        ids.add(frame['id'])
        params = dict(frame['params'])
        if rust:
            assert params.pop('direction') == 'forward'
            assert params.pop('maxBytes') == 512 * 1024
        assert set(params) == {'cursor', 'limit', 'sessionId'} and params['limit'] == 200
        out.append({'method':frame['method'], 'params':params})
    return out


def verify(directory):
    root = Path(__file__).resolve().parents[1]
    fixture = json.loads((directory / 'fixture.json').read_text())
    assert fixture == build()
    rust = json.loads((directory / 'rust.json').read_text())
    ts = json.loads((directory / 'ts.json').read_text())
    assert len(rust['cases']) == len(ts['cases']) == len(fixture['cases']) == 8
    frame_count = sum(len(c['initial']) + 1 + len(c['history']) + len(c['during']) + len(c['tail']) for c in fixture['cases'])
    assert rust['validatedFrames'] == frame_count
    for actual, expected, case in zip(rust['cases'], ts['cases'], fixture['cases']):
        assert actual['id'] == expected['id'] == case['id']
        assert actual['final'] == expected['final']
        assert actual['current'] == expected['current'] == ('fault' not in case)
        assert bool(actual['error']) == bool(expected['errors']) == ('fault' in case)
        assert actual['restartVerified']
        assert calls(actual, True) == calls(expected, False)
    old_rust = json.loads((directory / 'before/rust.json').read_text())['cases']
    old_ts = json.loads((directory / 'before/ts.json').read_text())['cases']
    differences = [a['id'] for a,b in zip(old_rust,old_ts) if a['final'] != b['final']]
    assert differences == ['late-gap']
    before_coalesced = next(c for c in old_rust if c['id'] == 'coalesced-gap')
    assert len(before_coalesced['sent']) == 3
    assert 'a_late_gap_cannot_roll_session_state_back_after_restart ... FAILED' in (directory / 'late-gap-before.log').read_text()
    related = (directory / 'related-tests.log').read_text()
    assert '15 passed; 0 failed' in related and '21 passed; 0 failed' in related
    prior_dir = directory.parent / 'msp-session-view-20261104'
    previous = json.loads((prior_dir / 'verification.json').read_text())
    for name in ['fixture.json', 'ts.json']:
        assert sha(directory / 'state-replay' / name) == previous['artifactSha256'][name]
    state_replay = compare(directory / 'state-replay')
    assert state_replay['differentCases'] == 0 and state_replay['cases'] == 531
    assert all(c['restartVerified'] for c in json.loads((directory / 'state-replay/rust.json').read_text())['cases'])
    unchanged = ['crates/intercat-daemon/src/msp.rs', 'crates/intercat-daemon/src/execution.rs',
                 'crates/intercat-daemon/src/repair_execution.rs', 'crates/intercat-daemon/src/approval.rs']
    prior_approval = json.loads((directory.parent / 'msp-repair-oracle-20261104/verification.json').read_text())
    for name in unchanged:
        assert sha(root / name) == prior_approval['sourceSha256'][name]
    for name, digest in json.loads((directory / 'regression-inputs.json').read_text()).items():
        assert sha(root / name) == digest, name
    log = (directory / 'rust-tests.log').read_text()
    totals = re.findall(r'test result: ok\. (\d+) passed; 0 failed; (\d+) ignored', log)
    assert totals and 'FAILED' not in log and 'Doc-tests intercat_git' in log
    assert 'a_late_gap_cannot_roll_session_state_back_after_restart ... ok' in log
    assert 'a_replayed_completion_remains_the_latest_restore_fact ... ok' in log
    assert 'Finished' in (directory / 'clippy.log').read_text()
    assert not (directory / 'format.log').read_text().strip()
    sdk = root / 'research/verify/support/node_modules/@muse-code/sdk'
    assert tree(sdk, (sdk / 'dist').rglob('*.js')) == previous['oracleTreeSha256']
    frontend = tree(root, [p for folder in ['src','electron'] for p in (root/folder).rglob('*') if p.is_file()])
    assert frontend == previous['frontendTreeSha256']
    files = ['crates/intercat-daemon/src/history.rs', 'crates/intercat-daemon/src/history_store.rs',
             'crates/intercat-daemon/src/reconcile_store.rs', 'crates/intercat-daemon/tests/history.rs',
             'crates/intercat-daemon/examples/msp_gap.rs', 'tools/build_msp_gap_fixture.py',
             'tools/msp_gap_oracle.mjs', 'tools/verify_msp_gaps.py']
    artifacts = ['fixture.json','rust.json','ts.json','before/fixture.json','before/rust.json','before/ts.json',
                 'late-gap-before.log','related-tests.log','rust-tests.log','clippy.log','format.log',
                 'regression-inputs.json','state-replay/fixture.json','state-replay/ts.json','state-replay/rust.json']
    return {'requirement':'M1.14', 'requirementComplete':False, 'scope':'production history recovery and consumption of its returned buffered frames',
            'completionSignal':'Rust recovery return success compared with TS fold currency; not persisted worker state',
            'failureBoundary':'returned live frames are consumed by the adapter; worker disconnect/reconnect handling remains unverified',
            'cases':8, 'typedFixtureFrames':frame_count, 'restartVerifiedCases':8,
            'beforeStateDifferences':differences, 'stateReplayCases':531, 'stateReplayFrames':19219,
            'stateReplayDifferences':0, 'commonPageRequestParametersMatched':True,
            'requestPolicyDifference':{'rust':{'direction':'forward','maxBytes':524288},
                                       'typescript':'omits these optional parameters'},
            'remaining':['persisted coalesced gaps and interrupted fills', 'compaction and snapshot recovery',
                         'transient delta display','host opening and handshake','reconnect and resend decisions'],
            'rustPassed':sum(int(p) for p,i in totals), 'rustIgnored':sum(int(i) for p,i in totals),
            'oracleTreeSha256':previous['oracleTreeSha256'], 'frontendTreeSha256':frontend,
            'desktopPassedFromUnchangedFrontend':76, 'paidTurnsThisTurn':0,'cumulativePaidTurns':154,
            'sourceSha256':{n:sha(root/n) for n in files}, 'artifactSha256':{n:sha(directory/n) for n in artifacts}}


if __name__ == '__main__':
    directory = Path(sys.argv[1])
    result = verify(directory)
    (directory/'verification.json').write_text(json.dumps(result,ensure_ascii=False,indent=2)+'\n')
    print(json.dumps({k:result[k] for k in ['cases','stateReplayCases','stateReplayDifferences','rustPassed','rustIgnored','requirementComplete']}))
