# P0 코어: `podd` 데몬 + `pod-holder` + `pod` CLI

작성일: 2026-11-08 (v2: holder·attach·Job Object·OSC 7501) · 코드: `crates/podd`, `crates/pod-cli` · 상위 기획: [01 §4.2 (1)(3), §5.3, §6 P0](01-plan.md) · 상태 프로토콜: [04 §1, §4.4](04-status-detection-research.md)

P0 완료 기준 "앱 없이 CLI만으로 Claude 실행·재attach"를 만족한다. 터미널은 **holder 프로세스**가 소유한다. 따라서 데몬을 멈추거나 죽였다 다시 켜도 프로그램이 계속 돌고, 새 데몬이 holder에 다시 붙는다. 사람은 `pod attach`로, 에이전트는 `pod run/send/wait/read`로 터미널을 조작한다.
다른 제품의 코드를 쓰지 않은 클린룸 구현이다.

---

## 1. 크레이트 구성

```
crates/podd/                 lib + bin "podd" + bin "pod-holder"
  src/protocol.rs            JSON-RPC 타입(메서드·알림·오류 코드), schema()
  src/client.rs              블로킹 클라이언트, 데몬 자동 시작, attach 스트림, spawn_detached
  src/endpoint.rs            파이프 이름·상태 디렉터리 규칙, POD_SOCKET / POD_TERMINAL_ID
  src/env.rs                 터미널 환경 정리(런처 표식 제거)
  src/keys.rs                키 이름 → 바이트
  ── feature "server" ──
  src/server/mod.rs          데몬: 레지스트리, RPC 처리, attach 스트리밍, holder 재발견
  src/server/terminal.rs     데몬 쪽 터미널 = holder 연결 + VT 미러
  src/holder.rs              pod-holder 본체 (ConPTY·Job Object 소유, 프레임 서버)
  src/holder_proto.rs        데몬↔holder 바이너리 프레임, HolderSpec/HolderRecord
  src/vt.rs                  alacritty_terminal VT + seq 카운터 + ANSI 스냅샷 + OSC 7501 질의 감지
  src/pty.rs                 ConPTY 직접 생성(번들 conpty.dll 우선), Job Object, PATHEXT 해석
  src/winsec.rs              현재 사용자 전용 DACL, 레지스트리 기준 사용자 환경
  build.rs                   vendor/conpty → target/<profile>/ 복사
  examples/tap.rs            디버그: attach해서 원시 스트림(또는 OSC만) 출력
  src/server/tasks.rs        (P3) 저장소 레지스트리·Task 모델 → docs/08
  src/git.rs                 (P3) git CLI 래퍼(porcelain/-z 파서)
  tests/common/mod.rs        통합 테스트 공용 하네스(테스트 전용 파이프·상태·worktree 루트)
  tests/daemon.rs            실제 podd·pod-holder 바이너리로 하는 통합 테스트 6개
  tests/tasks.rs             (P3) Task 통합 테스트 2개
crates/pod-cli/              bin "pod" (clap), src/attach.rs = 대화형 콘솔 클라이언트
tools/fetch-conpty.ps1       Microsoft ConPTY NuGet 패키지 받기(버전·SHA-256 고정)
deny.toml                    cargo-deny 라이선스/출처 정책
```

CLI가 별도 크레이트인 이유는 v1과 같다. `pod-cli`는 `podd = { default-features = false }`로 프로토콜 타입과 클라이언트만 쓴다(tokio·alacritty 의존 없음).

## 2. 프로세스 모델

```
pod / pod attach / (P2) pod-app ──┐  \\.\pipe\podd-<USER>  (NDJSON JSON-RPC 2.0, 현재 사용자 DACL)
에이전트 안의 pod (POD_SOCKET) ────┘
                                   podd  (분리 실행, 콘솔·창 없음, 단일 인스턴스)
                                    │  터미널마다 \\.\pipe\pod-holder-<podd-USER>-<id> (바이너리 프레임)
              ┌─────────────────────┼──────────────────────┐
         pod-holder t1         pod-holder t2            ...   (분리 실행, 데몬과 수명 독립)
          ConPTY(OpenConsole)    ConPTY
          Job Object ─ claude    Job Object ─ powershell ─ (자식들)
```

- **holder**: 터미널 하나당 프로세스 하나. ConPTY와 자식 프로세스 트리(Job Object)를 소유한다. 출력을 읽어 자기 VT에 먹이고(질의 응답·스냅샷용), 연결된 데몬에 원시 바이트로 흘려보낸다. 데몬이 없어도 계속 돈다.
- **데몬**: holder를 띄우고 연결한다. holder 스트림으로 **VT 미러**를 유지해 `read`/`wait`/attach 스냅샷을 처리한다. attach 클라이언트에는 원시 바이트를 중계한다.
- **시작·재발견**: holder는 `%LOCALAPPDATA%\pod\state\<pipe key>\<id>.json`(HolderRecord: id, name, pipe, holder_pid, child_pid)을 쓴다. 데몬은 시작할 때 이 디렉터리를 훑어 각 holder 파이프에 연결한다. 연결되면 터미널로 복원하고, 파이프가 없으면(NotFound) 기록을 지운다. 다음 id는 복원한 최대 id + 1이다.
- **터미널 생성**: 데몬이 `<id>.spec.json`(argv, cwd, **완성된 env**, 크기, 파이프 이름)을 쓰고 `pod-holder --spec <file>`을 분리 실행한다. 그다음 holder 파이프가 열릴 때까지 연결을 재시도한다(최대 15초). holder가 그 전에 끝나면 `<id>.err`의 메시지를 `SPAWN_FAILED`로 돌려준다.
- **분리 실행**: `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW`와 `CREATE_BREAKAWAY_FROM_JOB`을 함께 쓴다. 잡이 breakaway를 막으면 breakaway 없이 다시 시도한다. 데몬→holder와 CLI·pod-app→데몬에 같은 함수(`client::spawn_detached` = `detach.rs`)를 쓴다.
  - **상속 핸들 없음**(podd-hardening): `std::process::Command`는 `bInheritHandles = TRUE`로 호출자의 상속 가능한 핸들을 **전부** 넘긴다. 그래서 `pod ls | ...`가 데몬을 자동 시작하면 podd가 호출자의 stdout 파이프를 물려받아, 읽는 쪽이 podd가 끝날 때까지 EOF를 받지 못했다(잡이 breakaway를 막는 환경, 즉 터미널·에이전트·CI 안에서 재현). 이제 `CreateProcessW`를 직접 부르고 `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`로 상속 집합을 새로 연 `NUL` 핸들 하나로 제한한다. 그 핸들이 stdin/stdout/stderr다(`STARTF_USESTDHANDLES`). 회귀 테스트: `tests/detach.rs`(breakaway를 막는 잡 안에서 stdout을 파이프로 받은 `pod ls`가 데몬이 살아 있는 동안 EOF에 도달), `detach::tests::child_does_not_inherit_our_pipe`(같은 조건에서 예전 `Command` 방식은 실패함을 확인했다).
