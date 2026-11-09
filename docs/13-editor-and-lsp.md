# P3 편집: 파일 트리 · 퀵오픈 · 코드 에디터 · LSP (`crates/pod-editor`)

작성 2026-11-09. 브랜치 `SJin-Choi/p3-editor`. **pod-app 통합 완료(2026-11-09, 브랜치 `SJin-Choi/p3-integrate`, §8)**. 이 문서는 구조, pod-app용 API, 서버 설정, 통합, 남은 일을 정리한다.

## 1. 구성

```
pod-editor (lib)
├─ GUI 없는 핵심 (단위 테스트 대상)
│  ├─ tree.rs     .gitignore 인식 지연 로딩 트리 모델 + 키보드 이동
│  ├─ git.rs      `git status --porcelain=v1 -z` → 파일/폴더 표시(M/A/U/R/D/!)
│  ├─ watch.rs    notify 감시 + 250ms 디바운스, .gitignore 경로 제외, .git/index·HEAD 변경 보고
│  ├─ fuzzy.rs    자체 퍼지 매처(파일 이름·단어 시작·연속 문자 가중치, 일치 위치 반환)
│  ├─ index.rs    백그라운드 파일 색인(ignore 워커), 최근 파일 가중치, 50만 개 상한
│  ├─ text.rs     CRLF/BOM 보존 읽기·쓰기, 2MB/16MB 크기 제한, 바이너리 감지, UTF-8/16/32 열 변환
│  ├─ uri.rs      Windows 드라이브 문자·UNC·한글/공백 경로 ↔ file URI
│  ├─ fixture.rs  데모·스모크 테스트용 작은 git 저장소 생성
│  └─ lsp/
│     ├─ rpc.rs        Content-Length 프레이밍, 메시지 분류
│     ├─ client.rs     요청/응답 짝 맞추기(oneshot), 서버→클라이언트 요청 기본 응답
│     ├─ job.rs        Windows Job Object(KILL_ON_JOB_CLOSE)로 서버 프로세스 트리 소유
│     ├─ registry.rs   언어 판별, 서버 설정(기본값 + JSON 덮어쓰기), 루트 찾기, PATH/PATHEXT 검색, 설치 확인
│     ├─ server.rs     프로세스 실행, initialize/shutdown, 재시작 백오프(1·2·4·8·16초, 5회)
│     ├─ store.rs      작업 트리당 GPUI 엔티티 LspStore: 서버 상태, 문서 동기화, 진단 저장, 요청
│     └─ providers.rs  gpui-component 에디터 Completion/Hover/Definition 공급자 어댑터
└─ ui/ (GPUI)
   ├─ workspace.rs  EditorWorkspace: 트리 | 탭·배너·에디터·문제/참조 패널 | 상태 표시줄 + 오버레이
   └─ quick_open.rs Ctrl+P 퀵오픈
bin/pod-editor-demo   창 하나(보조 모니터)로 실행하는 데모, --scenario로 스크린샷용 상태 재현
bin/pod-editor-fake-lsp  테스트용 가짜 언어 서버(제품 아님)
```

에디터 자체는 gpui-component 0.9의 `EditorState`(코드 에디터 모드)를 그대로 쓴다: Tree-sitter 강조(Rust, Python, TypeScript/TSX, JavaScript, JSON, TOML, Markdown — `gpui-kit` 기능 플래그로 켬), 줄 번호, 찾기/바꾸기(Ctrl+F/Ctrl+H, `searchable(true)`), 진단 밑줄, 호버/완성 팝업, Ctrl+클릭 정의 이동.

### 데이터 흐름

```
EditorState ──InputEvent::Change──▶ EditorWorkspace ──120ms 디바운스──▶ LspStore.change_document (전체 텍스트 didChange)
     ▲   공급자 호출(완성/호버/정의) 시 먼저 현재 rope를 flush ──▶ LspStore.request ──▶ Client(stdin)
     │                                                                         │
     └── set_editor_diagnostics ◀── LspStoreEvent::DiagnosticsChanged ◀── publishDiagnostics(reader 스레드 → mpsc → GPUI 태스크)
```

