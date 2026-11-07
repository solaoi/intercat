"""Compare identical token streams with and without prefix reuse (local only)."""
import argparse
import copy
import hashlib
import importlib.metadata
import json
from pathlib import Path
import shutil
import sys
import time

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'services/editor/src'))
from intercat_editor.engine import Engine


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--model', required=True)
    parser.add_argument('--fixture', required=True, type=Path)
    parser.add_argument('--output', required=True, type=Path)
    parser.add_argument('--precisions', nargs='+', choices=['bfloat16', 'float32', 'bfloat16-restored'],
                        default=['bfloat16', 'float32', 'bfloat16-restored'])
    parser.add_argument('--indices', nargs='+', type=int)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=False)
    shutil.copy2(__file__, args.output / 'probe-source.py')
    save = lambda name, value: (args.output / name).write_text(json.dumps(value, ensure_ascii=False, indent=2) + '\n')
    fixture = json.loads(args.fixture.read_text())
    engine = Engine(args.model)
    mx, model, tokenizer = engine.mx, engine.model, engine.tokenizer
    import numpy as np
    from mlx.utils import tree_flatten

    encode = lambda text: tokenizer.encode(text, add_special_tokens=False)
    prefix = encode(fixture['prefix'])
    tails = [encode(tail) for tail in fixture['tailsDecoded']]
    labels = [engine.label_id(' ' + label) for label in 'ABCDEFGH']
    separate = [prefix + tail for tail in tails]
    joined = [encode(fixture['prefix'] + tail) for tail in fixture['tailsDecoded']]
    boundary = [{'index': i, 'separateLength': len(a), 'joinedLength': len(b), 'sameIds': a == b}
                for i, (a, b) in enumerate(zip(separate, joined))]
    save('token-boundaries.json', {'prefixEndsInNewline': fixture['prefix'].endswith('\n'), 'cases': boundary})
    save('versions.json', {name: importlib.metadata.version(name) for name in ['mlx', 'mlx-lm', 'tokenizers']})

    def cache_hash(cache):
        digest = hashlib.sha256()
        for name, value in tree_flatten([entry.state for entry in cache]):
            digest.update(name.encode())
            digest.update(np.asarray(value.astype(mx.float32)).tobytes() if isinstance(value, mx.array) else repr(value).encode())
        return digest.hexdigest()

    def forward(tokens, cache):
        start = time.perf_counter()
        logits = model(mx.array([tokens]), cache=cache)[:, -1, :]
        scores = logits[0, mx.array(labels)]
        mx.eval(scores, [entry.state for entry in cache])
        values = scores.tolist()
        return {'scores': values, 'choice': 'ABCDEFGH'[max(range(8), key=values.__getitem__)],
                'milliseconds': (time.perf_counter() - start) * 1000, 'dtype': str(scores.dtype)}

    results = []
    for precision in args.precisions:
        model.set_dtype(mx.float32 if precision == 'float32' else mx.bfloat16)
        mx.eval(model.parameters())
        mx.clear_cache()
        base = engine.make_cache(model)
        forward(prefix, base)
        before = cache_hash(base)
        # The restored phase repeats every original mismatch to check that
        # changing precision, rather than warming the model, explained it.
        indices = range(len(tails)) if precision != 'bfloat16-restored' else [
            row['index'] for row in results if row['precision'] == 'bfloat16' and row['full']['choice'] != row['cached']['choice']]
        if args.indices is not None:
            indices = args.indices
        for index in indices:
            modes = ['full', 'cached', 'fresh_split'] if index % 2 == 0 else ['cached', 'full', 'fresh_split']
            row = {'precision': precision, 'index': index,
                   'inputIdsSha256': hashlib.sha256(json.dumps(separate[index]).encode()).hexdigest()}
            for mode in modes:
                cache = copy.deepcopy(base) if mode == 'cached' else engine.make_cache(model)
                if mode == 'fresh_split':
                    forward(prefix, cache)
                row[mode] = forward(separate[index] if mode == 'full' else tails[index], cache)
                del cache
            row['maxScoreDifference'] = max(abs(a - b) for a, b in zip(row['full']['scores'], row['cached']['scores']))
            row['splitReuseDifference'] = max(abs(a - b) for a, b in zip(row['fresh_split']['scores'], row['cached']['scores']))
            results.append(row)
            with (args.output / 'results.jsonl').open('a') as output:
                output.write(json.dumps(row) + '\n')
            print(json.dumps({key: row[key] for key in ['precision', 'index', 'maxScoreDifference', 'splitReuseDifference']} |
                             {'choices': [row['full']['choice'], row['cached']['choice']]}), flush=True)
        assert before == cache_hash(base), 'The reusable prefix cache changed'
        del base
        mx.clear_cache()
    summary = {precision: {'cases': len(rows),
                           'choiceDifferences': [row['index'] for row in rows if row['full']['choice'] != row['cached']['choice']],
                           'maxScoreDifference': max((row['maxScoreDifference'] for row in rows), default=0),
                           'maxSplitReuseDifference': max((row['splitReuseDifference'] for row in rows), default=0)}
               for precision in args.precisions
               for rows in [[row for row in results if row['precision'] == precision]]}
    save('summary.json', {'phases': summary, 'inputTokenIdsIdenticalWithinEveryPair': True,
                          'tokenizationBoundaryDifferences': sum(not row['sameIds'] for row in boundary),
                          'prefixTokens': len(prefix), 'modelMemory': engine.memory(), 'paidTurns': 0})
    print(json.dumps(summary), flush=True)


if __name__ == '__main__':
    main()