- **수명**:
  - `server.stop`(기본): 데몬만 끝나고 holder는 계속 돈다.
  - `server.stop {kill:true}`: 모든 터미널을 kill한 뒤 release하고 끝난다.
  - holder는 **release**를 받아야만 끝난다. release는 `terminal.rm`이 보내며, 자식이 이미 종료된 경우에만 받아들인다.
  - 자식이 끝난 뒤에도 holder는 남아 최종 화면과 종료 코드를 보관하므로, 끝난 터미널도 데몬 재시작 뒤에 그대로 보인다.
  - holder가 죽으면 Job Object(`KILL_ON_JOB_CLOSE`)가 프로세스 트리를 정리한다. 데몬은 이 터미널을 `lost`로 표시한다.
- **단일 데몬 락**: 첫 파이프 인스턴스를 `FILE_FLAG_FIRST_PIPE_INSTANCE`로 만든다. 이미 있으면 "lock held"를 남기고 종료 코드 3으로 끝난다. holder 파이프도 같은 방식이다.
- **보안**: 데몬·holder 파이프 모두 SDDL `D:P(A;;GA;;;<현재 사용자 SID>)`로 만든다. 보호된 DACL이라 Everyone/Anonymous 읽기 권한이 없다. `reject_remote_clients`도 켠다.
- **Kill = 프로세스 트리 전체**: 자식을 `CREATE_SUSPENDED`로 만들고, Job Object에 넣은 뒤에 `ResumeThread`한다. 따라서 자식이 만드는 모든 손자도 잡에 속한다. kill은 `TerminateJobObject`다. 콘솔에 붙지 않은 손자(숨은 별도 콘솔의 `ping`)도 함께 끝나는 것을 테스트로 확인했다.

### 2.1 터미널 환경 정리 (`env.rs`)

`pod`는 아무 셸(에이전트 도구 셸, 다른 IDE의 터미널, VS Code…)에서나 데몬을 자동 시작한다. 그래서 런처의 변수가 터미널로 새면 안 된다. 실제로 겪은 사례는 두 가지다. `NO_COLOR=1`이면 Claude 색이 사라진다. `CLAUDECODE`/`CLAUDE_CODE_*` 표식이 있으면 Claude가 자신을 자식 세션으로 착각한다(트랜스크립트를 끄고 경고를 띄운다). 환경은 다음 순서로 만든다.

1. **바탕**: `CreateEnvironmentBlock(token, FALSE)`. 레지스트리의 시스템·사용자 변수만 쓰므로 새로 로그온한 것과 같다. 실패하면 데몬 자신의 환경을 쓴다.
2. **제거**: `NO_COLOR`, `FORCE_COLOR`, `CLICOLOR_FORCE`, `CLAUDECODE`, `CLAUDE_PID`, `CLAUDE_EFFORT`, `TERM`, `TERM_PROGRAM(_VERSION)`, `TERM_SESSION_ID`, `COLORTERM`, `WT_*`, `ConEmu*`, `POD_*`, `CLAUDE_CODE_*`, `CODEX_*`, `VSCODE_*`, 부모 IDE·터미널 멀티플렉서가 넣는 접두어 변수(목록은 `crates/podd/src/env.rs`). 사용자 설정은 남긴다: `CODEX_HOME`, `CLAUDE_CODE_USE_BEDROCK/VERTEX`, `CLAUDE_CODE_SKIP_*_AUTH`, `CLAUDE_CODE_GIT_BASH_PATH`, `CLAUDE_CODE_MAX_OUTPUT_TOKENS`, `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC`.
3. **추가**: `TERM=xterm-256color`, `COLORTERM=truecolor`, `POD_TERMINAL_ID=<id>`, `POD_SOCKET=<데몬 파이프>`.
4. **덮어쓰기**: `terminal.run`의 `env`(무엇이든 설정 가능). `pod run --inherit-env`는 CLI 셸의 환경을 (2)로 걸러 `env`로 보낸다(venv 같은 셸 상태가 필요할 때).

`argv[0]`은 자식 환경의 `PATH`/`PATHEXT`로 찾는다. `.cmd`/`.bat`(npm의 `claude.cmd` 같은 shim)은 `cmd.exe /d /s /c "..."`로 실행한다.

## 3. 데몬 ↔ holder 채널 (`holder_proto.rs`)

holder의 전용 파이프 위에서 바이너리 프레임을 주고받는다: `u32 len(LE, 이후 바이트 수) | u8 kind | body`. 원시 PTY 바이트는 이 구간에서 base64 없이 그대로 간다. 연결이 하나 새로 오면 이전 연결을 대체한다(재시작한 데몬이 넘겨받는 방식).

| kind | 방향 | body | 의미 |
|---|---|---|---|
| 0x01 Hello | H→D | JSON `{version,id,name,argv,cwd,created_ms,holder_pid,child_pid,cols,rows,conpty}` | 연결 직후 첫 프레임 |
| 0x02 Snapshot | H→D | `u64 seq` + ANSI | holder VT의 현재 상태(스크롤백 전체). 출력 `seq`바이트 직후 상태 |
| 0x03 Output | H→D | `u64 seq` + 바이트 | PTY 출력 `[seq, seq+len)` |
| 0x04 Resized | H→D | `u64 seq, u16 cols, u16 rows` | 이 위치부터 새 크기(스트림 순서 안에서) |
| 0x05 Exited | H→D | JSON `{seq, exit_code, killed, exited_ms}` | 마지막 출력 뒤 |
| 0x81 Input | D→H | 바이트 | 자식 입력 |
| 0x82 Resize | D→H | `u16 cols, u16 rows` | ConPTY + holder VT 크기 변경 → Resized로 확인 응답 |
| 0x83 Kill | D→H | – | `TerminateJobObject` |
| 0x84 Release | D→H | – | 종료된 터미널만: 기록 삭제 후 holder 종료 |
| 0x85 DaemonHello (holder 프로토콜 2) | D→H | JSON `{version}` | Hello를 받은 직후. holder 버전이 2 이상일 때만 보낸다(1은 모르는 kind를 오류로 끊는다) |

**버전 협상(podd-gaps2, holder 프로토콜 2)**: Hello의 `version`을 데몬이 확인한다. 지원 범위는 `HOLDER_PROTOCOL_MIN(1)..=HOLDER_PROTOCOL(2)`이다. 범위 밖이거나 Hello가 이 빌드의 구조로 읽히지 않으면(`ForeignHello`: JSON은 읽히지만 필드가 다름) 연결을 닫고, 그 터미널을 `incompatible_holder` 사유와 함께 **종료된 터미널**로 목록에 둔다(id·이름·argv는 Hello에서 가능한 만큼). 화면에는 `[pod: incompatible holder: holder protocol 99, this podd supports 1..=2. …]` 설명을 쓴다. 입력·리사이즈는 받지 않고, `terminal.rm`은 `force`일 때만 holder 프로세스를 `TerminateProcess`로 끝낸다(Job Object가 트리를 정리). 프로세스가 살아 있으므로 종료 터미널 보존 정책(§7-1)으로는 지우지 않는다. `pod ls`는 기본 목록에도 "incompatible holder"로 보인다. 프로토콜 1 holder는 그대로 동작하며 `DaemonHello`를 받지 않는다. holder 2는 받은 데몬 버전을 로그에 남긴다(지금은 동작이 바뀌지 않음). Hello와 spec에는 선택 필드 `parent_terminal`/`parent_task`가 더해졌다(holder가 들고 있으므로 데몬 재시작 뒤에도 부모가 남는다).

