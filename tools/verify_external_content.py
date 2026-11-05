"""Reconcile the explicit URL review, local network, and real worker evidence."""
import hashlib
import json
from pathlib import Path
import re
import sqlite3
import sys


def verify(directory):
    repository = Path(__file__).resolve().parents[1]
    load = lambda name: json.loads((directory / name).read_text())
    network = load('image-network.json')
    assert network['requests'] == [{'path': '/image.png?label=preview'}]
    assert all(url.startswith(('intercat://app/', 'data:image/')) for url in network['rendererRequests'])
    assert network['cancelledWithoutRequest'] and network['replayRejected']
    blocked = load('blocked-network.json')
    assert blocked == {'paths': ['/redirect'], 'secretBlocked': True, 'redirectBlocked': True}
    svg = load('svg-and-link.json')
    assert svg['inactiveSVG'] and svg['paths'] == ['/preview.svg']
    assert len(svg['opened']) == 1 and svg['opened'][0].endswith('/documentation?label=preview')
    long_url = load('long-url.json')
    assert long_url['length'] > 7000 and long_url['requests'] == 0
    assert long_url['scrollable'] and long_url['cancelled'] and long_url['zoom'] == 2
    assert load('timeline-image.json') == {'beforeConfirmation': 0, 'afterConfirmation': 1, 'passed': True}
    for name in ('url-review-200.png', 'long-url-review-200.png', 'native-worker-image.png', 'timeline-image.png'):
        assert (directory / name).read_bytes().startswith(b'\x89PNG\r\n\x1a\n')

    native = load('native/worker.json')
    result = load('native/result.json')
    screen = load('native/ui.json')
    attempt = load('native/attempt.json')
    assert native['verified'] and native['requestedTurns'] == 1
    assert native['session']['permissionState']['profileId'] == 'intercat-worker'
    assert native['report']['terminal'] == 'completed'
    assert result['passed'] and result['paidTurns'] == 1
    assert screen['passed'] and screen['requests'] == 1
    assert f"![Review image]({screen['confirmedURL']})" in result['text']
    assert result['text'] == '\n\n'.join(native['report']['messages'])
    assert attempt == {'attemptedPaidTurns': 1, 'previousPaidTurns': 150}
    with sqlite3.connect(f'file:{directory / "native/evidence.sqlite"}?mode=ro', uri=True) as database:
        assert database.execute('PRAGMA integrity_check').fetchone()[0] == 'ok'
        assert database.execute('PRAGMA user_version').fetchone()[0] == 50
        assert database.execute("SELECT count(*) FROM operations WHERE kind='turn.start' AND state='complete'").fetchone()[0] == 1
        frames = [json.loads(payload) for payload, in database.execute("SELECT payload FROM events WHERE kind='msp.received'")]
        items = [frame.get('params', {}).get('item', {}) for frame in frames]
        assert not any(item.get('kind') == 'toolCall' for item in items)
        messages = [item['text'] for frame, item in zip(frames, items)
                    if frame.get('method') == 'item/completed' and item.get('kind') == 'agentMessage']
        assert messages == [result['text']]
    native_log = (directory / 'native-ui.log').read_text()
    assert re.search(r'✓.*a real worker Markdown image is fetched only after the user confirms its URL', native_log)

    rust = (directory / 'rust.log').read_text()
    counts = re.findall(r'test result: ok\. (\d+) passed; 0 failed; (\d+) ignored', rust)
    assert counts and 'Doc-tests intercat_git' in rust and 'FAILED' not in rust
    for name in ('expired_confirmation_is_consumed_without_network_access',
                 'review_and_cancel_never_connect_and_confirmation_is_consumed_once',
                 'redirects_and_non_images_do_not_get_a_second_connection_or_retry',
                 'secret_urls_and_encoded_secrets_are_rejected_before_connecting',
                 'confirmed_links_and_image_reviews_cannot_exchange_authority',
                 'images_are_bounded_by_download_size_and_dimensions'):
        assert re.search(rf'{name} \.\.\. ok', rust)
    ui = (directory / 'ui.log').read_text()
    assert re.search(r'71 passed', ui) and re.search(r'18 skipped', ui)
    assert not re.search(r'\d+ failed', ui)
    targeted = (directory / 'ui-targeted-final.log').read_text()
    assert '5 passed' in targeted and '1 skipped' in targeted
    assert 'ℹ pass 30' in (directory / 'node.log').read_text()
    assert 'ℹ fail 0' in (directory / 'node.log').read_text()
    assert not (directory / 'format.log').read_text().strip()
    clippy = (directory / 'clippy.log').read_text()
    assert 'Finished' in clippy and 'error:' not in clippy
    assert 'error TS' not in (directory / 'typecheck.log').read_text()
    assert 'built in' in (directory / 'web-build.log').read_text()
    assert 'Finished' in (directory / 'rust-build.log').read_text()
    assert load('npm-audit.json')['metadata']['vulnerabilities']['total'] == 0
    package = json.loads((repository / 'package.json').read_text())
    assert package['overrides']['monaco-editor']['dompurify'] == '3.4.17'
    main = (repository / 'electron/main.mjs').read_text()
    assert "connect-src 'none'" in main and "img-src 'self' data: blob:" in main
    sources = [
        'crates/intercat-daemon/src/external_content.rs',
        'crates/intercat-daemon/src/rpc.rs',
        'crates/intercat-daemon/src/lib.rs',
        'crates/intercat-daemon/tests/external_content.rs',
        'electron/external-content.mjs', 'electron/external-content.test.mjs',
        'electron/main.mjs', 'electron/preload.cjs', 'src/bridge.d.ts',
        'src/markdown/MarkdownMessage.tsx', 'src/markdown/markdown.css',
        'src/timeline/StepInspector.tsx', 'src/desk/Workers.tsx',
        'src/desk/ReportQuestions.tsx', 'src/commit/RepairPanel.tsx',
        'tests/external-content.spec.ts', 'tools/verify_worker_markdown.py',
        'tools/verify_external_content.py', 'package.json', 'package-lock.json',
        'crates/intercat-daemon/Cargo.toml', 'Cargo.lock',
    ]
    # A failed early UI fixture is retained; the final regular run and the paid
    # native case are checked separately, rather than hiding that failed run.
    return {'milestoneRequirement': 'M1.28', 'schema': 50,
            'rustPassed': sum(int(a) for a, _ in counts),
            'rustIgnored': sum(int(b) for _, b in counts),
            'uiPassed': 71, 'uiSkipped': 18, 'nodePassed': 30,
            'nativeWorkerTurns': 1, 'workerToolCalls': 0,
            'externalImageRequestsAfterConfirmation': 1,
            'cancelledAndSecretURLsNotFetched': True,
            'redirectsNotFollowed': True, 'svgRemainedInactive': True,
            'fullURLReviewAt200Percent': True,
            'additionalPaidTurns': 1, 'cumulativePaidTurns': 151,
            'sourceSha256': {path: hashlib.sha256((repository / path).read_bytes()).hexdigest() for path in sources}}


if __name__ == '__main__':
    directory = Path(sys.argv[1]).resolve()
    result = verify(directory)
    (directory / 'verification.json').write_text(json.dumps(result, ensure_ascii=False, indent=2) + '\n')
    print(json.dumps({key: value for key, value in result.items() if key != 'sourceSha256'}, ensure_ascii=False))
