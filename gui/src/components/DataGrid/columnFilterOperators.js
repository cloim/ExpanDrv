/**
 * 엑셀 스타일 컬럼 필터의 "조건" 탭에서 쓰는 연산자 목록.
 * DataGrid.jsx(필터 매칭 로직)와 ColumnFilterPopover.jsx(연산자 선택 UI) 양쪽에서 쓰므로
 * 순환 참조를 피하기 위해 별도 파일로 둔다.
 */
export const COLUMN_FILTER_CONDITION_OPERATORS = [
  ['contains', '포함'],
  ['gte', '이상'],
  ['gt', '초과'],
  ['lte', '이하'],
  ['lt', '미만'],
];
