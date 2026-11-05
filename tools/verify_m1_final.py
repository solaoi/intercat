"""Join M1's native acceptance evidence with the final per-binary regression."""
import hashlib
import json
from pathlib import Path
import re
import sys

from verify_timeline_performance import measurement


def summary(text):
    result = re.findall(r'test result: ok\. (\d+) passed; 0 failed; (\d+) ignored;', text)
    assert len(result) == 1 and 'test result: FAILED' not in text
    return dict(zip(['passed', 'ignored'], map(int, result[0])))


def verify(directory):
    root = Path(__file__).resolve().parents[1]
    read = lambda name: (directory / name).read_text()
    tested_source = json.loads(read('tested-source.json'))
    for name, expected in tested_source.items():
        assert hashlib.sha256((root / name).read_bytes()).hexdigest() == expected, name
    manifest = re.findall(r'Executable .*? \((target/debug/deps/[^)]+)\)', read('rust-build.log'))
    assert len(manifest) == len(set(manifest)) and manifest
    split = next(i for i, name in enumerate(manifest) if '/external_content-' in name)
    # Preserve failed attempts. Accept only the successful prefix and the
    # separate final rerun of every remaining binary from the same build.
    original = read('rust-before-isolation.log')
    sections = dict(re.findall(r'Running[^\n]*\((target/debug/deps/[^)]+)\)\n(.*?)(?=\n\s*Running|\Z)', original, re.S))
    binaries = {name: summary(sections[name]) for name in manifest[:split]}
    binaries[manifest[split]] = summary(read('rust-external-final.log'))
    external_parallel = summary(read('rust-external-parallel-final.log'))
    assert external_parallel == {'passed': 5, 'ignored': 0}
    remaining = json.loads(read('rust-remaining/results.json'))
    assert [row['binary'] for row in remaining] == manifest[split + 1:]
    for row in remaining:
        assert row['exitCode'] == 0
        binaries[row['binary']] = summary(read('rust-remaining/' + Path(row['binary']).name + '.log'))
    assert len(binaries) == len(manifest)
    assert read('doctests.log').count('test result: ok.') == 2
    assert 'FAILED' not in read('doctests.log')
    ui = {}
    for name in ['ui-mock-final.log', 'ui-native-final.log', 'ui-timeline-final.log']:
        log = re.sub(r'\x1b\[[0-9;]*m', '', read(name))
        count = re.findall(r'(\d+) passed', log)
        assert count and not re.search(r'\b[1-9]\d* failed\b', log)
        ui[name] = int(count[-1])
    assert ui == {'ui-mock-final.log': 74, 'ui-native-final.log': 9, 'ui-timeline-final.log': 1}
    assert 'Ran 13 tests' in read('python.log') and read('python.log').rstrip().endswith('OK')
    assert not read('fmt.log').strip()
    assert 'error TS' not in read('typecheck.log') and 'tsc --noEmit' in read('typecheck.log')
    assert 'Finished' in read('clippy.log') and 'error:' not in read('clippy.log')
    assert 'two_clients_cannot_refresh_while_a_saved_commit_waits_to_publish ... ok' in read(
        'rust-remaining/' + next(Path(name).name for name in manifest if '/integration-' in name) + '.log')
    acceptance = (root / 'docs/m1-acceptance.md').read_text()
    rows = re.findall(r'^\| (\d+) \|[^\n]+', acceptance, re.M)
    assert list(map(int, rows)) == list(range(1, 61))
    assert all('**通過**' in row or '**完了**' in row for row in acceptance.splitlines()
               if re.match(r'^\| \d+ \|', row))
    evidence = {}
    for link in re.findall(r'\]\(([^)]+)\)', acceptance):
        name = link.split('#')[0]
        if name and not name.startswith('http'):
            path = (root / 'docs' / name).resolve()
            if path == directory / 'verification.json':
                continue
            assert path.is_file(), path
            evidence[str(path.relative_to(root))] = hashlib.sha256(path.read_bytes()).hexdigest()
    modes = measurement(directory / 'timeline')
    source = {}
    for folder in ['crates', 'src', 'electron', 'services/editor/src', 'tests', 'tools']:
        for path in (root / folder).rglob('*'):
            if path.is_file() and '__pycache__' not in path.parts and path.suffix != '.pyc':
                source[str(path.relative_to(root))] = hashlib.sha256(path.read_bytes()).hexdigest()
    for name in ['Cargo.toml', 'Cargo.lock', 'package.json', 'package-lock.json',
                 'docs/design.md', 'docs/milestones.md', 'docs/m1-acceptance.md',
                 'design/boards/index.json', 'README.md']:
        source[name] = hashlib.sha256((root / name).read_bytes()).hexdigest()
    return {'milestone': 1, 'complete': True, 'acceptedConditions': list(range(1, 61)),
            'rustBinaries': binaries, 'rustPassed': sum(row['passed'] for row in binaries.values()),
            'rustIgnored': sum(row['ignored'] for row in binaries.values()),
            'externalParallelRerun': external_parallel, 'ui': ui,
            'pythonPassed': 13, 'timeline': modes, 'newPaidTurns': 0, 'cumulativePaidTurns': 189,
            'evidenceSha256': evidence, 'sourceSha256': source}


if __name__ == '__main__':
    directory = Path(sys.argv[1]).resolve()
    result = verify(directory)
    (directory / 'verification.json').write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps({name: result[name] for name in ['milestone', 'complete', 'rustPassed',
                                                   'rustIgnored', 'ui', 'pythonPassed']}))
