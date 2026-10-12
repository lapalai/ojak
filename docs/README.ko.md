<p align="center">
  <img src="../apps/desktop/src/assets/ojak-icon.png" width="96" height="96" alt="Ojak — 다리 위에 앉은 까치">
</p>

<h1 align="center">Ojak · 오작</h1>

<p align="center"><strong>내 Claude Code·Codex 계정을 한곳에서.</strong></p>

<p align="center">
  <a href="https://github.com/lapalai/ojak/releases/latest"><strong>다운로드</strong></a> ·
  <a href="#시작하기">시작하기</a> ·
  <a href="usage.ko.md">사용 안내</a> ·
  <a href="https://github.com/lapalai/ojak/issues">문제 제보</a>
</p>

<p align="center">
  <a href="../README.md">English</a> · <strong>한국어</strong> · <a href="README.id.md">Bahasa Indonesia</a>
</p>

<p align="center">
  <a href="https://github.com/lapalai/ojak/releases/latest"><img src="https://img.shields.io/github/v/release/lapalai/ojak" alt="최신 버전"></a>
  <a href="https://github.com/lapalai/ojak/actions/workflows/ci.yml"><img src="https://github.com/lapalai/ojak/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="../LICENSE"><img src="https://img.shields.io/github/license/lapalai/ojak" alt="MIT 라이선스"></a>
</p>

Ojak은 본인 소유의 Claude Code·Codex 구독 계정을 관리하는 **macOS 메뉴바·Windows 트레이 앱**이에요. 계정별 남은 한도와 리셋 시각을 한눈에 확인하고, 터미널에서는 평소처럼 `claude`·`codex`를 쓰세요. 터미널 명령 연결을 마치면 Ojak이 새 작업에 쓸 수 있는 계정을 골라 줍니다.

**무료 오픈소스예요.** 공식 Claude Code 또는 Codex CLI를 먼저 설치하고 본인 계정을 연결해 주세요. 공급자 구독과 사용 요금은 별도예요.

