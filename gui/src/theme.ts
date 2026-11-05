// 화면 테마 선택(시스템, 밝게, 어둡게). 선택은 이 PC의 웹뷰 저장소에만 둡니다.
import { useState } from 'react';

export type ThemePref = 'system' | 'light' | 'dark';
const KEY = 'expandrv.theme';
const darkQuery = () => window.matchMedia('(prefers-color-scheme: dark)');

function read(): ThemePref {
  try {
    const v = localStorage.getItem(KEY);
    return v === 'light' || v === 'dark' ? v : 'system';
  } catch {
    return 'system';
  }
}

function apply(pref: ThemePref) {
  const dark = pref === 'dark' || (pref === 'system' && darkQuery().matches);
  document.documentElement.dataset.theme = dark ? 'dark' : 'light';
}

/** 첫 렌더 전에 적용해 깜박임을 막고, 시스템 테마가 바뀌면 따라갑니다. */
export function initTheme() {
  apply(read());
  darkQuery().addEventListener('change', () => apply(read()));
}

export function useTheme(): [ThemePref, (p: ThemePref) => void] {
  const [pref, setPref] = useState<ThemePref>(read);
  const change = (p: ThemePref) => {
    try {
      localStorage.setItem(KEY, p);
    } catch {
      // 저장하지 못해도 이번 실행에는 적용합니다.
    }
    apply(p);
    setPref(p);
  };
  return [pref, change];
}
