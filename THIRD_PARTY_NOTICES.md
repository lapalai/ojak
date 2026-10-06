# 제3자 고지

이 저장소의 코드는 [MIT](LICENSE)입니다. Copyright (c) 2026 lapal. 아래는 함께 배포하는 글꼴·아이콘·의존성 고지입니다.

## Pretendard — SIL Open Font License 1.1

고지 의무가 있습니다. 글꼴을 앱에 넣었으므로 라이선스 전문을 함께 둡니다.

- Copyright (c) 2021, Kil Hyung-jin (https://github.com/orioncactus/pretendard), with Reserved Font Name Pretendard.
- 파일: `apps/desktop/src/assets/fonts/PretendardVariable.woff2`
- 전문: `apps/desktop/src/assets/fonts/Pretendard-OFL.txt`
- 설치본: `licenses/Pretendard-OFL.txt` (앱 리소스)

글꼴을 단독으로 팔지 않습니다. 수정본에 Reserved Font Name `Pretendard`를 쓰지 않습니다. 글꼴 파일 자체는 이 라이선스 이외로 재배포하지 않습니다.

## Lucide — ISC

고지 의무가 있습니다. `lucide-react` 0.468.0 아이콘을 앱 UI에 넣습니다.

Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2022 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2022.

Permission to use, copy, modify, and/or distribute this software for any purpose with or without fee is hereby granted, provided that the above copyright notice and this permission notice appear in all copies.

https://github.com/lucide-icons/lucide

## MPL-2.0 — 파일 단위, 고지와 함께 사용

직접 의존성은 아닙니다. Tauri HTML 스택(`dom_query`, `selectors`)을 통해 앱 바이너리에 들어갑니다. 수정하지 않았습니다. MPL-2.0은 해당 파일에만 적용되며, 이 프로젝트의 MIT 코드로 바꾸지 않습니다. 소스는 crates.io의 같은 버전입니다.

| crate | version |
|---|---|
| cssparser | 0.37.0 |
| cssparser-macros | 0.7.1 |
| dtoa-short | 0.3.5 |
| option-ext | 0.2.0 |
| selectors | 0.38.0 |

## 선택 라이선스

`r-efi` 5.3.0과 6.0.0은 MIT OR Apache-2.0 OR LGPL-2.1-or-later입니다. 이 프로젝트는 MIT 또는 Apache-2.0을 선택합니다. LGPL 의무는 없습니다.

## 그림·아이콘

- 앱 아이콘(`apps/desktop/src-tauri/icons/`, `apps/desktop/src/assets/ojak-icon.png`)은 프로젝트 원본입니다. 제3자 라이선스 파일이 없습니다.
- `docs/brand/concepts/*.png`와 앱의 `tiger-*.webp`는 2026-10-03 ChatGPT `gpt-image` 산출물입니다. C2PA의 softwareAgent는 ChatGPT, version은 gpt-image입니다. OpenAI Terms of Use(2026-01-01) Ownership of content에 따라, 이용자와 OpenAI 사이에서는 산출물 권리가 이용자에게 있습니다. 사람 그림이라고 표기하지 않습니다. 기존 저작물과의 유사성은 이 약관이 보장하지 않습니다.
- Galmuri 글꼴 파일은 이 저장소에 없습니다. 컨셉 HTML의 `font-family` 이름뿐입니다. OFL 고지 대상이 아닙니다.

## 이름

소스 라이선스 보유자는 lapal입니다. 앱 번들의 publisher·copyright, 앱이 여는 홈페이지, Cargo.toml과 package.json의 homepage는 모두 lapal과 저장소 `https://github.com/lapalai/ojak`을 가리킵니다. 만든 사람 표기는 "lio by lapal"입니다.
