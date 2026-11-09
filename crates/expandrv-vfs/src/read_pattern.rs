//! 연속해서 실제로 읽은 바이트만으로 선행 읽기 창을 늘립니다.
#[derive(Default)]
pub(crate) struct ReadPattern {
    end: Option<u64>,
    sequential_bytes: u64,
}

impl ReadPattern {
    /// `offset`이 앞 읽기에 이어지면 지금까지 연속으로 읽은 바이트 수, 아니면 0
    pub fn continued(&self, offset: u64) -> u64 {
        if self.end == Some(offset) {
            self.sequential_bytes
        } else {
            0
        }
    }

    pub fn observe(&mut self, offset: u64, end: u64, block_size: u64, max: u32) -> u32 {
        self.sequential_bytes = if self.end == Some(offset) {
            self.sequential_bytes
                .saturating_add(end.saturating_sub(offset))
        } else {
            end.saturating_sub(offset)
        };
        self.end = Some(end);
        // 짧은 헤더 조회와 임의 위치 탐색은 선행 다운로드를 일으키지 않습니다.
        (self.sequential_bytes / block_size.max(1)).min(u64::from(max)) as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn small_random_reads_do_not_prefetch_and_sequential_reads_grow() {
        let mut p = ReadPattern::default();
        assert_eq!(p.observe(0, 16, 1024, 4), 0);
        assert_eq!(p.observe(4096, 4112, 1024, 4), 0);
        assert_eq!(p.observe(0, 1024, 1024, 4), 1);
        assert_eq!(p.observe(1024, 4096, 1024, 4), 4);
        assert_eq!(p.observe(8192, 8208, 1024, 4), 0);
        assert_eq!(p.observe(0, 8192, 1024, 0), 0);
    }
}