- 위치 단위: gpui-component는 `Position.character`를 **유니코드 문자 수**로 해석한다. 클라이언트는 `positionEncodings: ["utf-32","utf-16"]`을 제시하고(rust-analyzer는 utf-32 선택, 변환 없음), utf-16 서버는 `text.rs`가 경계에서 변환한다. 열려 있지 않은 파일의 범위(정의 대상, 진단)는 디스크 내용으로 변환한다.
- 줄 끝: 버퍼는 항상 LF. CRLF 파일은 저장할 때 CRLF로 되돌린다(BOM도 유지). 줄 번호가 같으므로 LSP 위치도 그대로 유효하다.
- 동기화: 전체 텍스트(TextDocumentSyncKind::FULL 방식). 2MB 넘는 파일은 읽기 전용·강조 없음·LSP 없음이라 비용 상한이 있다. 증분 동기화는 남은 일.

### 외부 변경(에이전트가 파일을 바꾼 경우)

감시기 배치에 열린 파일이 있으면 디스크 내용을 읽어 비교한다.
- 우리 저장과 같으면 무시.
- 버퍼가 깨끗하면 자동으로 다시 불러오고 커서 위치 유지, 상태 표시줄에 알림(`auto_reload_clean`, 기본 켬).
- 저장하지 않은 변경이 있으면 경고 배너: **디스크 내용 불러오기 / 내 변경 유지**. 삭제된 경우 **탭 닫기 / 내 변경 유지**.
- 생성·삭제가 있으면 퀵오픈 색인을 다시 만들고, 모든 배치 뒤 git 상태를 다시 읽는다.

### 단축키 (키 문맥 `PodEditor`)

| 키 | 동작 |
|---|---|
| Ctrl+P | 퀵오픈 (↑↓ PgUp/PgDn, Enter, Esc) |
| Ctrl+S / Ctrl+K S | 저장 / 모두 저장 |
| Ctrl+W, Ctrl+Tab, Ctrl+Shift+Tab | 탭 닫기(저장 안 된 탭은 닫지 않고 알림), 다음/이전 탭 |
| Ctrl+G | 줄로 이동 (`42`, `42:5`) |
| Ctrl+F / Ctrl+H | 파일 안 찾기 / 바꾸기 (gpui-component 기본) |
| F12, Ctrl+클릭 | 정의로 이동(다른 파일이면 탭 열기) |
| Shift+F12 | 참조 찾기 → 아래 패널 |
| F2 | 이름 바꾸기 → 미리 보기(파일별 줄 전/후) → Enter 적용 |
| F8 | 다음 문제 |
| Ctrl+Space | 완성 목록 |
| Ctrl+J | 문제/참조 패널 열고 닫기 |
| Alt+1 / Alt+2 | 트리 / 에디터로 초점 이동 |
| 트리(문맥 `PodEditorTree`) | ↑↓ 이동, → 펼치기/자식, ← 접기/부모, Enter·Space 열기, Home/End |

pod-app의 전역 단축키(Ctrl+Shift+…)와 겹치지 않게 골랐다. 겹치더라도 문맥이 더 깊은 쪽이 이긴다.

## 2. pod-app용 API

```rust
// 앱 시작 시 한 번 (gpui_kit::init, pod_ui::apply_theme 다음)
pod_editor::init(cx);

// 작업(워크트리)마다 하나
let editor = cx.new(|cx| pod_editor::EditorWorkspace::new(task.worktree_path.clone(), mode, window, cx));
cx.subscribe(&editor, |this, _, ev: &pod_editor::EditorEvent, cx| match ev {
    EditorEvent::FileOpened(p) | EditorEvent::FileClosed(p) => {}
    EditorEvent::ActiveFileChanged(p) => {}          // 탭 제목/인스펙터 갱신
    EditorEvent::DirtyChanged { path, dirty } => {}   // 작업 목록에 "저장 안 됨" 표시
    EditorEvent::FileSaved(p) => {}                   // 리뷰 diff 새로고침
    EditorEvent::ExternalChange { path, reloaded } => {} // 에이전트가 파일을 바꿈
}).detach();
```

