# redcode

A macOS desktop workbench written in Rust and rendered with [GPUI](https://github.com/zed-industries/zed/tree/main/crates/gpui): terminal emulator, AI coding agent console, GitKraken-style Git graph and a code editor in one window.

Rust로 작성하고 GPUI로 그리는 macOS용 개발 워크벤치입니다. 터미널, AI 코딩 에이전트, Git 그래프, 파일 편집기를 한 창에서 탭과 분할로 다룹니다.

> 상태: 초기 버전(0.1). macOS(Apple Silicon)에서만 빌드·테스트했습니다. UI 문구는 대부분 한국어입니다.

## 기능

- **터미널**: [`alacritty_terminal`](https://crates.io/crates/alacritty_terminal) 기반. 256색/트루컬러, 스크롤백, 마우스 선택·복사, bracketed paste, 마우스 리포트(vim/htop), 한글 IME 조합, 검색(Cmd+F), 새 탭의 작업 디렉터리 승계.
- **탭·분할**: 탭 드래그로 재배치·분할, Cmd+D / Cmd+Shift+D 분할.
- **명령 팔레트**: Cmd+Shift+P. 모든 기능을 이름으로 실행.
- **파일**: 좌측 파일 트리(.gitignore 준수), 구문 강조 에디터, Cmd+P 파일 찾기, 외부 변경 감지.
- **Git**: 커밋 그래프(레인·머지·ref 배지), 커밋 상세와 파일별 diff, stage/unstage/commit/amend, 브랜치 생성·체크아웃·머지·삭제, 충돌 표시와 Abort Merge, fetch/pull/push, stash. 시스템 `git` CLI를 그대로 사용합니다.
- **AI 에이전트**:
  - **ACP**(기본): [Agent Client Protocol](https://agentclientprotocol.com) 에이전트를 자식 프로세스로 실행합니다. 기본값은 Claude Code 어댑터이며, 설정만 바꾸면 Codex·Gemini 등도 쓸 수 있습니다.
  - **Native**: Anthropic Messages API를 직접 호출하고 자체 도구 루프(파일 읽기/검색/쓰기/편집, 셸 명령)를 돌립니다. 쓰기·명령 실행은 승인 후 실행됩니다.

## 요구 사항

- macOS (Apple Silicon에서 검증)
- [rustup](https://rustup.rs) — `rust-toolchain.toml`에 고정된 Rust 1.98.1을 자동으로 설치합니다.
- Xcode Command Line Tools
- `git`
- ACP 기본 에이전트를 쓰려면: Node.js 22 이상(`npx`)과 Claude Code 로그인

## 설치와 실행

```sh
git clone https://github.com/yoko57822/redcode
cd redcode
cargo install --path . --locked   # ~/.cargo/bin/redcode

redcode                  # 현재 디렉터리를 워크스페이스로 열기
redcode ~/some/project   # 지정한 폴더 열기
redcode path/to/file.rs  # 파일의 폴더를 열고 그 파일을 에디터로
```

개발 중에는 `cargo run -- <경로>`.

## 단축키

| 키 | 동작 |
|:---|:---|
| Cmd+Shift+P | 명령 팔레트 |
| Cmd+T | 새 터미널 탭 |
| Cmd+D / Cmd+Shift+D | 오른쪽 / 아래로 분할 |
| Cmd+W | 탭 닫기 |
| Cmd+} / Cmd+{ | 다음 / 이전 탭 |
| Cmd+B | 사이드바 토글 |
| Cmd+Shift+E / Ctrl+Shift+G | 파일 / Git 패널로 이동 |
| Cmd+P | 파일 찾기 |
| Cmd+S | 저장 |
| Cmd+Shift+G | Git 그래프 |
| Cmd+Shift+A | 에이전트 스레드 |
| Cmd+, | 설정 파일 열기 |
| Cmd+= / Cmd+- / Cmd+0 | 글꼴 확대 / 축소 / 원래대로 |
| 터미널: Cmd+C / Cmd+V / Cmd+K / Cmd+F | 복사 / 붙여넣기 / 지우기 / 검색 |
| 에이전트: Esc | 실행 중인 턴 취소 |

## 설정

`~/.config/redcode/settings.toml` (첫 실행 때 기본값으로 생성). redcode 안에서 저장하면 즉시 적용됩니다.

```toml
[appearance]
theme = "system"        # "system" | "dark" | "light"
font_family = "Menlo"
font_size = 13.0
line_height = 1.3

[terminal]
shell = ""              # 비우면 $SHELL, 없으면 /bin/zsh
shell_args = ["-l"]
scrollback_lines = 10000
cursor_blink = true
option_as_meta = true
copy_on_select = false

[editor]
tab_size = 4
soft_wrap = false

[agent]
backend = "acp"         # "acp" | "native"

[agent.acp]
# /bin/zsh -lc "exec <command>" 로 실행됩니다.
command = "npx -y @agentclientprotocol/claude-agent-acp@0.106.0"
# 예: Codex      "npx -y @agentclientprotocol/codex-acp@2.6.0"
#     Gemini CLI "npx -y @google/gemini-cli@0.68.0 --acp"

[agent.acp.env]

[agent.native]
model = "claude-opus-5-5"
max_tokens = 32000
thinking = "adaptive"   # "adaptive" | "off"
effort = ""             # 비우면 모델 기본값
base_url = "https://api.anthropic.com"
api_key_env = "ANTHROPIC_API_KEY"
```

Native 백엔드의 API 키는 `api_key_env` 환경변수에서 먼저 찾고, 없으면 macOS 키체인에서 찾습니다. 키체인에는 명령 팔레트의 **Agent: Set Anthropic API Key**로 저장합니다.

## 알려진 제한

- macOS 전용입니다(작업 디렉터리 승계에 `proc_pidinfo` 사용, 다른 플랫폼은 미검증).
- 에이전트: 대화 저장·복원, 이미지·파일 첨부, 슬래시 명령은 지원하지 않습니다. ACP `authenticate` 흐름이 없으므로 에이전트 CLI에 미리 로그인해 두어야 합니다.
- 에디터: LSP, 자동완성, 새 파일/이름 변경/삭제 UI가 없습니다.
- Git: 자격증명 프롬프트를 띄우지 않습니다(`GIT_TERMINAL_PROMPT=0`). 인증이 필요한 원격은 credential helper나 SSH 키를 미리 설정하세요.
- 키맵 사용자화, 세션·레이아웃 복원, `.app` 번들은 아직 없습니다.
- 원격이 있는 pull/push와 Native 백엔드의 실제 API 대화는 개발 중 충분히 검증되지 않았습니다.
- GPUI(`gpui-pre`)는 안정 API가 없어 정확한 버전으로 고정되어 있습니다.

## 개발

```sh
cargo test
cargo clippy --all-targets
cargo test -- --ignored acp_smoke_ready   # 네트워크와 npx 필요: ACP 에이전트 연결 확인
```

소스 구성: `src/terminal`(터미널 엔진 연동·렌더링), `src/git`(CLI·파서·그래프 레이아웃·패널), `src/files`(트리·에디터), `src/agent`(공통 스레드 모델, ACP, Native), `src/workspace.rs`(도크·탭·액션).

## 라이선스

MIT 또는 Apache-2.0 중 선택([LICENSE-MIT](LICENSE-MIT), [LICENSE-APACHE](LICENSE-APACHE)).
