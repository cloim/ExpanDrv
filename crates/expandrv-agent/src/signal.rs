//! 마운트 프로세스와 `unmount` 명령 사이의 세션 범위 신호.
//!
//! 3단계 IPC(ACL 파이프) 전까지 `Local\` 이름 공간의 이름 있는 이벤트를 씁니다.
//! `Local\` 객체는 현재 로그온 세션에서만 보이고 기본 DACL이 생성 사용자로 제한됩니다.

use expandrv_core::{Error, Result};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{
    CreateEventW, EVENT_MODIFY_STATE, INFINITE, OpenEventW, SetEvent, WaitForSingleObject,
};
use windows::core::HSTRING;

fn name(id: &str) -> HSTRING {
    HSTRING::from(format!("Local\\expandrv-unmount-{id}"))
}

pub struct UnmountSignal(HANDLE);

// SAFETY: 이벤트 핸들은 스레드 간 공유가 허용되는 커널 객체입니다.
unsafe impl Send for UnmountSignal {}
unsafe impl Sync for UnmountSignal {}

impl UnmountSignal {
    /// 이미 같은 이름이 있으면 다른 프로세스가 마운트 중이므로 거부합니다.
    pub fn create(id: &str) -> Result<Self> {
        if is_mounted(id) {
            return Err(Error::Config(format!(
                "드라이브 {id}는 이미 마운트되어 있습니다"
            )));
        }
        let h = unsafe { CreateEventW(None, true, false, &name(id)) }
            .map_err(|e| Error::Io(std::io::Error::other(e)))?;
        Ok(Self(h))
    }

    /// 신호가 올 때까지 막습니다. 전용 스레드에서 부릅니다.
    pub fn wait(&self) -> bool {
        unsafe { WaitForSingleObject(self.0, INFINITE) == WAIT_OBJECT_0 }
    }

    pub fn set(&self) {
        let _ = unsafe { SetEvent(self.0) };
    }
}

impl Drop for UnmountSignal {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

pub fn is_mounted(id: &str) -> bool {
    match unsafe { OpenEventW(EVENT_MODIFY_STATE, false, &name(id)) } {
        Ok(h) => {
            let _ = unsafe { CloseHandle(h) };
            true
        }
        Err(_) => false,
    }
}

/// 실행 중인 마운트 프로세스에 해제를 요청합니다.
pub fn request_unmount(id: &str) -> Result<()> {
    let h = unsafe { OpenEventW(EVENT_MODIFY_STATE, false, &name(id)) }
        .map_err(|_| Error::Config(format!("드라이브 {id}는 마운트되어 있지 않습니다")))?;
    let r = unsafe { SetEvent(h) };
    let _ = unsafe { CloseHandle(h) };
    r.map_err(|e| Error::Io(std::io::Error::other(e)))
}
