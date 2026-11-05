"""Check the wire portion of M1.14; session-state/reconnect remain separate."""
import hashlib
import json
from pathlib import Path
import re
import sys


def verify(directory):
    repository = Path(__file__).resolve().parents[1]
    fixture = json.loads((directory / 'transport-fixture.json').read_text())
    rust = json.loads((directory / 'rust-transport.json').read_text())
    oracle = json.loads((directory / 'ts-transport.json').read_text())
    inventory = fixture['inventory']
    assert not inventory['unparseableLines']
    assert inventory['serverRequestIdsChecked'] > 0 and not inventory['invalidServerRequestIds']
    assert len(inventory['requiredClientMethods']) == 22
    assert {entry['method'] for entry in fixture['calls']} | {fixture['notify']['method']} == set(inventory['requiredClientMethods'])
    assert {entry['frame']['method'] for entry in fixture['notifications']} == set(inventory['requiredNotifications']) | set(inventory['otherRecordedNotifications'])
    assert {entry['frame']['method'] for entry in fixture['requests']} == set(inventory['requiredServerRequests'])
    assert rust['schemaFingerprint'] == oracle['schemaFingerprint']
    assert rust['validatedTypes'] == len(fixture['validation']) == 87
    assert len({value['name'] for value in fixture['validation']}) == 84
    assert rust['results'] == oracle['results'] == [entry['result'] for entry in fixture['calls']]
    assert rust['sent'] == oracle['sent'] and len(rust['sent']) == 25
    assert rust['requests'] == oracle['requests'] == [entry['frame'] for entry in fixture['requests']]
    assert len(rust['notifications']) == len(oracle['notifications']) == 47
    deltas = 0
    for actual, expected in zip(rust['notifications'], oracle['notifications']):
        if expected['method'] == 'item/delta':
            expected = json.loads(json.dumps(expected))
            value = expected['params'].pop('delta')
            expected['params']['deltaBytes'] = len(value.encode()) if isinstance(value, str) else len(json.dumps(value, ensure_ascii=False, separators=(',', ':')).encode())
            deltas += 1
        assert actual == expected, expected['method']
    assert deltas == 1
    before = json.loads((directory / 'invalid-request-ids-before.json').read_text())
    after = json.loads((directory / 'invalid-request-ids-result.json').read_text())
    assert before['rust']['exitCode'] == 0 and before['typescript']['exitCode'] == 1
    assert all(after[language]['exitCode'] == 1 and after[language]['rejectedInvalidId'] for language in ['rust', 'typescript'])
    assert not after['requiresResolution']
    assert 'invalid_server_request_ids_never_engage_or_present_a_question ... ok' in (directory / 'transport-tests.log').read_text()
    assert 'Finished' in (directory / 'clippy.log').read_text()
    assert not (directory / 'format.log').read_text().strip()
    rust_log = (directory / 'rust.log').read_text()
    totals = re.findall(r'test result: ok\. (\d+) passed; 0 failed; (\d+) ignored', rust_log)
    assert totals and 'FAILED' not in rust_log and 'Doc-tests intercat_git' in rust_log
    sdk = repository / 'research/verify/support/node_modules/@muse-code/sdk'
    assert json.loads((sdk / 'package.json').read_text())['version'] == '1.4.14'
    digest = hashlib.sha256()
    for path in sorted((sdk / 'dist').rglob('*.js')):
        digest.update(str(path.relative_to(sdk)).encode() + b'\0' + hashlib.sha256(path.read_bytes()).digest())
    paths = ['crates/intercat-daemon/src/msp.rs', 'crates/intercat-daemon/examples/msp_transport.rs',
             'crates/intercat-daemon/examples/support/msp_transport_types.rs',
             'tools/audit_msp_corpus.py', 'tools/build_msp_transport_fixture.py',
             'tools/msp_transport_oracle.mjs', 'tools/verify_msp_transport.py']
    rust_tree = hashlib.sha256()
    rust_inputs = list((repository / 'crates').rglob('*.rs')) + list((repository / 'crates').rglob('Cargo.toml')) + [repository / 'Cargo.toml', repository / 'Cargo.lock']
    for path in sorted(rust_inputs):
        rust_tree.update(str(path.relative_to(repository)).encode() + b'\0' + hashlib.sha256(path.read_bytes()).digest())
    frontend_tree = hashlib.sha256()
    for path in sorted([path for folder in ['src', 'electron'] for path in (repository / folder).rglob('*') if path.is_file()]):
        frontend_tree.update(str(path.relative_to(repository)).encode() + b'\0' + hashlib.sha256(path.read_bytes()).digest())
    ui = json.loads((directory.parent / 'timeline-regression-20261104/verification.json').read_text())
    assert frontend_tree.hexdigest() == ui['frontendTreeSha256']
    return {'requirement': 'M1.14', 'scope': 'wire transport only', 'requirementComplete': False,
            'clientMethodsCompared': 22, 'serverRequestsCompared': 2, 'schemaNotificationsCompared': 45, 'otherRecordedNotificationsCompared': 2, 'notificationsCompared': 47,
            'typedValuesValidated': 87, 'fixtureReferencesValidated': 84,
            'persistedDeltaBodyOmittedAsDesigned': True,
            'recordedSources': len(inventory['wireLogs']), 'recordedFrames': inventory['recordedFrames'],
            'validRecordedServerRequestIds': inventory['serverRequestIdsChecked'], 'invalidServerRequestRejectedByBoth': True,
            'missingRecordedNotificationKinds': inventory['missingRecordedNotifications'],
            'remaining': ['full recorded-session replay', 'session-local view', 'handshake and method orchestration', 'approval correlation and abort', 'reconnect and resend decisions'],
            'rustPassed': sum(int(passed) for passed, _ in totals), 'rustIgnored': sum(int(ignored) for _, ignored in totals),
            'schemaFingerprint': rust['schemaFingerprint'], 'oracleVersion': '1.4.14',
            'oracleTreeSha256': digest.hexdigest(), 'rustTreeSha256': rust_tree.hexdigest(),
            'desktopPassedFromUnchangedFrontend': ui['desktopPassed'],
            'artifactSha256': {name: hashlib.sha256((directory / name).read_bytes()).hexdigest() for name in ['transport-fixture.json', 'rust-transport.json', 'ts-transport.json', 'invalid-request-ids.json', 'invalid-request-ids-before.json', 'invalid-request-ids-result.json']},
            'paidTurnsThisTurn': 0, 'cumulativePaidTurns': 154,
            'sourceSha256': {name: hashlib.sha256((repository / name).read_bytes()).hexdigest() for name in paths}}


if __name__ == '__main__':
    directory = Path(sys.argv[1])
    result = verify(directory)
    (directory / 'verification.json').write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps({key: value for key, value in result.items() if key != 'sourceSha256'}))
