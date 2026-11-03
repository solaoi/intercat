# 板

画面イメージ（板）54 枚の一覧と、まだ描いていないもの、PNG の作り直し方をまとめる。正本は各 `*.html` と `index.json` で、PNG は `../boards-png/` にある。

## 一覧

板の番号順。説明は `index.json` の `title` と同じ文字列にする（照合のときに、この二つを比べる）。Markdown で山括弧が消えないように、`<…>` だけは `` ` `` で囲む。

| ファイル | 説明 |
| --- | --- |
| `Onboarding.html` | 0 · 初回：Editor が自己紹介する。準備は二段で、To start with text（Muse Code・Sign in・ローカルの Editor の Download Glimmer）と Optional（声と画像）。タイムラインはまだ無い |
| `NewProject.html` | 0b · New project：名前と、このコンピューターのフォルダ（0〜n）。フォルダごとにワーカーのホストが立つ。Codex のプロジェクト作成と同じ型 |
| `FolderStart.html` | 0c · フォルダで始まる：既存のリポジトリに未コミットの変更があれば一度だけ聞く（Include / Leave out。50 MB を超えるファイルは除いて並べる）。カードの二行目は、変更の行き先（the first commit on the session branch, and any PR）。Git の無いフォルダは git を始め、作った .gitignore を仮定のカードにする。吹き出し「Started git here.」 |
| `Main.html` | 1 · 何も選んでいない：ログは出ない（タイムラインが語る）。ログの場所には、各トラックの最新の報告の一行と、未統合の変更の数の一覧（押すとそのトラックを選ぶ）。机にはカード・帯・入力欄。未来の列はレーンの右端に固定（2 枚分）で、再生ヘッドの右に目盛は無い。見出しは仕事の名前（「[A] login retry」など）で、再生ヘッドは時間の領域の 80% にある。何も選ばずに走っているので、状態の行の左に「■ Stop all turns · 2 sessions」 |
| `Planned.html` | 1c · 大きな依頼を置いた（熟慮の道）：Planner が 3 トラックに分解。計画のクリップは点線のタイルで、まだどれも走っていない。タイルは、レーンの右端に固定した未来の列（2 枚分）に同じ幅で並ぶ。A の changelog の後に走る C も先頭の列に置き、「after A」の札を付ける（連結線は選んだときだけ）。C の link it は二枚目に入る。Start は帯の計画のチップの白い「Start plan · 3 tracks」にだけあり、その左に文字のボタン「Review plan」がある。カードは 2 枚までで、入力欄に近い方から Can’t place と衝突のカード（A と C がどちらも lycoris-next の docs/index.md に触る）。docs の場所の仮定は「+1 more」に畳む。吹き出し「Split it three ways. Nothing has run yet.」。見出しは仕事の名前（「[A] login retry」など）で、再生ヘッドは時間の領域の 80% にある。A が走っているので、状態の行の左に「■ Stop all turns · 2 sessions」 |
| `PlanLimit.html` | 1d · プランの上限：新しい仕事が一斉に止まる。見出しは実際の状態のままで、帯に「New work paused · plan limit until 16:20」。リングは琥珀で満杯。カード「Plan limit until 16:20. New work waits.」に「Wait / Use your API key」（上限はプロンプト数なので Lower effort は出さない）。帯の言葉は waiting for the plan |
| `Listening.html` | 1e · 聞いている最中：マイクが白く灯り、部分的な文字起こしが入力欄に薄く流れ込む。入力欄のマイクの横に Cancel（esc）。Editor の行は「listening」 |
| `Masked.html` | 1f · Privacy gate がマスクした：帯のチップに `<redacted:api-key#1>`（値ごとの番号）、原文はローカルのログにだけ。吹き出し「I kept an API key here before it left.」 |
| `BackAfterHours.html` | 1g · 数時間後に戻った：舞台は離席区間が入るズームに一度だけ合い、全トラックが何も走らせていない区間だけが ⟋⟋ に畳まれる（目盛は両側で進む。札は「⟋⟋ 2 h 20 m · A waited on you」）。A は 14:35 から stopped、B は離席中に 6 本を終えて done。Editor の行に一言「While you were away: B finished · A waits for you · 2 tracks uncommitted」と more ›。止まったカードだけを開き、提案と Can’t place は「+2 more」 |
| `Dropping.html` | 1h · ファイルをドロップしている：落とせる場所（レーン・カード・入力欄）だけが薄く光る。レーン A の上では「Drop to attach · to Worker A」で、A だけに添付する |
| `PlanHeld.html` | 1i · 計画の Hold（設定で自動の開始にしたとき）：計画の点線の札が「held」になる（依存のある C は「after A · held」）。Start は帯のチップ「held · nothing has run」の白い「Start plan · 3 tracks」にだけあり、押すまで走らない。その左に「Review plan」 |
| `DependencyHeld.html` | 1j · 依存先が succeeded で終わらない：A の changelog を切ったので、それに依存する C の docs page は「held · A’s changelog was cut」。依存先が無いので、未来の列の先頭に並ぶ。帯の計画のチップには「Review plan」と「Start plan · 3 tracks」。札を押すとポップオーバーが開き、Restore A’s changelog（⌘Z と同じ）・Change dependency…・Start anyway を出す |
| `AgentSelected.html` | 2 · Worker A を選んだ：台本は A だけ。選ぶのは閲覧なので、入力欄の文脈チップは「Editor decides」（A は判定の手がかり） |
| `WorkerDone.html` | 2b · Worker A が完了：レーンの末尾に ✓、見出しが done、台本に done の行、Editor が commit を提案するカード。Can’t place もカードで出し、A の仮定は「+1 more」に畳む。B は続行中 |
| `ModelPick.html` | 2c · 「Worker A · Spark 1.3 · high ▾」を押した：このトラックだけのモデル（Meta の版）と effort（6 段）の上書き。Editor の選択を既定として示し、「Reset to the Editor’s pick」で戻す |
| `SkillGroup.html` | 2d · スキルのグループクリップを開いた：release-notes の 3 手順が親バーの下に細い行で並ぶ。閉じれば一本のバー |
| `StopHover.html` | 2e · 走っているバーの右端にホバー：■ が出る（押すと ⌘. と同じ。「Stop now · ⌘.」）。レーンには、ターンの終わりの目盛（1px の薄い縦線） |
| `StopMenu.html` | 2f · トラック見出しの ⋯：Say to A（錠の入口）、Stop now（⌘.）と Stop after this turn（⇧⌘.）。B は人が止めて done（灰の ■ stopped）、境目待ちの言葉は「held · Start」で帯に残る |
| `ReviewOutdated.html` | 2h · Reviewer の古い指摘：見た木の SHA の後に行が変わったので「may be outdated」。台本に残すだけで、fix の点線にはしない |
| `CenterFocus.html` | 3 · 途中の言葉を選んだ：解釈を直す、入力欄はその言葉について |
| `BoundDraft.html` | 3c · 下書きの宛先：B の ⋯ の「Say to B」で錠にした下書きは、A を見ても「Only B」のまま。文脈チップのメニューは「Only A instead / Editor decides / Clear」。帯の trim は「sent · stopping」 |
| `ConfirmMode.html` | 3b · 確認モード：訂正率が上がり、各判定を「waiting for OK · OK / Change」で見せて、OK まで置かない |
| `ReplyState.html` | 4 · 質問に Reply：入力欄が返答になる |
| `SendMenu.html` | 4b · 入力欄に文字がある：右端のマイクが送信（↑）に替わり、▾ に New track（⌘⇧↵）と Plan only（⌘⇧P） |
| `Blocked.html` | 5 · 止まる：A だけ不可逆な一手で stopped になる。他は進む。吹き出しに「more ›」 |
| `BlockedMore.html` | 5b · 「more ›」を押した：吹き出しの上に理由の全文、more › は less ‹ に変わる。Esc で閉じる。理由はログにも残る（Kept in Worker A’s log） |
| `SecretAsk.html` | 5d · secret の入力：B が npm install のトークンを求める。変数の名前と伏せた値の欄、保存の選択肢（既定は Save for this project で、範囲を文で示す / This time only）。値は環境変数でホストに渡し、Intercat のログには残さない。A のカードは別の窓で答えられ、その場で「answered elsewhere」になってから片付く |
| `Notification.html` | 5e · 窓が非アクティブ：止まったトラックの macOS の通知。文はカードと同じ実行状態から作る |
| `Reconnect.html` | 5c · 再接続と喪失：A は reconnecting · attempt 2（言葉は帯で待つ）、B はセッション喪失（見出しは lost）のカード「Pick up where it was?」 |
| `RenderCheck.html` | 6 · 回収する（14:08:00）：Commit の手前でだけ、仮定を一覧にして決める。トラックの行は二段で、入る分（Included · up to A:3）と走っている分（Not included · running now）。Check はフォルダごとに一行。シートは舞台と台本だけを覆い、机は生きたまま |
| `CommitEffects.html` | 6c · Commit シートの「Effects outside git · 1 · pushed origin 14:05 · not undone by branch or restore」（ボタンなし）と、「1 turn finished since you opened this · Refresh」 |
| `CommitSecret.html` | 6d · 統合の差分に Check environment の値：「Secret found in the diff」の行に Remove / Keep。答えるまで Commit は灰 |
| `IntegrationFailed.html` | 6e · シートを閉じた後の統合の失敗：B のレーンに仕事の印「integration failed · needs you」、カードは統合の仕事のもの。B は走り続ける |
| `CommitConflict.html` | 6b · Commit 時の衝突（14:08:00・1 フォルダの例）：後のトラック（B）の固定したチェックポイントを一時 ref の先頭に当てた merge worktree で、merge が解けなかった後で、統合の仕事は failed、担当は B（B の状態は変えない）。「merge could not resolve 2 hunks」、Let me resolve / Let B try again / Leave B out。三つは対等で既定は無く、Leave B out は薄い文字。統合 worktree での Check が緑になるまで Commit は押せない |
| `Branch.html` | 7 · 過去の点を選ぶ：ここから分岐。元のトラックは止めない（止めるのは選択） |
| `StageMode.html` | 7b · 舞台モード：仕切りを下げ切ると台本とカードが一行に畳まれ、舞台が机の上段まで広がる。帯・Editor の行・入力欄は残る。6 トラック |
| `StageNeedsYou.html` | 7c · 舞台モードで Needs you を押した：バッジの上にカードがそのまま重なって出る。舞台の配置は変えない。Esc で閉じる |
| `AfterCommit.html` | 8 · Commit の後（14:09:20）：ヘッダーの一枠が Push に変わり、▾ のメニューにフォルダ別の Push と Open 2 PRs。板 7 の続きで、人がシートで A を選んだ場面（A′ は幽霊レーンに退避、Restore で戻る）。Editor は PR を提案するだけ |
| `DocumentSession.html` | 9 · 文書のセッション：同じタイムライン、仕上げが Export、結果はプレビュー |
| `ImageTrack.html` | 9b · 文書セッションに Image トラック：Muse Image が表紙を作り、A が頁に置く。版は全部残る |
| `PreviewOpen.html` | 9c · Preview を開いた：Commit シートと同じシートが舞台と台本を覆い、頁を見開きで読む（vs v1）。机（カード・帯・入力欄・状態の行）は生きたまま。Esc で閉じる |
| `Settings.html` | 10a · 設定 Account：Meta のサインイン、Model API キー、アーカイブ、既定フォルダ、クラッシュ報告（既定オフ）、Mac の外へ出るもの（声の原音は Meta へ）。シートの上の適用先は App で、プロジェクトの行の Contributor mode は灰で「choose a project」。真偽の行は、今の値を示すスイッチ |
| `SettingsAgents.html` | 10b · 設定 Agents：適用先は App。Muse Code、worktree、Stops、Reviewer、Image の行は、今の値・影響・変える操作だけ。Checks（フォルダごと）はプロジェクトの行なので灰で「choose a project」。真偽の行は、今の値を示すスイッチ（Subagents は Not available · capacity 1） |
| `SettingsEditor.html` | 10c · 設定 Editor：Reflex（Glimmer。Local（MLX）/ Hosted（Modal））/ Planning（Wonder）/ Plans start / Privacy gate / 確認モード / キャラクター。真偽の行は、今の値を示すスイッチ |
| `SettingsSkills.html` | 10d · 設定 Skills：組み込みの intercat-report（View だけ）と、リポジトリ・個人・Muse Code のスキル一覧、使用時刻、オン/オフ、Add。適用先は App なので、リポジトリの行（スキルと AGENTS.md）は灰で「choose a project」 |
| `SettingsVoice.html` | 10e · 設定 Voice：マイク、Push to talk（窓の中は長押しかトグル、ほかのアプリからはトグル）、言語（既定は UI の言語）、キーワード |
| `SettingsShortcuts.html` | 10f · 設定 Shortcuts：セッションとタイムラインの操作（⌘Z は編集の Undo、⇧⌘Z は Redo） |
| `SettingsUsage.html` | 10g · 設定 Usage：Muse Code のプラン（5 時間・週間）と Model API の従量（音声・画像）、セッション別の内訳。ポップオーバーの Show all → の行き先 |
| `Character.html` | Intercat · キャラクターシート：箱が猫。Duo の四要素に倣った構造（一つの形の胴、伸縮するピルの目、固有のシルエット、離れた足）、8 状態、3 サイズ、何がいつ動くか |
| `QuitAsk.html` | 12 · ⌘Q の問い：Keep working in the background（既定）/ Stop everything と Remember |
| `QuitLeftover.html` | 12b · Stop everything の後に止まらない組が残った：「1 group is still running · npm run dev · PID 4312」（PID は選んでコピーできる）、Try again / Quit anyway |
| `Home.html` | 11 · Home：全セッションを最後に動いた順に一行ずつ（プロジェクトのタグ・状態・一行要約・時刻）。机には全セッションのカードがセッション名のタグ付きで集まる。走っているセッションがあるので、状態の行の左に「■ Stop all turns · 2 sessions」 |
| `SmallWindow.html` | 13 · 最小の窓（1040×760）：承認の対象が長く、カードの中で 4 行を超えてスクロールし、末尾を見るまで Allow once は灰。台本は一行（「▸ Worker A」）に畳み、余った高さは舞台に回す。カードは 1 枚、帯は 1 行で入らないチップは「+2」。状態の行の左の端に「■ Stop all turns · 2 sessions」、その右に「■ Stop A」。未来の列は 1 枚分で、入らないタイルは「+1」。見出しは仕事の名前（「[A] login retry」など）で、再生ヘッドは時間の領域の 80% にある |
| `SheetSmallWindow.html` | 14 · 最小の窓（1040×760）で Commit シートを開いたまま、A の承認に答える：シートはモーダルではない領域で、舞台と台本だけを覆う。本文はフォルダを切り替える一列で（「lycoris-next · A」と「hitoiro-api · B · 1 assumption」）、差分は Expand で広げる。Check は「Merged checks passed · log」。確定のボタンの列（Discard と Commit）は下に固定し、右上の閉じるボタンはこの窓のシートを閉じるだけ。フォーカスは ⌃Tab で机の承認のカードに移っている（輪郭）。状態の行の左の端に「■ Stop all turns · 2 sessions」、その右に「■ Stop A」。見出しは仕事の名前（「[A] login retry」など）で、再生ヘッドは時間の領域の 80% にある |

