"""Ask a real worker for a dummy Markdown image without fetching that URL."""
import argparse
import json
from pathlib import Path
import subprocess


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--url', required=True)
    parser.add_argument('--muse', type=Path, required=True)
    args = parser.parse_args()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    source = output / 'source'
    source.mkdir()
    (source / 'README.md').write_text('# Disposable Markdown report fixture\n')
    for command in [['git', 'init', '-q', '-b', 'main'], ['git', 'add', '.'], ['git', '-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid', 'commit', '-qm', 'Fixture']]:
        subprocess.run(command, cwd=source, check=True)
    instruction = f'Return the following Markdown as your final response, not in a code fence. Do not call any tool, open the URL, or change a file. This is only a text rendering test.\n\n# Image review\n\nThis is an external reference.\n\n![Review image]({args.url})'
    request = output / 'request.json'
    request.write_text(json.dumps({'repository': str(source), 'instruction': instruction, 'allowed_shell_commands': []}))
    (output / 'attempt.json').write_text(json.dumps({'attemptedPaidTurns': 1, 'previousPaidTurns': 150}))
    result = subprocess.run([str(Path('target/debug/intercatd').resolve()), 'diagnose-turn', str(args.muse.resolve()), str(output), str(request)], check=True, capture_output=True, text=True)
    saved = json.loads(result.stdout)
    text = '\n\n'.join(saved['report']['messages'])
    if f'![Review image]({args.url})' not in text:
        raise RuntimeError('The worker did not return the requested Markdown image.')
    (output / 'worker.json').write_text(json.dumps(saved, indent=2))
    (output / 'result.json').write_text(json.dumps({'text': text, 'paidTurns': saved['requestedTurns'], 'passed': True}, indent=2))


if __name__ == '__main__':
    main()
