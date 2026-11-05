import { cloneElement, useEffect, useId, useLayoutEffect, useRef, useState } from 'react';
import type { FocusEvent, MouseEvent, ReactElement } from 'react';
import { createPortal } from 'react-dom';

type TriggerProps = {
  onMouseEnter?: (e: MouseEvent) => void;
  onMouseLeave?: (e: MouseEvent) => void;
  onFocus?: (e: FocusEvent) => void;
  onBlur?: (e: FocusEvent) => void;
  'aria-describedby'?: string;
};

const GAP = 6;
const EDGE = 8;

/** 호버와 키보드 포커스에 뜨는 툴팁. 포털로 띄우고 화면 경계 안으로 위치를 맞춥니다. */
export function Tooltip({
  label,
  children,
  side = 'bottom',
}: {
  label: string;
  children: ReactElement<TriggerProps>;
  side?: 'top' | 'bottom';
}) {
  const id = useId();
  const [open, setOpen] = useState(false);
  const [pos, setPos] = useState<{ left: number; top: number } | null>(null);
  const anchor = useRef<Element | null>(null);
  const tip = useRef<HTMLDivElement>(null);

  useLayoutEffect(() => {
    if (!open || !anchor.current || !tip.current) return;
    const a = anchor.current.getBoundingClientRect();
    const t = tip.current.getBoundingClientRect();
    let top = side === 'top' ? a.top - t.height - GAP : a.bottom + GAP;
    if (top + t.height > window.innerHeight - EDGE) top = a.top - t.height - GAP;
    if (top < EDGE) top = a.bottom + GAP;
    const left = Math.min(
      Math.max(EDGE, a.left + a.width / 2 - t.width / 2),
      window.innerWidth - t.width - EDGE,
    );
    setPos({ left, top });
  }, [open, side, label]);

  useEffect(() => {
    if (!open) return;
    const close = (e: KeyboardEvent) => e.key === 'Escape' && setOpen(false);
    const hide = () => setOpen(false);
    window.addEventListener('keydown', close);
    window.addEventListener('scroll', hide, true);
    return () => {
      window.removeEventListener('keydown', close);
      window.removeEventListener('scroll', hide, true);
    };
  }, [open]);

  const show = (e: { currentTarget: Element }) => {
    anchor.current = e.currentTarget;
    setPos(null);
    setOpen(true);
  };
  const hide = () => setOpen(false);
  const p = children.props;

  return (
    <>
      {cloneElement(children, {
        onMouseEnter: (e: MouseEvent) => {
          p.onMouseEnter?.(e);
          show(e);
        },
        onMouseLeave: (e: MouseEvent) => {
          p.onMouseLeave?.(e);
          hide();
        },
        onFocus: (e: FocusEvent) => {
          p.onFocus?.(e);
          show(e);
        },
        onBlur: (e: FocusEvent) => {
          p.onBlur?.(e);
          hide();
        },
        'aria-describedby': open ? id : p['aria-describedby'],
      })}
      {open
        ? createPortal(
            <div
              ref={tip}
              id={id}
              role="tooltip"
              className="tooltip"
              style={pos ? { left: pos.left, top: pos.top } : { left: -9999, top: -9999 }}
            >
              {label}
            </div>,
            document.body,
          )
        : null}
    </>
  );
}
