// 공용 DataGrid(JS) 형식 선언. props 설명은 data-grid 표준 문서와 같습니다.
import type { ReactElement } from 'react';

export interface DataGridSortState {
  colId: string;
  headerName: string;
  direction: 'asc' | 'desc';
}

export interface DataGridProps {
  rowData: unknown[];
  columnDefs: object[];
  getRowId?: (params: { data: never }) => string;
  onRowClicked?: (event: never) => void;
  onSortChange?: (state: DataGridSortState | null) => void;
  initialSortState?: DataGridSortState | null;
  initialFirstRowIndex?: number;
  onFirstDisplayedRowChange?: (index: number) => void;
  onDisplayedRowsChange?: (rows: unknown[]) => void;
  collapsedGroups?: Set<string>;
  isLoading?: boolean;
  errorMessage?: string;
  emptyMessage?: string;
  headerHeight?: number;
  rowHeight?: number;
  className?: string;
  [gridProp: string]: unknown;
}

export function DataGrid(props: DataGridProps): ReactElement;
export function buildDataGridSortNoteText(
  sortState: DataGridSortState | null,
  defaultText: string,
  columnLabelOverrides?: Record<string, string>,
): string;
