# M1 受入条件の照合表

正本は [milestones.md](milestones.md) の「完了の条件」1〜60。2026-11-04 に番号を照合した。下表の短い名称は索引であり、正本の条件を省略・変更しない。

2026-11-05に全60条件の照合を完了した。連続した実Museの受入、境界ごとの実機検証、同じソースの最終自動回帰を組み合わせて確認している。60条件すべてを一つの実機セッションで試したという意味ではない。最終回帰の集計とソースの対応は[検証結果](../research/verify/m1-regression-20261105/verification.json)に記録する。

| 条件 | 対象 | 現在の確認状態 |
| --- | --- | --- |
| 1 | 最初の依頼・配置・送信遅延 | **通過**：[連続実機受入](../research/verify/m1-continuous-20261105/seventh/verification.json)。指定依頼、チップ15.9ms、保存→userMessage 11,513ms、判定待ち0.018msと推論329msを分離記録。 |
| 2 | 境目待ちの取消し | **通過**：[連続実機受入](../research/verify/m1-continuous-20261105/seventh/verification.json)で、動作中のAへ置いたCHANGELOGの点線をBackspaceで取り消し、Aは継続。 |
| 3 | push の拒否と実行順 | **通過**：[連続実機受入](../research/verify/m1-continuous-20261105/seventh/verification.json)。最初のpushを実カードで拒否し、local bareの不変と許可決定→実行の順序をDBで照合。 |
| 4 | 四部の報告とチェックポイント | **通過**：[連続実機受入](../research/verify/m1-continuous-20261105/seventh/verification.json)。変更・理由・実npm testの2件成功・次の行動の報告とチェックポイントを確認。 |
| 5 | 同じ push の一回限りの許可 | **通過**：[連続実機受入](../research/verify/m1-continuous-20261105/seventh/verification.json)。拒否後、同じAで再依頼したpushをAllow onceし、local bareへの到達とEffectを確認。 |
| 6 | shell・通信の許可前副作用ゼロ | **通過**：[連続実機受入](../research/verify/m1-continuous-20261105/seventh/verification.json)。shellの5開始すべてが決定保存後。ローカルHTTPへの未許可通信は0回。 |
| 7 | Check・Effect・Commit | **通過**：[連続実機受入](../research/verify/m1-continuous-20261105/seventh/verification.json)。同じプロジェクトのCheck・Effect・trailer付きCommitを実UIで通し、元checkoutのHEAD不変を確認。 |
| 8 | 即時停止・境目停止 | **通過**：[停止の連続実機受入](../research/verify/m1-stop-20261105/third/verification.json)。⇧⌘.で現ターン継続と後続held、即時停止で子消滅、正常完了doneと中止stoppedを確認。 |
| 9 | kill -9 後の同一セッション復旧 | **通過**：[同一セッション復旧](../research/verify/reconnect-20261104/result.json)でSIGKILL後3,821ms、開始・承認の重複なし。現行版の最終回帰でも通過。 |
| 10 | チェックポイントからの分岐 | **通過**：[分岐](../research/verify/branch-20261104/result.json)と[分岐試験](../crates/intercat-daemon/tests/branch.rs)で親SHA・子の木・workspaceRoots・指紋継承を照合。現行版の最終回帰でも通過。 |
| 11 | アプリ・デーモン再起動 | **通過**：[連続実機受入](../research/verify/m1-continuous-20261105/seventh/verification.json)で同じTimeline復元とモデル非起動。[Keychain再起動](../research/verify/connection-current-20261105/verification.json)で同じ鍵・下書き・旧DB拒否。 |
| 12 | 終了時の子孫プロセス回収 | **通過**：[実機終了](../research/verify/quit-20261105/verification.json)と[host喪失後の終了](../research/verify/m1-stop-20261105/third/verification.json)。背景継続、同一daemonへの再接続、Stop everything後の所有プロセス0件。 |
| 13 | 300 × 300 クリップ・100 本伸長の性能 | **通過**：[最終描画性能](../research/verify/m1-regression-20261105/timeline/verification.json)。末尾時刻を削除した現行版で約120fps、JS最大0.842ms。100本の実幅増加と表示フレームを照合。 |
| 14 | MSP 全使用メソッドの SDK 比較 | **完了**：[現行版の総合照合](../research/verify/msp-current-20261105/verification.json)。22メソッド・2要求・47通知、531状態ケース／19,219段階・再読込、履歴補完8・snapshot5・初期化17、通常と修復の承認各8、途中表示16、未確認開始9ケースを確認。正本どおり保存記録と偽ホストで比較し、実ホストは疎通証跡で補う |
| 15 | 同一ターン中の削除・復元 | **通過**：[削除と復元](../research/verify/deletion-ui-20261104/validation.json)。[削除試験](../crates/intercat-daemon/tests/deletion.rs)と実画面の復元操作を現行版の最終回帰でも確認。 |
| 16 | 二度目の Commit の差分 | **通過**：[統合試験](../crates/intercat-daemon/tests/integration.rs)のa_second_integration_only_carries_new_work_and_keeps_other_tracks_changesで2回目に新規差分だけを統合。現行版の最終回帰でも通過。 |
| 17 | シート中に完了したターンと Refresh | **通過**：[統合試験](../crates/intercat-daemon/tests/integration.rs)のfreezes_saved_turns_across_windows_and_refresh_is_explicitで固定したチェックポイントと明示Refreshを確認。現行版の最終回帰でも通過。 |
| 18 | 遅い Check 中の承認・停止 | **通過**：[並行実機受入](../research/verify/m1-concurrent-20261105/first/verification.json)。30秒Check中にBの承認へ1,299ms、Cの停止へ129msで応答し、Check継続も確認。 |
| 19 | .muse/hooks.json の書込拒否 | **通過**：[秘密境界](../research/verify/secret-boundary-20261104-run4/verification.json)と[ホスト方針](../crates/intercat-daemon/tests/policy.rs)。現行版の最終回帰でも通過。 |
| 20 | .env の拒否・.env.example の読取 | **通過**：[証跡](../research/verify/secret-boundary-20261104-run4/verification.json)。実ワーカーのread_file・shell、例示4種とUnicode名、統合checkoutとCheckの元ファイル拒否を確認。現行版の最終回帰でも通過 |
| 21 | Commit 中断からの ref 更新復旧 | **通過**：[ref公開](../research/verify/publication-20261104/result.json)と[統合試験](../crates/intercat-daemon/tests/integration.rs)で8更新の全9中断点から同じ候補へ復旧。現行版の最終回帰でも通過。 |
| 22 | 300 行の移動と VoiceOver | **通過**：[実VoiceOverの証跡](../research/verify/voiceover-20261105/first/verification.json)。実macOSの↓キー300回、各選択行の表示、名前と状態の実読み上げをすべて確認 |
| 23 | 削除したファイルと統合の木の一致 | **通過**：[候補試験](../crates/intercat-daemon/tests/candidate.rs)と[チェックポイント試験](../crates/intercat-daemon/tests/checkpoint.rs)で削除を含むディスクと木を照合。現行版の最終回帰でも通過。 |
| 24 | ローカル npm 依存と不一致の拒否 | **通過**：[証跡](../research/verify/local-dependencies-20261104/verification.json)。現行版の最終回帰でも通過 |
| 25 | 遅い Check 中の終了・保留維持 | **通過**：[並行実機受入](../research/verify/m1-concurrent-20261105/first/verification.json)。Check中のStop everythingで子消滅。再起動後はCheck失敗・入力held・全体停止を維持。 |
| 26 | Check による追跡ファイルの変更 | **通過**：[候補試験](../crates/intercat-daemon/tests/candidate.rs)でCheckによる変更を検出し、[修復の実機](../research/verify/repair-ui-20261104/real/result.json)で取り込み後に再Check。現行版の最終回帰でも通過。 |
| 27 | Refresh 後の生成物再準備 | **通過**：[依存試験](../crates/intercat-daemon/tests/dependencies.rs)でmanifest不変でも候補ごとに再準備し、古い生成物を使わない。現行版の最終回帰でも通過。 |
| 28 | 外部画像の接続前確認 | **通過**：[証跡](../research/verify/external-content-20261104/verification.json)。実HostのMarkdown、台本での表示、確認前0回・確認後1回の取得を確認。現行版の最終回帰でも通過 |
| 29 | 承認待ちの入力と queued の回収 | **通過**：[実行試験](../crates/intercat-daemon/tests/execution.rs)、[再照合試験](../crates/intercat-daemon/tests/reconcile.rs)、[SDK比較](../research/verify/msp-current-20261105/verification.json)で承認・完了・保存の関門とqueuedの取り戻し。現行版の最終回帰でも通過。 |
| 30 | ログ障害中の停止・再起動 | **通過**：[保存障害の最終実機検証](../research/verify/m1-storage-fault-20261105/final/verification.json)。SQLITE_FULL・2秒待ち・保存前クラッシュ・境目停止後の正常終了とクラッシュを確認。再起動後はheld、モデルの追加開始なし。 |
| 31 | Stop all turns と保留の維持 | **通過**：[停止の連続実機受入](../research/verify/m1-stop-20261105/third/verification.json)。⌥⌘.、完了A選択中の再停止、Resume後も2件held、独立した開発サーバー継続を確認。 |
| 32 | Muse の写しの改変拒否 | **通過**：[写し](../research/verify/V43-20261104-2/result.json)と[起動試験](../crates/intercat-daemon/tests/runtime.rs)で改変した実行ファイルを起動前に拒否。現行版の最終回帰でも通過。 |
| 33 | force-add・index の印・削除 | **通過**：[チェックポイント試験](../crates/intercat-daemon/tests/checkpoint.rs)でforce-add・indexの印・削除と元indexの不変を確認。現行版の最終回帰でも通過。 |
| 34 | プロセスグループ外の子孫回収 | **通過**：[停止の連続実機受入](../research/verify/m1-stop-20261105/third/verification.json)。start_new_session=TrueのHTTPサーバーを残し、host SIGKILL後でもStop everythingで回収。 |
| 35 | 報告の質問が後続入力を止める | **通過**：[報告試験](../crates/intercat-daemon/tests/reports.rs)でWaiting for you・未回答・未判定中の後続を保留し、返答順を保存。現行版の最終回帰でも通過。 |
| 36 | 拒否する pre-commit hook | **通過**：[候補試験](../crates/intercat-daemon/tests/candidate.rs)のhook_rejection_and_outside_hooks_stop_before_check_and_skip_is_explicitで拒否と出力保持。現行版の最終回帰でも通過。 |
| 37 | 書込待ちの入力を Stop が追い越す | **通過**：[保存障害の最終実機検証](../research/verify/m1-storage-fault-20261105/final/verification.json)。2秒の書込待ちを全体停止が103msで追い越し、先に置かれた入力もheldになる。 |
| 38 | シート・承認・キーボード・IME | **通過**：[並行実機受入](../research/verify/m1-concurrent-20261105/first/verification.json)で1040×760のCommit中にキーボードで承認、机↔シート移動。[IME検証](../research/verify/ime-escape-20261105/verification.json)で変換中のEsc維持。 |
| 39 | リポジトリの Muse hook 無効化 | **通過**：[ホストのhook検証](../tools/verify_host_hooks.py)と[起動の記録](../research/verify/V48-20261104-3/result.json)。現行版の最終回帰でも通過。 |
| 40 | Commit 確定中の別窓との排他 | **通過**：[2窓のUI試験](../tests/commit.spec.ts)で両窓の「Committing… · wait」とRefresh無効化を確認。[最終統合試験](../research/verify/m1-regression-20261105/rust-remaining/integration-aef4bd20f85bd2fc.log)で確定を5秒保留し、両クライアントのRefresh拒否と同じ候補の完了を確認。 |
| 41 | Editor 復旧・3 回失敗・再ダウンロード | **通過**：[証跡](../research/verify/editor-dispatch-20261104/verification.json)。実モデルの修復・再起動、Retry / Details、取得の中断再開、停止と承認のUIに加え、復旧後の実ワーカーへの自動送信・チェックポイントを確認。現行版の最終回帰でも通過 |
| 42 | デーモンの唯一性・古い socket | **通過**：[唯一性](../crates/intercat-daemon/tests/core.rs)、[socket](../crates/intercat-daemon/tests/rpc.rs)、[実機終了](../research/verify/quit-20261105/verification.json)。現行版の最終回帰でも通過。 |
| 43 | 遅い Check を Refresh が止める | **通過**：[統合試験](../crates/intercat-daemon/tests/integration.rs)のrefresh_cancels_and_awaits_the_check_but_a_stale_request_does_notで古い結果を排除。現行版の最終回帰でも通過。 |
| 44 | 停止後の Reviewer 依頼の保留 | **通過**：[実行試験](../crates/intercat-daemon/tests/execution.rs)の永続holdと[修復試験](../crates/intercat-daemon/tests/candidate.rs)の停止中の予約。新しいユーザー入力での同一セッション再開は[実機](../research/verify/m1-stop-20261105/third/verification.json)でも確認。現行版の最終回帰でも通過。 |
| 45 | 承認拒否の永続化と実行枠 | **通過**：[承認試験](../crates/intercat-daemon/tests/questions.rs)で保存前の送信禁止と停止の追越し、[実行枠試験](../crates/intercat-daemon/src/runtime_tests.rs)で承認待ち中も枠を保持。現行版の最終回帰でも通過。 |
| 46 | Git オブジェクトと ref の保存順 | **通過**：[ref公開](../research/verify/publication-20261104/result.json)、[統合試験](../crates/intercat-daemon/tests/integration.rs)でオブジェクトの同期後に操作ログ・ref更新、全中断点のSHAを照合。現行版の最終回帰でも通過。 |
| 47 | 接続切断後の同一操作 ID 再送 | **通過**：[保存UI](../tests/desktop.spec.ts)で応答喪失後に同じ入力操作IDを再送し、[永続化](../crates/intercat-daemon/tests/core.rs)と[実行](../crates/intercat-daemon/tests/execution.rs)で再送を重複適用しない。現行版の最終回帰でも通過。 |
| 48 | ワーカーからの RPC・token 非露出 | **通過**：[証跡](../research/verify/rpc-boundary-20261104/verification.json)。実デーモン・引数・環境・Keychain・5プロファイルを確認。現行版の最終回帰でも通過 |
| 49 | core.symlinks=false の継承 | **通過**：[checkout試験](../crates/intercat-daemon/tests/checkout.rs)のcopied_attributes_filters_crlf_and_symlink_mode_match_the_original_configurationでcore.symlinks=falseを継承。現行版の最終回帰でも通過。 |
| 50 | 外部 fetch の承認・Effect の秘匿 | **通過**：[方針試験](../crates/intercat-daemon/tests/policy.rs)、[Effect試験](../crates/intercat-daemon/tests/effects.rs)、[実機のEffect](../research/verify/effects-ui-20261104/validation.json)で未許可通信とqueryの非保存。現行版の最終回帰でも通過。 |
| 51 | 仕事ごとの tmp 隔離 | **通過**：[checkout試験](../crates/intercat-daemon/tests/checkout.rs)で隣接する仕事のファイルと外部書込を拒否し、自分のTMPDIRだけを使用。現行版の最終回帰でも通過。 |
| 52 | Check の子孫が消えるまで完了しない | **通過**：[checkout試験](../crates/intercat-daemon/tests/checkout.rs)でコマンド終了後の子孫を回収し、取消・タイムアウト・future破棄も検証。現行版の最終回帰でも通過。 |
| 53 | HEAD のないリポジトリ | **通過**：[証跡](../research/verify/unborn-project-20261104/verification.json)。現行版の最終回帰でも通過 |
| 54 | force-add した添付の除外 | **通過**：[添付除外](../research/verify/attachment-exclusion-20261104/verification.json)。force-addと否定ignoreを含む[チェックポイント](../crates/intercat-daemon/tests/checkpoint.rs)・[候補](../crates/intercat-daemon/tests/candidate.rs)を現行版の最終回帰でも確認。 |
| 55 | ユーザー checkout の未解決競合 | **通過**：[Git受付試験](../crates/intercat-git/tests/administration.rs)で元checkoutの未解決競合を拒否し、indexを変更しない。現行版の最終回帰でも通過。 |
| 56 | SQLITE_FULL 中の UI → RPC → MSP Stop | **通過**：[保存障害の最終実機検証](../research/verify/m1-storage-fault-20261105/final/verification.json)。実SQLiteのページ数固定によるFULL中に、UI→RPC→MSPのStopが119msで到達。新規下書きは未保存、復旧後はheldとunlogged。 |
| 57 | ホスト専用スキルの隔離 | **通過**：[ホスト専用hook・skill検証](../tools/verify_host_hooks.py)と[ホスト方針](../crates/intercat-daemon/tests/policy.rs)で専用カタログとworktree外配置。現行版の最終回帰でも通過。 |
| 58 | LFS・filter・sparse の受付拒否 | **通過**：[Git受付試験](../crates/intercat-git/tests/administration.rs)でLFS・filter・sparseをプロジェクト開始前に拒否。現行版の最終回帰でも通過。 |
| 59 | 変わる hook と候補の一回だけの作成 | **通過**：[候補試験](../crates/intercat-daemon/tests/candidate.rs)でbuild ID・commit-msgの結果を含む候補を一度だけ作成し、[統合試験](../crates/intercat-daemon/tests/integration.rs)でメッセージ編集時に再構築。現行版の最終回帰でも通過。 |
| 60 | ワーカーの未解決競合と Commit の禁止 | **通過**：[チェックポイント](../crates/intercat-daemon/tests/checkpoint.rs)で未解決indexを記録し、[統合](../crates/intercat-daemon/tests/integration.rs)で競合行としてCommitを禁止。現行版の最終回帰でも通過。 |

