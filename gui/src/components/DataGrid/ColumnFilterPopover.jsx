import React, { useEffect, useMemo, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
import { Search, X } from 'lucide-react';
import { COLUMN_FILTER_CONDITION_OPERATORS } from './columnFilterOperators.js';

/**
 * 엑셀 스타일 컬럼 필터 팝업.
 *
 * AG Grid 헤더/셀과 같은 이유로 그리드 컨테이너 바깥(document.body)에 포털로 렌더링해
 * overflow:hidden에 잘리지 않게 하고, 버튼 위치를 기준으로 고정 좌표에 띄운다.
 *
 * 두 가지 모드를 탭으로 제공한다(엑셀의 "필터" + "텍스트/숫자 필터"와 동일한 구성):
 *   - 값 목록: 고유값을 체크박스로 골라낸다(검색으로 목록을 좁힐 수 있음).
 *   - 조건: 포함/이상/초과/이하/미만 중 하나를 골라 값을 입력해 비교한다.
 *
 * @param anchorRect   필터 버튼의 getBoundingClientRect() 결과. 팝업 위치 기준.
 * @param options      [{ value, label, count }] 형태의 전체 값 목록(그 컬럼의 고유값, 다른
 *                     컬럼 필터까지 반영된 행 기준으로 계산해 넘겨준다).
 * @param filterState  현재 적용된 필터 상태. { mode: 'values', selection: Set } |
 *                     { mode: 'condition', op, value } | null(필터 없음).
 * @param onApply      적용 버튼 클릭 시 다음 필터 상태 또는 null(전체)을 전달.
 * @param onClose      팝업 닫기(바깥 클릭, ESC, 취소).
 */
export function ColumnFilterPopover({ anchorRect, options, filterState, onApply, onClose }) {
  const [activeTab, setActiveTab] = useState(filterState?.mode === 'condition' ? 'condition' : 'values');
  const [keyword, setKeyword] = useState('');
  // 검색어를 입력하지 않은 상태의 "원래 선택값". 검색으로 목록을 좁혔다가 지우면 이 값으로
  // 되돌리기 위해 별도로 들고 있는다(검색 중 자동으로 좁혀진 선택을 그대로 저장값으로
  // 덮어쓰면, 검색을 지웠을 때 원래 선택이 사라져 버린다).
  const [baseSelection, setBaseSelection] = useState(() =>
    filterState?.mode === 'values' ? new Set(filterState.selection) : new Set(options.map((option) => option.value)),
  );
  const [conditionOp, setConditionOp] = useState(filterState?.mode === 'condition' ? filterState.op : 'contains');
  const [conditionValue, setConditionValue] = useState(filterState?.mode === 'condition' ? filterState.value : '');
  const popoverRef = useRef(null);

  useEffect(() => {
    const handlePointerDown = (event) => {
      if (popoverRef.current && !popoverRef.current.contains(event.target)) {
        onClose();
      }
    };
    const handleKeyDown = (event) => {
      if (event.key === 'Escape') {
        onClose();
      }
    };
    document.addEventListener('mousedown', handlePointerDown, true);
    document.addEventListener('keydown', handleKeyDown, true);
    return () => {
      document.removeEventListener('mousedown', handlePointerDown, true);
      document.removeEventListener('keydown', handleKeyDown, true);
    };
  }, [onClose]);

  const filteredOptions = useMemo(() => {
    const lowerKeyword = keyword.trim().toLowerCase();
    if (!lowerKeyword) {
      return options;
    }
    return options.filter((option) => option.label.toLowerCase().includes(lowerKeyword));
  }, [options, keyword]);

  // 검색어가 있을 때는 "그 검색 결과만 선택된 상태"를 보여준다(엑셀 필터와 동일한 동작).
  // 검색어를 지우면 검색 시작 전의 선택 상태로 되돌아간다. 체크박스 개별 조작은 baseSelection을
  // 갱신해 검색어를 지운 뒤에도 유지되게 한다.
  const draftSelection = useMemo(() => {
    if (!keyword.trim()) {
      return baseSelection;
    }
    const matchedValues = new Set(filteredOptions.map((option) => option.value));
    return new Set([...baseSelection].filter((value) => matchedValues.has(value)));
  }, [baseSelection, keyword, filteredOptions]);

  const allFilteredChecked = filteredOptions.length > 0 && filteredOptions.every((option) => draftSelection.has(option.value));

  const toggleAllFiltered = () => {
    setBaseSelection((current) => {
      const next = new Set(current);
      if (allFilteredChecked) {
        filteredOptions.forEach((option) => next.delete(option.value));
      } else {
        filteredOptions.forEach((option) => next.add(option.value));
      }
      return next;
    });
  };

  const toggleOption = (value) => {
    setBaseSelection((current) => {
      const next = new Set(current);
      if (next.has(value)) {
        next.delete(value);
      } else {
        next.add(value);
      }
      return next;
    });
  };

  const applyValuesFilter = () => {
    // 검색 중이었다면 검색 결과로 좁혀진 선택값을 그대로 적용한다(검색만 하고 적용을 누르면
    // "그 검색어를 포함하는 값들만" 필터링되어야 한다). 전체 값이 다 선택되어 있으면
    // "필터 없음"으로 취급해 다른 필터 상태와 동일하게 맞춘다.
    const finalSelection = draftSelection;
    onApply(finalSelection.size === options.length ? null : { mode: 'values', selection: new Set(finalSelection) });
  };

  const applyConditionFilter = () => {
    if (!conditionValue.trim()) {
      onApply(null);
      return;
    }
    onApply({ mode: 'condition', op: conditionOp, value: conditionValue.trim() });
  };

  const resetFilter = () => {
    onApply(null);
  };

  if (!anchorRect) {
    return null;
  }

  const style = computePopoverStyle(anchorRect);

  return createPortal(
    <div className="column-filter-popover" style={style} ref={popoverRef} role="dialog" aria-label="필터">
      <div className="column-filter-tabs" role="tablist">
        <button
          type="button"
          role="tab"
          aria-selected={activeTab === 'values'}
          className={activeTab === 'values' ? 'is-active' : ''}
          onClick={() => setActiveTab('values')}
        >
          값 목록
        </button>
        <button
          type="button"
          role="tab"
          aria-selected={activeTab === 'condition'}
          className={activeTab === 'condition' ? 'is-active' : ''}
          onClick={() => setActiveTab('condition')}
        >
          조건
        </button>
      </div>

      {activeTab === 'values' ? (
        <>
          <div className="column-filter-popover-search">
            <Search size={13} aria-hidden="true" />
            <input
              autoFocus
              value={keyword}
              placeholder="값 검색"
              aria-label="필터 값 검색"
              onChange={(event) => setKeyword(event.target.value)}
            />
          </div>
          <label className="column-filter-option column-filter-option-all">
            <input type="checkbox" checked={allFilteredChecked} onChange={toggleAllFiltered} />
            <span>전체 선택</span>
          </label>
          <div className="column-filter-option-list">
            {filteredOptions.length === 0 ? (
              <div className="column-filter-empty">검색 결과 없음</div>
            ) : (
              filteredOptions.map((option) => (
                <label className="column-filter-option" key={option.value}>
                  <input type="checkbox" checked={draftSelection.has(option.value)} onChange={() => toggleOption(option.value)} />
                  <span className="column-filter-option-label">{option.label}</span>
                  <span className="column-filter-option-count">{option.count}</span>
                </label>
              ))
            )}
          </div>
          <div className="column-filter-popover-actions">
            <button type="button" className="column-filter-reset" onClick={resetFilter}>
              초기화
            </button>
            <button type="button" className="column-filter-apply" onClick={applyValuesFilter}>
              적용
            </button>
          </div>
        </>
      ) : (
        <>
          <div className="column-filter-condition-form">
            <select
              className="column-filter-condition-op"
              value={conditionOp}
              aria-label="필터 조건"
              onChange={(event) => setConditionOp(event.target.value)}
            >
              {COLUMN_FILTER_CONDITION_OPERATORS.map(([op, label]) => (
                <option value={op} key={op}>
                  {label}
                </option>
              ))}
            </select>
            <input
              autoFocus
              className="column-filter-condition-value"
              value={conditionValue}
              placeholder="값 입력"
              aria-label="필터 조건 값"
              onChange={(event) => setConditionValue(event.target.value)}
              onKeyDown={(event) => {
                if (event.key === 'Enter') {
                  applyConditionFilter();
                }
              }}
            />
          </div>
          <div className="column-filter-popover-actions">
            <button type="button" className="column-filter-reset" onClick={resetFilter}>
              초기화
            </button>
            <button type="button" className="column-filter-apply" onClick={applyConditionFilter}>
              적용
            </button>
          </div>
        </>
      )}
      <button type="button" className="column-filter-popover-close" onClick={onClose} aria-label="필터 팝업 닫기">
        <X size={14} aria-hidden="true" />
      </button>
    </div>,
    document.body,
  );
}

function computePopoverStyle(anchorRect) {
  const width = 240;
  const maxHeight = 360;
  const margin = 6;
  let left = anchorRect.right - width;
  left = Math.max(margin, Math.min(left, window.innerWidth - width - margin));
  let top = anchorRect.bottom + margin;
  if (top + maxHeight > window.innerHeight - margin) {
    top = Math.max(margin, anchorRect.top - maxHeight - margin);
  }
  return {
    position: 'fixed',
    top,
    left,
    width,
    maxHeight,
  };
}
