import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import { Popover } from "./Popover";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { isTauri } from "@tauri-apps/api/core";
import "./styles.css";
import { locale } from "./i18n";

document.documentElement.lang = locale;
// 같은 번들을 두 창이 쓴다. 메뉴바 팝오버 창은 잔여 한도만 보여 준다.
const popover = isTauri() && getCurrentWindow().label === "popover";
document.documentElement.classList.toggle("is-popover", popover);
// Windows에는 창 뒤 블러(vibrancy)가 없어 투명 배경이 비어 보인다. CSS가 불투명 배경을 칠하게 표시한다.
document.documentElement.classList.toggle("is-windows", /Windows/.test(navigator.userAgent));
ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>{popover ? <Popover /> : <App />}</React.StrictMode>,
);
