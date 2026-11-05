import { useEffect, useRef, useState } from 'react';
import type { ChangeEvent, ReactNode } from 'react';
import { useNavigate } from 'react-router-dom';
import { CircleAlert, CircleCheck, Cloud, Folder, Loader2, PlugZap, Server, Database, Network } from 'lucide-react';
import type { LucideIcon } from 'lucide-react';
import { Modal } from '../components/Modal';
import SelectControl from '../components/SelectControl';
import { api, errorText } from '../api';
import type { BackendInput, BackendKind, NewDrive, ProxyInput } from '../api';
import { useStatus } from '../status';
import { Field, Skeleton, Switch } from '../ui/controls';
import { useToast } from '../ui/Toast';

const KINDS: { kind: BackendKind; title: string; desc: string; icon: LucideIcon }[] = [
  { kind: 'webdav', title: 'WebDAV', desc: 'Nextcloud, NAS 등 HTTPS 파일 서버', icon: Cloud },
  { kind: 'sftp', title: 'SFTP', desc: 'SSH 서버의 폴더', icon: Server },
  { kind: 's3', title: 'S3 호환', desc: '버킷과 접두사 단위 객체 저장소', icon: Database },
  { kind: 'ftp', title: 'FTP, FTPS', desc: '명시적, 암묵적 TLS 지원', icon: Network },
  { kind: 'local', title: '로컬 폴더', desc: '테스트와 검증용', icon: Folder },
];

const STEPS = ['저장소 종류', '연결 정보', '드라이브'];

const ENCODINGS = ['utf-8', 'euc-kr', 'shift_jis', 'gbk', 'big5', 'iso-8859-1', 'windows-1252'];

type Form = Record<string, string>;

const DEFAULTS: Form = {
  root: '',
  url: '',
  username: '',
  password: '',
  pinnedCertSha256: '',
  host: '',
  port: '',
  privateKey: '',
  keyPassphrase: '',
  remoteRoot: '/',
  knownHosts: '',
  endpoint: '',
  region: 'us-east-1',
  bucket: '',
  prefix: '',
  accessKeyId: '',
  secretAccessKey: '',
  tls: 'explicit',
  encoding: 'utf-8',
};

const DEFAULT_PORT: Partial<Record<BackendKind, string>> = { sftp: '22', ftp: '21' };

