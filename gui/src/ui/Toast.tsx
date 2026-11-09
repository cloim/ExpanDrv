import { createContext, useCallback, useContext, useMemo, useRef, useState } from 'react';
import type { ReactNode } from 'react';
import { createPortal } from 'react-dom';
import { CircleAlert, CircleCheck, X } from 'lucide-react';

type Tone = 'ok' | 'error';
type ToastItem = { id: number; tone: Tone; text: string };
type ToastApi = { ok: (text: string) => void; error: (text: string) => void };

const ToastContext = createContext<ToastApi | null>(null);
const MAX = 4;

/** 비차단 알림. 화면 오른쪽 아래에 겹쳐 띄우고 레이아웃을 밀지 않습니다. */
export function ToastProvider({ children }: { children: ReactNode }) {
  const [items, setItems] = useState<ToastItem[]>([]);
  const seq = useRef(0);

  const dismiss = useCallback((id: number) => setItems((xs) => xs.filter((x) => x.id !== id)), []);
  const push = useCallback(
    (tone: Tone, text: string) => {
      const id = ++seq.current;
      setItems((xs) => [...xs, { id, tone, text }].slice(-MAX));
      window.setTimeout(() => dismiss(id), tone === 'error' ? 8000 : 3500);
    },
    [dismiss],
  );
  const api = useMemo<ToastApi>(
    () => ({ ok: (t) => push('ok', t), error: (t) => push('error', t) }),
    [push],
  );

  return (
    <ToastContext.Provider value={api}>
      {children}
      {createPortal(
        <div className="pointer-events-none fixed right-5 bottom-5 z-[2500] flex w-[360px] max-w-[calc(100vw-40px)] flex-col gap-2">
          {items.map((t) => (
            <div
              key={t.id}
              role={t.tone === 'error' ? 'alert' : 'status'}
              className="rise pointer-events-none flex items-start gap-2.5 rounded-xl border border-line-strong bg-surface px-3.5 py-3 text-[13px] text-ink shadow-[0_12px_30px_var(--shadow-color)]"
            >
              {t.tone === 'ok' ? (
                <CircleCheck size={17} className="mt-px flex-none text-ok" aria-hidden="true" />
              ) : (
                <CircleAlert size={17} className="mt-px flex-none text-danger" aria-hidden="true" />
              )}
              <span className="min-w-0 flex-1 break-words">{t.text}</span>
              <button
                type="button"
                aria-label="알림 닫기"
                onClick={() => dismiss(t.id)}
                className="pointer-events-auto -m-1 rounded p-1 text-ink-3 hover:text-ink"
              >
                <X size={14} aria-hidden="true" />
              </button>
            </div>
          ))}
        </div>,
        document.body,
      )}
    </ToastContext.Provider>
  );
}

export function useToast(): ToastApi {
  const ctx = useContext(ToastContext);
  if (!ctx) throw new Error('ToastProvider가 필요합니다');
  return ctx;
}
