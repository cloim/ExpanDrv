import { createContext, useCallback, useContext, useEffect, useMemo, useRef, useState } from 'react';
import type { ReactNode } from 'react';
import { api, errorText, STATE_LABEL } from './api';
import type { StatusView } from './api';

const POLL_MS = 3000;
const MAX_EVENTS = 500;

export interface ActivityEvent {
  seq: number;
  at: Date;
  driveId: string;
  kind: 'state' | 'uploads' | 'agent';
  text: string;
  detail: string;
}

interface StatusContextValue {
  status: StatusView | null;
  error: string;
  loading: boolean;
  refreshing: boolean;
  refresh: () => Promise<void>;
  events: ActivityEvent[];
  clearEvents: () => void;
}

const StatusContext = createContext<StatusContextValue | null>(null);

/** 이전 상태와 비교해 활동 기록을 만듭니다. */
function diff(prev: StatusView | null, next: StatusView, nextSeq: () => number): ActivityEvent[] {
  if (!prev) return [];
  const at = new Date();
  const out: ActivityEvent[] = [];
  const add = (driveId: string, kind: ActivityEvent['kind'], text: string, detail = '') =>
    out.push({ seq: nextSeq(), at, driveId, kind, text, detail });
  if (prev.running !== next.running) {
    add('', 'agent', next.running ? '에이전트 시작' : '에이전트 종료', next.agentVersion ?? '');
  }
  if (prev.locked !== next.locked) add('', 'agent', next.locked ? '드라이브 잠금' : '잠금 해제');
  for (const d of next.drives) {
    const before = prev.drives.find((p) => p.id === d.id);
    if (!before) {
      add(d.id, 'state', '드라이브 추가', d.mountPoint);
      continue;
    }
    if (before.state !== d.state) {
      add(d.id, 'state', STATE_LABEL[d.state], d.message ?? d.mountPoint);
    }
    if (before.pendingUploads !== d.pendingUploads) {
      const text = d.pendingUploads === 0 ? '업로드 완료' : `업로드 대기 ${d.pendingUploads}개`;
      add(d.id, 'uploads', text, `${before.pendingUploads} → ${d.pendingUploads}`);
    }
  }
  for (const p of prev.drives) {
    if (!next.drives.some((d) => d.id === p.id)) add(p.id, 'state', '드라이브 삭제');
  }
  return out;
}

/** 에이전트 상태를 화면이 보일 때만 주기적으로 가져오고, 바뀐 내용을 활동 기록으로 남깁니다. */
export function StatusProvider({ children }: { children: ReactNode }) {
  const [status, setStatus] = useState<StatusView | null>(null);
  const [error, setError] = useState('');
  const [refreshing, setRefreshing] = useState(false);
  const [events, setEvents] = useState<ActivityEvent[]>([]);
  const last = useRef<StatusView | null>(null);
  const seq = useRef(0);
  const inFlight = useRef(false);

  const refresh = useCallback(async () => {
    if (inFlight.current) return;
    inFlight.current = true;
    setRefreshing(true);
    try {
      const next = await api.status();
      const found = diff(last.current, next, () => ++seq.current);
      if (found.length) setEvents((xs) => [...found.reverse(), ...xs].slice(0, MAX_EVENTS));
      last.current = next;
      setStatus(next);
      setError('');
    } catch (e) {
      setError(errorText(e));
    } finally {
      inFlight.current = false;
      setRefreshing(false);
    }
  }, []);

  useEffect(() => {
    let timer = 0;
    const tick = () => {
      window.clearTimeout(timer);
      if (document.visibilityState === 'visible') void refresh();
      timer = window.setTimeout(tick, POLL_MS);
    };
    tick();
    const onVisible = () => document.visibilityState === 'visible' && tick();
    document.addEventListener('visibilitychange', onVisible);
    return () => {
      window.clearTimeout(timer);
      document.removeEventListener('visibilitychange', onVisible);
    };
  }, [refresh]);

  const value = useMemo<StatusContextValue>(
    () => ({
      status,
      error,
      loading: status === null && !error,
      refreshing,
      refresh,
      events,
      clearEvents: () => setEvents([]),
    }),
    [status, error, refreshing, refresh, events],
  );
  return <StatusContext.Provider value={value}>{children}</StatusContext.Provider>;
}

export function useStatus(): StatusContextValue {
  const ctx = useContext(StatusContext);
  if (!ctx) throw new Error('StatusProvider가 필요합니다');
  return ctx;
}
