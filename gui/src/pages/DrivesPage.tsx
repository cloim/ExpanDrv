import { useMemo, useRef, useState } from 'react';
import type { ReactNode } from 'react';
import { Outlet, useNavigate } from 'react-router-dom';
import { Check, Minus, Play, Plus, RefreshCw, Settings2, Square, Trash2, TriangleAlert } from 'lucide-react';
import { DataGrid } from '../components/DataGrid';
import { Modal } from '../components/Modal';
import { api, BACKEND_LABEL, errorText, STATE_LABEL } from '../api';
import type { DriveRow } from '../api';
import { PageFrame } from '../shell/AppShell';
import { useStatus } from '../status';
import { IconButton, Led, Skeleton } from '../ui/controls';
import { useToast } from '../ui/Toast';

type Busy = Record<string, 'mount' | 'unmount' | 'delete'>;

function stateText(d: DriveRow) {
  if (d.state === 'mounted' && d.pendingUploads > 0) return '업로드 중';
  if (d.state === 'unmounted' && d.pendingUploads > 0) return '해제 중';
  return STATE_LABEL[d.state];
}

function StateCell({ data }: { data: DriveRow }) {
  const busy = data.pendingUploads > 0;
  return (
    <span className="inline-flex items-center gap-2">
      <Led state={data.state === 'mounted' ? 'on' : data.state === 'failed' ? 'fail' : 'off'} busy={busy} />
      <span className={data.state === 'failed' ? 'font-semibold text-danger' : ''}>{stateText(data)}</span>
    </span>
  );
}

/** 에이전트 상태 줄. 높이를 고정해 상태가 바뀌어도 목록 위치가 흔들리지 않습니다. */
function AgentStrip() {
  const { status, loading, error } = useStatus();
  let body: ReactNode;
  if (loading) body = <Skeleton className="w-72" />;
  else if (error) body = <span className="text-danger">에이전트 상태를 읽지 못했습니다: {error}</span>;
  else if (status?.configError)
    body = <span className="text-danger">설정 파일 오류: {status.configError}</span>;
  else if (!status?.running)
    body = (
      <span className="text-ink-2">
        에이전트가 실행 중이 아니며, 마운트하면 백그라운드로 자동 시작합니다
      </span>
    );
  else {
    const mounted = status.drives.filter((d) => d.state === 'mounted').length;
    const pending = status.drives.reduce((n, d) => n + d.pendingUploads, 0);
    body = (
      <span className="flex flex-wrap items-center gap-x-4 text-ink-2">
        <span>
          에이전트 실행 중
          {status.agentVersion ? <span className="kbd-mono ml-1.5 text-ink-3">v{status.agentVersion}</span> : null}
        </span>
        <span>
          마운트 <strong className="kbd-mono text-ink">{mounted}</strong>
          <span className="text-ink-3"> / {status.drives.length}</span>
        </span>
        <span>
          업로드 대기 <strong className="kbd-mono text-ink">{pending}</strong>
        </span>
      </span>
    );
  }
  const tone = error || status?.configError ? 'fail' : status?.running ? 'on' : 'off';
  return (
    <div
      className="panel flex h-11 flex-none items-center gap-3 px-4 text-[13px]"
      role="status"
      aria-live="polite"
    >
      {tone === 'fail' ? (
        <TriangleAlert size={16} className="flex-none text-danger" aria-hidden="true" />
      ) : (
        <Led state={tone} />
      )}
      <div className="min-w-0 truncate">{body}</div>
    </div>
  );
}

