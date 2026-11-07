"""Local-only regression corpus for the report-question classifier."""
import argparse, json, re, sqlite3
from pathlib import Path
from intercat_editor.engine import Engine
from intercat_editor.contract import DecisionRequest

CASES = [
    ("complete", "Done. All tests passed. No further action is required.", "0"),
    ("file", "Created proof.txt containing exactly done. No other files were changed. No tests ran. Next step: none, the file is ready.", "0"),
    ("assumed", "I assumed SQLite and finished the implementation. The tests passed.", "0"),
    ("offer", "The requested fix is complete. Would you like me to add more tests next?", "0"),
    ("ja_done", "依頼どおり実装し、テストも通りました。次の作業はありません。", "0"),
    ("missing_schema", "No changes made. I need the API schema from you before I can continue.", "1"),
    ("choice", "Which database should I use, PostgreSQL or SQLite? I cannot continue until you choose.", "1"),
    ("ja_question", "実装前に確認が必要です。保存先はローカルとクラウドのどちらにしますか？回答を待ちます。", "1"),
    ("explicit_confirmation", "The deploy command is ready. Please confirm the target account before I run it.", "1"),
]

def main():
    parser=argparse.ArgumentParser();parser.add_argument('--model',required=True);parser.add_argument('--output',required=True);args=parser.parse_args()
    source=Path('crates/intercat-daemon/src/reports.rs').read_text()
    match=re.search(r'"prefix":("(?:[^"\\]|\\.)*")',source)
    current=json.loads(match[1])
    cases=list(CASES) + [
        ('holdout_complete', 'No edits were needed: the existing implementation already satisfies the requirement. I verified the checks locally.', '0'),
        ('holdout_filename', 'Please send the filename that should be changed; I have not started editing.', '1'),
        ('holdout_ja_done', 'READMEも更新しました。残っている作業はありません。', '0'),
        ('holdout_behavior', 'I can preserve the public method or remove it. Which behavior do you want?', '1'),
        ('holdout_region', 'In order to continue I require a value for REGION.', '1'),
    ]
    db=sqlite3.connect('research/verify/report-ui-20261104/events.sqlite')
    for (value,) in db.execute("select record from report_questions where state='open'"):
        cases.append(('real_completed_fixture',json.loads(value)['text'],'0'))
    engine=Engine(args.model);rows=[]
    for variant,prefix,labels,prompt in [('production',current,['0','1','2'],'\nLABEL:')]:
        for name,report,expected in cases:
            request=DecisionRequest.parse({'prefix':prefix,'cachePrefix':False,'tail':json.dumps({'report':report},ensure_ascii=False),'stages':[{'name':'question','prompt':prompt,'candidates':labels}],'temperature':1})
            value=engine.decide(request);stage=value['stages']['question'];selected=stage['selected'];predicted=selected; gate='clear' if selected=='0' and stage['confidence']>=.8 else 'hold'
            rows.append({'variant':variant,'name':name,'expected':expected,'predicted':predicted,'gate':gate,'result':value})
            print(json.dumps({'variant':variant,'name':name,'expected':expected,'predicted':predicted,'gate':gate,'confidence':stage['confidence']}),flush=True)
    output=Path(args.output);output.parent.mkdir(parents=True,exist_ok=True);output.write_text(json.dumps(rows,ensure_ascii=False,indent=2))
    assert all(row['gate'] == ('clear' if row['expected']=='0' else 'hold') for row in rows), 'Report gate differs from this corpus expectations'
if __name__=='__main__':main()