holder는 연결이 오면 `Hello → Snapshot(seq) → (종료됐으면 Exited)`를 보내고 구독을 등록한다. 이 전체를 출력 처리와 **같은 락 안에서** 하므로, 첫 Output의 `seq`는 정확히 스냅샷의 `seq`다. 데몬은 Snapshot으로 새 VT 미러를 만들고 Output을 이어서 먹인다. holder VT에 스크롤백 전체가 있으므로 데몬 재시작 뒤 `read --history`도 그대로다.

**질의 응답은 holder만 한다.** holder VT의 DA/DSR 응답과 OSC 7501 응답만 PTY 입력으로 간다. 데몬 미러는 응답하지 않는다. attach 클라이언트가 붙은 실제 터미널이 같은 질의에 답하면 데몬이 그 응답을 attach 입력에서 걸러낸다(§4.1, v7).

## 4. 클라이언트 프로토콜 v2

> v3(P3)에서 `repo.*`·`task.*` 메서드와 `terminal.run`의 `task` 파라미터, 오류 코드 -32006..-32013이 추가됐다. 자세한 내용은 [08-tasks.md](08-tasks.md).
>
> **현재 프로토콜 번호는 9**다(`PROTOCOL_VERSION`, `server.status.protocol`). v9 = v8 + 수명 이벤트(`status.subscribe`의 `lifecycle`, `terminal.changed`·`task.changed`·`task.removing`), `task.touch`, 색 질의 응답 중복 제거와 늦은 응답 차단(`SendResult.dropped_replies`). 모두 추가형(§4.1-6, §4.3, §7.3).

전송: 한 줄에 JSON 하나, JSON-RPC 2.0. 연결 하나에서 요청은 순서대로 처리된다. 전체 스키마는 `pod server schema` 또는 `podd schema`로 볼 수 있다.

| 메서드 | params | result |
|---|---|---|
| `server.status` | `{}` | `{version, protocol, pid, pipe, started_ms, uptime_ms, terminals, running, state_dir}` |
| `server.stop` | `{kill?: bool}` | `{stopping, killed, detached}`. 기본은 터미널을 holder에 남긴다 |
| `terminal.run` | `{argv, cwd?, name?, env?, cols?, rows?, task?, parent_terminal? (v7)}` | `TerminalInfo` |
| `terminal.list` | `{}` | `{terminals: TerminalInfo[]}` |
| `terminal.send` | `ref + {text, enter?, confirm? (v8, 기본 true)}` | `{id, bytes, submit? (v8)}`. Enter는 별도로 쓴다. v8(e2e-fix1, docs/12 B1)부터 30 ms 고정 대기가 아니라 **프로그램이 텍스트를 그리고 300 ms 조용해진 뒤**(최소 150 ms, 출력이 없으면 1 s, 최대 1 s + 글자당 0.5 ms ≤ 5 s) 누른다. 에이전트 상태가 보이는 터미널이면 4 s 안에 턴 시작(working/blocked/turn 증가)을 확인하고 없으면 Enter를 한 번 더 누른다. 결과 `submit = {state: confirmed\|unconfirmed\|unverifiable, enters, signal?, entered_ms?, settled_ms?}`([11 §3.1](11-review-and-orchestration-backend.md)). `pod send`는 unconfirmed면 경고를 stderr에 쓴다. `--no-confirm`은 확인·재시도 없이 한 번만 |
| `terminal.keys` | `ref + {keys: string[]}` | `{id, bytes}`. `enter tab shift+tab esc backspace space up down left right home end pageup pagedown insert delete f1–f12 ctrl+<a-z@[\]^_> alt+<키>`, 한 글자. 화살표는 DECCKM(앱 커서 모드)을 따른다 |
| `terminal.input` | `ref + {data: base64}` | `{id, bytes, dropped_replies? (v9)}`. 원시 입력(attach 클라이언트가 사용). 중복·늦은 질의 응답은 지운다(§4.1-5, 6) |
| `terminal.read` | `ref + {mode?: screen\|history, lines?}` | `{id, mode, lines, cursor, state}` |
| `terminal.wait` | `ref + {until: "exit" \| {output_contains}, timeout_ms?}` | `{outcome: matched\|timed_out\|exited, terminal, waited_ms}`. `pod wait`(그리고 `--status`/`--turn-end`/`--gate`)는 데몬이 재시작되어 연결이 끊기면 같은 파이프에 백오프(100 ms → 2 s)로 다시 붙어 남은 시간으로 같은 조건을 다시 기다린다(docs/12 B6). 데몬을 직접 띄우지는 않는다. `POD_RECONNECT_TIMEOUT`(초, 기본 60) 안에 돌아오지 않으면 분명한 오류로 끝난다 |
| `terminal.resize` | `ref + {cols, rows}` | `TerminalInfo`. holder가 Resized로 확인한 뒤에 돌아온다 |
| `terminal.kill` | `ref` | `TerminalInfo`. 프로세스 트리 전체 |
| `terminal.rm` | `ref + {force?}` | `{id}`. 종료된 터미널만 지운다(`force`면 kill 후 지움). holder가 끝난다 |
| `terminal.keep` (v6) | `ref + {keep?: bool = true}` | `{id, kept, expires_ms?}`. 종료 터미널 보존 정책에서 제외(`false`면 다시 포함). `<state dir>\<id>.keep`에 저장 |
| `terminal.attach` | `ref + {scrollback?, cols?, rows?}` | `{terminal, seq, snapshot: base64}`. 이후 연결은 스트림이 된다 |

`TerminalInfo = {id, name?, argv, cwd, state, exit_code?, pid?, holder_pid?, cols, rows, created_ms, exited_ms?, killed, lost, attached, conpty?, status?, parent_terminal? (v7), parent_task? (v7), incompatible_holder? (v7)}`

v7 추가(모두 선택 필드, 프로토콜 번호 7): `parent_terminal`은 이 터미널을 띄운 Pod 터미널이다. `pod run`이 Pod 터미널 안에서(같은 데몬의 `POD_SOCKET`) 실행되면 호출자의 `POD_TERMINAL_ID`를 `RunParams.parent_terminal`로 보낸다. 데몬이 아는 터미널일 때만 기록하고, 그 터미널이 속한 Task가 `parent_task`가 된다. 모르는 id는 로그만 남기고 무시한다. Task의 에이전트 터미널은 Task의 부모(`task.create`/`group.create` 호출자)를 부모로 받는다. Rust에서 `TerminalInfo`를 직접 만드는 코드는 `TerminalInfo::new(...)` 또는 `..Default::default()`를 쓴다(pod-app의 테스트 하나가 그렇게 바뀌었다).

오류 코드: 표준 코드 + `-32001` 없음, `-32002` 이름 중복, `-32003` 실행 실패, `-32004` 이미 종료, `-32005` 아직 실행 중.

### 4.1 attach (결정 A: 원시 바이트 + 클라이언트 VT)

