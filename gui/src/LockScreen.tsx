import { useState } from 'react';
import { LockKeyhole, Loader2 } from 'lucide-react';
import { api, errorText } from './api';
import { useStatus } from './status';
import { Field } from './ui/controls';

export function LockScreen() {
  const { status, refresh } = useStatus();
  const [pin, setPin] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const locking = status?.locking ?? false;

  const unlock = async () => {
    if (busy || locking) return;
    const submitted = pin;
    setPin('');
    setBusy(true);
    setError('');
    try {
      await api.unlock(submitted);
      await refresh();
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="flex flex-1 items-center justify-center p-6">
      <form className="panel w-full max-w-sm space-y-6 p-7" onSubmit={(e) => { e.preventDefault(); void unlock(); }}>
        <div className="flex items-center gap-3">
          <LockKeyhole size={24} className="text-accent" aria-hidden="true" />
          <h1 className="text-xl font-bold">{locking ? '드라이브를 잠그는 중' : '드라이브 잠김'}</h1>
        </div>
        <p className="text-sm leading-relaxed text-ink-2" aria-live="polite">
          {locking ? '드라이브를 해제하고 남은 업로드를 정리하고 있습니다' : 'PIN으로 잠금을 해제한 뒤 필요한 드라이브를 마운트하세요'}
        </p>
        <Field label="잠금 PIN">
          {(p) => <input {...p} type="password" inputMode="numeric" autoComplete="current-password" className="control" value={pin} maxLength={12} minLength={6} pattern="[0-9]{6,12}" required disabled={busy || locking} onChange={(e) => setPin(e.target.value)} />}
        </Field>
        {error ? <p role="alert" className="text-sm text-danger">{error}</p> : null}
        <button type="submit" className="btn btn-primary w-full" disabled={busy || locking || pin.length < 6} aria-busy={busy || locking}>
          {busy || locking ? <Loader2 size={16} className="spin" aria-hidden="true" /> : <LockKeyhole size={16} aria-hidden="true" />}
          {locking ? '업로드 정리 중' : busy ? '확인 중' : '잠금 해제'}
        </button>
      </form>
    </div>
  );
}

export function PinSettings() {
  const { status, refresh } = useStatus();
  const [current, setCurrent] = useState('');
  const [pin, setPin] = useState('');
  const [confirmation, setConfirmation] = useState('');
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState('');
  const [error, setError] = useState('');
  const configured = status?.pinConfigured ?? false;
  const save = async () => {
    if (busy) return;
    setError('');
    setMessage('');
    if (pin !== confirmation) { setError('새 PIN 확인이 일치하지 않습니다'); return; }
    const submitted = { current, pin };
    setCurrent(''); setPin(''); setConfirmation('');
    setBusy(true);
    try {
      await api.setPin(submitted.current, submitted.pin);
      setMessage('PIN을 저장했습니다');
      await refresh();
    } catch (e) { setError(errorText(e)); }
    finally { setBusy(false); }
  };
  const field = (label: string, value: string, change: (value: string) => void, existing = false) => (
    <Field label={label}>
      {(p) => <input {...p} className="control" type="password" inputMode="numeric" autoComplete={existing ? 'current-password' : 'new-password'} value={value} minLength={6} maxLength={12} pattern="[0-9]{6,12}" required disabled={busy || !status} onChange={(e) => change(e.target.value)} />}
    </Field>
  );
  return (
    <form className="space-y-4" onSubmit={(e) => { e.preventDefault(); void save(); }}>
      <p className="text-sm leading-relaxed text-ink-2">숫자 6~12자리 PIN으로 전체 드라이브를 잠글 수 있습니다<br />PIN 등록 후 앱을 다시 시작하면 잠긴 상태로 시작합니다</p>
      {configured ? field('현재 PIN', current, setCurrent, true) : null}
      {field('새 PIN', pin, setPin)}
      {field('새 PIN 확인', confirmation, setConfirmation)}
      {error ? <p role="alert" className="text-sm text-danger">{error}</p> : null}
      {message ? <p role="status" className="text-sm text-ink-2">{message}</p> : null}
      <button type="submit" className="btn btn-primary" disabled={busy || !status} aria-busy={busy}>
        {busy ? <Loader2 size={16} className="spin" aria-hidden="true" /> : null}
        {busy ? '저장 중' : configured ? 'PIN 변경' : 'PIN 등록'}
      </button>
    </form>
  );
}
