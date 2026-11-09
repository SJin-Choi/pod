# Contributing to Pod

감사합니다! Pod는 pre-alpha라 구조가 자주 바뀝니다. 큰 변경은 먼저 이슈로 의견을 나눠 주세요.
(English is welcome in issues and pull requests.)

## 시작하기

- Windows 10/11 x64, Rust stable (msvc toolchain), Git.
- 번들 ConPTY: `powershell -ExecutionPolicy Bypass -File tools/fetch-conpty.ps1`
- 빌드: `cargo build --workspace`, 테스트: `cargo test --workspace`
- 설계 배경은 [docs/01-plan.md](docs/01-plan.md)와 각 크레이트의 문서(docs/03–14)를 보세요.

## Pull request 전에

1. `cargo fmt --all` 과 `cargo clippy --workspace --all-targets -- -D warnings`
2. `cargo test --workspace`
3. 의존성을 추가했다면 `cargo deny check` — Apache-2.0/MIT 호환 라이선스만 허용합니다(GPL/AGPL 불가).
4. UI를 바꿨다면 바뀐 화면의 스크린샷을 PR에 붙여 주세요(가능하면 다크·라이트, 100%·150% 배율).
5. 동작이나 프로토콜을 바꿨다면 관련 `docs/` 문서도 고쳐 주세요.

## 원칙

- **클린룸**: 다른 제품의 코드·에셋(아이콘, 이미지, CSS)을 복사하지 마세요. 공개 규격과 공개 문서만 참고합니다.
- 사용자 설정(`~/.claude`, `~/.codex` 등)을 쓰는 코드는 반드시 백업과 dry-run 경로를 둡니다. 테스트는 임시 디렉터리만 사용합니다.
- 커밋 메시지는 무엇을 왜 바꿨는지 짧게 적어 주세요.

## 라이선스

기여한 코드는 저장소와 같은 [Apache License 2.0](LICENSE)으로 배포됩니다(Apache-2.0 §5).
