"""Audit the actual 300-row VoiceOver/keyboard run, separately from performance."""
import hashlib
import json
from pathlib import Path
import re
import sys

from verify_msp_replay import tree


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def verify(directory):
    root = Path(__file__).resolve().parents[1]
    result = json.loads((directory / 'voiceover.json').read_text())
    rows = result['rows']
    assert not result['errors'] and len(rows) == 300
    assert [row['index'] for row in rows] == list(range(1, 301))
    for row in rows:
        index = row['index']
        assert re.search(rf'利用者認証と変更履歴を確認するトラック {index}(?!\d)', row['phrase'])
        assert ('working' if index <= 100 else 'done') in row['phrase']
    assert rows == json.loads((directory / 'voiceover-progress.json').read_text())
    geometry = json.loads((directory / 'geometry.json').read_text())
    assert not geometry['errors'] and 0 < geometry['rows'] < 300
    assert geometry['canvasHeight'] <= geometry['viewportHeight'] * geometry['dpr']
    log = (directory / 'desktop.log').read_text()
    assert re.search(r'\b1 passed\b', log) and not re.search(r'\b\d+ failed\b', log)
    assert '300 tracks remain drawable and reachable' in log
    assert 'built in' in (directory / 'build.log').read_text()
    assert 'error TS' not in (directory / 'typecheck.log').read_text()
    picture = (directory / 'last-track.png').read_bytes()
    assert picture.startswith(b'\x89PNG\r\n\x1a\n') and len(picture) > 1000
    sources = ['tests/timeline-load.spec.ts', 'tests/support/voiceover.ts',
               'src/timeline/Timeline.tsx', 'src/timeline/useWindow.ts',
               'tools/verify_voiceover.py']
    frozen = json.loads((directory / 'run-sources.json').read_text())
    assert frozen == {name: sha(root / name) for name in sources}
    frontend = tree(root, [p for d in ['src', 'electron'] for p in (root / d).rglob('*') if p.is_file()])
    previous = json.loads((directory.parent.parent / 'msp-unacknowledged-20261105/verification.json').read_text())
    assert frontend == previous['frontendTreeSha256']
    files = ['voiceover.json', 'voiceover-progress.json', 'geometry.json',
             'last-track.png', 'desktop.log', 'build.log', 'typecheck.log', 'run-sources.json']
    return {
        'requirement': 'M1.22', 'requirementComplete': True, 'milestoneComplete': False,
        'scope': '300 real macOS down-arrow presses; each selected row stays visible and actual VoiceOver output contains its name and status',
        'rowsVerified': 300, 'firstRow': rows[0], 'lastRow': rows[-1],
        'keyboard': 'System Events key code 125 (HIToolbox kVK_DownArrow)',
        'speech': 'VoiceOver AppleScript last phrase; no synthesized test speech',
        'performanceRerun': False,
        'frontendTreeSha256': frontend, 'paidTurnsThisStep': 0,
        'sourceSha256': frozen, 'artifactSha256': {name: sha(directory / name) for name in files},
    }


if __name__ == '__main__':
    directory = Path(sys.argv[1]).resolve()
    result = verify(directory)
    (directory / 'verification.json').write_text(json.dumps(result, ensure_ascii=False, indent=2) + '\n')
    print(json.dumps({key: result[key] for key in ['requirement', 'requirementComplete', 'rowsVerified', 'milestoneComplete']}))
