import { useCallback, useEffect, useRef, useState } from 'react';
import { Copy, Power, RefreshCw } from 'lucide-react';
import SelectControl from '../components/SelectControl';
import { Modal } from '../components/Modal';
import { api, errorText } from '../api';
import type { SettingsView } from '../api';
import { PageFrame } from '../shell/AppShell';
import { useStatus } from '../status';
import { useTheme } from '../theme';
import type { ThemePref } from '../theme';
import { Field, IconButton, Led, Section, Skeleton, Switch } from '../ui/controls';
import { useToast } from '../ui/Toast';

function PathRow({ label, value, note }: { label: string; value?: string; note?: string }) {
  const toast = useToast();
  const copy = async () => {
    if (!value) return;
    try {
      await navigator.clipboard.writeText(value);
      toast.ok(`${label} 경로를 복사했습니다`);
    } catch {
      toast.error('클립보드에 복사하지 못했습니다');
    }
  };
  return (
    <div className="flex items-center gap-3">
      <div className="min-w-0 flex-1">
        <div className="text-xs font-semibold text-ink-2">
          {label}
          {note ? <span className="ml-1.5 font-normal text-danger">{note}</span> : null}
        </div>
        <div className="kbd-mono mt-1 truncate text-[12.5px]">{value ?? <Skeleton className="w-72" />}</div>
      </div>
      <IconButton small label={`${label} 경로 복사`} icon={Copy} disabled={!value} onClick={() => void copy()} />
    </div>
  );
}

export function SettingsPage() {
  const toast = useToast();
  const { refresh } = useStatus();
  const [view, setView] = useState<SettingsView | null>(null);
  const [error, setError] = useState('');
  const [busy, setBusy] = useState<'autostart' | 'stop' | 'load' | null>('load');
  const [confirmStop, setConfirmStop] = useState(false);
  const cancelRef = useRef<HTMLButtonElement>(null);
  const [theme, setTheme] = useTheme();

  const load = useCallback(async () => {
    setBusy('load');
    try {
      setView(await api.settings());
      setError('');
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(null);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const toggleAutostart = async (on: boolean) => {
    setBusy('autostart');
    try {
      setView(await api.setAutostart(on));
      toast.ok(on ? '로그인할 때 에이전트를 자동으로 시작합니다' : '자동 시작을 껐습니다');
    } catch (e) {
      toast.error(errorText(e));
    } finally {
      setBusy(null);
    }
  };

  const stop = async () => {
    setConfirmStop(false);
    setBusy('stop');
    try {
      const was = await api.stopAgent();
      toast.ok(was ? '모든 드라이브를 해제하고 에이전트를 종료했습니다' : '에이전트가 실행 중이 아니었습니다');
    } catch (e) {
      toast.error(errorText(e));
    } finally {
      setBusy(null);
      void refresh();
      void load();
    }
  };

  return (
    <PageFrame
      title="설정"
      scroll
      actions={<IconButton label="새로 고침" icon={RefreshCw} busy={busy === 'load'} onClick={() => void load()} />}
    >
      <div className="rise mx-auto grid w-full max-w-[1100px] grid-cols-1 gap-4 p-5 lg:grid-cols-2">
        {error ? (
          <div role="alert" className="panel col-span-full px-5 py-3 text-danger">
            설정을 읽지 못했습니다: {error}
          </div>
        ) : null}

        <Section title="에이전트">
          <div className="flex items-center justify-between gap-3">
            <div className="flex items-center gap-2.5 text-[13px]">
              <Led state={view?.agentRunning ? 'on' : 'off'} busy={busy === 'stop'} />
              {view ? (
                view.agentRunning ? (
                  <span>
                    실행 중 <span className="kbd-mono text-ink-3">v{view.agentVersion}</span>
                  </span>
                ) : (
                  <span className="text-ink-2">실행 중이 아닙니다</span>
                )
              ) : (
                <Skeleton className="w-28" />
              )}
            </div>
            <button
              type="button"
              className="btn btn-ghost"
              disabled={!view?.agentRunning || busy === 'stop'}
              onClick={() => setConfirmStop(true)}
            >
              <Power size={15} aria-hidden="true" className={busy === 'stop' ? 'spin' : ''} />
              {busy === 'stop' ? '종료하는 중' : '에이전트 종료'}
            </button>
          </div>
          <dl className="grid grid-cols-[110px_1fr] gap-y-2 text-[13px]">
            <dt className="text-ink-3">GUI 버전</dt>
            <dd className="kbd-mono">{view ? view.guiVersion : <Skeleton className="w-16" />}</dd>
          </dl>
        </Section>

        <Section title="자동 실행">
          <Switch
            label="로그인할 때 에이전트 시작"
            hint="자동 마운트를 켠 드라이브를 함께 마운트합니다"
            checked={view?.autostart ?? false}
            disabled={!view || busy === 'autostart' || (!view.cliFound && !view.autostart)}
            onChange={(v) => void toggleAutostart(v)}
          />
          {view?.autostartCommand ? (
            <p className="kbd-mono rounded-lg bg-surface-2 px-3 py-2 text-[11.5px] break-all text-ink-2">
              {view.autostartCommand}
            </p>
          ) : null}
        </Section>

        <Section title="경로" className="lg:col-span-2">
          <PathRow label="설정 파일" value={view?.configPath} />
          <PathRow label="로그 폴더" value={view?.logDir} />
          <PathRow
            label="CLI 실행 파일"
            value={view?.cliPath}
            note={view && !view.cliFound ? '파일이 없어 에이전트를 시작할 수 없습니다' : undefined}
          />
        </Section>

        <Section title="화면">
          <Field label="테마">
            {(p) => (
              <SelectControl {...p} value={theme} onChange={(e) => setTheme(e.target.value as ThemePref)}>
                <option value="system">시스템 설정 따르기</option>
                <option value="light">밝게</option>
                <option value="dark">어둡게</option>
              </SelectControl>
            )}
          </Field>
        </Section>
      </div>

      <Modal
        isOpen={confirmStop}
        onClose={() => setConfirmStop(false)}
        title="에이전트 종료"
        size="sm"
        draggable={false}
        initialFocusRef={cancelRef}
        footer={
          <>
            <button ref={cancelRef} type="button" className="btn btn-ghost" onClick={() => setConfirmStop(false)}>
              취소
            </button>
            <button type="button" className="btn btn-danger" onClick={() => void stop()}>
              종료
            </button>
          </>
        }
      >
        <p className="leading-relaxed">
          마운트한 드라이브를 모두 해제하고 에이전트를 종료할까요?
          <br />
          남은 업로드가 있으면 끝날 때까지 기다립니다
        </p>
      </Modal>
    </PageFrame>
  );
}
