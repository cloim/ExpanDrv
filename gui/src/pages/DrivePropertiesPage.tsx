import { useCallback, useEffect, useRef, useState } from 'react';
import { useNavigate, useParams } from 'react-router-dom';
import { KeyRound, Loader2, Play, PlugZap, RotateCcw, Save, Square } from 'lucide-react';
import { Modal } from '../components/Modal';
import { api, BACKEND_LABEL, errorText, STATE_LABEL } from '../api';
import type { CacheSettings, DriveDetail, SecretInfo, TransferSettings } from '../api';
import { PageFrame } from '../shell/AppShell';
import { useStatus } from '../status';
import { Field, IconButton, Led, Section, Skeleton, Switch } from '../ui/controls';
import { useToast } from '../ui/Toast';

interface Draft {
  label: string;
  mountPoint: string;
  readOnly: boolean;
  autoMount: boolean;
  cache: Record<keyof CacheSettings, string>;
  transfer: Record<keyof TransferSettings, string>;
  proxyOn: boolean;
  proxyUrl: string;
  proxyUser: string;
  proxyPassword: string;
}

function toDraft(d: DriveDetail): Draft {
  const str = <T extends object>(o: T) =>
    Object.fromEntries(Object.entries(o).map(([k, v]) => [k, String(v)])) as Record<keyof T, string>;
  return {
    label: d.label,
    mountPoint: d.mountPoint,
    readOnly: d.readOnly,
    autoMount: d.autoMount,
    cache: str(d.cache),
    transfer: str(d.transfer),
    proxyOn: d.proxy !== null,
    proxyUrl: d.proxy?.url ?? '',
    proxyUser: d.proxy?.username ?? '',
    proxyPassword: '',
  };
}

const CACHE_FIELDS: { key: keyof CacheSettings; label: string; unit: string; min: number; max: number }[] = [
  { key: 'metadataTtlSecs', label: '목록과 속성 캐시 유지', unit: '초', min: 0, max: 86400 },
  { key: 'blockSizeMib', label: '읽기 블록 크기', unit: 'MiB', min: 1, max: 64 },
  { key: 'memoryLimitMib', label: '블록 캐시 메모리 상한', unit: 'MiB', min: 8, max: 8192 },
  { key: 'pollIntervalSecs', label: '원격 변경 확인 주기', unit: '초, 0이면 끔', min: 0, max: 86400 },
];

const TRANSFER_FIELDS: { key: keyof TransferSettings; label: string }[] = [
  { key: 'uploadKibPerSec', label: '업로드 제한' },
  { key: 'downloadKibPerSec', label: '다운로드 제한' },
];

function SecretRow({ info, onChange }: { info: SecretInfo; onChange: () => void }) {
  const state =
    info.source === 'credential'
      ? info.stored
        ? { text: '자격 증명 관리자에 저장됨', tone: 'text-ok' }
        : { text: '저장된 값이 없습니다', tone: 'text-danger' }
      : info.source === 'env'
        ? { text: `환경 변수 ${info.reference}`, tone: 'text-ink-2' }
        : { text: '사용 안 함', tone: 'text-ink-3' };
  return (
    <div className="flex items-center justify-between gap-3 border-b border-line pb-3 last:border-0 last:pb-0">
      <div className="min-w-0">
        <div className="text-[13px] font-semibold">{info.label}</div>
        <div className={`truncate text-xs ${state.tone}`}>{state.text}</div>
      </div>
      <button type="button" className="btn btn-ghost h-8 flex-none px-3 text-xs" onClick={onChange}>
        <KeyRound size={14} aria-hidden="true" />
        {info.source === 'none' ? '설정' : '변경'}
      </button>
    </div>
  );
}