## 残りの作業（2026-11-05）

M1の残りはない。全60条件を照合し、ローカルcommitの対象として確定した。以後はM2へ進み、認証など人の操作が必要な場合だけ確認する。

最終回帰はRust481件成功・28件除外、UI84件成功（通常画面74件、実デーモン9件、描画性能1件）、Python13件成功。TypeScript・Clippy・書式検査も通過した。除外した有料実機・外部接続の試験は、各行の既存の実機証跡で補っている。条件14の実機保持期限ストレスはM5の範囲であり、M1へ追加しない。

Rust全体は同じビルドの50実行ファイルを照合した。並行した事前起動でmacOSの起動待ちが延び、外部コンテンツ4件が時間切れになった最初の実行も保存している。事前起動を止め、該当5件は単独・2件並行の両方で成功し、残りを直列で完了した。失敗ログを成功の集計へ混ぜていない。

保存障害は4ケース（SQLITE_FULL・2秒待ち・即時停止後のクラッシュ・境目停止後のクラッシュ）が通過。SQLiteは現在のページ数に制限し、追加は実測3行・2,634バイト、ページ増加なし。最終回帰の有料呼出しは0件、ダミー実行の完了済み累計は189 / 許可上限1000ターン。

## ユーザー追加のUI修正

- タイムライン右端の追従時刻を削除。通常の時刻目盛りもWAITING TO RUNへはみ出さないものだけ表示する。
- 差分の「▯」：Codiconフォントを登録・同梱し、[実表示で検証済み](../research/verify/diff-font-20261105/verification.json)。
- WorkerのStart／Stopは6px、New track／Putは8px。カード間・入力フォーム前は10px。有効時のフォーカスと無効時の表示を分けた。
- 常設の「Words become a plan you can steer」カードを削除。空のREQUESTSは文字列のまま。
- DESK右横は短いトラック名に代えて元の依頼を表示。固定30文字ではなく使える幅で省略し、ツールチップと読み上げ用の本文は全文を保持する。
- Connect workers／Connecting…、Retry、Stop here、Stop this worker、ファイル復旧などのカード操作を右寄せに統一。狭い表示では右寄せのまま折り返す。通常と200%表示で確認。
- Attach reference fileは「＋」アイコンに変更して入力欄左下へ移動。添付がある場合だけファイル名の行を出す。読み上げ名・ツールチップ・同じファイルの再試行は保持する。

これらは[1040px・1440pxの実画面と添付再試行で確認済み](../research/verify/controls-layout-20261105/verification.json)。