1. `terminal.attach` 응답에는 `snapshot`이 들어 있다. 같은 크기의 빈 터미널에 쓰면 현재 화면과 스크롤백 N줄(기본 1000줄)이 재현되는 ANSI다. 함께 오는 `seq`는 그 스냅샷이 대응하는 출력 오프셋이다.
2. 그다음 같은 연결로 알림만 온다.
   - `terminal.output {id, seq, data}`: 원시 바이트 `[seq, seq+len)`. 클라이언트는 `seq == 직전 seq + 직전 len`인지 확인한다. 데몬 미러 락 안에서 스냅샷을 찍고 구독을 등록하므로 빠지거나 겹치는 바이트가 없다.
   - `terminal.resized {id, seq, cols, rows}`: 스트림 순서 안의 크기 변경.
   - `terminal.exited {id, seq, exit_code, killed}`: 마지막 알림. 이후 데몬이 연결을 닫는다.
   - `terminal.lagged {id, seq, queued_bytes, limit_bytes}` (v7): 이 클라이언트에 쌓인 출력이 데몬의 클라이언트별 상한(`podd --attach-queue-limit <bytes>`, 환경 변수 `POD_ATTACH_QUEUE_LIMIT`, 기본 16 MiB)을 넘었다. 남은 큐를 버리고 이 알림을 보낸 뒤 연결을 닫는다. `seq`는 받지 못한 첫 바이트다. 클라이언트는 다시 attach해 새 스냅샷부터 이어 간다(`pod attach`는 자동으로 다시 붙는다). 그래서 읽지 않는 클라이언트가 데몬 메모리를 무한정 늘리지 못한다.
3. 입력과 리사이즈는 **다른 연결로** 보낸다(`terminal.input`/`keys`/`resize`). Windows 동기 파이프 핸들은 I/O를 직렬화하므로, 한 연결에서 읽기를 기다리는 동안 쓰기가 막힌다. 클라이언트가 연결을 닫으면 detach다.
4. **크기 정책**: 마지막 resize가 이긴다. 어떤 클라이언트든 `terminal.resize`나 attach의 `cols/rows`로 크기를 바꿀 수 있고, 모든 attach 클라이언트가 `terminal.resized`를 받는다. `pod attach`는 붙을 때 자기 콘솔 크기로 맞추고, 콘솔 크기가 바뀌면 따라간다(`--no-resize`면 크기를 건드리지 않는다).
5. **질의 응답 중복 제거(v7)**: holder가 DA1(`CSI c`)·DA2(`CSI > c`)·DSR 5/6(`CSI 5 n`/`CSI 6 n`)·DECXCPR(`CSI ? 6 n`)·OSC 7501 질의에 이미 답한다. 데몬은 출력 스트림에서 이 질의를 찾으면 그때 붙어 있는 attach 클라이언트 수만큼 응답을 "기대"한다(5초). `terminal.input`으로 들어온 바이트에서 기대한 종류의 완전한 응답 시퀀스(`CSI ? … c`, `CSI > … c`, `CSI 0 n`, `CSI r;c R`, `CSI ? r;c(;p) R`, `OSC 7501;?:…`)를 하나씩 지우고 나머지만 PTY로 보낸다. 결과 `bytes`는 실제로 보낸 바이트 수다. 기대가 없으면 아무것도 지우지 않는다(수정자 F3 `CSI 1;5R`처럼 CPR과 모양이 같은 키도 질의 직후 5초 안이 아니면 그대로 간다). 한계: 한 `terminal.input` 안에서 완결된 응답만 알아본다(두 번에 나뉘어 오면 통과). ~~색 질의(OSC 10/11)는 holder가 답하지 않으므로 대상이 아니다(09 §5-2).~~ v9에서 색 질의와 늦은 응답도 다룬다(아래 6).
6. **색 질의 응답과 늦은 응답(v9, fu-daemon)**: 출력 스캐너가 OSC 10/11/12 질의(`OSC 10;?`, `OSC 10;?;?`는 10과 11)와 OSC 4 질의(`OSC 4;n;?`, 색 번호마다 하나)도 찾는다. 질의마다 "그때 붙은 클라이언트 수, 지금까지 받은 응답 수, 질의 시각"을 기록하고 `terminal.input`의 응답을 하나씩 판정한다(`server/query_dedup.rs`).
   - holder가 답하는 질의(DA1/DA2/DSR/CPR/7501): 클라이언트 응답은 모두 중복이라 버린다(v7과 같음).
   - 색 질의: **첫 응답만 통과**, 다른 클라이언트의 응답은 중복으로 버린다. **결정: holder가 팔레트로 직접 답하지 않는다.** 색의 정답은 사용자가 보고 있는 UI 테마이고 holder는 그것을 모른다(설정 팔레트는 앱 테마·`pod attach`가 붙은 Windows Terminal의 테마와 어긋나 밝은/어두운 판단을 틀리게 만든다). 붙은 클라이언트가 없으면 아무도 답하지 않는 것은 실제 터미널 없는 PTY와 같다(프로그램은 시간 초과 후 기본값을 쓴다). 테마가 바뀌어도 클라이언트가 계속 맞는 값을 준다.
   - **늦은 응답 차단(모든 클라이언트)**: 질의 뒤 응답 창(기본 500 ms, `podd --reply-window <ms>`, 환경 변수 `POD_REPLY_WINDOW_MS`, `0`이면 끔)보다 늦게 온 응답, 그리고 기다리는 질의가 없는 응답은 버린다. 프로그램은 이미 기다림을 끝냈으므로 그대로 보내면 키 입력이 된다(docs/12 B4: Codex 입력칸의 `]10;rgb:…\`). 시간은 데몬이 출력에서 질의를 본 순간부터 데몬이 입력을 받은 순간까지다(클라이언트 왕복 포함). 질의는 5 s 동안 기억한다.
   - 키와 겹치는 모양: `CSI r;c R`(CPR)은 수정자 F3(`CSI 1;5R`)와 같으므로 CPR 질의가 기다리는 중일 때만 응답으로 본다(그 밖에는 키로 통과). DA(`CSI ? … c`, `CSI > … c`), `CSI 0 n`, `CSI ? r;c R`, OSC 응답은 사람이 입력하는 모양이 아니다. 색 *설정*(`OSC 10;#fff`가 아니라 입력 쪽 `OSC 10;?`)이나 마우스·포커스·win32-input-mode 키는 응답 모양이 아니어서 그대로 간다. 한계: bracketed paste 안에 응답 모양의 바이트열이 있으면 지워진다. 응답이 두 `terminal.input`으로 나뉘면 여전히 통과한다(끝의 단독 ESC는 Esc 키라서 붙잡아 둘 수 없다).
   - 결과: `terminal.input`의 `SendResult.dropped_replies = {duplicate, late}`(뭔가 지웠을 때만, v9). 데몬 로그에도 남는다.
   - 테스트: `query_dedup::tests`(분할 경계 전부에서 색 질의 포함 스캔, 색 응답 첫째만 통과, 늦은/질의 없는 응답 버림, 정확히 500 ms는 통과, 창 0이면 예전 동작, F3·Alt+]·불완전 시퀀스는 키), `tests/gaps.rs::attach_query_replies_are_not_duplicated`(번들 ConPTY의 실제 holder + fake-agent가 DA1과 OSC 11을 묻는다: 붙은 클라이언트의 DA1 응답 = 중복, 첫 OSC 11 응답은 프로그램에 도착, 질의 없는 두 번째 DA1·색 응답 = late로 버려짐, 사이의 글자는 도착).

