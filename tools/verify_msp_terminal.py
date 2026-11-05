"""Check terminal-status display fixes against the SDK and targeted regressions."""
import hashlib
import json
from pathlib import Path
import re
import sys

from build_msp_progress_fixture import build


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def verify(directory):
    root = Path(__file__).resolve().parents[1]
    before = json.loads((directory / 'rust-before.json').read_text())['cases'][0]
    oracle_before = json.loads((directory / 'ts-before.json').read_text())['cases'][0]
    assert before['steps'][:-1] == oracle_before['steps'][:-1]
    assert before['steps'][-1] != oracle_before['steps'][-1]
    fixture = json.loads((directory / 'fixture.json').read_text())['cases']
    rust = json.loads((directory / 'rust.json').read_text())['cases']
    oracle = json.loads((directory / 'ts.json').read_text())['cases']
    engine = json.loads((directory / 'engine.json').read_text())['cases']
    assert fixture[:13] == build(root)['cases']
    assert len(fixture) == len(rust) == len(oracle) == 16 and len(engine) == 32
    assert sum(len(c['actions']) for c in fixture) == 115
    modes = {(c['id'], c['privacy']): c for c in engine}
    statuses = set()
    withheld = []
    for case, actual, expected in zip(fixture, rust, oracle):
        assert case['id'] == actual['id'] == expected['id']
        assert actual['steps'] == expected['steps'] == modes[case['id'], 'known-values']['steps']
        rules = modes[case['id'], 'rules']
        assert len(rules['steps']) == len(expected['steps'])
        for scan in rules['scans']:
            assert scan['result'] in {'verified', 'withheld'}
            if scan['result'] == 'withheld':
                assert scan['reason'].startswith('Secret preview ')
                withheld.append({'case': case['id'], **scan})
        for index, (safe, raw) in enumerate(zip(rules['steps'], expected['steps'])):
            if safe is not None:
                assert raw is not None
                assert {k: v for k, v in safe.items() if k != 'text'} == {k: v for k, v in raw.items() if k != 'text'}
                assert raw['text'].startswith(safe['text'])
            elif raw is not None:
                assert any(s['result'] == 'withheld' and s['step'] <= index for s in rules['scans'])
            if expected['finalized'][index]:
                assert safe == raw
        for preview in actual['steps']:
            if preview is not None:
                assert len(preview['text'].encode()) <= 65536
        if case['id'].startswith('higher-update-'):
            statuses.add(case['actions'][4]['frame']['params']['item']['status'])
            for index in [3, 4, 5]:
                assert rules['steps'][index] == expected['steps'][index]
            assert actual['steps'][-1]['text'] == 'corrected completed text\n'
    assert statuses == {'completed', 'failed', 'futureTerminal'}
    assert '1 passed; 0 failed' in (directory / 'engine-tests.log').read_text()
    lib = (directory / 'lib-tests.log').read_text()
    integration = (directory / 'integration-tests.log').read_text()
    assert '81 passed; 0 failed; 13 ignored' in lib
    for name in ['terminal_item_updates_never_accept_late_deltas',
                 'terminal_repair_updates_wait_for_a_verified_body']:
        assert name + ' ... ok' in lib
    groups = re.findall(r'test result: ok\. (\d+) passed; 0 failed', integration)
    assert sorted(map(int, groups)) == [3, 6]
    assert 'FAILED' not in lib + integration
    clippy = (directory / 'clippy.log').read_text()
    assert 'Finished' in clippy and not re.search(r'(warning|error):', clippy)
    assert not (directory / 'format.log').read_text().strip()
    baseline = json.loads((directory.parent / 'msp-handshake-20261105/verification.json').read_text())
    assert baseline['rustPassed'] == 464
    assert baseline['sourceSha256']['crates/intercat-daemon/src/msp.rs'] == sha(root / 'crates/intercat-daemon/src/msp.rs')
    sources = ['crates/intercat-daemon/src/progress.rs', 'crates/intercat-daemon/src/msp.rs',
               'crates/intercat-daemon/tests/progress.rs', 'crates/intercat-daemon/tests/secrets.rs',
               'crates/intercat-daemon/examples/msp_progress.rs',
               'tools/build_msp_progress_fixture.py', 'tools/msp_progress_oracle.mjs',
               'tools/verify_msp_terminal.py']
    frozen = json.loads((directory / 'run-sources.json').read_text())
    assert frozen == {name: sha(root / name) for name in sources}
    artifacts = ['fixture.json', 'before-fixture.json', 'rust-before.json', 'ts-before.json',
                 'rust.json', 'ts.json', 'engine.json', 'engine-tests.log', 'reader.log',
                 'lib-tests.log', 'integration-tests.log', 'clippy.log', 'format.log', 'run-sources.json']
    return {
        'requirement': 'M1.14', 'requirementComplete': False,
        'scope': 'higher item revisions retain explicit or unknown terminal status; late deltas do not append, and repair replacement waits for content validation',
        'readerCases': 16, 'readerSteps': 115, 'engineCases': 32, 'engineSteps': 230,
        'withheldScans': withheld,
        'finalRelatedPassed': 90, 'finalRelatedIgnored': 13,
        'baselineRustPassed': 464, 'fullRegressionRerunForTerminalFix': True,
        'validationOrder': 'Combined SDK replay and final library/integration checks preceded the full workspace regression, run serially after the terminal-status fix.',
        'paidTurnsThisStep': 0, 'cumulativePaidTurns': 156,
        'sourceSha256': frozen, 'artifactSha256': {name: sha(directory / name) for name in artifacts},
    }


if __name__ == '__main__':
    directory = Path(sys.argv[1]).resolve()
    result = verify(directory)
    (directory / 'verification.json').write_text(json.dumps(result, ensure_ascii=False, indent=2) + '\n')
    print(json.dumps({key: result[key] for key in ['readerCases', 'engineSteps', 'finalRelatedPassed', 'requirementComplete']}))
