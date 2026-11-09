import { Loader2, TriangleAlert } from 'lucide-react';
import { recoveryAdvice } from '../recovery';

export function RecoveryPanel({ message, onAction, busy = false }: { message: string; onAction: (action: ReturnType<typeof recoveryAdvice>['action']) => void; busy?: boolean }) {
  const advice = recoveryAdvice(message);
  return <div role="alert" className="panel flex flex-wrap items-start gap-3 border-warn p-4">
    <TriangleAlert size={18} className="mt-0.5 flex-none text-warn" aria-hidden="true" />
    <div className="min-w-0 flex-1 basis-52">
      <p className="font-semibold text-ink">{advice.title}</p>
      <p className="mt-1 text-xs leading-relaxed text-ink-2">{advice.text}</p>
      <details className="mt-2 text-xs text-ink-2"><summary className="cursor-pointer">오류 원문</summary><p className="mt-2 break-all">{message}</p></details>
    </div>
    <button type="button" className="btn btn-ghost" disabled={busy} onClick={() => onAction(advice.action)}>
      {busy ? <Loader2 size={15} className="spin" aria-hidden="true" /> : null}
      {advice.action === 'retry' ? '다시 검사' : advice.action === 'credentials' ? '자격 증명 확인' : advice.action === 'mountPoint' ? '마운트 지점 확인' : '연결 정보 확인'}
    </button>
  </div>;
}