5d は、secret の入力欄の高さの分、畳みの行（「+1 more」）を外し、台本の欄を 18px 短くしている（130px → 112px）。answered elsewhere のカードも一行にまとめている。ほかの板と違う所は、ここだけだ。

## まだ板に描いていないもの

Main・Planned・SmallWindow・SheetSmallWindow の四枚は、2026-11-03 に今の仕様で描き直した。未来の列・常設の Stop・シートの操作の組み合わせを、絵で確かめた。時間の領域が狭くなったので、重なる後の言葉のラベルは菱形だけにしている（板 13 と同じ規則）。Main・Planned・Home には、同じ日に、状態の行の「■ Stop all turns · 2 sessions」を足した。走っている間は、選択に依らず出すからだ。Planned は、同じ日に、C の最初のタイルを先頭の列に移した（どの幅でも各レーンの先頭を見せる）。主ストーリーラインの判断の札「fork」は、同じ日に、45 枚で「new track」に替えた（内部の値と session/fork は fork のまま）。同じ日に、ほかの札も結果の語に替えた。枚数は、extend（to A）が 50、trim（redirect A）が 44、park（parked）が 48、ask（asked）が 45、plan（planned）が 4 だ。Main と Planned では、「parked」と「redirect A」が 7px まで寄り、一つの句に読めた。重なる後の言葉の規則（設計書 3.2）のとおり、trim の札も外して菱形だけにした。extend と trim は、言葉の文から宛先を確かめた（どれも A 宛て）。SmallWindow と SheetSmallWindow では、「asked」が「new track」に 3px 重なった。そこで、「Make it faster」の印を 68% から 66% へ寄せた。閲覧と宛先を分けたので、文脈チップは、見出しを選んだだけなら「Editor decides」、錠なら「Only A」にした。

