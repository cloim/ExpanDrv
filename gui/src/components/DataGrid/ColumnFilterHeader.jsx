import React, { useRef, useState } from 'react';
import { ChevronDown, ChevronUp, Filter } from 'lucide-react';
import { ColumnFilterPopover } from './ColumnFilterPopover.jsx';

/**
 * AG Grid 커스텀 headerComponent.
 *
 * 그리드가 제공하는 기본 헤더(리사이즈, 키보드 내비게이션 등)는 그대로 감싸는 셀이 담당하고,
 * 이 컴포넌트는 그 안쪽 내용(헤더명 + 정렬 표시 + 필터 버튼)만 구성한다. 정렬은 AG Grid가
 * 넘겨주는 setSort 콜백을 그대로 호출해 기존 정렬 로직을 그대로 재사용한다(직접 구현하지 않음).
 *
 * colDef.filterable === true인 컬럼에만 오른쪽에 필터 버튼을 표시한다. 필터 버튼을 누르면
 * ColumnFilterPopover가 열리고, 그 컬럼의 고유값 체크박스로 값을 골라 rowData를 좁힐 수 있다.
 * 실제 필터 상태 저장과 적용은 DataGrid가 담당하며(params.context.columnFilterApi), 이 헤더는
 * 그 API를 호출만 한다.
 */
export function ColumnFilterHeader(props) {
  const { displayName, column, enableSorting, setSort, context } = props;
  const [sortDirection, setSortDirection] = useState(column.getSort());
  const [isPopoverOpen, setIsPopoverOpen] = useState(false);
  const [anchorRect, setAnchorRect] = useState(null);
  const filterButtonRef = useRef(null);

  // AG Grid가 정렬 변경 시 헤더를 다시 그리지 않을 수 있어(같은 컴포넌트 인스턴스 재사용),
  // 컬럼 이벤트로 직접 갱신한다.
  React.useEffect(() => {
    const onSortChanged = () => setSortDirection(column.getSort());
    column.addEventListener('sortChanged', onSortChanged);
    return () => column.removeEventListener('sortChanged', onSortChanged);
  }, [column]);

  const field = column.getColDef().field;
  const filterable = Boolean(column.getColDef().filterable);
  const columnFilterApi = context?.columnFilterApi;
  const isFilterActive = columnFilterApi ? columnFilterApi.isActive(field) : false;

  const onHeaderClick = () => {
    if (enableSorting) {
      setSort(sortDirection === 'asc' ? 'desc' : sortDirection === 'desc' ? null : 'asc');
    }
  };

  const openFilterPopover = (event) => {
    event.stopPropagation();
    if (filterButtonRef.current) {
      setAnchorRect(filterButtonRef.current.getBoundingClientRect());
    }
    setIsPopoverOpen(true);
  };

  const closeFilterPopover = () => {
    setIsPopoverOpen(false);
    setAnchorRect(null);
  };

  const applyFilter = (nextSelection) => {
    columnFilterApi?.setFilter(field, nextSelection);
    closeFilterPopover();
  };

  return (
    <div className="column-filter-header">
      <button type="button" className="column-filter-header-label" onClick={onHeaderClick} disabled={!enableSorting}>
        <span>{displayName}</span>
        {sortDirection === 'asc' ? <ChevronUp size={13} aria-hidden="true" className="column-filter-sort-icon" /> : null}
        {sortDirection === 'desc' ? <ChevronDown size={13} aria-hidden="true" className="column-filter-sort-icon" /> : null}
      </button>
      {filterable ? (
        <button
          type="button"
          ref={filterButtonRef}
          className={isFilterActive ? 'column-filter-trigger is-active' : 'column-filter-trigger'}
          onClick={openFilterPopover}
          aria-label={`${displayName} 필터`}
        >
          <Filter size={12} aria-hidden="true" />
        </button>
      ) : null}
      {isPopoverOpen && columnFilterApi
        ? (
          <ColumnFilterPopover
            anchorRect={anchorRect}
            options={columnFilterApi.getOptions(field)}
            filterState={columnFilterApi.getFilterState(field)}
            onApply={applyFilter}
            onClose={closeFilterPopover}
          />
        )
        : null}
    </div>
  );
}
