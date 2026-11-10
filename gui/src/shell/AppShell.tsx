import { NavLink, Outlet, Link, useNavigate } from 'react-router-dom';
import { Activity, HardDrive, Settings, LockKeyhole } from 'lucide-react';
import { useState } from 'react';
import type { ReactNode } from 'react';
import { LockScreen } from '../LockScreen';
import { api, errorText } from '../api';
import { useToast } from '../ui/Toast';
import { useStatus } from '../status';
import { Led, Skeleton, IconButton } from '../ui/controls';

const NAV = [
  { to: '/drives', label: '드라이브', icon: HardDrive },
  { to: '/activity', label: '활동', icon: Activity },
  { to: '/settings', label: '설정', icon: Settings },
];

function BrandMark() {
  return (
    <svg viewBox="0 0 32 32" width="30" height="30" aria-hidden="true">
      <rect x="1" y="1" width="30" height="30" rx="8" fill="#1f2226" stroke="#3a3f47" />
      <rect x="7" y="9" width="18" height="5" rx="1.5" fill="#4a5059" />
      <rect x="7" y="18" width="18" height="5" rx="1.5" fill="#4a5059" />
      <circle cx="21.5" cy="11.5" r="1.6" fill="#ff7a33" />
      <circle cx="21.5" cy="20.5" r="1.6" fill="#3ddc84" />
    </svg>
  );
}

function AgentBadge() {
  const { status, loading, error } = useStatus();
  let body: ReactNode;
  if (loading) body = <Skeleton className="w-24" />;
  else if (error) body = <span>상태를 읽지 못했습니다</span>;
  else if (status?.running)
    body = (
      <span>
        에이전트 실행 중
        {status.agentVersion ? <span className="kbd-mono ml-1.5 text-[11px]">v{status.agentVersion}</span> : null}
      </span>
    );
  else body = <span>에이전트 중지됨</span>;
  return (
    <div className="flex h-10 items-center gap-2.5 rounded-lg bg-rail-2 px-3 text-xs text-rail-text" aria-live="polite">
      <Led state={status?.running ? 'on' : error ? 'fail' : 'off'} />
      {body}
    </div>
  );
}

/** 왼쪽 레일(내비게이션)과 본문. 상단바는 각 화면이 PageFrame으로 그립니다. */
export function AppShell() {
  const { status, loading, error } = useStatus();
  return (
    <div className="flex h-full">
      <aside className="flex w-[212px] flex-none flex-col bg-rail px-3 py-4 text-rail-text">
        <Link
          to="/drives"
          className="mb-7 flex items-center gap-2.5 rounded-lg px-2 py-1 text-rail-text-strong"
          aria-label="ExpanDrv 드라이브 목록으로"
        >
          <BrandMark />
          <span className="text-[17px] font-[750] tracking-tight [font-stretch:92%]">ExpanDrv</span>
        </Link>
        <nav aria-label="주 메뉴" className="flex flex-col gap-1">
          {NAV.map(({ to, label, icon: Icon }) => (
            <NavLink
              key={to}
              to={to}
              className={({ isActive }) =>
                `flex h-10 items-center gap-3 rounded-lg px-3 text-[13px] font-semibold transition-colors ${
                  isActive
                    ? 'bg-rail-text-strong text-rail shadow-[inset_3px_0_0_var(--accent)]'
                    : 'hover:bg-rail-2 hover:text-rail-text-strong'
                }`
              }
            >
              <Icon size={17} aria-hidden="true" />
              {label}
            </NavLink>
          ))}
        </nav>
        <div className="mt-auto">
          <AgentBadge />
        </div>
      </aside>
      <main className="flex min-w-0 flex-1 flex-col">
        {loading ? <div className="p-6"><Skeleton className="h-40 w-full" /></div> : error ? <div role="alert" className="p-6 text-danger">{error}</div> : status?.locked ? <LockScreen /> : <Outlet />}
      </main>
    </div>
  );
}

/** 화면 상단바(제목 또는 경로, 오른쪽 아이콘 동작)와 본문 */
export function PageFrame({
  title,
  crumbs,
  actions,
  children,
  scroll = false,
}: {
  title: string;
  crumbs?: { to: string; label: string }[];
  actions?: ReactNode;
  children: ReactNode;
  /** 목록이 없는 긴 화면만 본문 전체를 스크롤합니다. */
  scroll?: boolean;
}) {
  return (
    <>
      <header className="flex h-[60px] flex-none items-center justify-between gap-4 border-b border-line bg-surface px-6">
        <nav aria-label="현재 위치" className="flex min-w-0 items-center gap-2 text-[15px]">
          {crumbs?.map((c) => (
            <span key={c.to} className="flex items-center gap-2 text-ink-3">
              <Link to={c.to} className="font-semibold hover:text-ink">
                {c.label}
              </Link>
              <span aria-hidden="true">/</span>
            </span>
          ))}
          <h1 data-ui-audit-context-title className="truncate text-[17px] font-[750] tracking-tight text-ink [font-stretch:94%]">{title}</h1>
        </nav>
        <div className="flex flex-none items-center gap-1">{actions}<LockAction /></div>
      </header>
      <div
        data-ui-audit-scroll-root={scroll ? true : undefined}
        className={`dot-grid flex min-h-0 flex-1 flex-col ${scroll ? 'overflow-y-auto' : 'overflow-hidden'}`}
      >
        {children}
      </div>
    </>
  );
}

function LockAction() {
  const { status, refresh } = useStatus();
  const navigate = useNavigate();
  const toast = useToast();
  const [busy, setBusy] = useState(false);
  const lock = async () => {
    if (!status?.pinConfigured) { navigate('/settings?tab=security'); return; }
    setBusy(true);
    try { await api.lock(); }
    catch (e) { toast.error(errorText(e)); }
    finally { await refresh(); setBusy(false); }
  };
  return <IconButton label={status?.pinConfigured ? '전체 드라이브 잠금' : '잠금 PIN 설정'} icon={LockKeyhole} busy={busy} disabled={!status} onClick={() => void lock()} />;
}