export function DrivesPage() {
  const { status, loading, error, refresh, refreshing } = useStatus();
  const toast = useToast();
  const navigate = useNavigate();
  const [busy, setBusy] = useState<Busy>({});
  const [mountingAll, setMountingAll] = useState(false);
  const [confirm, setConfirm] = useState<DriveRow | null>(null);
  const cancelRef = useRef<HTMLButtonElement>(null);

  const rows = useMemo(() => status?.drives ?? [], [status]);

  const run = async (id: string, kind: Busy[string], work: () => Promise<string>) => {
    setBusy((b) => ({ ...b, [id]: kind }));
    try {
      toast.ok(await work());
    } catch (e) {
      toast.error(`${id}: ${errorText(e)}`);
    } finally {
      setBusy(({ [id]: _, ...rest }) => rest);
      void refresh();
    }
  };

  const mount = (d: DriveRow) =>
    run(d.id, 'mount', async () => {
      await api.mount(d.id);
      return `${d.id}를 ${d.mountPoint}에 마운트했습니다`;
    });
  const unmount = (d: DriveRow) =>
    run(d.id, 'unmount', async () => {
      await api.unmount(d.id);
      return `${d.id} 마운트를 해제했습니다`;
    });
  const remove = (d: DriveRow) => {
    setConfirm(null);
    void run(d.id, 'delete', async () => {
      const n = await api.deleteDrive(d.id);
      return n > 0 ? `${d.id}와 저장한 비밀 값 ${n}개를 삭제했습니다` : `${d.id}를 삭제했습니다`;
    });
  };

  const mountAll = async () => {
    setMountingAll(true);
    try {
      const results = await api.mountAll();
      const failed = results.filter((r) => r.error);
      if (failed.length === 0) toast.ok(`드라이브 ${results.length}개를 마운트했습니다`);
      for (const f of failed) toast.error(`${f.id}: ${f.error}`);
    } catch (e) {
      toast.error(errorText(e));
    } finally {
      setMountingAll(false);
      void refresh();
    }
  };

  const columns = useMemo(
    () => [
      {
        field: 'state',
        headerName: '상태',
        width: 112,
        filterable: true,
        filterValueGetter: (d: DriveRow) => stateText(d),
        cellRenderer: StateCell,
        tooltipValueGetter: ({ data }: { data: DriveRow }) => data.message ?? stateText(data),
      },
      {
        field: 'mountPoint',
        headerName: '지점',
        width: 84,
        cellRenderer: ({ data }: { data: DriveRow }) => (
          <span className={`bay ${data.state === 'mounted' ? 'is-on' : ''}`}>{data.mountPoint}</span>
        ),
      },
      {
        field: 'label',
        headerName: '레이블',
        minWidth: 120,
        flex: 1,
        cellRenderer: ({ data }: { data: DriveRow }) => (
          <span className="font-semibold">
            {data.label}
            {data.configured ? null : <span className="ml-2 text-xs font-normal text-ink-3">(설정에서 삭제됨)</span>}
          </span>
        ),
      },
      {
        field: 'id',
        headerName: 'ID',
        width: 132,
        cellRenderer: ({ value }: { value: string }) => <span className="kbd-mono text-[12.5px]">{value}</span>,
      },
      {
        field: 'backend',
        headerName: '종류',
        width: 100,
        filterable: true,
        valueFormatter: ({ value }: { value: string }) => BACKEND_LABEL[value] ?? value,
        filterValueGetter: (d: DriveRow) => BACKEND_LABEL[d.backend] ?? d.backend,
      },
      {
        field: 'pendingUploads',
        headerName: '업로드 대기',
        width: 96,
        cellRenderer: ({ value }: { value: number }) => (
          <span className={`kbd-mono ${value > 0 ? 'font-bold text-warn' : 'text-ink-3'}`}>{value}</span>
        ),
      },
      {
        field: 'autoMount',
        headerName: '자동 마운트',
        width: 96,
        valueFormatter: ({ value }: { value: boolean }) => (value ? '켜짐' : '꺼짐'),
        cellRenderer: ({ value }: { value: boolean }) =>
          value ? (
            <Check size={16} className="text-ok" aria-label="켜짐" />
          ) : (
            <Minus size={16} className="text-ink-3" aria-label="꺼짐" />
          ),
      },
      {
        colId: 'actions',
        headerName: '작업',
        width: 118,
        sortable: false,
        tooltipValueGetter: () => undefined,
        cellRenderer: ({ data }: { data: DriveRow }) => {
          const b = busy[data.id];
          const mounted = data.state === 'mounted';
          return (
            <span className="inline-flex items-center gap-0.5">
              {mounted ? (
                <IconButton
                  small
                  label={`${data.id} 마운트 해제`}
                  icon={Square}
                  busy={b === 'unmount'}
                  disabled={!!b}
                  onClick={() => unmount(data)}
                />
              ) : (
                <IconButton
                  small
                  label={`${data.id} 마운트`}
                  icon={Play}
                  busy={b === 'mount'}
                  disabled={!!b || !data.configured}
                  onClick={() => mount(data)}
                />
              )}
              <IconButton
                small
                label={`${data.id} 속성`}
                icon={Settings2}
                disabled={!data.configured}
                onClick={() => navigate(`/drives/${encodeURIComponent(data.id)}`)}
              />
              <IconButton
                small
                tone="danger"
                label={mounted ? '마운트를 해제한 뒤 삭제할 수 있습니다' : `${data.id} 삭제`}
                icon={Trash2}
                busy={b === 'delete'}
                disabled={!!b || mounted || !data.configured}
                onClick={() => setConfirm(data)}
              />
            </span>
          );
        },
      },
    ],
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [busy],
  );

  return (
    <PageFrame
      title="드라이브"
      actions={
        <>
          <IconButton label="드라이브 추가" icon={Plus} tone="accent" onClick={() => navigate('/drives/new')} />
          <IconButton label="새로 고침" icon={RefreshCw} busy={refreshing && !loading} onClick={() => void refresh()} />
          <IconButton
            label="전체 마운트"
            icon={Play}
            busy={mountingAll}
            disabled={rows.length === 0}
            onClick={() => void mountAll()}
          />
        </>
      }
    >
      <div className="rise flex min-h-0 flex-1 flex-col gap-3 p-5">
        <AgentStrip />
        <div className="relative min-h-[300px] flex-1">
          <div className="absolute inset-0 flex">
            <DataGrid
              rowData={rows}
              columnDefs={columns}
              getRowId={({ data }: { data: DriveRow }) => data.id}
              isLoading={loading}
              errorMessage={status ? '' : error}
              emptyMessage="드라이브가 없으니 오른쪽 위 + 버튼으로 추가하세요"
              rowHeight={52}
              onRowClicked={({ data, event }: { data: DriveRow; event: MouseEvent }) => {
                const target = event.target as HTMLElement | null;
                if (data.configured && !target?.closest('button')) navigate(`/drives/${encodeURIComponent(data.id)}`);
              }}
            />
          </div>
        </div>
      </div>

      <Modal
        isOpen={confirm !== null}
        onClose={() => setConfirm(null)}
        title="드라이브 삭제"
        size="sm"
        draggable={false}
        initialFocusRef={cancelRef}
        footer={
          <>
            <button ref={cancelRef} type="button" className="btn btn-ghost" onClick={() => setConfirm(null)}>
              취소
            </button>
            <button type="button" className="btn btn-danger" onClick={() => confirm && remove(confirm)}>
              삭제
            </button>
          </>
        }
      >
        {confirm ? (
          <p className="leading-relaxed">
            <strong className="kbd-mono">{confirm.id}</strong> ({confirm.label}) 드라이브를 삭제할까요?
            <br />
            설정과 이 드라이브용으로 저장한 비밀 값이 함께 지워집니다
            <br />
            원격 저장소의 파일은 그대로 남습니다
          </p>
        ) : null}
      </Modal>

      <Outlet />
    </PageFrame>
  );
}
