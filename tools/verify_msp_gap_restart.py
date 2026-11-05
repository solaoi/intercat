"""Verify the persisted gap boundary and its bounded completion receipt."""
import json
from pathlib import Path
import re
import sys

from build_msp_gap_fixture import build
from compare_msp_replay import compare
from verify_msp_gaps import calls, sha
from verify_msp_replay import tree


def verify(directory):
    root=Path(__file__).resolve().parents[1]
    previous=json.loads((directory.parent/'msp-gap-20261104/verification.json').read_text())
    fixture=json.loads((directory/'fixture.json').read_text())
    assert fixture==build()
    rust=json.loads((directory/'rust.json').read_text())
    ts=json.loads((directory/'ts.json').read_text())
    assert rust['schemaVersion']==51 and rust['validatedFrames']==51
    assert len(rust['cases'])==len(ts['cases'])==8
    for a,b,c in zip(rust['cases'],ts['cases'],fixture['cases']):
        assert a['id']==b['id']==c['id']
        assert a['final']==b['final'] and a['current']==b['current']
        assert a['recoverySucceeded']==a['current']==('fault' not in c)
        assert a['restartBeforeFill'] and a['restartVerified']
        if b['pendingGap'] is None:
            assert a['pendingGap'] is None
        else:
            assert a['pendingGap']['after']==b['pendingGap']['after']
            assert a['pendingGap']['next']==b['pendingGap']['next']
            assert a['pendingGap']['through']>0
        assert calls(a,True)==calls(b,False)
    before=(directory/'coalesced-before.log').read_text()
    assert 'coalesced_raw_gaps_keep_the_first_hole_after_restart ... FAILED' in before
    assert 'Some("later")' in before and 'Some("first")' in before
    names=['coalesced_raw_gaps_keep_the_first_hole_after_restart',
           'finishing_one_gap_keeps_a_later_receipt_and_its_lower_bound',
           'interrupted_gap_reopens_its_first_bound_and_does_not_duplicate_items',
           'the_first_gap_during_a_read_rebases_before_marking_it_filled',
           'a_restored_gap_before_the_read_head_stops_at_that_head',
           'rebasing_a_new_gap_replays_the_head_after_the_missing_prefix']
    related=(directory/'related-tests.log').read_text()
    assert sorted(int(n) for n in re.findall(r'test result: ok\. (\d+) passed',related))==[1,2,21,21]
    settings=json.loads((directory/'run-settings.json').read_text())
    assert settings=={'rustTests':['cargo','test','--workspace','--','--test-threads=1'],'schema':51,'concurrentProbes':False}
    log=(directory/'rust-tests.log').read_text()
    assert all(name+' ... ok' in log and name+' ... ok' in related for name in names)
    totals=re.findall(r'test result: ok\. (\d+) passed; 0 failed; (\d+) ignored',log)
    assert totals and 'FAILED' not in log and 'Doc-tests intercat_git' in log
    assert 'a_restored_gap_before_the_read_head_stops_at_that_head ... FAILED' in (directory/'read-head-before.log').read_text()
    assert 'The head must be restored after the older prefix' in (directory/'rebase-head-before.log').read_text()
    assert 'The newly discovered lower bound must be read' in (directory/'late-first-gap-before.log').read_text()
    assert 'retiring_a_connection_preserves_children_and_next_host_inherits_their_ownership ... ok' in log
    assert 'Elapsed(())' in (directory/'initial-rust-tests.log').read_text()
    initial_inputs=json.loads((directory/'initial-regression-inputs.json').read_text())
    assert sha(root/'crates/intercat-daemon/tests/host_recovery.rs')==initial_inputs['crates/intercat-daemon/tests/host_recovery.rs']
    assert 'Elapsed(())' in (directory/'bounds-rust-tests.log').read_text()
    assert 'retired_host_children_are_visible_and_only_attributed_children_can_be_stopped ... ok' in log
    assert sha(root/'crates/intercat-daemon/tests/project_secret_usage.rs')==initial_inputs['crates/intercat-daemon/tests/project_secret_usage.rs']
    assert 'Finished' in (directory/'clippy.log').read_text()
    assert not (directory/'format.log').read_text().strip()
    for name,digest in json.loads((directory/'regression-inputs.json').read_text()).items():
        assert sha(root/name)==digest,name
    initial=directory.parent/'msp-session-view-20261104'
    for name in ['fixture.json','ts.json']:
        assert sha(directory/'state-replay'/name)==sha(initial/name)
    replay=compare(directory/'state-replay')
    assert replay['cases']==531 and replay['comparedFrames']==19219 and replay['differentCases']==0
    assert all(case['restartVerified'] for case in json.loads((directory/'state-replay/rust.json').read_text())['cases'])
    sdk=root/'research/verify/support/node_modules/@muse-code/sdk'
    assert tree(sdk,(sdk/'dist').rglob('*.js'))==previous['oracleTreeSha256']
    frontend=tree(root,[p for folder in ['src','electron'] for p in (root/folder).rglob('*') if p.is_file()])
    assert frontend==previous['frontendTreeSha256']
    sources=['crates/intercat-daemon/src/history.rs','crates/intercat-daemon/src/history_store.rs',
             'crates/intercat-daemon/src/journal.rs','crates/intercat-daemon/src/session_retirement.rs',
             'crates/intercat-daemon/tests/history.rs','crates/intercat-daemon/examples/msp_gap.rs',
             'tools/msp_gap_oracle.mjs','tools/verify_msp_gap_restart.py','tests/connection.spec.ts']
    artifacts=['fixture.json','rust.json','ts.json','coalesced-before.log','related-tests.log','rust-tests.log','build.log','rust.log','ts.log',
               'clippy.log','format.log','regression-inputs.json','run-settings.json','late-first-gap-before.log',
               'initial-rust-tests.log','initial-regression-inputs.json','bounds-rust-tests.log',
               'read-head-before.log','rebase-head-before.log','state-replay/fixture.json',
               'state-replay/ts.json','state-replay/rust.json','state-replay/rust.log']
    return {'requirement':'M1.14','requirementComplete':False,
            'scope':'persisted gap bounds, completion receipt races, restart before and after fill',
            'restartBoundary':'Journal close/open; host and daemon process restart orchestration is separate',
            'schema':51,'nativeKeychainSchemaAssertionUpdatedNotRun':True,'gapCases':8,'gapInputFramesValidated':51,'gapRestartsBeforeFill':8,'gapRestartsAfterFill':8,
            'regressions':names,'initialProcessTimeoutsThenPassedUnchanged':True,'stateReplayCases':531,'stateReplayFrames':19219,'stateReplayDifferences':0,
            'requestPolicyDifference':previous['requestPolicyDifference'],
            'remaining':['compaction and snapshot recovery','transient delta display','host opening and handshake',
                         'worker disconnect/reconnect and resend decisions'],
            'testThreads':1,'rustPassed':sum(int(p) for p,i in totals),'rustIgnored':sum(int(i) for p,i in totals),
            'oracleTreeSha256':previous['oracleTreeSha256'],'frontendTreeSha256':frontend,
            'desktopPassedFromUnchangedFrontend':76,'paidTurnsThisTurn':0,'cumulativePaidTurns':154,
            'sourceSha256':{name:sha(root/name) for name in sources},
            'artifactSha256':{name:sha(directory/name) for name in artifacts}}


if __name__=='__main__':
    directory=Path(sys.argv[1]); result=verify(directory)
    (directory/'verification.json').write_text(json.dumps(result,ensure_ascii=False,indent=2)+'\n')
    print(json.dumps({k:result[k] for k in ['gapCases','stateReplayCases','stateReplayDifferences','rustPassed','rustIgnored','requirementComplete']}))
