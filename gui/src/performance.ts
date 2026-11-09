import type { CacheSettings } from './api';

export const PERFORMANCE_PRESETS = [
  { id: 'balanced', label: '균형', description: '일반 작업에 맞춰 캐시와 원격 변경 확인을 균형 있게 사용', cache: { metadataTtlSecs: 30, blockSizeMib: 4, memoryLimitMib: 64, pollIntervalSecs: 60, diskLimitMib: 1024, readaheadBlocks: 4 } },
  { id: 'documents', label: '문서 작업', description: '작은 읽기 블록과 30초 변경 확인으로 문서 탐색에 집중', cache: { metadataTtlSecs: 15, blockSizeMib: 1, memoryLimitMib: 32, pollIntervalSecs: 30, diskLimitMib: 512, readaheadBlocks: 2 } },
  { id: 'media', label: '대용량 미디어', description: '최대 8블록 선행 읽기와 넉넉한 캐시로 연속 읽기에 집중', cache: { metadataTtlSecs: 60, blockSizeMib: 4, memoryLimitMib: 128, pollIntervalSecs: 120, diskLimitMib: 4096, readaheadBlocks: 8 } },
  { id: 'metered', label: '데이터 절약', description: '선행 다운로드와 주기적 원격 확인을 끄고 요청한 데이터 위주로 전송', cache: { metadataTtlSecs: 300, blockSizeMib: 1, memoryLimitMib: 16, pollIntervalSecs: 0, diskLimitMib: 1024, readaheadBlocks: 0 } },
] satisfies { id: string; label: string; description: string; cache: CacheSettings }[];

export function performancePreset(cache: Record<keyof CacheSettings, string>) {
  return PERFORMANCE_PRESETS.find((p) => Object.entries(p.cache).every(([key, value]) => Number(cache[key as keyof CacheSettings]) === value));
}