| 메서드 | 용도 |
|---|---|
| `open_file(path)` / `goto(path, line, col)` | 파일 열기(상대 경로는 루트 기준) / 1부터 세는 줄·열로 이동. 리뷰 화면 "에디터에서 열기", 터미널 경로 클릭에 사용 |
| `open_quick_open()`, `quick_open_query(q)` | 퀵오픈 |
| `save_active()`, `save_all()`, `dirty_paths()` | 작업 삭제·전환 전 저장 확인 |
| `active_path()`, `open_paths()`, `active_editor()` | 상태 조회 |
| `set_mode(Mode)` | 다크/라이트(전역 `apply_theme`는 호스트가 호출) |
| `show_panel(references)` | 문제/참조 패널 |
| `shutdown(cx) -> Task<()>` | 언어 서버 정중히 종료(2초 뒤 프로세스 트리 강제 종료). 작업 닫을 때, 앱 종료(`on_app_quit`) 때 호출 |
| `lsp() -> &Entity<LspStore>` | `statuses()`, `diagnostic_counts()`, `set_enabled(false)` 등 |
| `session(cx) -> EditorSession`, `restore_session(&s)` | (통합 때 추가) 열린 탭·커서·활성 탭 저장/복원. 복원한 탭은 뷰가 처음 포커스를 받을 때까지 LSP를 시작하지 않는다 |
| `close_path(path, force)`, `has_overlay()` | (추가) 탭 닫기(저장 안 됨이면 확인), 오버레이 여부 |
| `start_new_entry(dir, folder)`, `start_rename_entry(path)` | (추가) 트리 파일 조작 프롬프트 |
| `LspStore::running_servers()`, `suspend(key)`, `touch(path)` | (추가) 호스트의 동시 서버 상한·LRU: 실행 중 서버와 마지막 사용 시각, 정중히 멈춤(`ServerState::Suspended`), 다시 쓰면 재시작 |
| `ServerStatus::short_label()` | (추가) `rust-analyzer 실행 중 / 색인 중 / 설치 안 됨 / 일시 중지됨` |
| 액션 `QuickOpenFile, Save, SaveAll, CloseTab, NextTab, PrevTab, GotoLine, FindReferences, RenameSymbol, NextProblem, TogglePanel, FocusTree, FocusEditor` | 명령 팔레트(Ctrl+K)에 등록 |

### 통합 계획 (→ §8에서 구현, 차이는 §8에 적음)

1. **Dock 패널로 넣기**: `EditorWorkspace`를 감싸는 얇은 `Panel` 구현(`pod-app/src/editor_panel.rs`)을 만들어 작업마다 하나씩 Dock 탭으로 연다(제목 "편집 · <작업 이름>"). 터미널 패널과 같은 Dock에 두면 나란히 볼 수 있다. 뷰 하나 = 워크트리 하나 = LspStore 하나라서 작업 간 서버가 섞이지 않는다.
2. **수명**: 작업 삭제/닫기 → `dirty_paths()`가 비어 있지 않으면 확인 대화상자 → `shutdown().await` 후 drop. 앱 종료 시 모든 에디터 `shutdown` (Job Object 덕분에 비정상 종료여도 서버는 같이 죽음).
3. **연결 지점**: 리뷰 화면 파일 목록·라인 코멘트 → `goto`; 사이드바 작업 컨텍스트 메뉴 "파일 열기" → `open_quick_open`; 팔레트에 위 액션 등록; 상태 배지 옆에 `DirtyChanged` 표시.
4. **설정**: 설정 화면에 "언어 서버" 섹션 — `LspStore::statuses()`로 상태 표시, `%APPDATA%\Pod\lsp.json` 편집 안내, `set_enabled`.
5. 테마: pod-app이 테마를 바꿀 때 `apply_theme` 다음 각 에디터 `set_mode`.
6. podd 경유는 필요 없다: 편집은 로컬 파일 작업이고 언어 서버는 UI 프로세스의 자식이다(UI를 끄면 서버도 끈다 — 에이전트 실행과 무관하므로 "UI를 꺼도 에이전트는 계속 돈다" 원칙과 충돌하지 않음).