스냅샷 재현 범위는 그리드의 문자, SGR(색 16/256/24비트, 굵게·흐리게·기울임·밑줄·반전·숨김·취소선), 소프트 랩, 전각 문자, 커서 위치·표시 여부, 입력 관련 모드다(앱 커서·키패드, bracketed paste, 포커스, 마우스 1000/1002/1003/1005/1006, 삽입, autowrap 끔). v7부터 대체 화면이면 **주 화면(스크롤백 포함)과 그 커서를 먼저 쓰고** `?1049h` 뒤에 대체 화면을 쓴다. 그래서 프로그램이 나중에 `?1049l`로 나오면 원래 주 화면이 보인다(alacritty는 비활성 그리드를 숨기므로 스냅샷을 찍는 순간 그리드를 잠깐 바꿔 읽고 되돌린다. `snapshot_ansi`가 `&mut`인 이유). DECSTBM 스크롤 영역과 origin 모드(`?6h`, 커서는 영역 기준으로 계산)도 재현한다(영역은 바이트 스트림에서 따로 추적: `CSI t;b r`, `ESC c`·`CSI ! p`·리사이즈에서 초기화, 잘못된 영역은 무시 — alacritty와 같은 규칙). 창 제목과 OSC 7501 보고는 전부터 `osc_replay`로 다시 보낸다. **여전히 재현하지 않는 것**: 탭 위치, 문자셋(G0–G3, DEC 선 그리기), pending-wrap 상태, `?1049`가 지키는 것 외의 DECSC 저장 커서, 커서 모양, 팔레트(OSC 4/10/11), 하이퍼링크(OSC 8), kitty 키보드 모드.

### 4.2 `output_contains`와 입력 에코

`terminal.send`로 보낸 텍스트(줄 단위, 최근 32개)를 기억한다. 화면·스크롤백의 줄에서 **바늘을 포함한 보낸 텍스트 부분을 지운 뒤에도** 바늘이 남아 있어야 일치로 본다. 예를 들어 `send "echo pod-ok"` 뒤 `wait pod-ok`는 `PS> echo pod-ok` 줄에서는 맞지 않고 출력 줄 `pod-ok`에서 맞는다. 테스트는 wait 직후 read에 출력 줄이 이미 있는지 확인한다. 한계: TUI 입력 상자에 테두리와 함께 여러 줄로 감긴 에코는 걸러내지 못한다. 이전에 이미 출력된 같은 문자열도 일치한다.

### 4.3 수명 이벤트와 `task.touch` (v9)

폴링 대신 이벤트를 받는 길. 목록 호출(`terminal.list`, `task.list`)은 그대로 있다.

- **구독**: `status.subscribe {ids?, lifecycle: true}`. `lifecycle`은 선택(기본 false)이라 예전 클라이언트는 새 알림을 보지 않는다. 순번은 `status.changed`와 같은 데몬 전체 `seq`다. 느린 구독자는 기존처럼 `status.lagged {missed}`를 받고 목록으로 다시 맞춘다. `ids`를 주면 그 터미널의 `terminal.changed`만 오고 task 알림은 오지 않는다.
- `terminal.changed {seq, id, change, terminal?}`: `change` = `created`(`terminal.run`, task 안 포함) · `exited`(프로그램 종료 또는 holder 유실, `terminal`에 종료 코드) · `removed`(`terminal.rm`, task 삭제, 보존 정책. 그 id의 마지막 알림, `terminal` 없음) · `renamed`(예약: 지금 podd에는 이름을 바꾸는 메서드가 없다). 상태 변화는 계속 `status.changed`다.
- `task.changed {seq, id, change, task?, status?, git?, terminals?, paths?}`: 바뀐 것만 담는다(나머지는 `task.show`).
  - `created`: `task.create` 완료, `task`에 전체 `TaskInfo`.
  - `removed`: `task.remove` 완료(마지막 알림).
  - `status`: 집계 상태(`TaskStatus`)가 바뀜. 태스크 터미널의 `status.changed`마다 git 없이 다시 계산한다. `status`가 없으면 상태 있는 터미널이 없어진 것.
  - `git`: ahead/behind/dirty/unpushed/HEAD가 바뀜, `git`에 새 값. 처음 본 값은 기준이라 알림이 없다. 다시 계산하는 때: 태스크 터미널의 상태가 바뀐 뒤(에이전트 턴이 보통 파일을 바꾸거나 커밋한다), `task.touch`, 그리고 lifecycle 구독자가 하나라도 있으면 5 s마다(Pod 밖의 편집). 구독자가 없으면 폴링하지 않는다. git은 250 ms마다 모아서 한 태스크씩 돈다.
  - `terminals`: 태스크의 터미널 목록이 바뀜(새 목록).
  - `touched`: 클라이언트가 파일을 저장했다(`task.touch`). 개수가 같아도 diff 내용은 바뀌었을 수 있으니 리뷰·diff 뷰는 새로 고친다. `paths`는 클라이언트가 준 그대로.
  - `renamed`: 예약(태스크 이름은 브랜치·워크트리 경로를 정하므로 바꾸는 메서드가 없다).
- `task.removing {seq, id, worktree, force, grace_ms}`: `task.remove`가 검사(dirty/unpushed)를 통과하고 터미널을 죽이고 워크트리를 지우기 **직전**. 편집기는 그 워크트리의 파일을 저장하거나 닫는다. lifecycle 구독자가 있으면 데몬이 `grace_ms`(기본 300 ms, `POD_TASK_REMOVE_GRACE_MS`, 최대 10 s, 구독자가 없으면 0) 기다린 뒤, force가 아니면 **다시 검사한다**: 편집기가 그 사이 저장해서 dirty가 되면 `TASK_DIRTY`로 멈추고 워크트리는 남는다(이후 `removed`는 오지 않는다). `TaskRemoveParams`에는 필드를 더하지 않았다(pod-app이 구조체를 직접 만든다).
- `task.touch {task, paths?}` → `{task, git?, git_error?, git_changed}`: 클라이언트가 워크트리에 파일을 저장했다고 알린다. 데몬은 git 사실을 바로 다시 계산하고(바뀌었으면 `task.changed git`), 이어서 `task.changed touched`를 보낸다. CLI: `pod task touch <task> [paths…]`.
- Rust 클라이언트: `podd::client::SubscribeEvent::{Terminal, Task, TaskRemoving}`(enum은 `non_exhaustive`).
- 코드: `server/lifecycle.rs`(태스크별 마지막 상태·git 값과 워커), 발행 지점은 `server/mod.rs`(run/rm), `server/terminal.rs`(종료), `server/tasks.rs`(create/remove/터미널 목록).
- 테스트: `tests/lifecycle.rs::lifecycle_events_and_task_touch`(실제 podd/holder, 임시 git 저장소: 터미널 created→exited(코드 3)→removed와 seq 증가, task created(TaskInfo 포함), 파일 저장 + `task.touch` → `git_changed`, `git`(dirty 1) 다음 `touched`(paths), 변화 없는 touch는 `touched`만, Pod 밖 커밋은 폴링으로 `git`(ahead 1, dirty 0), 태스크 안 `terminal.run` → `terminals`, `task.removing` 뒤 grace 동안 저장 → `TASK_DIRTY`이고 파일 유지, force 삭제 → `removing`·터미널 `removed`·task `removed`, `lifecycle` 없는 구독자는 새 알림을 하나도 받지 않음), `lifecycle::tests`(git 기준값·변화, 상태 변화 판정).

