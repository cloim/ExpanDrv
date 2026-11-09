//! 자동 잠금 판단과 현재 Windows 세션의 입력 유휴 시간.
use expandrv_core::config::SecurityConfig;
use std::time::Duration;
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    Manual,
    Session,
    Idle,
}

pub fn due(
    policy: &SecurityConfig,
    reason: Reason,
    idle: Option<Duration>,
    since_unlock: Duration,
) -> bool {
    match reason {
        Reason::Manual => true,
        Reason::Session => policy.lock_on_session_lock,
        Reason::Idle => {
            let threshold = Duration::from_secs(u64::from(policy.idle_minutes) * 60);
            policy.idle_minutes > 0
                && since_unlock >= threshold
                && idle.is_some_and(|idle| idle >= threshold)
        }
    }
}

fn elapsed(now: u32, last: u32) -> Option<Duration> {
    let elapsed = now.wrapping_sub(last);
    // 일부 합성 입력의 미래 시각은 유휴로 간주하지 않습니다.
    (elapsed <= i32::MAX as u32).then(|| Duration::from_millis(u64::from(elapsed)))
}

pub fn idle_time() -> Option<Duration> {
    let mut info = LASTINPUTINFO {
        cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
        ..Default::default()
    };
    unsafe {
        if !GetLastInputInfo(&mut info).as_bool() {
            return None;
        }
        elapsed(GetTickCount(), info.dwTime)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn policies_require_threshold_and_unlock_grace() {
        let mut policy = SecurityConfig::default();
        let minutes = |n: u64| Duration::from_secs(n * 60);
        assert!(due(&policy, Reason::Session, None, minutes(0)));
        assert!(!due(&policy, Reason::Idle, Some(minutes(60)), minutes(60)));
        policy.lock_on_session_lock = false;
        policy.idle_minutes = 5;
        assert!(!due(&policy, Reason::Session, None, minutes(60)));
        assert!(!due(&policy, Reason::Idle, Some(minutes(5)), minutes(1)));
        assert!(!due(&policy, Reason::Idle, Some(minutes(1)), minutes(5)));
        assert!(!due(&policy, Reason::Idle, None, minutes(5)));
        assert!(due(&policy, Reason::Idle, Some(minutes(5)), minutes(5)));
        assert!(due(&policy, Reason::Manual, None, minutes(0)));
    }
    #[test]
    fn tick_rollover_and_future_input_are_distinct() {
        assert_eq!(elapsed(20, u32::MAX - 20), Some(Duration::from_millis(41)));
        assert_eq!(elapsed(20, 30), None);
        assert_eq!(elapsed(1000, 100), Some(Duration::from_millis(900)));
    }
}
