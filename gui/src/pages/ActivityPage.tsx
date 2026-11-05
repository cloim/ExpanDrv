import { useMemo } from 'react';
import { Eraser } from 'lucide-react';
import { DataGrid } from '../components/DataGrid';
import { PageFrame } from '../shell/AppShell';
import { useStatus } from '../status';
import type { ActivityEvent } from '../status';
import { IconButton, Led, Skeleton } from '../ui/controls';

const KIND_LABEL: Record<ActivityEvent['kind'], string> = {
  state: '상태',
  uploads: '업로드',
  agent: '에이전트',
};

const time = (d: Date) =>
  d.toLocaleTimeString('ko-KR', { hour12: false, hour: '2-digit', minute: '2-digit', second: '2-digit' });

/** 드라이브별 현재 상태와, 이 창을 연 뒤 바뀐 상태와 업로드 대기 기록 */
export function ActivityPage() {
  const { status, loading, events, clearEvents } = useStatus();

  const columns = useMemo(
    () => [
      {
        field: 'at',
        headerName: '시각',
        width: 110,
        valueFormatter: ({ value }: { value: Date }) => time(value),
        comparator: (a: Date, b: Date) => a.getTime() - b.getTime(),
        cellRenderer: ({ value }: { value: Date }) => <span className="kbd-mono text-[12.5px]">{time(value)}</span>,
      },
      {
        field: 'driveId',
        headerName: '드라이브',
        width: 150,
        filterable: true,
        filterValueGetter: (e: ActivityEvent) => e.driveId || '(에이전트)',
        cellRenderer: ({ value }: { value: string }) =>
          value ? <span className="kbd-mono text-[12.5px]">{value}</span> : <span className="text-ink-3">-</span>,
      },
      {
        field: 'kind',
        headerName: '구분',
        width: 110,
        filterable: true,
        valueFormatter: ({ value }: { value: ActivityEvent['kind'] }) => KIND_LABEL[value],
        filterValueGetter: (e: ActivityEvent) => KIND_LABEL[e.kind],
      },
      { field: 'text', colId: 'label', headerName: '내용', minWidth: 200, flex: 1 },
      { field: 'detail', headerName: '세부', minWidth: 200, flex: 1 },
    ],
    [],
  );

  const drives = status?.drives ?? [];

  return (
    <PageFrame
      title="활동"
      actions={<IconButton label="기록 지우기" icon={Eraser} disabled={events.length === 0} onClick={clearEvents} />}
    >
      <div className="rise flex min-h-0 flex-1 flex-col gap-3 p-5">
        <ul className="flex flex-none flex-wrap gap-2" aria-label="드라이브별 현재 상태">
          {loading
            ? [0, 1, 2].map((i) => (
                <li key={i} className="panel flex h-11 w-44 items-center px-4">
                  <Skeleton className="w-28" />
                </li>
              ))
            : drives.map((d) => (
                <li key={d.id} className="panel flex h-11 items-center gap-2.5 px-4 text-[13px]">
                  <Led
                    state={d.state === 'mounted' ? 'on' : d.state === 'failed' ? 'fail' : 'off'}
                    busy={d.pendingUploads > 0 || d.activeDownloads > 0}
                  />
                  <span className="kbd-mono font-semibold">{d.id}</span>
                  <span className="kbd-mono text-ink-3">{d.mountPoint}</span>
                  <span className={d.pendingUploads > 0 ? 'font-semibold text-warn' : 'text-ink-3'}>
                    업로드 대기 {d.pendingUploads}
                  </span>
                  <span className={d.activeDownloads > 0 ? 'font-semibold text-accent' : 'text-ink-3'}>
                    다운로드 {d.activeDownloads}
                  </span>
                </li>
              ))}
          {!loading && drives.length === 0 ? (
            <li className="panel flex h-11 items-center px-4 text-ink-3">설정된 드라이브가 없습니다</li>
          ) : null}
        </ul>
        <p className="sr-only" aria-live="polite">
          활동 기록 {events.length}건
        </p>
        <div className="relative min-h-[260px] flex-1">
          <div className="absolute inset-0 flex">
            <DataGrid
              rowData={events}
              columnDefs={columns}
              getRowId={({ data }: { data: ActivityEvent }) => String(data.seq)}
              emptyMessage="아직 기록이 없으며, 마운트와 해제, 업로드 대기 변화가 여기에 쌓입니다"
              rowHeight={44}
            />
          </div>
        </div>
      </div>
    </PageFrame>
  );
}