## 3. 언어 서버 설정

기본 등록(`ServerRegistry::default`):

| id | 언어 | 명령 | 루트 표식 | 루트 규칙 | 설치 확인 |
|---|---|---|---|---|---|
| rust-analyzer | rust | `rust-analyzer` | Cargo.toml | 가장 바깥(워크스페이스) | `--version` 실행 (rustup 프록시는 구성요소가 없어도 PATH에 있으므로) |
| pyright | python | `pyright-langserver --stdio` | pyproject.toml, setup.py, setup.cfg, pyrightconfig.json, requirements.txt | 가장 가까운 | PATH |
| typescript-language-server | typescript, typescriptreact, javascript, javascriptreact | `typescript-language-server --stdio` | tsconfig.json, jsconfig.json, package.json | 가장 가까운 | `--version` |

루트는 워크트리 밖으로 나가지 않으며, 표식이 없으면 워크트리 루트. (서버 id, 루트)마다 프로세스 하나.

덮어쓰기/추가: `%APPDATA%\Pod\lsp.json`(사용자) → `<워크트리>\.pod\lsp.json`(저장소). 같은 `id`는 통째로 바뀐다.

```json
{ "servers": [
  { "id": "rust-analyzer", "languages": ["rust"], "command": "C:/tools/rust-analyzer.exe",
    "root_markers": ["Cargo.toml"], "root_strategy": "outermost",
    "initialization_options": { "checkOnSave": false }, "probe_args": ["--version"] },
  { "id": "taplo", "languages": ["toml"], "command": "taplo", "args": ["lsp", "stdio"], "root_strategy": "nearest" },
  { "id": "pyright", "languages": ["python"], "command": "pyright-langserver", "enabled": false }
] }
```

상태 표시(상태 표시줄 오른쪽 점 + 문구, 필요하면 에디터 위 배너): 확인 중 → 시작 중 → 실행 중(진행 중이면 `rust-analyzer · Indexing 45%`) / **설치 안 됨**(배너에 이유와 설치 명령) / 재시작 중 (n/5) / 중지됨(오류, 배너에 "다시 시작" 버튼) / 종료됨.

Windows 처리: 서버마다 Job Object(KILL_ON_JOB_CLOSE)라서 rustup 프록시 → 실제 rust-analyzer → cargo check/proc-macro 서버, node 하위 프로세스까지 한 번에 죽는다(테스트로 확인). `CREATE_NO_WINDOW`로 콘솔 창이 뜨지 않는다. `.cmd` 셈(npm 전역 설치)은 PATHEXT로 찾는다. URI는 `file:///C:/...`로 보내고 서버가 `c%3A`, 소문자 드라이브로 돌려줘도 같은 파일로 본다.

디버깅: 환경 변수 `POD_EDITOR_LSP_LOG=<파일>`이면 주고받은 알림·서버 요청을 한 줄씩 남긴다.

설치 현황(이 PC, 2026-11-09): rust-analyzer는 `rustup component add rust-analyzer`로 설치(1.99.1). Node는 있으나 pyright/typescript-language-server는 설치하지 않음(지시대로) → 해당 파일을 열면 "설치 안 됨" 상태가 표시된다.

## 4. 라이선스와 의존성