> **초기 미리보기 버전입니다.** 동작과 설치 방법이 바뀔 수 있어요. Ojak은 Anthropic·OpenAI·Google·xAI와 관계없는 비공식 도구예요. 본인 계정만 연결하세요. 공급자의 한도를 늘리거나 우회하지 않습니다. [사용 전 고지](#고지)를 확인해 주세요.

## 어떤 점이 편한가요?

- **계정마다 들어가서 확인할 필요가 없어요.** 남은 한도와 리셋 시각을 한 화면에서 볼 수 있어요.
- **쓰던 터미널에서 그대로 작업하세요.** 공식 CLI와 `claude`·`codex` 명령을 그대로 써요.
- **한도를 다 쓰면 자동으로 이어 가요.** 지원되는 대화형 실행에서 명시적인 한도 소진을 감지하면, 기존 실행 종료를 확인한 뒤 다른 계정에서 같은 대화를 다시 엽니다. [지원 범위와 제한](usage.ko.md#한도를-다-쓴-대화-이어가기).
- **토큰 사용량도 확인하세요.** 이 컴퓨터의 omp·Claude Code·Codex 기록으로 토큰 수와 API 요금 기준 환산액을 보여 줘요. 실제 구독 청구액은 아니며, 가격을 모르는 모델은 따로 표시합니다.

앱 화면은 **한국어·영어·인도네시아어**를 지원해요. CLI·서비스·설치기의 일부 메시지는 아직 한국어로 나옵니다.

## AI에게 설치 맡기기

로컬 명령을 실행할 수 있는 Claude Code·Codex·omp에 다음 문장을 붙여 넣으세요.

```text
이 안내에 따라 내 컴퓨터에 Ojak을 설치하고 설정해 줘. 기존 설정과 로그인은 유지하고, 준비 상태까지 확인해 줘:
https://raw.githubusercontent.com/lapalai/ojak/main/docs/install-for-agents.md
```

omp도 연결하려면 “이미 설치된 omp도 연결해 줘”를 덧붙이세요. 기본으로 꺼져 있는 선택 기능이니 [고지](#고지)를 먼저 확인해 주세요. 로그인 승인이나 OS 보안 확인은 직접 해야 할 수 있어요. 업데이트를 위해 진행 중인 작업을 강제로 종료하지 않습니다.

직접 설치하려면 [Mac](usage.ko.md#mac-고급-설치)·[Windows](usage.ko.md#windows-터미널-설치) 터미널 안내를 따르거나 아래 파일을 받으세요. [에이전트용 설치 안내](install-for-agents.md).

## 다운로드

[최신 릴리스](https://github.com/lapalai/ojak/releases/latest)에서 아래 파일을 받으세요. 새 버전이 나와도 이 링크는 그대로 쓸 수 있어요.

| 운영체제 | 받을 파일 | 요구 사항 |
|---|---|---|
| macOS | `Ojak_<버전>_aarch64.dmg` | macOS 13 이상, Apple silicon(M 시리즈) |
| Windows 미리보기 | `Ojak_<버전>_x64-setup.exe` | Windows 10 1809 이상 또는 Windows 11, x64; WebView2 필요 |

Intel Mac과 Linux용 빌드는 아직 없어요. WebView2는 Windows 11에 포함되어 있고, Windows 10에서는 별도 설치가 필요할 수 있어요.

### Mac

1. DMG를 열고 **Ojak**을 **응용 프로그램** 폴더로 옮기세요.
2. Ojak을 여세요. 현재 앱은 ad-hoc 서명이며 Apple 공증을 받지 않았어요. 확인되지 않은 개발자라는 안내로 실행이 막히면, 이 저장소에서 받은 파일인지 확인한 뒤 **시스템 설정 → 개인정보 보호 및 보안 → 그래도 열기**를 누르세요.
3. 아래 시작하기를 진행하세요. 업데이트 뒤에는 macOS가 권한을 다시 물을 수 있어요.

### Windows

1. Orca나 omp를 쓰고 있다면 **설치·업데이트 전에 완전히 종료하세요.** Orca 창만 닫으면 omp가 계속 실행될 수 있어요.
2. 평소 쓰는 Windows 사용자로 설치 파일을 실행하세요. **‘관리자 권한으로 실행’은 쓰지 마세요.**
3. 현재 설치 파일은 서명하지 않았어요. SmartScreen 경고가 뜨면 다운로드 출처를 확인한 뒤 **추가 정보 → 실행**을 선택하세요. 백신을 끄지는 마세요. 파일이 차단되거나 설치가 멈추면 [설치 문제 해결](usage.ko.md#windows-설치)을 확인해 주세요.

## 시작하기

1. **Ojak을 열고 ‘시작하기’를 누르세요.** 백그라운드 서비스와 터미널 명령 연결을 설정합니다.
2. **계정을 연결하세요.** 이 컴퓨터에 있는 Claude Code·Codex 로그인을 찾습니다. 필요하면 앱의 로그인 버튼으로 공급자의 공식 로그인을 진행하세요. 계정을 더 추가하려면 **연결** 화면을 여세요.
3. **프로젝트 폴더에서 새 터미널을 열고** 둘 중 하나를 입력하세요.

   ```sh
   claude
   # 또는
   codex
   ```

   Mac에서는 시작하기 화면의 ‘터미널 열기’로 명령이 입력된 터미널을 열 수 있어요. Enter를 누르면 실행됩니다. Windows에서는 명령을 복사해 새 터미널에 붙여 넣으세요.

4. **세션 화면에서 배정된 계정을 확인하세요.**

설정을 끝내지 않았다면 시작하기 카드에 남은 단계가 나와요. 쓸 수 있는 계정이 없을 때는 원본 CLI로 실행될 수도 있으니, [안내 메시지와 계정 제외 이유](usage.ko.md#계정-배정)를 확인해 주세요.

**omp도 쓰시나요?** omp 연동은 기본으로 꺼져 있는 선택 기능이에요. CLI 계정 배정에 꼭 필요하지는 않습니다. 구독 자격 증명을 로컬 프록시로 중개하므로 공급자 약관과 충돌할 수 있어요. 켜기 전에 [연결 방법과 주의 사항](usage.ko.md#omp-연동)을 읽어 주세요.

## 개인정보와 계정 보호

Ojak의 계정 기록·설정·진단 로그는 이 컴퓨터에 저장됩니다. **Ojak은 사용 통계를 전송하거나 진단 파일을 자동으로 업로드하지 않아요.** 다만 오프라인 앱이라는 뜻은 아닙니다. 로그인·사용량 조회·모델 요청은 공급자와 통신하고, 업데이트 확인은 GitHub에 접속해요.

- 새 관리 세션을 시작할 때 계정을 고릅니다. 지원되는 대화형 실행은 한도 소진이 확인되면 다른 계정에서 자동으로 다시 열어요. 실행 중인 CLI의 인증을 바꾸는 것이 아니라 프로세스를 종료한 뒤 재개합니다.
- 세션이 끝났는지 확인할 수 없으면, 중복 배정을 피하려고 그 계정의 실행 자리를 유지합니다.
- 토큰 비용 환산액은 청구서가 아니에요. 선택 기능인 Codex 크레딧·Claude 추가 사용량은 공급자 과금이 생길 수 있어 새 배정에 쓰는 설정이 기본으로 꺼져 있어요. 이미 실행 중인 공식 CLI의 과금을 Ojak이 막아 주지는 못합니다.

[데이터 위치·과금 범위·문제 해결](usage.ko.md)과 [보안 정책](../SECURITY.md)을 확인해 주세요.

## 고지

본인 소유 계정만 사용하세요. 계정 공유나 한도 회피 용도로 쓰지 마세요. 공급자 약관을 지킬 책임은 사용자에게 있으며, 계정 제한이나 정지가 발생할 수 있어요. 이 프로젝트는 특정 사용 방식이 허용된다거나 위험이 없다고 보장하지 않습니다.

선택 기능인 omp 연동은 구독 자격 증명을 로컬 프록시로 통과시키므로 공급자 약관과 충돌할 수 있어요. [Anthropic 법률·규정 준수 안내](https://code.claude.com/docs/en/legal-and-compliance)와 [OpenAI 이용약관](https://openai.com/policies/row-terms-of-use/)을 확인해 주세요.

## 문서와 기여

- **사용 안내:** [한국어](usage.ko.md) · [English](usage.md) · [Bahasa Indonesia](usage.id.md)
- **달라진 점:** [변경 기록](../CHANGELOG.md)
- **빌드·기여:** [기여 안내](../CONTRIBUTING.md)
- **구조·배포:** [도메인 색인](domains/INDEX.md) · [릴리스 정책](release-policy.md)
- **커뮤니티:** [행동 강령](../CODE_OF_CONDUCT.md) · [보안 문제 비공개 제보](../SECURITY.md)

직접 빌드하려면 [기여 안내](../CONTRIBUTING.md)의 준비 사항을 확인한 뒤 저장소 루트에서 `npm ci`, `npm run build`를 실행하세요.

## 이름에 담긴 이야기

Ojak은 **오작교**에서 따왔어요. 일 년에 한 번, 까마귀와 까치가 은하수에 다리를 놓아 견우와 직녀를 만나게 해 준다는 이야기죠. Ojak도 작업을 알맞은 계정으로 이어 주는 다리가 되고자 합니다.

[lapal](https://github.com/lapalai)의 [lio](https://github.com/Liopark)가 만듭니다. [MIT 라이선스](../LICENSE) · [제3자 고지](../THIRD_PARTY_NOTICES.md).