export function DrivePropertiesPage() {
  const { id = '' } = useParams();
  const navigate = useNavigate();
  const toast = useToast();
  const { status, refresh } = useStatus();
  const [detail, setDetail] = useState<DriveDetail | null>(null);
  const [loadError, setLoadError] = useState('');
  const [draft, setDraft] = useState<Draft | null>(null);
  const [errors, setErrors] = useState<Record<string, string>>({});
  const [saving, setSaving] = useState(false);
  const [checking, setChecking] = useState(false);
  const [toggling, setToggling] = useState(false);
  const [secretFor, setSecretFor] = useState<SecretInfo | null>(null);
  const [secretValue, setSecretValue] = useState('');
  const [secretSaving, setSecretSaving] = useState(false);
  const secretInput = useRef<HTMLInputElement>(null);

  const live = status?.drives.find((d) => d.id === id);
  const mounted = live?.state === 'mounted';

  const load = useCallback(async () => {
    try {
      const d = await api.drive(id);
      setDetail(d);
      setDraft(toDraft(d));
      setLoadError('');
    } catch (e) {
      setLoadError(errorText(e));
    }
  }, [id]);

  useEffect(() => {
    void load();
  }, [load]);

  const dirty = detail && draft ? JSON.stringify(toDraft(detail)) !== JSON.stringify(draft) : false;

  const patchDraft = (p: Partial<Draft>) => setDraft((d) => (d ? { ...d, ...p } : d));
  const clearError = (key: string) => setErrors(({ [key]: _, ...rest }) => rest);

  const validate = (d: Draft) => {
    const e: Record<string, string> = {};
    if (!d.label.trim()) e.label = '레이블을 입력하세요';
    if (!/^[A-Za-z]:$/.test(d.mountPoint.trim()) && !/^[A-Za-z]:\\/.test(d.mountPoint.trim()))
      e.mountPoint = 'X: 또는 절대 경로를 입력하세요';
    for (const f of CACHE_FIELDS) {
      const n = Number(d.cache[f.key]);
      if (!Number.isInteger(n) || n < f.min || n > f.max) e[f.key] = `${f.min}~${f.max} 사이 정수`;
    }
    for (const f of TRANSFER_FIELDS) {
      const n = Number(d.transfer[f.key]);
      if (!Number.isInteger(n) || n < 0 || n > 10_000_000) e[f.key] = '0 이상 정수(0이면 제한 없음)';
    }
    if (d.proxyOn && !/^(socks5h?|http):\/\/[^/@\s]+:\d+\/?$/i.test(d.proxyUrl.trim()))
      e.proxyUrl = 'socks5://, socks5h:// 또는 http://호스트:포트';
    return e;
  };

  const save = async () => {
    if (!draft) return;
    const e = validate(draft);
    setErrors(e);
    if (Object.keys(e).length) {
      toast.error('입력값을 확인하세요');
      return;
    }
    setSaving(true);
    try {
      await api.updateDrive(id, {
        label: draft.label,
        mountPoint: draft.mountPoint,
        readOnly: draft.readOnly,
        autoMount: draft.autoMount,
        cache: {
          metadataTtlSecs: Number(draft.cache.metadataTtlSecs),
          blockSizeMib: Number(draft.cache.blockSizeMib),
          memoryLimitMib: Number(draft.cache.memoryLimitMib),
          pollIntervalSecs: Number(draft.cache.pollIntervalSecs),
        },
        transfer: {
          uploadKibPerSec: Number(draft.transfer.uploadKibPerSec),
          downloadKibPerSec: Number(draft.transfer.downloadKibPerSec),
        },
        proxy: draft.proxyOn
          ? { url: draft.proxyUrl, username: draft.proxyUser, password: draft.proxyPassword }
          : null,
      });
      toast.ok(mounted ? '저장했고, 다시 마운트하면 적용됩니다' : '저장했습니다');
      await load();
      void refresh();
    } catch (err) {
      toast.error(errorText(err));
    } finally {
      setSaving(false);
    }
  };

  const check = async () => {
    setChecking(true);
    try {
      toast.ok(await api.checkDrive(id));
    } catch (e) {
      toast.error(`연결 실패: ${errorText(e)}`);
    } finally {
      setChecking(false);
    }
  };

  const toggleMount = async () => {
    setToggling(true);
    try {
      if (mounted) {
        await api.unmount(id);
        toast.ok(`${id} 마운트를 해제했습니다`);
      } else {
        await api.mount(id);
        toast.ok(`${id}를 마운트했습니다`);
      }
    } catch (e) {
      toast.error(errorText(e));
    } finally {
      setToggling(false);
      void refresh();
    }
  };

  const saveSecret = async () => {
    if (!secretFor || !secretValue) return;
    setSecretSaving(true);
    try {
      await api.setSecret(id, secretFor.field, secretValue);
      toast.ok(`${secretFor.label}을(를) 저장했습니다`);
      setSecretFor(null);
      await load();
    } catch (e) {
      toast.error(errorText(e));
    } finally {
      setSecretValue('');
      setSecretSaving(false);
    }
  };

  const num = (group: 'cache' | 'transfer', key: string, label: string, unit: string) => (
    <Field key={key} label={label} hint={unit} error={errors[key]}>
      {(p) =>
        draft ? (
          <input
            {...p}
            className="control kbd-mono"
            inputMode="numeric"
            value={(draft[group] as Record<string, string>)[key]}
            onChange={(e) => {
              patchDraft({ [group]: { ...draft[group], [key]: e.target.value } } as Partial<Draft>);
              clearError(key);
            }}
          />
        ) : (
          <FieldSkeleton />
        )
      }
    </Field>
  );

  const ready = draft !== null && detail !== null;

  return (
    <PageFrame
      title={id}
      crumbs={[{ to: '/drives', label: '드라이브' }]}
      scroll
      actions={
        <>
          <IconButton
            label="연결 확인"
            icon={PlugZap}
            busy={checking}
            disabled={!ready}
            onClick={() => void check()}
          />
          <IconButton
            label={mounted ? '마운트 해제' : '마운트'}
            icon={mounted ? Square : Play}
            busy={toggling}
            disabled={!ready}
            onClick={() => void toggleMount()}
          />
          <IconButton
            label="변경 취소"
            icon={RotateCcw}
            disabled={!dirty}
            onClick={() => {
              if (detail) setDraft(toDraft(detail));
              setErrors({});
            }}
          />
          <IconButton
            label={dirty ? '저장' : '저장(바뀐 내용 없음)'}
            icon={Save}
            tone={dirty ? 'accent' : 'default'}
            busy={saving}
            disabled={!dirty}
            onClick={() => void save()}
          />
        </>
      }
    >
      {loadError ? (
        <div className="flex flex-1 flex-col items-center justify-center gap-3 text-center">
          <p className="text-ink-2">{loadError}</p>
          <button type="button" className="btn btn-ghost" onClick={() => navigate('/drives')}>
            드라이브 목록으로
          </button>
        </div>
      ) : (
        <div className="rise mx-auto grid w-full max-w-[1100px] grid-cols-1 gap-4 p-5 lg:grid-cols-2">
          <Section
            title="일반"
            actions={
              <span className="flex items-center gap-2 text-xs text-ink-2">
                <Led state={mounted ? 'on' : live?.state === 'failed' ? 'fail' : 'off'} busy={(live?.pendingUploads ?? 0) > 0} />
                {live ? STATE_LABEL[live.state] : <Skeleton className="w-12" />}
              </span>
            }
          >
            <div className="grid grid-cols-2 gap-4">
              <Field label="레이블" error={errors.label}>
                {(p) =>
                  draft ? (
                    <input
                      {...p}
                      className="control"
                      maxLength={32}
                      value={draft.label}
                      onChange={(e) => {
                        patchDraft({ label: e.target.value });
                        clearError('label');
                      }}
                    />
                  ) : (
                    <FieldSkeleton />
                  )
                }
              </Field>
              <Field label="마운트 지점" hint="다시 마운트하면 적용" error={errors.mountPoint}>
                {(p) =>
                  draft ? (
                    <input
                      {...p}
                      className="control mono"
                      value={draft.mountPoint}
                      onChange={(e) => {
                        patchDraft({ mountPoint: e.target.value });
                        clearError('mountPoint');
                      }}
                    />
                  ) : (
                    <FieldSkeleton />
                  )
                }
              </Field>
            </div>
            <Switch
              label="자동 마운트"
              hint="에이전트가 시작할 때 함께 마운트합니다"
              checked={draft?.autoMount ?? false}
              disabled={!draft}
              onChange={(v) => patchDraft({ autoMount: v })}
            />
            <Switch
              label="읽기 전용"
              hint="원격 파일을 바꾸지 않습니다"
              checked={draft?.readOnly ?? false}
              disabled={!draft}
              onChange={(v) => patchDraft({ readOnly: v })}
            />
          </Section>

          <Section title={`연결 정보${detail ? ` · ${BACKEND_LABEL[detail.backend]}` : ''}`}>
            <dl className="grid grid-cols-[120px_1fr] gap-x-4 gap-y-2.5 text-[13px]">
              {detail
                ? detail.connection.map((f) => (
                    <div key={f.label} className="contents">
                      <dt className="text-ink-3">{f.label}</dt>
                      <dd className="kbd-mono min-w-0 break-all text-[12.5px]">{f.value}</dd>
                    </div>
                  ))
                : [0, 1, 2].map((i) => (
                    <div key={i} className="contents">
                      <dt>
                        <Skeleton className="w-16" />
                      </dt>
                      <dd>
                        <Skeleton className="w-48" />
                      </dd>
                    </div>
                  ))}
            </dl>
            <p className="text-xs text-ink-3">연결 정보를 바꾸려면 드라이브를 지우고 다시 추가하세요</p>
          </Section>

          <Section title="비밀 값">
            {detail ? (
              detail.secrets.length ? (
                detail.secrets.map((s) => (
                  <SecretRow
                    key={s.field}
                    info={s}
                    onChange={() => {
                      setSecretValue('');
                      setSecretFor(s);
                    }}
                  />
                ))
              ) : (
                <p className="text-ink-3">이 드라이브는 비밀 값을 쓰지 않습니다</p>
              )
            ) : (
              <Skeleton className="w-56" />
            )}
          </Section>

          <Section title="프록시">
            {detail?.backend === 'local' ? (
              <p className="text-ink-3">로컬 폴더 드라이브에는 프록시를 쓸 수 없습니다</p>
            ) : (
              <>
                <Switch
                  label="프록시 사용"
                  hint="SOCKS5(socks5h) 또는 HTTP CONNECT"
                  checked={draft?.proxyOn ?? false}
                  disabled={!draft}
                  onChange={(v) => patchDraft({ proxyOn: v })}
                />
                {draft?.proxyOn ? (
                  <>
                    <Field label="프록시 URL" error={errors.proxyUrl}>
                      {(p) => (
                        <input
                          {...p}
                          className="control mono"
                          value={draft.proxyUrl}
                          placeholder="socks5h://proxy.example.com:1080"
                          spellCheck={false}
                          onChange={(e) => {
                            patchDraft({ proxyUrl: e.target.value });
                            clearError('proxyUrl');
                          }}
                        />
                      )}
                    </Field>
                    <div className="grid grid-cols-2 gap-4">
                      <Field label="사용자" hint="선택">
                        {(p) => (
                          <input
                            {...p}
                            className="control"
                            autoComplete="off"
                            value={draft.proxyUser}
                            onChange={(e) => patchDraft({ proxyUser: e.target.value })}
                          />
                        )}
                      </Field>
                      <Field label="새 암호" hint="비우면 그대로">
                        {(p) => (
                          <input
                            {...p}
                            className="control"
                            type="password"
                            autoComplete="new-password"
                            value={draft.proxyPassword}
                            onChange={(e) => patchDraft({ proxyPassword: e.target.value })}
                          />
                        )}
                      </Field>
                    </div>
                  </>
                ) : null}
              </>
            )}
          </Section>

          <Section title="캐시">
            <div className="grid grid-cols-2 gap-4">
              {CACHE_FIELDS.map((f) => num('cache', f.key, f.label, f.unit))}
            </div>
          </Section>

          <Section title="전송 속도 제한">
            <div className="grid grid-cols-2 gap-4">
              {TRANSFER_FIELDS.map((f) => num('transfer', f.key, f.label, 'KiB/s, 0이면 제한 없음'))}
            </div>
          </Section>
        </div>
      )}

      <Modal
        isOpen={secretFor !== null}
        onClose={() => {
          setSecretFor(null);
          setSecretValue('');
        }}
        title={`${secretFor?.label ?? ''} 변경`}
        size="sm"
        draggable={false}
        initialFocusRef={secretInput}
        footer={
          <>
            <button type="button" className="btn btn-ghost" onClick={() => setSecretFor(null)}>
              취소
            </button>
            <button
              type="button"
              className="btn btn-primary"
              disabled={!secretValue || secretSaving}
              onClick={() => void saveSecret()}
            >
              {secretSaving ? <Loader2 size={16} className="spin" aria-hidden="true" /> : null}
              저장
            </button>
          </>
        }
      >
        <form
          className="flex flex-col gap-3"
          onSubmit={(e) => {
            e.preventDefault();
            void saveSecret();
          }}
        >
          <Field label={`새 ${secretFor?.label ?? ''}`}>
            {(p) => (
              <input
                {...p}
                ref={secretInput}
                className="control"
                type="password"
                autoComplete="new-password"
                value={secretValue}
                onChange={(e) => setSecretValue(e.target.value)}
              />
            )}
          </Field>
          <p className="text-xs leading-relaxed text-ink-3">
            값은 화면으로 다시 보내지 않고 자격 증명 관리자에 현재 사용자 전용으로 저장하고, 마운트 중이면 다시 마운트할 때 적용됩니다
          </p>
        </form>
      </Modal>
    </PageFrame>
  );
}

function FieldSkeleton() {
  return (
    <div className="flex h-9 items-center rounded-lg border border-line px-3">
      <Skeleton className="w-24" />
    </div>
  );
}