**pod-app이 폴링을 줄이는 방법(pod-app 후속, 이번 작업에서 pod-app은 고치지 않음)**: 지금 pod-app은 0.5 s마다 `terminal.list`, 3 s마다 `task.list`(git 계산 포함)를 부른다. (1) 상태 연결의 `status.subscribe`에 `lifecycle: true`를 더하고 `SubscribeEvent::Terminal/Task/TaskRemoving`을 처리한다. (2) 시작할 때와 `status.lagged`·재연결·`epoch` 변화 때만 `terminal.list`/`task.list`를 부르고, 이후는 이벤트로 모델을 고친다(`terminal.changed`의 `terminal`, `task.changed created`의 `task`, `status`/`git`/`terminals` 필드). 안전망으로 목록 폴링을 30–60 s로 늘려 남겨도 된다. (3) 편집기가 저장하면 `task.touch {task, paths}`를 부르고, 리뷰·diff 패널은 `touched`/`git`에서 `task.diff`를 다시 부른다. (4) `task.removing`을 받으면 그 워크트리의 편집기 버퍼를 저장/닫고 LSP를 멈춘다(앱 자신의 삭제 대화는 그대로). (5) 앱의 `ReplyFilter`(500 ms)는 데몬 쪽 차단과 겹쳐도 해가 없다. 붙은 패널이 여럿이어도 색 응답은 데몬이 하나만 통과시킨다. (6) `SendResult.dropped_replies`는 진단용(무시해도 됨).

## 5. ConPTY와 OSC 7501 (docs/04 §1.4의 해결)

**결론**: Windows 기본(inbox) ConPTY에서는 OSC 7501 응답이 입력 경로에서 버려진다. Microsoft의 최신 ConPTY(`conpty.dll` + `OpenConsole.exe`)를 쓰면 응답이 통과하고 Claude가 상태 보고를 켠다. Pod는 번들 ConPTY를 기본으로 쓴다.