2026-11-04 に、残りの板も今の仕様で描き直し、`stale` の 303 行を解消した。そろえたのは、未来の列・仕事の名前の見出し・状態の行の二つの Stop・仮定の既定の文字のボタンだ。Can’t place の出し方・Commit シートの Discard と Check の行・設定の適用先も、全部の板でそろえた。

次の五枚も、同じ日に今の仕様で描き直した。日本語と 200% は、それぞれの機能を作る前に通す（2026-11-03 のレビュー）。

- Onboarding：ローカルの Editor が必須になった形（マイルストーン 1 の前）
- Main：ログの場所の成果の一覧（マイルストーン 2 の前）
- SmallWindow：状態の行の二つの Stop（マイルストーン 1 の前）
- SheetSmallWindow：シートの一列の形と、テストの数を出さない Check の行（マイルストーン 4 の前）
- SettingsAgents：各行を今の値・影響・操作に絞った形（マイルストーン 11 の前）

マイルストーン 1 から 6 の前に描くものは、2026-11-01 に描いた（板 0c・1i・1j・2e・2f・2h・3c・4b・5d・5e・6c・6d・6e・11・12・12b）。その後に決めたものと、実装の値で中身が決まるものが残っている。

- その機能を作る前に描くもの
  - landed した言葉のポップオーバーの「Mark as misread」（マイルストーン 2 の前に描く）
  - 走り出した後の帯の計画のチップの「Hold the rest」（マイルストーン 3 の前に描く）
  - 衝突のカード「B conflicts with A in middleware/index.ts · Leave B out of this commit / Discard」（提出の前に描く）
  - その副文の継承の連鎖「Leaving B out also leaves C out (inherits B)」（マイルストーン 3 の前に描く）
  - Push の確認シート（宛先・ブランチ・送るコミット。マイルストーン 4 の前に描く）
  - Push の後の行「1 commit added after you reviewed · not pushed」（マイルストーン 4 の前に描く）
  - Push を拒んだときの「Destination changed since you reviewed · Review」（マイルストーン 4 の前に描く）
  - 追跡済みの秘密ファイルのカード「This repository tracks .env · Untrack it / Continue」（既定は Untrack it。マイルストーン 5 の前に描く）
  - Commit シートの「Changed in another window · Review」の帯（マイルストーン 1 の前に描く）
  - 台本の外部の画像の枠と load、リンクを開く前の確認（マイルストーン 1 の前に描く）
  - ログに書けないときの帯の一行「New work paused · can’t save to the log · disk is full · Stop still works」と、言葉のチップの「not saved」（マイルストーン 1 の前に描く）
  - 切り離しのカード「Run it under Intercat / Don’t run it / Reply」と、その後の running shell のクリップ（マイルストーン 5 の前に描く）
  - 帯の一行「Intercat stopped unexpectedly · 3 held」（マイルストーン 1 の前に描く）
  - 印の無いプロセスのカード「Code has files open in A’s worktree · Go ahead / Wait」（マイルストーン 4 の前に描く）
  - 帯の「New work paused · signed out of Meta」（マイルストーン 5 の前に描く）
  - Contributor mode の Off の適用：設定の行「requested (Off) · active (On) · 2 hosts to restart · Apply now」と、帯の一行（マイルストーン 11 の前に描く）
  - メニューバーの Agents の Stop all turns（⌥⌘.）と、帯の一行「Turns stopped · 2 processes still running · Resume · 3 held」、10f の ⌥⌘. の行（マイルストーン 1 の前に描く）
  - held の一覧のポップオーバー（Start all と項目ごとの Start。マイルストーン 1 の前に描く）
  - 依存元の差分を当てる前のカード「C’s dev server is still running · Stop it / Keep (A’s changes wait)」（マイルストーン 3 の前に描く）
  - Editor の行の「Placed in Login hardening · Open」（マイルストーン 6 の前に描く）
  - 文書のトラックの点線「use cover v3」、台本の見出しの「manuscript uses v2 · v3 available」、Export のシートの「cover: v2 · v3 available」の行（マイルストーン 8 の前に描く）
  - 点線のタイルを選んだときの、机の項目の中身（元の依頼の一行・実行本文・制約・完了の条件）と、文脈チップ「about: docs page」、「The plan changed · review again」（マイルストーン 3 の前に描く）
  - Move to project の断り「Q3 report doesn’t have lycoris-next · can’t move this session」と、移動中の帯の一行、カード「Moved, but B’s host couldn’t start · Retry」（マイルストーン 6 の前に描く）
  - Muse Code の写しのカード「Muse Code 1.4.15 is not verified · Download 1.4.14 (424 MB)」（マイルストーン 1 の前に描く）
  - Planner の時間切れのチップ「planning · timed out · Retry / Run as one job」（マイルストーン 3 の前に描く）
  - 合わせ直しのカード「A’s dev server is still running · Stop it / Keep (realign waits)」（マイルストーン 4 の前に描く）
  - Archived の Delete の前のカード「A’s dev server is still running · Stop it / Keep (Delete waits)」（マイルストーン 11 の前に描く）
  - 貸し出しのカード「A’s dev server is still running · Stop it / Keep (B waits)」と、チップの「waiting for A’s dev server」。Export のシートの同じカード（マイルストーン 8 の前に描く）
  - レーンの頭の「next waits for your answer」と、質問のカードの副文「asked before your last message」（マイルストーン 2 の前に描く）
  - hook のカード「Hooks need files outside the repo · Commit without hooks / Discard」と、Check の行の「hooks skipped」、行「Hook rejected A’s commit · pre-commit → Worker A / open log」（マイルストーン 1 の前に描く）
  - 計画の札の「held · A didn’t finish」と「held · A’s result unclear」、言葉のチップの「queued · 2nd」（マイルストーン 3 の前に描く）
  - Stop all turns and processes… の確認の窓（マイルストーン 1 の前に描く）
  - 10b の Secret files の行の「allowed: .env.test (tracked)」（マイルストーン 5 の前に描く）
  - UI の倍率 200% の画面（既定の窓。マイルストーン 11 の前に描く）
  - Reviewer の往復のカード「Reviewer still flags middleware/index.ts after 2 fixes · Fix once more / Keep it」（マイルストーン 3 の前に描く）
  - 置き換えのシートの行「Replaces A’s changes since 14:04 · adds 1 undo commit」（マイルストーン 4 の前に描く）
  - Project hooks を On にする画面（hooks.json の中身と「runs outside the sandbox, without approval」。マイルストーン 5 の前に描く）
  - Read as の「after the turn containing A:14」（マイルストーン 2 の前に描く）
  - Commit シートの「Committing… · wait」と、行「left out: config/.env.staging (secret file)」（マイルストーン 1 の前に描く）
  - 計画のチップの「planning · invalid plan · Retry / Run as one job」（マイルストーン 3 の前に描く）
  - 点線のタイルの ⌃↩ のメニュー「Move earlier / Move later / Move to track…」（マイルストーン 3 の前に描く）
  - 10f の行：⌃Tab / ⌃⇧Tab、⌃⌘= / ⌃⌘− / ⌃⌘0、⌃↩、⌥← / ⌥→、⌥↑ / ⌥↓、⇧⌘Z（それぞれの機能のマイルストーンの前に描く）
  - 状態の行の「can’t undo: run tests has started」（マイルストーン 2 の前に描く）
  - カード「output too fast to record」と、帯の「log lagging」（マイルストーン 1 の前に描く）
  - 合わせ直しの復旧のカード「Intercat stopped during a realign · A’s files don’t match the record · Use the recorded files / Keep the files (A stays held)」（マイルストーン 4 の前に描く）
  - ref の更新の復旧のカード「Intercat stopped during a commit · refs don’t match the record · Finish the commit / Show me」（マイルストーン 1 の前に描く）
  - 復旧のカード「Intercat stopped during a commit · the branch moved · Review again / Show me」（マイルストーン 1 の前に描く）
  - Discard の確かめ「Discard · drops B’s merge and A’s fix」（マイルストーン 1 の前に描く。Close と Discard は板 14 に描いた）
  - 別の窓のシートの「Discarded in another window」（マイルストーン 1 の前に描く）
  - 合わせ直しの記録の食い違いのカード「A’s record is inconsistent · Show me」（マイルストーン 4 の前に描く）
  - 置き換えの影響の行「B inherits A’s later changes · Realign B onto A′ / Leave B out of this commit」と、後続のコミット経由で分けられないときのカード「A’s later changes are mixed into B’s commit · can’t replace A automatically · Keep A / Show me」（マイルストーン 4 の前に描く）
  - 宛先を聞くカード「Where should “also bump the version” go? · Worker A / Worker B」（マイルストーン 2 の前に描く）
  - 秘密を消した後の行「Removed · still in use by 2 hosts · Apply now」（マイルストーン 5 の前に描く）
  - 完了の見出しの「done · partial」（琥珀の ◐）・「failed」（赤の ✗）・「done · result unclear」（灰）と、提案のカード「A finished part of it · Ask A to continue / Commit what’s there」（マイルストーン 3 の前に描く）
  - 操作の判定が返らないときのカード「Send to A as an instruction / Hold」（マイルストーン 2 の前に描く）
  - orphaned の言葉のチップ。補足は「the question was answered meanwhile」で、元の質問と Edit / Send to… / Take back を添える（マイルストーン 2 の前に描く）
  - fix の Run の細い行（「for the commit」）と、承認のカードの「A · for the commit」（マイルストーン 4 の前に描く）
  - 細い行の ■ と、細い行を選んだときの状態の行「■ Stop A · for the commit」（マイルストーン 4 の前に描く）
  - カードの候補の「recommended」の印（マイルストーン 2 の前に描く）
  - Try it two ways の文（「Only A′ passes the checks · Compare the diffs」など）と、Compare の画面（マイルストーン 4 の前に描く）
  - Try it two ways の「The checks can’t be compared · Compare the diffs」（マイルストーン 4 の前に描く）
  - テストの変更の行「A′ changes tests · 1 deleted, 1 skip added」と、走れなかった行（マイルストーン 4 の前に描く）
  - 走らせ方の変更の行「A′ changes how tests run · package.json scripts」（マイルストーン 4 の前に描く）
  - held の札のポップオーバーの「Retry A’s item」（failed と result unclear のとき。マイルストーン 3 の前に描く）
  - 質問のカードの「reply waiting · sends after this turn」と Take back（マイルストーン 1 の前に描く）
  - Include のカードの行「left out: video.mp4 (new, 1.2 GB)」（マイルストーン 6 の前に描く）
  - 同じカードの行「kept as committed: assets.bin (changed, too large)」（マイルストーン 6 の前に描く）
  - 帯の「Editor starting · words wait」（マイルストーン 1 の前に描く）
  - 帯の「Editor unavailable · words wait · Stop still works」（マイルストーン 1 の前に描く）
  - 初回の一行「This Mac can’t run the Editor (needs 32 GB)」（マイルストーン 1 の前に描く）
  - 承認の Reply（拒否に添える文）と、feedback を添えられないときの「Stop and send」（マイルストーン 2 の前に描く）
  - 予約のカード「Scheduled jobs aren’t supported · Don’t run it / Reply」（マイルストーン 5 の前に描く）
  - 長い貼り付けのカード「What should I do with this?」と、一行の依頼の欄（マイルストーン 2 の前に描く）
  - メモリを待つチップ「queued · waiting for memory」（マイルストーン 3 の前に描く）
  - 差分が空のトラックの行「already in the commit · no changes」（提出の前に描く）
  - Branch from here のポップオーバーの「Instructions: current / as of the checkpoint」（マイルストーン 4 の前に描く）
  - シートの行「includes instruction files · AGENTS.md」（マイルストーン 4 の前に描く）
  - fix が残したプロセスのカード「A’s fix left a process running · Stop it / Keep」（マイルストーン 4 の前に描く）
  - 外から録音している間の小さな窓（宛先・状態・Send・Cancel）（マイルストーン 9 の前に描く）
  - 操作中に強調するターンの範囲（マイルストーン 2 の前に描く）
  - 統合の差分のチップと、状態の行の「fix for this commit · A」、帯の「the commit changed」（マイルストーン 4 の前に描く）
  - 衝突エディタ（衝突の一覧、hunk ごとの Take A / Take B / Edit、Open in your editor）（マイルストーン 4 の前に描く）
  - フォルダの重複のカード「Same repository already added · Use the root / Cancel」（マイルストーン 4 の前に描く）
  - ignore されたサブフォルダのカード（マイルストーン 4 の前に描く）
  - Include のカードの行「kept as committed: config/local.json (skip-worktree)」（マイルストーン 6 の前に描く）
  - Refresh の確かめ「Refresh stops A’s fix · Refresh / Wait」（マイルストーン 1 の前に描く）
  - シートの「checked with previous settings · Re-check」（マイルストーン 1 の前に描く）
  - 承認のカードの「approved · waiting for a slot · Cancel」（マイルストーン 3 の前に描く）
  - 帯の「Editor unavailable」の Retry / Details と、Details の「Download again」（マイルストーン 1 の前に描く）
  - trim のカード「Stop A now? · Stop and send / Send after this turn」（マイルストーン 2 の前に描く）
  - 操作中の「now →」の印と「↓ now」、1 枚分の未来の列の「after A」（マイルストーン 7 の前に描く）
  - 入力欄の上の札「draft to B · Resume」（マイルストーン 2 の前に描く）
  - 入力の質問のカード（1〜3 問、「Send answers」「Explain instead」・Stop、残りの時間）（マイルストーン 2 の前に描く）
  - 時機を聞くカード「When should this run? · Now / After this turn / When I say」（マイルストーン 2 の前に描く）
  - 拒否の枠待ちの「declined · waiting for a slot」（マイルストーン 3 の前に描く）
  - シートの「A was realigned · Refresh」（マイルストーン 4 の前に描く）
  - サブモジュールのカード「Repositories with submodules aren’t supported yet · Cancel」（マイルストーン 4 の前に描く）
  - 計画のチップのポップオーバーの「Deleted plans · 2」と Restore（マイルストーン 3 の前に描く）
  - You の行のフォーカスと、密集した言葉の一覧（「4 messages」の組を押したときも同じ一覧。マイルストーン 11 の前に描く）
  - 遅れた trim のカード「the turn it was for has ended · Send as instruction / Take back」（マイルストーン 2 の前に描く）
  - 長すぎる入力のカード「Too long for the Editor · give it a one-line request」（マイルストーン 2 の前に描く）
  - 長すぎる声のカードと、欄に入れた Editor の一行の案（マイルストーン 9 の前に描く）
  - チップのメニューの「Only A」（マイルストーン 2 の前に描く）
  - チップの補足「editing in another window」と、切れた後の「held · editing was interrupted · Start」（マイルストーン 2 の前に描く）
  - 時機のカードの「Stop A and send」（マイルストーン 2 の前に描く）
  - 合図を待つチップ「waiting for your go · Start」と、待つ言葉が二つあるときの候補のカード（マイルストーン 2 の前に描く）
  - 質問の取り出しが終わらないときのカード「Did A ask you something? · Continue / Hold」（マイルストーン 2 の前に描く）
  - Mark as misread のポップオーバーの「Stop A」「Show A’s changes」「A has moved on」（マイルストーン 2 の前に描く）
  - 何も選ばないときのログの場所の、各トラックの最新の報告と未統合の数の一覧（マイルストーン 2 の前に描く）
  - 帯のチップの「landed」の補足（ポインタが帯にある間。マイルストーン 2 の前に描く）
  - 見出しの ✓（見たことにする。マイルストーン 2 の前に描く）
  - Editor の行の「No commits yet · starting from your files」（マイルストーン 6 の前に描く）
  - Open PR の灰の理由「no base branch yet」（マイルストーン 6 の前に描く）
  - 錠の文脈チップ「Only A」と、Hold の「held · Start」（マイルストーン 2 の前に描く）
  - 帯の ▾ の「Taken back · 2」の一覧（マイルストーン 2 の前に描く）
  - 「Plans ▾」の一覧（計画が二つのとき。マイルストーン 3 の前に描く）
  - 停止に応えないカード「A isn’t responding to stop · Force stop (restart host) / Wait」（マイルストーン 5 の前に描く）
  - 履歴を作り直せないカード「Couldn’t rebuild A’s history · Pick up from files / Retry」（マイルストーン 5 の前に描く）
  - 履歴の Check のカード「This check needs Git history」と、その二つのボタン（マイルストーン 4 の前に描く）
  - 依存の直しのカード「Dependencies need a fix · Ask Worker A to fix / open log」（マイルストーン 4 の前に描く）
  - 未解決の競合で始めるのを止めるカード「Your checkout has unresolved conflicts」（マイルストーン 1 の前に描く）
  - チェックポイントのカード「A left unresolved conflicts in 2 files」（マイルストーン 1 の前に描く）
  - 外からの録音を始められない行「Esc is taken by another app · record in Intercat」（マイルストーン 9 の前に描く）
  - チップの補足「preparing」と、言い直しの「already sent · Send as a new instruction / Take back」（マイルストーン 2 の前に描く）
  - 600 字を超えて貼った文の「Keep as the request / Attach as reference」（マイルストーン 2 の前に描く）
  - Home の中くらいの確信の「→ Login hardening · OK / Change ▾」（マイルストーン 6 の前に描く）
  - lost の見出し「connection lost · may still be running」（マイルストーン 5 の前に描く）
  - 更新の帯の「update waits for A’s approval · Answer / Stop A」（マイルストーン 5 の前に描く）
  - DB を戻した後のカード「values may appear unmasked · Pick up from files / Discard history」（マイルストーン 5 の前に描く）
  - シートの行「LFS binary not scanned · assets/logo.png」と、LFS の送り先の行（マイルストーン 4 の前に描く）
  - スキルのカード「intercat-report is already in the repository · using the fallback」（マイルストーン 1 の前に描く）
  - 見出しの ⋯ のメニューの「Say to A」と、文脈チップ「Editor decides」（マイルストーン 2 の前に描く）
  - 時間切れのカードの一文「Picking one re-reads what to do」（マイルストーン 2 の前に描く）
  - 確認モードのチップの補足「back to automatic?」と、Change ▾ の「Back to automatic」（マイルストーン 2 の前に描く）
  - 計画のカード「Unlock and split? · Yes / Keep on A」と、直している項目の札「editing」（マイルストーン 3 の前に描く）
  - Needs you のカードの先頭の印（needs an answer / not started / assumed, running）と、一行に畳んだ仮定（マイルストーン 5 の前に描く）
  - 分岐のモードの点の印と、確定の「Branch from 14:02 · end of A’s turn 3」、灰の「no checkpoint yet」（マイルストーン 4 の前に描く）
  - 非対応のリポジトリのカード「This repository uses Git LFS · not supported yet · Cancel」（マイルストーン 1 の前に描く）
  - シートの行「A has unresolved conflicts in 2 files · Ask A to resolve」（マイルストーン 1 の前に描く）
  - fix のカード「the fix needed B’s changes · added to B’s commit」（マイルストーン 4 の前に描く）
  - Git の無いフォルダの行「starts version history here · creates .git and the first commit」（マイルストーン 6 の前に描く）
  - 帯のトラックごとの次の補足「A’s next」と、新着の強調（マイルストーン 2 の前に描く）
  - Needs you の見出しの「+1 new」（マイルストーン 2 の前に描く）
  - 「■ Stop A · main work」と、細い行の「fix still running」（マイルストーン 4 の前に描く）
  - 別のセッションの下書きの宛先「to Worker A · Login hardening」（マイルストーン 6 の前に描く）
  - Push のシートで送り先が二つ並ぶ形（マイルストーン 4 の前に描く）
  - mirror の行「origin mirrors every ref · Intercat pushes one branch only」（マイルストーン 4 の前に描く）
  - 台本の「summarized before 14:02」の印（マイルストーン 5 の前に描く）
  - 退避する道のシートの行「running now · stops when you commit」（マイルストーン 4 の前に描く）
  - 幽霊レーンの「stopping…」と「1 group still running · Stop」（マイルストーン 4 の前に描く）
  - LFS のカード「LFS objects missing locally · fetch them in your checkout」（マイルストーン 4 の前に描く）
  - シートの行「LFS object missing · assets/logo.png · can’t commit」（マイルストーン 4 の前に描く）
  - 入れ子のリポジトリのカード「vendor/lib is a separate repository · Exclude it / Stop」（マイルストーン 1 の前に描く）
  - 外への取得のカード「A wants to fetch example.com · Allow for this session / Don’t」（マイルストーン 5 の前に描く）
