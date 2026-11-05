import { useId } from 'react';
import type { ButtonHTMLAttributes, ReactNode } from 'react';
import type { LucideIcon } from 'lucide-react';
import { Loader2 } from 'lucide-react';
import { Tooltip } from './Tooltip';

type IconButtonProps = Omit<ButtonHTMLAttributes<HTMLButtonElement>, 'children'> & {
  label: string;
  icon: LucideIcon;
  busy?: boolean;
  tone?: 'default' | 'accent' | 'danger';
  small?: boolean;
};

/** 아이콘 전용 버튼. 접근 가능한 이름과 툴팁을 함께 붙입니다. */
export function IconButton({
  label,
  icon: Icon,
  busy = false,
  tone = 'default',
  small = false,
  className = '',
  disabled,
  ...rest
}: IconButtonProps) {
  const cls = [
    'icon-btn',
    small ? 'sm' : '',
    tone === 'accent' ? 'is-accent' : tone === 'danger' ? 'is-danger' : '',
    className,
  ]
    .filter(Boolean)
    .join(' ');
  const Shown = busy ? Loader2 : Icon;
  return (
    <Tooltip label={label}>
      <button
        type="button"
        aria-label={label}
        aria-busy={busy || undefined}
        className={cls}
        disabled={disabled || busy}
        {...rest}
      >
        <Shown size={small ? 16 : 18} strokeWidth={2} aria-hidden="true" className={busy ? 'spin' : ''} />
      </button>
    </Tooltip>
  );
}

/** 켜고 끄는 스위치(role="switch") */
export function Switch({
  checked,
  onChange,
  label,
  hint,
  disabled,
}: {
  checked: boolean;
  onChange: (v: boolean) => void;
  label: string;
  hint?: string;
  disabled?: boolean;
}) {
  const id = useId();
  return (
    <div className="flex items-center justify-between gap-4 py-1">
      <label htmlFor={id} className="min-w-0 cursor-pointer">
        <span className="block text-[13px] font-semibold text-ink">{label}</span>
        {hint ? <span className="mt-0.5 block text-xs text-ink-3">{hint}</span> : null}
      </label>
      <button
        id={id}
        type="button"
        role="switch"
        aria-checked={checked}
        disabled={disabled}
        onClick={() => onChange(!checked)}
        className={`relative h-[22px] w-[40px] flex-none rounded-full border transition-colors disabled:cursor-not-allowed disabled:opacity-50 ${
          checked ? 'border-accent bg-accent' : 'border-line-strong bg-surface-2'
        }`}
      >
        <span
          aria-hidden="true"
          className={`absolute top-[2px] size-4 rounded-full bg-white shadow transition-[left] ${
            checked ? 'left-[20px]' : 'left-[2px]'
          }`}
        />
      </button>
    </div>
  );
}

/** 라벨, 입력, 필드 오류 오버레이. 오류는 레이아웃을 밀지 않게 입력 아래에 겹쳐 띄웁니다. */
export function Field({
  label,
  error,
  hint,
  children,
  className = '',
}: {
  label: string;
  error?: string;
  hint?: string;
  children: (props: { id: string; 'aria-invalid'?: boolean; 'aria-describedby'?: string }) => ReactNode;
  className?: string;
}) {
  const id = useId();
  const errId = `${id}-err`;
  return (
    <div className={`flex min-w-0 flex-col gap-1.5 ${className}`}>
      <label htmlFor={id} className="text-xs font-semibold text-ink-2">
        {label}
        {hint ? <span className="ml-1.5 font-normal text-ink-3">{hint}</span> : null}
      </label>
      <div className="relative">
        {children({
          id,
          'aria-invalid': error ? true : undefined,
          'aria-describedby': error ? errId : undefined,
        })}
        {error ? (
          <div id={errId} role="alert" className="field-error">
            {error}
          </div>
        ) : null}
      </div>
    </div>
  );
}

export function Section({
  title,
  actions,
  children,
  className = '',
}: {
  title: string;
  actions?: ReactNode;
  children: ReactNode;
  className?: string;
}) {
  return (
    <section className={`panel flex flex-col ${className}`} aria-label={title}>
      <header className="flex h-12 flex-none items-center justify-between gap-3 border-b border-line px-5">
        <h2 className="text-[13px] font-bold tracking-wide text-ink">{title}</h2>
        {actions ? <div className="flex items-center gap-1">{actions}</div> : null}
      </header>
      <div className="flex flex-col gap-4 p-5">{children}</div>
    </section>
  );
}

export function Led({ state, busy }: { state: 'on' | 'off' | 'fail'; busy?: boolean }) {
  const cls = busy ? 'is-busy' : state === 'on' ? 'is-on' : state === 'fail' ? 'is-fail' : '';
  return <span className={`led ${cls}`} aria-hidden="true" />;
}

export function Skeleton({ className = '' }: { className?: string }) {
  return <span className={`skeleton-text ${className}`} aria-hidden="true" />;
}