- **응답 주체**: holder가 출력 스트림에서 `ESC]7501;?`(BEL 또는 ST 종결, 청크 경계를 넘어도 됨)를 찾는다. 찾으면 그 위치까지만 VT에 먹이고 곧바로 `ESC]7501;?:states=idle,working,done,blocked,error:kinds=permission,question,auth ESC\`를 입력으로 쓴다. 그다음 나머지를 먹인다. 그래서 응답이 **같은 청크의 DA1 응답보다 먼저** 나간다(Claude는 DA1 응답을 "응답 없음" 판정 경계로 쓴다). 보고(`state=...`)는 해석하지 않고 원시 스트림 그대로 attach 클라이언트에 전달한다(파서는 pod-status 크레이트 담당).
- **번들 ConPTY**: NuGet `Microsoft.Windows.Console.ConPTY` 1.25.260930003(MIT)을 쓴다. `tools/fetch-conpty.ps1`이 고정 버전·SHA-256으로 받아 `vendor/conpty/`(git 무시)에 둔다. `build.rs`가 이를 `target/<profile>/`로 복사한다(`conpty.dll`, `x64\OpenConsole.exe`, 패키지 배치와 같음). `pty.rs`는 실행 파일 옆의 `conpty.dll`을 `LoadLibraryExW`로 읽어 `ConptyCreatePseudoConsole/Resize/Close`를 쓰고, 없으면 kernel32의 inbox 함수를 쓴다. `POD_CONPTY=inbox`는 inbox를 강제하고, `POD_CONPTY=<dir>`는 디렉터리를 지정한다. 어느 쪽인지는 `TerminalInfo.conpty`에 나온다. 두 바이너리 모두 Microsoft Authenticode 서명이 유효함을 확인했다.
- **증거 1: Claude Code 2.1.319, 신뢰된 폴더, 키 입력 없음.** `pod run --name c -- claude` 직후 `tap c --only-osc`로 원시 스트림을 본 결과:
  ```
  번들:  0.971 OSC \x1b]7501;?\x07
         1.071 OSC \x1b]7501;state=idle:app=claude-code\x07      ← 상태 보고 켜짐
  inbox: 1.691 OSC \x1b]7501;?\x07
         (12초 동안 7501 보고 없음)
  ```
- **증거 2: 원인 프로브.** PowerShell이 `ESC]7501;? BEL` + `ESC[c`(DA1)를 출력한 뒤 콘솔 입력으로 받은 것:
  ```
  번들:  <ESC>]7501;?:states=idle,...:kinds=permission,question,auth<ESC>\<ESC>[?6c     ← holder 응답 둘 다 순서대로 도착
  inbox: <ESC>[?61;6;7;21;22;23;24;28;32;42c                                          ← 7501 응답 없음, DA1은 conhost가 직접 응답
  ```
  inbox conhost는 입력 파이프로 들어온 OSC 응답을 버리고 DA1에는 자기가 직접 응답한다. 그래서 Claude는 7501 응답 없이 DA1 응답만 받고 보고를 끈다. 즉 win32-input-mode나 DA1 누락 문제가 아니라 **inbox conhost의 입력 파서가 OSC를 통과시키지 않는 것**이 원인이다.

## 6. 검증 결과

- 단위 테스트(podd 18, pod-cli 4): 프레임 왕복, 스냅샷 재현(색·랩·전각·스크롤백 제한·모드·대체 화면), OSC 7501 질의 스캐너(청크 분할·ST), 환경 필터, 키 매핑, 인자 인용·`.cmd` 해석, SID·DACL, 레지스트리 환경.
- 통합 테스트(`tests/daemon.rs`): 번들/inbox 두 ConPTY 모두에서 통과.
  1. `cmd /c echo hello` 종료 코드·출력; PowerShell send→wait→read(에코 필터); 환경 정리(`NO_COLOR`·`CLAUDECODE`·부모 IDE 변수를 넣고 띄운 데몬에서 터미널 안 값이 비어 있음, `POD_*`·`TERM` 설정됨); keys; 타임아웃; rm 거부/kill/rm 후 holder 종료; 오류 코드
  2. **데몬 재시작 생존**: stop(기본) → holder·셸 생존 → 새 데몬이 같은 pid로 재발견, `read --history`에 이전 출력, send/wait 동작, 새 id는 t2
  3. **attach**: 스냅샷으로 만든 클라이언트 VT + 라이브 바이트 768청크의 seq 연속성, `live 1..300`이 정확히 한 번씩, 리사이즈 알림이 스트림 순서대로, 최종 화면이 데몬과 같음, kill 시 exited 알림의 seq가 일치
  4. **트리 kill**: 숨은 별도 콘솔로 띄운 손자 `ping`까지 kill 후 사라짐
  5. 두 번째 데몬 거부(락), 6. JSON-RPC 오류 코드(없는 프로그램은 `SPAWN_FAILED`)
- 수동 데모(release, Claude Code): `pod run --name c -- claude` → `read c`로 시작 화면 → `server stop` → 상태 "not running"(holder 생존) → `pod ls`가 새 데몬을 자동 시작해 t1 재발견(같은 Claude pid) → `read c`로 같은 화면 → 다른 pod 터미널 안에서 `pod attach c` 실행: 화면 렌더링, 뷰어 콘솔을 90x24로 바꾸자 c도 따라 바뀌고 Claude가 다시 그린 바이트가 라이브로 반영 → `ctrl+]`로 detach("[pod: detached from t1]") → `kill c`, `rm`, Claude·holder 프로세스 없음 → `server stop`. Claude에는 프롬프트를 한 번도 입력하지 않았다.

### 6.1 cargo-deny (`deny.toml`, cargo-deny 0.20.2)

- 라이선스 허용 목록: Apache-2.0(+LLVM-exception), MIT, MIT-0, BSD-2/3, ISC, Zlib, 0BSD, BSL-1.0, CC0-1.0, Unlicense, Unicode-3.0. GPL·AGPL·LGPL은 목록에 없으므로 거부된다. 출처는 crates.io만 허용하고 git 의존은 금지한다.
- 예외 1건: `option-ext`(MPL-2.0, 약한 copyleft). pod-spike의 UI 스택에서만 빌드 시 쓰인다(`gpui-kit-assets → rust-embed → shellexpand → dirs → dirs-sys`). 수정 없이 쓰므로 Apache-2.0 작품에 포함해도 된다. UI 크레이트를 제품화할 때 다시 검토한다.
- 결과: `cargo deny check` → `advisories ok, bans ok, licenses ok, sources ok`.
- advisories 정책: 취약점은 깊이와 상관없이 실패로 처리한다. "unmaintained"는 워크스페이스가 직접 의존하는 크레이트일 때만 실패로 처리한다(`unmaintained = "workspace"`). 이 정책 전의 엄격한 실행(`all`)에서는 **advisories FAILED**였다: instant, paste, rustybuzz, ttf-parser의 unmaintained 4건이며, 모두 pod-spike의 GPUI 의존(gpui-kit/gpui-pre)에서만 온다. podd·pod-cli만 대상으로 하면(`--exclude pod-spike`) 엄격 정책으로도 전부 통과한다.

## 7. 알려진 한계

1. ~~holder는 release될 때까지 남는다.~~ **고침(v6, podd-hardening)**: 종료 터미널 보존 정책. 종료 후 `podd --retain-exited <기간>`(기본 `24h`, 환경 변수 `POD_RETAIN_EXITED`, `off`면 끔)이 지나면 데몬이 `terminal.rm`과 같은 경로로 지운다(holder release). 예외: `terminal.keep`/`pod keep <t>`로 보존한 터미널, Task에 속한 터미널(Task 삭제가 지운다). `terminal.list`에 `retention_ms`와 터미널별 `retention: [{id, kept, expires_ms?}]`가 붙는다(`TerminalInfo`는 바꾸지 않았다: 구조체를 리터럴로 만드는 클라이언트가 있다). `pod ls`는 실행 중인 터미널만 보이고 숨긴 수를 알려 준다. `pod ls --all`은 전부. 남은 한계: 데몬이 없으면 아무것도 지워지지 않는다(다음 데몬 시작 때 지난 것을 지운다). 테스트: `tests/retention.rs`.
2. ~~attach 중 질의 응답 중복~~ **고침(v7, §4.1-5)**: attach 입력에서 holder가 이미 답한 질의의 응답을 클라이언트 수만큼 지운다. 테스트: `server::query_dedup` 단위 테스트(모든 2분할 청크 경계, 클라이언트당 1회, 만료, 키는 통과), `tests/gaps.rs::attach_query_replies_are_not_duplicated`(실제 holder + fake-agent가 DA1을 묻고, 붙은 클라이언트의 응답은 버려지고 두 번째 응답·텍스트는 프로그램에 도착).
3. ~~attach 구독 큐에 상한이 없다~~ **고침(v7, §4.1-2)**: 클라이언트별 상한 + `terminal.lagged` + 재접속. 테스트: `tests/gaps.rs::slow_attach_client_is_dropped_and_can_reattach`(16 KiB 상한, 읽지 않는 클라이언트에 fake-agent `flood:6000`으로 약 300 KB 출력 → 받은 출력은 연속이고 전체보다 적음, `lagged` 뒤 스트림 종료, `attached = 0`, 다시 attach한 스냅샷에 끝 줄이 있음. e2e-fix1에서 64 KiB/PowerShell 2만 줄 루프(약 28 s, 부하 시 시간 초과)를 바꿔 약 2 s가 되었고, 스트림은 스레드에서 하나의 마감 시각으로 읽는다).
4. ~~스냅샷은 대체 화면 뒤의 주 화면, 창 제목, 스크롤 영역 등을 재현하지 않는다~~ **대부분 고침(v7, §4.1)**: 주 화면·스크롤 영역·origin 모드 추가(창 제목은 이미 재생). 남은 손실은 §4.1 끝의 목록. 테스트: `vt::tests::alt_screen_snapshot_keeps_main_screen`(스냅샷이 원본을 바꾸지 않음, 같은 스냅샷 재현, `?1049l` 뒤 주 화면·커서·색이 원본과 같음), `vt::tests::scroll_region_survives_snapshot`.
5. 에코 필터 한계(§4.2). VT 미러가 데몬과 holder 양쪽에 있어 파싱이 두 번 일어난다(CPU 비용, 큰 문제는 아님).
6. `.cmd` shim은 `cmd /c`를 거치므로 `& | < > ^`가 든 인자는 cmd가 해석한다.
7. 번들 ConPTY는 개발 빌드에서 `tools/fetch-conpty.ps1` + `build.rs` 복사로 배치한다. 배포 패키지(설치 관리자)에 넣는 단계는 아직 없다. x64만 검증했다.
8. ~~데몬과 holder가 같은 버전이라고 가정한다~~ **고침(holder 프로토콜 2, §3)**: 버전 교환과 `incompatible_holder`. 테스트: `holder_proto::tests::foreign_hello_and_version_check`, `tests/gaps.rs::holder_versions_are_negotiated`(테스트가 연 가짜 holder 파이프 두 개: 버전 99는 incompatible로 목록·화면에 나오고 입력은 `TERMINAL_EXITED`, `rm`은 force가 필요하고 force면 holder 프로세스가 끝나고 기록이 지워짐. 버전 1은 실행 중 터미널로 동작하고 입력을 받으며 `DaemonHello`를 받지 않음. 새 id는 재발견한 id 다음부터). 실제 holder가 데몬 버전을 받는 것은 `pod_run_inside_a_terminal_records_its_parent`가 holder 로그로 확인한다.

### 7.1 podd-hardening (프로토콜 v6) 정리

고친 것: 자동 시작 핸들 상속(§2), 종료 터미널 보존 정책(한계 1), `group.check` 백그라운드 작업·리뷰 큐 지연·게이트 만료([11](11-review-and-orchestration-backend.md) §6), 상태 turn·훅 복원([07](07-pod-status.md) §8.5), pod-hook 종료 경로와 시간 예산 테스트([07](07-pod-status.md) §5). v6 변경은 모두 추가형이다(`TerminalInfo`는 그대로 두고 보존 정보는 `ListResult.retention`에 넣었다).

일부러 남긴 것(위험 대비 이득이 작거나 동작 결정이 필요함):
- 한계 2(attach 질의 응답 중복), 3(attach 구독 큐 상한): 상한을 넘을 때 끊으면 pod-app의 재연결 경로를 함께 봐야 한다(pod-app은 이번 범위 밖).
- 한계 8(holder 버전 협상), 07의 "hook blocked 뒤 Esc로 취소 시 최대 120 s"(엔진 우선순위 규칙 변경), 08 §7-1(unpushed 규칙 완화), 11 §6-3(큐 타임아웃): 동작 정책 결정이 필요하다.
- 09 §5-4(제어 연결 하나에 명령이 줄 섬): 클라이언트 쪽 문제다. 데몬은 이미 연결마다 독립적으로 처리하고, 이번에 `group.check`도 연결을 막지 않게 됐다.

### 7.2 podd-gaps2 (프로토콜 v7) 정리

고친 것(모두 추가형, 프로토콜 번호는 한 번만 올림: 6 → 7):
1. **일반 터미널의 부모** (09 §8.6-1): `RunParams.parent_terminal`, `TerminalInfo.parent_terminal/parent_task`(§4). 테스트: `tests/gaps.rs::pod_run_inside_a_terminal_records_its_parent`(PowerShell 터미널 안에서 실제 `pod run` → 부모 기록, API로 아는/모르는 부모, **데몬 재시작 뒤에도 유지**, JSON에서 없는 선택 필드는 생략), `tests/orchestration.rs::run_from_a_task_terminal_records_parent_task`.
2. **attach 응답 중복 제거와 큐 상한** (한계 2, 3).
3. **스냅샷 충실도** (한계 4).
4. **holder 버전 협상** (한계 8).
5. **리뷰 큐 타임아웃** ([11](11-review-and-orchestration-backend.md) §6-3).
6. **훅 blocked 120 s** ([07](07-pod-status.md) §7): 같은 에이전트의 OSC 보고가 턴 끝으로 바뀌면 바로 풀린다.

검증(2026-11-09, 이 worktree의 `target`, sccache, jobs 4, 번들 ConPTY): `cargo build --release` 통과, `cargo test --workspace` 30개 묶음 244개 통과·실패 0, `cargo clippy -p podd -p pod-cli -p pod-status --all-targets` 경고 0, `cargo deny check` → `advisories ok, bans ok, licenses ok, sources ok`. 기존 `tests/status.rs::hooks_authenticated_and_outrank_osc`는 예전 규칙(훅 blocked가 새 OSC idle보다 우선)을 확인하던 단계를 새 규칙으로 바꿨다. 실제 Claude/Codex 턴은 돌리지 않았다.

일부러 남긴 것:
- 질의 응답 중복 제거는 응답이 한 `terminal.input` 안에서 완결될 때만 동작한다(Windows 콘솔 입력은 보통 한 번에 온다). 색 질의(OSC 10/11)는 holder가 답하지 않으므로 여전히 pod-app 패널이 둘이면 두 번 답한다(09 §5-2, holder가 색 질의에 답하게 하는 것은 팔레트 결정이 필요).
- `terminal.lagged`를 pod-app은 아직 모른다(pod-app은 이번 범위 밖). 연결이 닫히면 pod-app의 기존 재연결 경로를 탄다. 기본 상한 16 MiB는 보통의 UI에서는 닿지 않는다.
- 스냅샷의 남은 손실(§4.1 끝). 탭 위치·문자셋은 alacritty가 공개 API로 내주지 않아 스트림 추적이 필요하고, TUI는 보통 매번 다시 설정하므로 미뤘다.
- holder 2는 데몬 버전을 기록만 한다. 앞으로 프레임이 바뀌면 이 값으로 holder 쪽 분기를 넣는다. 지원 범위 밖 holder를 "이어 받는"(업그레이드) 경로는 없다(사용자가 그 버전의 podd를 쓰거나 `rm --force`).
- 08 §7-1(unpushed 규칙 완화): 동작 정책 결정이 필요해 그대로 둔다.
- 09 §5-4(제어 연결 하나에 명령이 줄 섬): 클라이언트 쪽 문제(그대로).

### 7.3 fu-daemon (프로토콜 v9) 정리

고친 것(모두 추가형, 프로토콜 번호는 한 번만 올림: 8 → 9):
1. **색 질의 응답 중복과 늦은 응답** (09 §9 "podd 후속" 1–2, 12 B4): §4.1-6. 결정과 이유도 거기에.
2. **폴링 대신 이벤트**: `terminal.changed`, `task.changed`, `task.removing`, `task.touch`(§4.3). pod-app이 폴링을 줄이는 방법은 §4.3 끝(pod-app 후속).
3. **`pod hooks uninstall`/`status`** ([07 §9.1](07-pod-status.md)): 제거는 이미 있었다. 이번에 Codex `config.toml`의 `[hooks.state]`에서 Pod 훅의 신뢰 항목도 지우고(백업 후, 다른 도구 항목은 위치가 바뀌면 새 위치로 옮김), `hooks status --json`이 설치된 `pod-hook.exe` 경로·버전과 이 빌드의 원본 경로·버전을 보인다(`pod-hook --version` 추가).
4. §7의 남은 항목 다시 보기: 색 질의는 1로 해결. 나머지(응답이 두 `terminal.input`으로 나뉘는 경우, 스냅샷의 탭 위치·문자셋, holder 업그레이드 경로, 08 §7-1, 09 §5-4)는 싸지 않거나 정책 결정이 필요해 그대로 둔다. 나뉜 응답은 끝의 단독 ESC가 Esc 키와 구별되지 않아 입력을 붙잡아 두면 Esc가 늦어진다.

pod-app 후속(이 작업은 pod-app을 고치지 않음): §4.3 끝의 (1)–(6), 그리고 `terminal.lagged` 처리(§7.2 그대로).

## 8. 다음 단계

- **UI attach(P2)**: pod-app은 `terminal.attach` 스트림을 그대로 쓴다. 비동기(tokio) 클라이언트를 추가하고, 스냅샷 + 라이브 바이트를 앱의 VT(ghostty-vt 전환 시에도 같은 경계)에 먹인다. 다중 attach와 크기 정책은 §4.1 그대로다.
- ~~**이벤트 구독**: `events.subscribe`(created/exited/resized/title + P1 상태 엔진의 7501 보고 → 상태 변화).~~ 상태는 `status.subscribe`(P1), 수명은 v9 `lifecycle`(§4.3). 남은 것: resized/title 알림(attach 스트림에는 resized가 있다).
- ~~attach 응답 중복 필터(한계 2), 구독 백프레셔(한계 3)~~: v7에서 구현(§7.2). (holder 정리 정책은 v6, §7.1.)
- Task = worktree + 터미널: 완료(P3 백엔드, [08](08-tasks.md)). `task.create`가 호출자의 `POD_TERMINAL_ID`를 부모로 기록한다.
- 부모-자식 트리 조회(`wait --children`, Task 트리), `terminal.label`, stitched read. (부모 기록 자체는 v7: `TerminalInfo.parent_terminal`, `terminal.keep`은 v6.)
- 번들 ConPTY의 배포 패키징·서명 확인 자동화, arm64.
