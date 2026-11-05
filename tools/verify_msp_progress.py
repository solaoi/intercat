"""Audit SDK stream ordering, bounded secret-safe previews and regression logs."""
import hashlib
import json
from pathlib import Path
import re
import sys

from build_msp_progress_fixture import build
from verify_msp_replay import tree


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def verify(directory):
    root = Path(__file__).resolve().parents[1]
    fixture = json.loads((directory / 'fixture.json').read_text())
    assert fixture == build(root)
    oracle = json.loads((directory / 'ts.json').read_text())['cases']
    reader = json.loads((directory / 'rust.json').read_text())['cases']
    engine = json.loads((directory / 'engine.json').read_text())['cases']
    assert len(oracle) == len(reader) == 13 and len(engine) == 26
    assert sum(len(c['steps']) for c in oracle) == 97
    by_mode = {(c['id'], c['privacy']): c for c in engine}
    withheld = []
    for case, expected, observed in zip(fixture['cases'], oracle, reader):
        assert case['id'] == expected['id'] == observed['id']
        assert observed['steps'] == expected['steps']
        assert by_mode[case['id'], 'known-values']['steps'] == expected['steps']
        rules = by_mode[case['id'], 'rules']
        assert len(rules['steps']) == len(expected['steps'])
        for scan in rules['scans']:
            assert scan['result'] in {'verified', 'withheld'}
            if scan['result'] == 'withheld':
                assert scan['reason'].startswith('Secret preview ')
                withheld.append({'case': case['id'], **scan})
        for index, (actual, raw) in enumerate(zip(rules['steps'], expected['steps'])):
            if actual is not None:
                assert raw is not None
                assert {k: v for k, v in actual.items() if k != 'text'} == {k: v for k, v in raw.items() if k != 'text'}
                assert raw['text'].startswith(actual['text'])
            elif raw is not None:
                assert any(s['result'] == 'withheld' and s['step'] <= index for s in rules['scans'])
            if expected['finalized'][index]:
                assert actual == raw
        for preview in observed['steps']:
            if preview is not None:
                assert len(preview['text'].encode()) <= 65536
    assert '2 failed' in (directory / 'duplicates-before.log').read_text()
    assert 'early_deltas_wait_for_the_item_without_losing_the_prefix ... FAILED' in (directory / 'early-before.log').read_text()
    assert '1 passed; 0 failed' in (directory / 'engine-tests.log').read_text()
    log = (directory / 'rust-tests.log').read_text()
    assert 'FAILED' not in log and 'Doc-tests intercat_git' in log
    counts = re.findall(r'test result: ok\. (\d+) passed; 0 failed; (\d+) ignored', log)
    assert sum(int(n) for n, _ in counts) == 460
    assert sum(int(n) for _, n in counts) == 27
    for name in [
        'duplicate_revisions_cannot_replace_live_text',
        'duplicate_revisions_cannot_replace_scanned_live_text',
        'early_deltas_wait_for_the_item_without_losing_the_prefix',
        'early_secret_fragments_are_masked_across_the_item_start',
        'early_buffers_share_the_item_limit_and_reset_at_gaps_and_new_commands',
        'only_submitted_or_recovered_turns_can_supply_a_preview',
        'reader_exposes_live_text_before_consumption_but_persists_only_delta_metadata',
        'repair_reader_releases_only_scanned_prefixes_and_still_resolves_replies',
    ]:
        assert name + ' ... ok' in log
    clippy = (directory / 'clippy.log').read_text()
    assert 'Finished' in clippy and not re.search(r'(warning|error):', clippy)
    assert not (directory / 'format.log').read_text().strip()
    previous = json.loads((directory.parent / 'msp-budget-runtime-20261105/verification.json').read_text())
    sdk = root / 'research/verify/support/node_modules/@muse-code/sdk'
    assert tree(sdk, (sdk / 'dist').rglob('*.js')) == previous['oracleTreeSha256']
    frontend = tree(root, [p for d in ['src', 'electron'] for p in (root / d).rglob('*') if p.is_file()])
    assert frontend == previous['frontendTreeSha256']
    sources = ['crates/intercat-daemon/src/progress.rs', 'crates/intercat-daemon/src/msp.rs',
               'crates/intercat-daemon/examples/msp_progress.rs', 'tools/build_msp_progress_fixture.py',
               'tools/msp_progress_oracle.mjs', 'tools/verify_msp_progress.py']
    hashes = json.loads((directory / 'run-sources.json').read_text())
    assert hashes == {name: sha(root / name) for name in sources}
    artifacts = ['fixture.json', 'ts.json', 'rust.json', 'engine.json', 'comparison.json',
                 'duplicates-before.log', 'early-before.log', 'engine-tests.log',
                 'reader.log', 'rust-tests.log', 'clippy.log', 'format.log', 'run-sources.json']
    return {
        'requirement': 'M1.14', 'requirementComplete': False, 'schema': 53,
        'scope': 'transient agent-message preview ordering under the declared active command; SDK text projection, secret-scanned prefix and byte/item limits',
        'readerCases': 13, 'readerSteps': 97, 'engineCases': 26, 'engineSteps': 194,
        'differentReaderCases': 0, 'withheldScans': withheld,
        'oraclePolicy': 'SDK item revisions and accumulated text; active-command filtering and 64-KiB UI prefix. Rules mode checks identical ordered scan inputs and only releases verified prefixes.',
        'rustPassed': 460, 'rustIgnored': 27, 'frontendTreeSha256': frontend,
        'desktopTestsRerun': False, 'oracleTreeSha256': previous['oracleTreeSha256'],
        'paidTurnsThisStep': 0, 'cumulativePaidTurns': 156,
        'remaining': ['full host initialization SDK comparison', 'fault-driven reconnect/resend decisions',
                      'native retained-history eviction and pruned-boundary reproduction',
                      'actual VoiceOver and all 60 acceptance gates'],
        'sourceSha256': hashes, 'artifactSha256': {name: sha(directory / name) for name in artifacts},
    }


if __name__ == '__main__':
    directory = Path(sys.argv[1]).resolve()
    result = verify(directory)
    (directory / 'verification.json').write_text(json.dumps(result, ensure_ascii=False, indent=2) + '\n')
    print(json.dumps({key: result[key] for key in ['readerCases', 'readerSteps', 'engineSteps', 'rustPassed', 'requirementComplete']}))