새 의존성: `lsp-types 0.97`(MIT, gpui-component와 같은 버전), `ignore`(Unlicense/MIT), `notify 8`(CC0-1.0, 이미 Cargo.lock에 있었고 deny 허용 목록에 있음), `futures`, `serde`, `serde_json`, `anyhow`(MIT/Apache), `windows-sys`(MIT/Apache), 테스트용 `tempfile`. Tree-sitter 문법은 gpui-component 기능 플래그(MIT).

**nucleo는 쓰지 않았다**: nucleo/nucleo-matcher는 MPL-2.0이라 "Apache/MIT/ISC만" 규칙에 맞지 않는다. 대신 퍼지 매처를 직접 작성했다(`fuzzy.rs`, fzf v1식 좌측 탐욕 + 오른쪽 조이기, 파일 이름/단어 시작/연속 가중치). 5만 개 경로 질의 테스트 포함.

클린룸: Zed 등 다른 제품의 코드를 보지도 복사하지도 않았다. gpui-component(Apache-2.0)는 공개 API만 사용.

## 5. 테스트

`cargo test -p pod-editor`: 트리/.gitignore(중첩 포함)/키보드, git porcelain 파싱·폴더 집계·실제 저장소, 감시기 필터·실제 배치, 퍼지 순위·일치 위치, 색인·최근 가중치·5만 개 질의, CRLF/BOM/바이너리/크기 제한, UTF-16/8 열 변환(이모지·한글), URI(드라이브 표기 4종, UNC, `\\?\`), JSON-RPC 프레이밍(유니코드 길이, 대소문자 헤더, 잘린 프레임), 클라이언트(서버 요청 응답, 서버 종료 시 대기 요청 실패), Job Object로 손자 프로세스 종료, 레지스트리 병합·루트 규칙·PATHEXT, 재시작 백오프, 이름 바꾸기 편집 적용, 줄:열 파싱.

`tests/lsp_lifecycle.rs`(가짜 서버 `pod-editor-fake-lsp`): initialize(utf-16 협상) → didOpen → 진단(이모지 열 변환 확인) → hover/definition/references/rename/completion → 알 수 없는 메서드 오류 → shutdown/exit; 크래시 시 Exited(세대 번호 포함)와 대기 요청 실패; 서버 drop 시 손자 프로세스 종료; 미설치 서버 보고.

`rust_analyzer_smoke`(`#[ignore]`, rust-analyzer 없으면 건너뜀): 작은 고정 저장소(공백 있는 경로)에서 실제 rust-analyzer로 initialize(utf-32) → main.rs 오류 진단 → 다른 파일(geometry.rs)로 정의 이동 → hover → 종료. `cargo test -p pod-editor --test lsp_lifecycle -- --ignored` (약 70초, CPU 사용 큼 — Pod 저장소가 아니라 임시 고정 저장소에서만).

## 6. 데모와 화면 확인

```
cargo build -p pod-editor --release
target\release\pod-editor-demo.exe <경로>                         # 보조 모니터에 창
target\release\pod-editor-demo.exe --fixture <빈 폴더> --scenario hover [--light] [--duration 45]
```

`--fixture`는 고정 저장소를 만들어 연다(자체 `[workspace]`를 가진 Cargo 패키지 — 다른 워크스페이스 안에 만들어도 `cargo metadata`가 실패하지 않게. 처음에는 이게 없어서 rust-analyzer가 크레이트 그래프를 못 만들고 진단이 0개였다). 시나리오: `tree, quickopen, editor, notinstalled, diagnostics, hover, completion, definition`.

스크린샷(150% 보조 모니터, DPI 인식 캡처, 다크/라이트) — `crates/pod-editor/qa/`:

| 파일 | 내용 |
|---|---|
| `tree-*.png` | 트리: 폴더 우선, git 표시(M 주황, U/추가 초록, 폴더 점), .gitignore 숨김(target.log 없음), 탭 2개 |
| `quick-open-*.png` | Ctrl+P `geo` → 일치 글자 굵게/강조, 경로 흐리게, 결과/파일 수 |
| `editor-highlighting-*.png` | Rust Tree-sitter 강조, 한글 문자열, 들여쓰기 안내선, 상태 표시줄(줄/열·LF·UTF-8·언어·서버 진행률) |
| `diagnostics-*.png` | rust-analyzer 오류 밑줄 + 문제 패널(오류 1, 관련 힌트) |
| `hover-*.png` | `greet` 호버(시그니처 + 문서) |
| `completion-*.png` | `task.fi` 완성 → 접두어로 거른 `finish` |
| `go-to-definition-*.png` | `perimeter`에서 F12 → `geometry.rs` 탭이 열리고 정의 줄로 이동 |
| `server-not-installed-*.png` | Python 파일: "pyright 설치 안 됨" 배너 + 상태 표시줄 |

참고: 이 PC에 rust-analyzer를 설치한 뒤, 다른 Claude Code 세션의 LSP 기능이 자기 저장소에서 rust-analyzer를 띄워 3GB 넘게 쓰는 것이 관찰됐다(우리 데모와 무관, 조정자에게 보고함).

## 7. 남은 일

- ~~증분 동기화~~ → fu-app: 편집기의 `InputEditsEvent`(바이트 범위 + 새 텍스트)를 `LspStore::edit_document`가 서버 단위(UTF-16/32) 범위로 바꿔 `didChange {range, text}`로 보낸다(서버가 `textDocumentSync` Incremental일 때, 아니면 전체 텍스트). 저장본이 어긋나면(문자 경계가 아님) 범위를 버리고 기존 120 ms 전체 텍스트 동기화가 복구한다. 테스트 `incremental_ranges_replay_to_the_same_text`(이모지·한글, UTF-16/32).
- ~~완성 항목 `resolve`, 스니펫, 코드 액션~~ → fu-app:
  - **resolve**: 서버가 `resolveProvider`면 거른 목록의 앞 5개(문서 없는 것)를 `completionItem/resolve`로 동시에 채운다. 메뉴에 지연 해석 고리가 없어서 최대 250 ms만 기다리고, 늦으면 원래 항목을 쓴다. (gpui-component의 `resolve_completions`는 메뉴가 호출하지 않아 쓸 수 없다.)
  - **스니펫**: `snippetSupport: true`로 알리고, 받은 스니펫을 일반 텍스트로 펼친다(`${1:x}`→`x`, `$0`/`$1` 제거, 선택지는 첫 값, 변수는 기본값, `\$` 이스케이프). 편집기에 스니펫 세션이 없어 탭 정지 이동은 없고 커서는 삽입 끝. `additionalTextEdits`(자동 import)는 아직 적용하지 않는다.
  - **코드 액션**: `CodeActionProvider`를 연결(Ctrl+.). 범위와 겹치는 진단을 같이 보내고, 고르면 `codeAction/resolve`(필요할 때) → 편집을 모든 파일에 적용(열려 있지 않으면 탭으로 열고 저장은 사용자) → `command`가 있으면 `workspace/executeCommand`. 서버가 `workspace/applyEdit`을 보내는 명령은 아직 효과가 없다(아래).
- 시그니처 도움말, 시맨틱 토큰, 인레이 힌트.
- 실제 rust-analyzer로 코드 액션·resolve를 화면에서 확인하지는 않았다(단위 테스트: 스니펫 펼침, 증분 범위; 앱 쪽 QA는 LSP 없는 픽스처).
- `workspace/applyEdit`(서버 주도 편집)와 `didChangeWatchedFiles` 미지원 → 에이전트가 만든 새 파일을 서버가 늦게 알 수 있음(rust-analyzer는 자체 감시로 대부분 해결).
- 이름 바꾸기 결과의 파일 생성/이름 변경 작업(resource operations)은 무시하고 텍스트 편집만 적용. 열려 있지 않은 파일은 탭으로 열어 적용하고 저장은 사용자가 한다.
- ~~저장 안 된 탭 닫기 확인 대화상자~~ → 통합 때 구현(저장 후 닫기 / 저장하지 않고 닫기 / 취소).
- ~~트리 파일 조작(새 파일/이름 바꾸기/삭제)~~ → 통합 때 구현(확인 대화, 워크트리 밖·`..`·`.git` 금지). 드래그, 파일 아이콘은 남음.
- 에디터 배경·구문 색은 gpui-component 기본 테마 그대로(pod-ui 결정과 같음). Pod 토큰에 맞춘 구문 색 세트는 D3에서.
- 여러 루트(멀티 루트 워크스페이스)와 원격(SSH/WSL) 경로.
- pyright/typescript-language-server 실제 연동 확인(이 PC에 미설치라 "설치 안 됨" 표시만 확인).

## 8. pod-app 통합 (2026-11-09)

브랜치 `SJin-Choi/p3-integrate`. 사용자 관점의 화면·조작은 [docs/09 §10](09-pod-app.md#10-p3-편집-통합-파일--퀵오픈--에디터--lsp-2026-11-09).

**구조**: `crates/pod-app/src/editor_panel.rs`의 `EditorPanel`(Dock `Panel`, 이름 `PodEditor`)이 작업마다 `EditorWorkspace` 하나를 가진다(§2 계획 1 그대로 — 파일 트리 + 내부 탭). 계획과 다른 점: 파일마다 별도 Dock 탭으로 나누지는 않았다(버퍼 소유와 뷰를 나눠야 함, 남은 일). `Workspace`(pod-app)가 `editors`를 들고 `open_editor(task, Some((rel, line, col)) | None, quick_open)`로 연다. 연결 지점: 전역 Ctrl+P, 팔레트 명령 3개, 사이드바 작업 메뉴, 리뷰 파일 헤더 `↗ 편집`·파일 목록·Ctrl+Shift+O, 저장 → 리뷰 diff 새로 고침, 외부 변경(저장 안 됨) → 경고 토스트.

**LSP 수명**:

| 사건 | 동작 |
|---|---|
| 파일 패널 열기, 트리 보기 | 서버 없음 |
| 지원 파일을 처음 활성화(트리·퀵오픈·리뷰·탭) | `open_document` → (서버 id, 루트) 서버 시작. pod-editor가 활성화 때로 미뤘다(이전엔 탭 생성 때) |
| 레이아웃 복원 | 탭만 되살림, 뷰가 처음 포커스를 받을 때 활성 탭의 서버 시작 |
| 서버 상태 변화 | 앱이 모든 파일 패널의 `running_servers()`를 모아 상한(기본 2) 초과분을 LRU로 `suspend` + 정보 토스트 |
| 일시 중지된 서버의 파일로 돌아감 | `touch`/`open_document`가 다시 시작 |
| 파일 패널 닫기 / 작업 삭제 / 작업이 목록에서 사라짐 | `shutdown()` (2초 정중 종료 후 Job Object 트리 종료) |
| 앱 종료 | `on_app_quit`: 레이아웃 저장, 모든 편집기 `shutdown()` 대기. 강제 종료여도 Job Object가 정리 |

**저장 안 함 보호**: 탭 닫기 확인(pod-editor 오버레이), 저장 안 한 파일이 있는 파일 패널은 Dock에서 닫을 수 없음(`closable`), 창 닫기 확인(`on_window_should_close` → 대화, 선택 후 `cx.quit()`), 작업 삭제 전 확인.

**접근성·테마**: 트리(`Tree`/`TreeItem`, 선택 상태, git 표시 포함 이름), 탭 목록(`TabList`/`Tab`, 저장 안 됨 표시), 탭 닫기 단추 이름, 에디터 이름 `편집기: <경로>`, 오버레이·배너 단추 이름과 포커스 링(`pod_ui::focus_style`), 확인 대화는 `AlertDialog`. gpui-component 에디터의 접기 단추에 이름이 없어서 vendor 패치에 `Input.Fold/Unfold` 키를 추가했다(POD-PATCHES.md, 한국어는 pod-app 카탈로그). 새 문구는 pod-app `strings.rs`, pod-editor는 `ui/strings.rs`(기존 문구는 코드 옆에 그대로).

**QA 장면**(`--ui-fixture --fixture-scene …`, `fixture_scenes.rs`): `editor`(시각 회귀 기준, LSP 끔), `editor-tree`, `editor-quickopen`, `editor-new-file`, `editor-close`, `editor-dirty`, `editor-agent-edit`, `review-editor`, `review-editor-open`, `editor-lsp`/`editor-hover`/`editor-completion`(작은 고정 저장소에서 실제 rust-analyzer), `editor-lsp-cap`(`.pod/lsp.json`으로 rust-analyzer/pyright id를 가짜 서버로 바꾸고 상한 1). 픽스처 소스는 이 장면들에서만 `task-000`에 워크트리를 주고 `task.diff`를 그 저장소의 git으로 답한다. 실행: `crates/pod-app/qa/editor/run-editor-qa.ps1`.

**스크린샷**(`crates/pod-app/qa/editor/`, 150% 보조 모니터, DPI 인식 캡처, 모두 직접 확인):

| 파일 | 내용 |
|---|---|
| `dark/light-editor-tree.png` | 작업 공간 안 파일 패널: 트리(폴더 우선, M/U 표시, 숨김 규칙), 탭 2개, Rust 강조, 상태 표시줄 |
| `dark/light-editor-quickopen.png` | Ctrl+P `geo` → `geometry.rs`, `settings.json`, 일치 글자 강조, 결과/파일 수 |
| `dark-editor-lsp.png` | rust-analyzer 진단: 문제 패널(오류 1 + 관련 힌트), 에디터·앱 상태 표시줄 `rust-analyzer 실행 중` |
| `dark-editor-hover.png` | `greet` 호버(시그니처 + 문서) |
| `dark-editor-completion.png` | `task.fi` → `finish` 완성 |
| `dark/light-review-editor.png` | 리뷰 diff 파일 헤더마다 `↗ 편집` |
| `dark/light-review-editor-open.png` | 리뷰에서 연 `src/main.rs`가 리뷰 옆 탭으로 열리고 첫 변경 줄(16)에 커서 |
| `dark/light-editor-agent-edit.png` | 에이전트가 저장 안 한 `lib.rs`를 바꿈: 배너(불러오기/유지) + 경고 토스트, 깨끗한 README는 자동 다시 불러옴(상태 표시줄 알림) |
| `dark/light-editor-close.png` | 저장 안 한 탭 닫기 확인 |
| `dark-quit-unsaved.png` | 창 닫기 확인(저장 안 한 파일 목록) |
| `dark/light-editor-new-file.png` | 트리 `새 파일 · src/` 프롬프트 |
| `dark-editor-lsp-cap.png` | 상한 1: LRU 일시 중지 토스트 2개, 돌아간 서버가 `실행 중` |
| `dark-restore-before/after.png` | 종료 전 탭 상태 / 다시 시작 후 복원된 탭·커서 |

결과 요약과 명령 로그: `qa-result.txt`, `clippy.txt`, `tests.txt`, `deny.txt`, `release.txt`, 시각 회귀 `visual/compare.json`.

**참고(작업 환경)**: 이 작업을 한 Claude Code 세션 자체가 파일을 고칠 때마다 Pod 저장소 전체에 rust-analyzer(약 3.5GB)를 자동으로 띄웠다. 지시대로 그때마다 종료했다(세션의 LSP 기능을 쓰지 않았음에도 발생). 워커 환경에서 이 자동 시작을 끄는 설정이 필요하다.
