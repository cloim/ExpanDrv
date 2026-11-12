import { useRef, useState } from 'react';
import { Loader2 } from 'lucide-react';
import { Modal } from '../components/Modal';
import SelectControl from '../components/SelectControl';
import type { DriveDetail } from '../api';
import { api, errorText } from '../api';
import { Field } from '../ui/controls';
import { RecoveryPanel } from '../ui/RecoveryPanel';
import { useToast } from '../ui/Toast';

const LABELS: Record<string, string> = { useSshConfig: '로컬 SSH 설정 사용', root: '루트 폴더', url: 'URL', host: '호스트', port: '포트', username: '사용자', pinnedCertSha256: '인증서 SHA-256 지문', privateKey: '개인키 파일', knownHosts: 'known_hosts 파일', endpoint: '엔드포인트', region: '리전', bucket: '버킷', prefix: '접두사', accessKeyId: '액세스 키 ID', pathStyle: '주소 방식', tls: 'TLS', encoding: '문자셋' };
const OPTIONS: Record<string, [string, string][]> = { useSshConfig: [['true', '사용'], ['false', '사용 안 함']], pathStyle: [['true', '경로 방식'], ['false', '가상 호스트']], tls: [['none', '사용 안 함'], ['explicit', '명시적 TLS'], ['implicit', '암묵적 TLS']] };

export function ConnectionEditor({ detail, onClose, onSaved, onCredentials }: { detail: DriveDetail; onClose: () => void; onSaved: () => Promise<void>; onCredentials: () => void }) {
  const [fields, setFields] = useState(detail.connectionEdit);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const toast = useToast();
  const firstField = useRef<HTMLInputElement>(null);
  const primary = detail.backend === 'webdav' ? 'url' : detail.backend === 's3' ? 'endpoint' : detail.backend === 'local' ? 'root' : 'host';
  const dirty = JSON.stringify(fields) !== JSON.stringify(detail.connectionEdit);
  const save = async () => {
    if (busy || !dirty) return;
    setBusy(true);
    setError('');
    try {
      const message = await api.updateConnection(detail.id, fields);
      await onSaved();
      toast.ok(`${message} | 연결 정보를 저장했습니다`);
      onClose();
    } catch (e) { setError(errorText(e)); }
    finally { setBusy(false); }
  };
  return <Modal isOpen onClose={() => { if (!busy) onClose(); }} title="연결 정보 수정" size="md" draggable={false} initialFocusRef={firstField} closeOnEscape={!busy} closeOnOverlayClick={!busy}
    footer={<><button type="button" className="btn btn-ghost" disabled={busy} onClick={onClose}>취소</button><button type="button" className="btn btn-primary" disabled={busy || !dirty} onClick={() => void save()}>{busy ? <Loader2 size={15} className="spin" aria-hidden="true" /> : null}{busy ? '연결 확인 중' : '검사 후 저장'}</button></>}>
    <div className="flex flex-col gap-4">
      <p className="text-xs leading-relaxed text-ink-2">접속과 루트 조회가 성공하면 저장합니다<br />저장된 자격 증명은 유지하고 기존 캐시는 별도 폴더에 보관합니다</p>
      {error ? <RecoveryPanel message={error} busy={busy} onAction={(action) => { if (action === 'credentials') { onCredentials(); } else if (action === 'retry') { void save(); } else { document.getElementById('connection-fields')?.querySelector('input')?.focus(); } }} /> : null}
      <div id="connection-fields" className="grid grid-cols-1 gap-4 sm:grid-cols-2">
        {Object.entries(fields).filter(([key]) => !(detail.backend === 'ftp' && fields.tls === 'none' && key === 'pinnedCertSha256') && !(detail.backend === 'sftp' && fields.useSshConfig === 'true' && ['port', 'username', 'privateKey', 'knownHosts'].includes(key))).map(([key, value]) => <Field key={key} label={LABELS[key] ?? key} hint={key === 'pinnedCertSha256' ? '서버 관리자에게 확인한 지문, 선택' : ['privateKey', 'knownHosts', 'prefix'].includes(key) ? '선택' : undefined}>
          {(p) => OPTIONS[key] ? <SelectControl {...p} disabled={busy} value={value} onChange={(e) => setFields((f) => ({ ...f, [key]: e.target.value, ...(detail.backend === 'ftp' && key === 'tls' && e.target.value === 'none' ? { pinnedCertSha256: '' } : {}) }))}>{OPTIONS[key].map(([v, label]) => <option key={v} value={v}>{label}</option>)}</SelectControl> : <input {...p} ref={key === primary ? firstField : undefined} type={key === 'port' ? 'number' : 'text'} min={key === 'port' ? 1 : undefined} max={key === 'port' ? 65535 : undefined} className="control mono" maxLength={2048} disabled={busy} value={value} onChange={(e) => setFields((f) => ({ ...f, [key]: e.target.value }))} />}
        </Field>)}
      </div>
    </div>
  </Modal>;
}