/** 단계별 필수 입력 확인. 최종 검증은 Rust 쪽에서 다시 합니다. */
function validateConnection(kind: BackendKind, f: Form, proxyOn: boolean, proxy: ProxyInput): Record<string, string> {
  const e: Record<string, string> = {};
  const need = (key: string, msg: string) => {
    if (!f[key]?.trim()) e[key] = msg;
  };
  const port = () => {
    const n = Number(f.port);
    if (!Number.isInteger(n) || n < 1 || n > 65535) e.port = '1~65535 사이 숫자를 입력하세요';
  };
  switch (kind) {
    case 'local':
      need('root', '폴더 경로를 입력하세요');
      break;
    case 'webdav':
      need('url', 'URL을 입력하세요');
      if (f.url && !/^https?:\/\//i.test(f.url.trim())) e.url = 'https:// 또는 http://로 시작해야 합니다';
      break;
    case 'sftp':
      need('host', '호스트를 입력하세요');
      need('username', '사용자를 입력하세요');
      port();
      if (!f.password && !f.privateKey.trim()) e.password = '암호나 개인키 중 하나가 필요합니다';
      break;
    case 's3':
      need('endpoint', '엔드포인트를 입력하세요');
      need('bucket', '버킷을 입력하세요');
      need('region', '리전을 입력하세요');
      need('accessKeyId', '액세스 키 ID를 입력하세요');
      need('secretAccessKey', '비밀 액세스 키를 입력하세요');
      if (f.endpoint && !/^https?:\/\//i.test(f.endpoint.trim()))
        e.endpoint = 'https:// 또는 http://로 시작해야 합니다';
      break;
    case 'ftp':
      need('host', '호스트를 입력하세요');
      need('username', '사용자를 입력하세요');
      port();
      break;
  }
  if (proxyOn && kind !== 'local') {
    if (!/^(socks5h?|http):\/\/[^/@\s]+:\d+\/?$/i.test(proxy.url.trim()))
      e.proxyUrl = 'socks5://, socks5h:// 또는 http://호스트:포트 형식이어야 합니다';
  }
  return e;
}

function toBackend(kind: BackendKind, f: Form): BackendInput {
  switch (kind) {
    case 'local':
      return { type: 'local', root: f.root };
    case 'webdav':
      return {
        type: 'webdav',
        url: f.url,
        username: f.username,
        password: f.password,
        pinnedCertSha256: f.pinnedCertSha256,
      };
    case 'sftp':
      return {
        type: 'sftp',
        host: f.host,
        port: Number(f.port),
        username: f.username,
        password: f.password,
        privateKey: f.privateKey,
        keyPassphrase: f.keyPassphrase,
        root: f.remoteRoot,
        knownHosts: f.knownHosts,
      };
    case 's3':
      return {
        type: 's3',
        endpoint: f.endpoint,
        region: f.region,
        bucket: f.bucket,
        prefix: f.prefix,
        accessKeyId: f.accessKeyId,
        secretAccessKey: f.secretAccessKey,
        pathStyle: f.pathStyle !== 'false',
      };
    case 'ftp':
      return {
        type: 'ftp',
        host: f.host,
        port: Number(f.port),
        username: f.username,
        password: f.password,
        tls: f.tls as 'none' | 'explicit' | 'implicit',
        encoding: f.encoding,
        root: f.remoteRoot,
      };
  }
}

type TestState = { phase: 'idle' | 'running' | 'ok' | 'fail'; message: string };

export function AddDriveWizard() {
  const navigate = useNavigate();
  const toast = useToast();
  const { refresh } = useStatus();
  const close = () => navigate('/drives');

  const [step, setStep] = useState(0);
  const [kind, setKind] = useState<BackendKind>('webdav');
  const [form, setForm] = useState<Form>({ ...DEFAULTS });
  const [proxyOn, setProxyOn] = useState(false);
  const [proxy, setProxy] = useState<ProxyInput>({ url: '', username: '', password: '' });
  const [id, setId] = useState('');
  const [label, setLabel] = useState('');
  const [letters, setLetters] = useState<string[] | null>(null);
  const [mountSel, setMountSel] = useState('');
  const [folder, setFolder] = useState('');
  const [readOnly, setReadOnly] = useState(false);
  const [autoMount, setAutoMount] = useState(true);
  const [errors, setErrors] = useState<Record<string, string>>({});
  const [test, setTest] = useState<TestState>({ phase: 'idle', message: '' });
  const [saving, setSaving] = useState(false);
  const firstField = useRef<HTMLInputElement>(null);
  const kindField = useRef<HTMLInputElement>(null);

  useEffect(() => {
    api
      .freeLetters()
      .then((ls) => {
        setLetters(ls);
        setMountSel((cur) => cur || ls[0] || 'folder');
      })
      .catch(() => {
        setLetters([]);
        setMountSel('folder');
      });
  }, []);

  useEffect(() => {
    if (step > 0) window.setTimeout(() => firstField.current?.focus(), 0);
  }, [step]);

  const set = (key: string) => (e: ChangeEvent<HTMLInputElement | HTMLSelectElement>) => {
    const value = e.target.value;
    setForm((f) => {
      const next = { ...f, [key]: value };
      // 암묵적 TLS로 바꾸면 기본 포트도 990으로 맞춥니다.
      if (key === 'tls' && kind === 'ftp' && (f.port === '21' || f.port === '990'))
        next.port = value === 'implicit' ? '990' : '21';
      return next;
    });
    setErrors(({ [key]: _, ...rest }) => rest);
    setTest({ phase: 'idle', message: '' });
  };

  const chooseKind = (k: BackendKind) => {
    setKind(k);
    setForm((f) => ({ ...f, port: DEFAULT_PORT[k] ?? '', username: k === 'ftp' && !f.username ? 'anonymous' : f.username }));
    setTest({ phase: 'idle', message: '' });
    setErrors({});
  };

  const proxyInput = (): ProxyInput | null => (proxyOn && kind !== 'local' ? proxy : null);

  const runTest = async () => {
    const e = validateConnection(kind, form, proxyOn, proxy);
    setErrors(e);
    if (Object.keys(e).length) return;
    setTest({ phase: 'running', message: '' });
    try {
      setTest({ phase: 'ok', message: await api.testConnection(toBackend(kind, form), proxyInput()) });
    } catch (err) {
      setTest({ phase: 'fail', message: errorText(err) });
    }
  };

  const validateDrive = () => {
    const e: Record<string, string> = {};
    if (!/^[A-Za-z0-9_-]{1,32}$/.test(id)) e.id = '영문, 숫자, -, _로 1~32자';
    if ([...(label || id)].length > 32) e.label = '32자 이하로 입력하세요';
    if (mountSel === 'folder' && !/^[A-Za-z]:\\/.test(folder.trim())) e.folder = 'C:\\Mounts\\nas 같은 절대 경로를 입력하세요';
    if (!mountSel) e.mount = '마운트 지점을 고르세요';
    return e;
  };

  const next = () => {
    if (step === 1) {
      const e = validateConnection(kind, form, proxyOn, proxy);
      setErrors(e);
      if (Object.keys(e).length) return;
    }
    setStep((s) => s + 1);
  };

  const save = async () => {
    const e = validateDrive();
    setErrors(e);
    if (Object.keys(e).length) return;
    const drive: NewDrive = {
      id,
      label: label.trim() || id,
      mountPoint: mountSel === 'folder' ? folder.trim() : mountSel,
      readOnly,
      autoMount,
      proxy: proxyInput(),
      backend: toBackend(kind, form),
    };
    setSaving(true);
    try {
      await api.addDrive(drive);
      toast.ok(`${id} 드라이브를 추가했습니다`);
      void refresh();
      close();
    } catch (err) {
      toast.error(`저장하지 못했습니다: ${errorText(err)}`);
      setSaving(false);
    }
  };

  const input = (key: string, labelText: string, opts: { type?: string; placeholder?: string; hint?: string; mono?: boolean; first?: boolean } = {}) => (
    <Field label={labelText} error={errors[key]} hint={opts.hint}>
      {(p) => (
        <input
          {...p}
          ref={opts.first ? firstField : undefined}
          className={`control ${opts.mono ? 'mono' : ''}`}
          type={opts.type ?? 'text'}
          value={form[key] ?? ''}
          placeholder={opts.placeholder}
          autoComplete={opts.type === 'password' ? 'new-password' : 'off'}
          spellCheck={false}
          onChange={set(key)}
        />
      )}
    </Field>
  );

  const connectionFields: Record<BackendKind, ReactNode> = {
    local: input('root', '폴더 경로', { placeholder: 'C:\\Data\\share', mono: true, first: true }),
    webdav: (
      <>
        {input('url', 'URL', { placeholder: 'https://dav.example.com/remote.php/dav/files/me/', mono: true, first: true })}
        <div className="grid grid-cols-2 gap-4">
          {input('username', '사용자', { hint: '선택' })}
          {input('password', '암호', { type: 'password', hint: '선택' })}
        </div>
        {input('pinnedCertSha256', '인증서 SHA-256 지문', { hint: '자체 서명 인증서일 때만', mono: true })}
      </>
    ),
    sftp: (
      <>
        <div className="grid grid-cols-[1fr_110px] gap-4">
          {input('host', '호스트', { placeholder: 'sftp.example.com', first: true })}
          {input('port', '포트', { type: 'number' })}
        </div>
        <div className="grid grid-cols-2 gap-4">
          {input('username', '사용자')}
          {input('password', '암호', { type: 'password', hint: '개인키를 쓰면 선택' })}
        </div>
        <div className="grid grid-cols-2 gap-4">
          {input('privateKey', '개인키 파일', { placeholder: 'C:\\Users\\me\\.ssh\\id_ed25519', mono: true, hint: '선택' })}
          {input('keyPassphrase', '개인키 암호문', { type: 'password', hint: '선택' })}
        </div>
        <div className="grid grid-cols-2 gap-4">
          {input('remoteRoot', '원격 루트', { mono: true })}
          {input('knownHosts', 'known_hosts 파일', { hint: '비우면 기본 위치', mono: true })}
        </div>
      </>
    ),
    s3: (
      <>
        {input('endpoint', '엔드포인트', { placeholder: 'https://s3.example.com', mono: true, first: true })}
        <div className="grid grid-cols-2 gap-4">
          {input('bucket', '버킷')}
          {input('region', '리전')}
        </div>
        {input('prefix', '접두사', { hint: '비우면 버킷 루트', placeholder: 'photos/', mono: true })}
        <div className="grid grid-cols-2 gap-4">
          {input('accessKeyId', '액세스 키 ID', { mono: true })}
          {input('secretAccessKey', '비밀 액세스 키', { type: 'password' })}
        </div>
        <Switch
          label="경로 방식 주소"
          hint="MinIO 등 호환 서버는 대개 켜고, 끄면 가상 호스트 방식을 씁니다"
          checked={form.pathStyle !== 'false'}
          onChange={(v) => setForm((f) => ({ ...f, pathStyle: v ? 'true' : 'false' }))}
        />
      </>
    ),
    ftp: (
      <>
        <div className="grid grid-cols-[1fr_110px] gap-4">
          {input('host', '호스트', { placeholder: 'ftp.example.com', first: true })}
          {input('port', '포트', { type: 'number' })}
        </div>
        <div className="grid grid-cols-2 gap-4">
          {input('username', '사용자')}
          {input('password', '암호', { type: 'password', hint: '익명이면 비움' })}
        </div>
        <div className="grid grid-cols-3 gap-4">
          <Field label="TLS">
            {(p) => (
              <SelectControl {...p} value={form.tls} onChange={set('tls')}>
                <option value="explicit">명시적(AUTH TLS)</option>
                <option value="implicit">암묵적(990)</option>
                <option value="none">사용 안 함</option>
              </SelectControl>
            )}
          </Field>
          <Field label="문자셋">
            {(p) => (
              <SelectControl {...p} value={form.encoding} onChange={set('encoding')}>
                {ENCODINGS.map((e) => (
                  <option key={e} value={e}>
                    {e}
                  </option>
                ))}
              </SelectControl>
            )}
          </Field>
          {input('remoteRoot', '원격 루트', { mono: true })}
        </div>
      </>
    ),
  };

  const footer = (
    <div className="flex w-full items-center justify-between gap-3">
      <span className="text-xs text-ink-3">
        {step + 1} / {STEPS.length} 단계
      </span>
      <div className="flex gap-2">
        <button type="button" className="btn btn-ghost" onClick={step === 0 ? close : () => setStep((s) => s - 1)}>
          {step === 0 ? '취소' : '이전'}
        </button>
        {step < STEPS.length - 1 ? (
          <button type="button" className="btn btn-primary" onClick={next}>
            다음
          </button>
        ) : (
          <button type="button" className="btn btn-primary" disabled={saving} onClick={() => void save()}>
            {saving ? <Loader2 size={16} className="spin" aria-hidden="true" /> : null}
            저장
          </button>
        )}
      </div>
    </div>
  );

  return (
    <Modal
      isOpen
      onClose={close}
      title="드라이브 추가"
      size="lg"
      resizable
      storageKey="add-drive"
      initialFocusRef={kindField}
      footer={footer}
    >
      <ol className="mb-5 flex items-center gap-2" aria-label="진행 단계">
        {STEPS.map((s, i) => (
          <li key={s} className="flex flex-1 items-center gap-2" aria-current={i === step ? 'step' : undefined}>
            <span
              className={`kbd-mono grid size-6 flex-none place-items-center rounded-full text-[11px] font-bold ${
                i < step ? 'bg-ok text-white' : i === step ? 'bg-accent text-white' : 'bg-surface-2 text-ink-3'
              }`}
            >
              {i + 1}
            </span>
            <span className={`text-[13px] font-semibold ${i === step ? 'text-ink' : 'text-ink-3'}`}>{s}</span>
            {i < STEPS.length - 1 ? <span className="h-px flex-1 bg-line" aria-hidden="true" /> : null}
          </li>
        ))}
      </ol>

      {step === 0 ? (
        <div role="radiogroup" aria-label="저장소 종류" className="grid grid-cols-2 gap-3">
          {KINDS.map(({ kind: k, title, desc, icon: Icon }) => (
            <label
              key={k}
              className={`flex cursor-pointer items-center gap-3 rounded-xl border p-4 transition-colors has-[:focus-visible]:ring-2 has-[:focus-visible]:ring-[var(--focus-ring)] ${
                kind === k ? 'border-accent bg-accent-soft' : 'border-line hover:bg-surface-2'
              }`}
            >
              <input
                type="radio"
                name="backend-kind"
                ref={kind === k ? kindField : undefined}
                className="sr-only"
                checked={kind === k}
                onChange={() => chooseKind(k)}
              />
              <span
                className={`grid size-10 flex-none place-items-center rounded-lg ${
                  kind === k ? 'bg-accent text-white' : 'bg-surface-2 text-ink-2'
                }`}
              >
                <Icon size={20} aria-hidden="true" />
              </span>
              <span className="min-w-0">
                <span className="block text-sm font-bold text-ink">{title}</span>
                <span className="block text-xs text-ink-3">{desc}</span>
              </span>
            </label>
          ))}
        </div>
      ) : null}

      {step === 1 ? (
        <div className="flex flex-col gap-4">
          {connectionFields[kind]}
          {kind !== 'local' ? (
            <div className="rounded-xl border border-line p-4">
              <Switch
                label="프록시 사용"
                hint="SOCKS5(socks5h) 또는 HTTP CONNECT"
                checked={proxyOn}
                onChange={(v) => {
                  setProxyOn(v);
                  setTest({ phase: 'idle', message: '' });
                }}
              />
              {proxyOn ? (
                <div className="mt-3 grid grid-cols-3 gap-4">
                  <Field label="프록시 URL" error={errors.proxyUrl}>
                    {(p) => (
                      <input
                        {...p}
                        className="control mono"
                        value={proxy.url}
                        placeholder="socks5h://proxy.example.com:1080"
                        spellCheck={false}
                        onChange={(e) => {
                          setProxy((x) => ({ ...x, url: e.target.value }));
                          setErrors(({ proxyUrl: _, ...rest }) => rest);
                        }}
                      />
                    )}
                  </Field>
                  <Field label="사용자" hint="선택">
                    {(p) => (
                      <input
                        {...p}
                        className="control"
                        value={proxy.username}
                        autoComplete="off"
                        onChange={(e) => setProxy((x) => ({ ...x, username: e.target.value }))}
                      />
                    )}
                  </Field>
                  <Field label="암호" hint="선택">
                    {(p) => (
                      <input
                        {...p}
                        className="control"
                        type="password"
                        autoComplete="new-password"
                        value={proxy.password}
                        onChange={(e) => setProxy((x) => ({ ...x, password: e.target.value }))}
                      />
                    )}
                  </Field>
                </div>
              ) : null}
            </div>
          ) : null}

          <div className="flex min-h-[44px] items-start gap-3 rounded-xl bg-surface-2 p-3">
            <button
              type="button"
              className="btn btn-ghost flex-none"
              disabled={test.phase === 'running'}
              onClick={() => void runTest()}
            >
              {test.phase === 'running' ? (
                <Loader2 size={16} className="spin" aria-hidden="true" />
              ) : (
                <PlugZap size={16} aria-hidden="true" />
              )}
              연결 확인
            </button>
            <p className="min-w-0 self-center text-[13px]" role="status" aria-live="polite">
              {test.phase === 'idle' ? (
                <span className="text-ink-3">저장하기 전에 접속과 루트 목록 조회를 확인합니다</span>
              ) : test.phase === 'running' ? (
                <span className="text-ink-2">연결하는 중입니다</span>
              ) : test.phase === 'ok' ? (
                <span className="inline-flex items-center gap-1.5 font-semibold text-ok">
                  <CircleCheck size={16} aria-hidden="true" />
                  {test.message}
                </span>
              ) : (
                <span className="inline-flex items-start gap-1.5 break-all text-danger">
                  <CircleAlert size={16} className="mt-0.5 flex-none" aria-hidden="true" />
                  {test.message}
                </span>
              )}
            </p>
          </div>
        </div>
      ) : null}

      {step === 2 ? (
        <div className="flex flex-col gap-4">
          <div className="grid grid-cols-2 gap-4">
            <Field label="드라이브 ID" hint="영문, 숫자, -, _" error={errors.id}>
              {(p) => (
                <input
                  {...p}
                  ref={firstField}
                  className="control mono"
                  value={id}
                  placeholder="nas"
                  maxLength={32}
                  spellCheck={false}
                  onChange={(e) => {
                    setId(e.target.value);
                    setErrors(({ id: _, ...rest }) => rest);
                  }}
                />
              )}
            </Field>
            <Field label="레이블" hint="탐색기에 보일 이름" error={errors.label}>
              {(p) => (
                <input
                  {...p}
                  className="control"
                  value={label}
                  placeholder={id || 'NAS'}
                  maxLength={32}
                  onChange={(e) => setLabel(e.target.value)}
                />
              )}
            </Field>
          </div>
          <div className="grid grid-cols-2 gap-4">
            <Field label="마운트 지점" error={errors.mount}>
              {(p) =>
                letters === null ? (
                  <div className="flex h-9 items-center rounded-lg border border-line px-3">
                    <Skeleton className="w-20" />
                  </div>
                ) : (
                  <SelectControl {...p} value={mountSel} onChange={(e) => setMountSel(e.target.value)}>
                    {letters.map((l) => (
                      <option key={l} value={l}>
                        {l} 드라이브
                      </option>
                    ))}
                    <option value="folder">빈 폴더 경로에 연결</option>
                  </SelectControl>
                )
              }
            </Field>
            {mountSel === 'folder' ? (
              <Field label="폴더 경로" hint="없는 폴더" error={errors.folder}>
                {(p) => (
                  <input
                    {...p}
                    className="control mono"
                    value={folder}
                    placeholder="C:\Mounts\nas"
                    onChange={(e) => {
                      setFolder(e.target.value);
                      setErrors(({ folder: _, ...rest }) => rest);
                    }}
                  />
                )}
              </Field>
            ) : null}
          </div>
          <div className="flex flex-col gap-2 rounded-xl border border-line p-4">
            <Switch label="자동 마운트" hint="에이전트가 시작할 때 함께 마운트합니다" checked={autoMount} onChange={setAutoMount} />
            <Switch label="읽기 전용" hint="원격 파일을 바꾸지 않습니다" checked={readOnly} onChange={setReadOnly} />
          </div>
          <p className="text-xs leading-relaxed text-ink-3">
            비밀 값은 설정 파일에 쓰지 않고 Windows 자격 증명 관리자에 현재 사용자 전용으로 저장합니다
          </p>
        </div>
      ) : null}
    </Modal>
  );
}