- 実装が追いついてから描くもの
  - 10b の Parallel workers・Planner and Reviewer turns・Check environment の行
  - 10b の Check network・Outbound hosts・Secret files・Protected files の行
  - 10g の cap と見込みの行
  - 密度（strip）・群レーン・ミニマップ・密集したクリップの札（「5 steps · ⇗ 1 · ◇ 1 · cut 1」）の舞台
  - Branch from here の脇の covers の一行
  - 文脈のホバー
  - 経路の表（設定 Account の What leaves this Mac の View。追跡済みの秘密ファイルの行を含む）
  - 10d の「—」のホバー「attached 12 times · never read by a worker」

## 描き直すときに直す所

本文が「描き直すときに直す」と書いた箇所では、板より本文が正だ（設計書 0 節の判定の原則）。その箇所は、`index.json` のその板の項目に `"stale": [...]` として一行ずつ持たせる。描き直したら、その行を消す。2026-11-04 の時点で、`stale` の行は無い。

- A のレーンの open PR（点線）と、Reviewer のレーンの review PR は、2026-11-04 に全部の板から外した。実装ではクリップにしない。ワーカーは push しないからだ（設計書 3.5 節の板 2b）。

## 板の再生成

`*.html` は手描きの HTML（インライン CSS）。PNG を作り直すときは、Chromium の headless で撮る。

- 大きさは `index.json` の w × h で、2x で撮る。
- 書体（Inter / JetBrains Mono）は Google Fonts から読むので、ネットワークのある所で描く。無いと system-ui で描かれ、見た目が変わる。
- `CHROME` は、手元の Chromium（Playwright の chrome-headless-shell など）に置き換える。

このフォルダ（`design/boards`）で実行する。

```bash
python3 -c "import json;d=json.load(open('index.json'));[print(k,d['boards'][k]['w'],d['boards'][k]['h']) for k in d['order']]" | while read f w h; do "$CHROME" --headless --hide-scrollbars --force-device-scale-factor=2 --window-size=$w,$h --virtual-time-budget=8000 --screenshot=../boards-png/${f%.html}.png "file://$PWD/$f"; done
```
