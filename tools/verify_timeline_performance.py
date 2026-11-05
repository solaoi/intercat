"""Verify M1.13 from raw browser traces, real bar growth, and UI regressions."""
import hashlib
import json
from pathlib import Path
import re
import sys


def trace_metrics(path):
    events = json.loads(path.read_text())['traceEvents']
    markers = sorted((item for item in events if item['name'] == 'intercat-bench-frame'), key=lambda item: item['ts'])
    assert len(markers) > 1
    thread = (markers[0]['pid'], markers[0]['tid'])
    assert all((item['pid'], item['tid']) == thread for item in markers)
    names = {'FireAnimationFrame', 'FunctionCall', 'EvaluateScript', 'RunMicrotasks', 'EventDispatch', 'MinorGC', 'MajorGC'}
    intervals = sorted((item['ts'], item['ts'] + item['dur']) for item in events
                       if (item.get('pid'), item.get('tid')) == thread and item.get('ph') == 'X'
                       and item.get('dur') and item['name'] in names)
    merged = []
    for start, end in intervals:
        if merged and start <= merged[-1][1]:
            merged[-1][1] = max(merged[-1][1], end)
        else:
            merged.append([start, end])
    samples = [sum(max(0, min(end, right['ts']) - max(start, left['ts'])) for start, end in merged) / 1000
               for left, right in zip(markers, markers[1:])]
    presented = sorted(item['ts'] for item in events
                       if item['name'] == 'AnimationFrame::Presentation' and (item.get('pid'), item.get('tid')) == thread
                       and markers[0]['ts'] <= item['ts'] <= markers[-1]['ts'])
    assert len(presented) >= len(samples) - 3
    return {'samples': len(samples), 'jsMaxMs': max(samples), 'jsMeanMs': sum(samples) / len(samples),
            'presentedFrames': len(presented), 'presentedFps': (len(presented) - 1) * 1e6 / (presented[-1] - presented[0])}


def measurement(directory):
    result = json.loads((directory / 'performance.json').read_text())
    assert result['trackCount'] == result['clipsPerTrack'] == 300
    assert result['inputMarkers'] == 600
    assert result['animatedTracks'] == result['visibleActive'] == result['growingBars'] == 100
    before, after = result['barsBefore'], result['barsAfter']
    assert len(before) == len(after) == 100
    assert len({bar['id'] for bar in before}) == 100
    assert [bar['id'] for bar in before] == [bar['id'] for bar in after]
    assert all(right['width'] > left['width'] and right['y'] == left['y'] for left, right in zip(before, after))
    geometry = result['allActiveGeometry']
    assert geometry['canvasHeight'] <= geometry['height'] * geometry['dpr'] + 1
    modes = {}
    for mode, frames, trace in [('detail', 240, 'detail'), ('overview', 240, 'overview'), ('allActive', 720, 'all-active')]:
        reported = result[mode]
        raw = trace_metrics(directory / f'{trace}-trace.json')
        assert reported['frames'] == reported['scriptSamples'] == raw['samples'] == frames
        assert reported['fps'] >= 60 and raw['presentedFps'] >= 60
        assert reported['jsP95Ms'] <= 2 and raw['jsMaxMs'] <= 2
        assert abs(reported['jsMaxMs'] - raw['jsMaxMs']) < 1e-9
        assert abs(reported['jsMeanMs'] - raw['jsMeanMs']) < 1e-9
        modes[mode] = {**reported, **raw}
    for name in ['last-track.png', 'overview.png', 'all-active.png']:
        picture = (directory / name).read_bytes()
        assert len(picture) > 1000 and picture.startswith(b'\x89PNG\r\n\x1a\n')
    return modes


def verify(output):
    repository = Path(__file__).resolve().parents[1]
    evidence = output.parent
    single = measurement(evidence / 'timeline-clock-group-20261104')
    regression = measurement(output / 'performance')
    log = re.sub(r'\x1b\[[0-9;]*m', '', (output / 'desktop.log').read_text())
    passed = re.findall(r'(\d+) passed', log)
    skipped = re.findall(r'(\d+) skipped', log)
    assert passed and int(passed[-1]) >= 76 and not re.search(r'\b[1-9]\d* failed\b', log)
    assert 'timeline-load.spec.ts' in log and 'built in' in log
    assert 'error TS' not in (output / 'typecheck.log').read_text()
    paths = ['src/timeline/scene.ts', 'src/timeline/Timeline.tsx', 'src/timeline/useWindow.ts',
             'src/main.tsx', 'tests/timeline-load.spec.ts', 'tests/support/timeline-benchmark.ts',
             'package.json', 'package-lock.json', 'tools/verify_timeline_performance.py']
    frontend_tree = hashlib.sha256()
    for path in sorted([path for folder in ['src', 'electron'] for path in (repository / folder).rglob('*') if path.is_file()]):
        frontend_tree.update(str(path.relative_to(repository)).encode() + b'\0' + hashlib.sha256(path.read_bytes()).digest())
    return {'milestoneRequirement': 'M1.13', 'milestoneComplete': False,
            'estimatedM1ProgressPercent': 99, 'previousM1ProgressPercent': 98.5,
            'trackCount': 300, 'clipsPerTrack': 300, 'inputMarkers': 600, 'verifiedGrowingBars': 100,
            'singleRun': single, 'regressionRun': regression,
            'desktopPassed': int(passed[-1]), 'desktopSkipped': int(skipped[-1]) if skipped else 0,
            'frontendTreeSha256': frontend_tree.hexdigest(),
            'paidTurnsThisTurn': 0, 'cumulativePaidTurns': 154,
            'sourceSha256': {name: hashlib.sha256((repository / name).read_bytes()).hexdigest() for name in paths}}


if __name__ == '__main__':
    output = Path(sys.argv[1])
    result = verify(output)
    (output / 'verification.json').write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps({key: value for key, value in result.items() if key != 'sourceSha256'}))
