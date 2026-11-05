//! WinFsp `FileSystemContext` 구현. 모든 동작을 [`Vfs`]로 넘깁니다.

use expandrv_core::{Entry, EntryKind, Error, RemotePath};
use expandrv_vfs::{FileHandle, Vfs};
use std::ffi::c_void;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use winfsp::filesystem::{
    DirBuffer, DirInfo, DirMarker, FileInfo, FileSecurity, FileSystemContext,
    ModificationDescriptor, OpenFileInfo, VolumeInfo, WideNameInfo,
};
use winfsp::{FspError, U16CStr};

const FILE_ATTRIBUTE_READONLY: u32 = 0x1;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_ARCHIVE: u32 = 0x20;
const FILE_DIRECTORY_FILE: u32 = 0x1;
const FILE_NON_DIRECTORY_FILE: u32 = 0x40;
const FSP_CLEANUP_DELETE: u32 = 0x1;
const STATUS_END_OF_FILE: i32 = 0xC000_0011_u32 as i32;
const STATUS_BUFFER_OVERFLOW: i32 = 0x8000_0005_u32 as i32;
/// 1601-01-01부터 1970-01-01까지 100ns 단위 간격
const FILETIME_UNIX_EPOCH: u64 = 116_444_736_000_000_000;
/// 백엔드가 용량을 알려 주지 않을 때 보고할 크기
const UNKNOWN_CAPACITY: u64 = 1 << 40;
const ALLOCATION_UNIT: u64 = 4096;

pub struct ExpanFs {
    vfs: Arc<Vfs>,
    label: String,
    security: Vec<u8>,
    mounted_at: SystemTime,
}

pub struct Context {
    handle: Arc<FileHandle>,
    dir_buffer: DirBuffer,
}

impl ExpanFs {
    pub fn new(vfs: Arc<Vfs>, label: String, security: Vec<u8>) -> Self {
        Self {
            vfs,
            label,
            security,
            mounted_at: SystemTime::now(),
        }
    }

    fn fill(&self, e: &Entry, fi: &mut FileInfo) {
        let mut attrs = if e.is_dir() {
            FILE_ATTRIBUTE_DIRECTORY
        } else {
            FILE_ATTRIBUTE_ARCHIVE
        };
        if e.readonly || (self.vfs.read_only() && !e.is_dir()) {
            attrs |= FILE_ATTRIBUTE_READONLY;
        }
        let modified = filetime(e.modified.unwrap_or(self.mounted_at));
        fi.file_attributes = attrs;
        fi.reparse_tag = 0;
        fi.file_size = e.size;
        fi.allocation_size = e.size.div_ceil(ALLOCATION_UNIT) * ALLOCATION_UNIT;
        fi.creation_time = e.created.map_or(modified, filetime);
        fi.last_access_time = modified;
        fi.last_write_time = modified;
        fi.change_time = modified;
        fi.index_number = 0;
        fi.hard_links = 0;
        fi.ea_size = 0;
    }

    fn copy_security(&self, buf: Option<&mut [c_void]>) -> winfsp::Result<u64> {
        if let Some(buf) = buf {
            if buf.len() < self.security.len() {
                return Err(FspError::NTSTATUS(STATUS_BUFFER_OVERFLOW));
            }
            // SAFETY: 길이를 확인했고 c_void 버퍼를 바이트로만 씁니다.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    self.security.as_ptr(),
                    buf.as_mut_ptr().cast::<u8>(),
                    self.security.len(),
                );
            }
        }
        Ok(self.security.len() as u64)
    }
}

pub fn filetime(t: SystemTime) -> u64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => FILETIME_UNIX_EPOCH + (d.as_nanos() / 100) as u64,
        Err(_) => FILETIME_UNIX_EPOCH,
    }
}

pub fn from_filetime(ft: u64) -> SystemTime {
    let ticks = ft.saturating_sub(FILETIME_UNIX_EPOCH);
    UNIX_EPOCH + Duration::from_nanos(ticks.saturating_mul(100))
}

fn fsp(e: Error) -> FspError {
    if !matches!(e, Error::NotFound | Error::AlreadyExists) {
        tracing::debug!(error = %e, "파일 시스템 요청 실패");
    }
    FspError::NTSTATUS(e.ntstatus())
}

fn parse(name: &U16CStr) -> winfsp::Result<RemotePath> {
    RemotePath::parse(&name.to_string_lossy()).map_err(fsp)
}

fn check_kind(create_options: u32, is_dir: bool) -> winfsp::Result<()> {
    if create_options & FILE_DIRECTORY_FILE != 0 && !is_dir {
        return Err(fsp(Error::NotADirectory));
    }
    if create_options & FILE_NON_DIRECTORY_FILE != 0 && is_dir {
        return Err(fsp(Error::IsADirectory));
    }
    Ok(())
}

impl FileSystemContext for ExpanFs {
    type FileContext = Context;

    fn get_security_by_name(
        &self,
        file_name: &U16CStr,
        security_descriptor: Option<&mut [c_void]>,
        _reparse_point_resolver: impl FnOnce(&U16CStr) -> Option<FileSecurity>,
    ) -> winfsp::Result<FileSecurity> {
        let (_, entry) = self.vfs.stat(&parse(file_name)?).map_err(fsp)?;
        let mut fi = FileInfo::default();
        self.fill(&entry, &mut fi);
        Ok(FileSecurity {
            reparse: false,
            sz_security_descriptor: self.copy_security(security_descriptor)?,
            attributes: fi.file_attributes,
        })
    }

    fn open(
        &self,
        file_name: &U16CStr,
        create_options: u32,
        _granted_access: u32,
        file_info: &mut OpenFileInfo,
    ) -> winfsp::Result<Context> {
        let requested = parse(file_name)?;
        let handle = self.vfs.open(&requested).map_err(fsp)?;
        if let Err(e) = check_kind(create_options, handle.is_dir()) {
            self.vfs.cleanup(&handle, false).ok();
            return Err(e);
        }
        let real = handle.path();
        if real != requested {
            // 원격 실제 대소문자를 탐색기에 알려 줍니다.
            let wide: Vec<u16> = real.as_str().replace('/', "\\").encode_utf16().collect();
            file_info.set_normalized_name(&wide, None);
        }
        self.fill(&self.vfs.info(&handle), file_info.as_mut());
        Ok(Context {
            handle,
            dir_buffer: DirBuffer::new(),
        })
    }

    fn close(&self, context: Context) {
        self.vfs.close(&context.handle);
    }

    fn create(
        &self,
        file_name: &U16CStr,
        create_options: u32,
        _granted_access: u32,
        _file_attributes: u32,
        _security_descriptor: Option<&[c_void]>,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        _extra_buffer_is_reparse_point: bool,
        file_info: &mut OpenFileInfo,
    ) -> winfsp::Result<Context> {
        let kind = if create_options & FILE_DIRECTORY_FILE != 0 {
            EntryKind::Dir
        } else {
            EntryKind::File
        };
        let handle = self.vfs.create(&parse(file_name)?, kind).map_err(fsp)?;
        self.fill(&self.vfs.info(&handle), file_info.as_mut());
        Ok(Context {
            handle,
            dir_buffer: DirBuffer::new(),
        })
    }

    fn cleanup(&self, context: &Context, file_name: Option<&U16CStr>, flags: u32) {
        let delete = file_name.is_some() && flags & FSP_CLEANUP_DELETE != 0;
        if let Err(e) = self.vfs.cleanup(&context.handle, delete) {
            tracing::error!(path = %context.handle.path(), delete, error = %e, "정리 실패");
        }
    }

    fn flush(&self, context: Option<&Context>, file_info: &mut FileInfo) -> winfsp::Result<()> {
        if let Some(c) = context {
            let e = self.vfs.flush(&c.handle).map_err(fsp)?;
            self.fill(&e, file_info);
        }
        Ok(())
    }

    fn get_file_info(&self, context: &Context, file_info: &mut FileInfo) -> winfsp::Result<()> {
        self.fill(&self.vfs.info(&context.handle), file_info);
        Ok(())
    }

    fn get_security(
        &self,
        _context: &Context,
        security_descriptor: Option<&mut [c_void]>,
    ) -> winfsp::Result<u64> {
        self.copy_security(security_descriptor)
    }

    fn set_security(
        &self,
        _context: &Context,
        _security_information: u32,
        _modification_descriptor: ModificationDescriptor,
    ) -> winfsp::Result<()> {
        // 원격 저장소에 ACL을 저장하지 않으므로 변경 요청은 무시합니다.
        Ok(())
    }

    fn overwrite(
        &self,
        context: &Context,
        _file_attributes: u32,
        _replace_file_attributes: bool,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        let e = self.vfs.overwrite(&context.handle).map_err(fsp)?;
        self.fill(&e, file_info);
        Ok(())
    }

    fn read_directory(
        &self,
        context: &Context,
        _pattern: Option<&U16CStr>,
        marker: DirMarker,
        buffer: &mut [u8],
    ) -> winfsp::Result<u32> {
        let path = context.handle.path();
        if let Ok(lock) = context.dir_buffer.acquire(marker.is_none(), None) {
            let list = self.vfs.list(&path).map_err(fsp)?;
            let mut di: DirInfo = DirInfo::new();
            if !path.is_root() {
                let me = self.vfs.info(&context.handle);
                for dot in [".", ".."] {
                    di.reset();
                    self.fill(&me, di.file_info_mut());
                    di.set_name(dot)?;
                    lock.write(&mut di)?;
                }
            }
            for e in list.iter() {
                di.reset();
                self.fill(e, di.file_info_mut());
                if di.set_name(e.name.as_str()).is_err() {
                    tracing::warn!(dir = %path, name = %e.name, "이름이 너무 길어 건너뜁니다");
                    continue;
                }
                lock.write(&mut di)?;
            }
        }
        Ok(context.dir_buffer.read(marker, buffer))
    }

    fn rename(
        &self,
        context: &Context,
        _file_name: &U16CStr,
        new_file_name: &U16CStr,
        replace_if_exists: bool,
    ) -> winfsp::Result<()> {
        let to = parse(new_file_name)?;
        self.vfs
            .rename(&context.handle, &to, replace_if_exists)
            .map_err(fsp)
    }

    fn set_basic_info(
        &self,
        context: &Context,
        _file_attributes: u32,
        _creation_time: u64,
        _last_access_time: u64,
        last_write_time: u64,
        _last_change_time: u64,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        let e = if last_write_time != 0 {
            self.vfs
                .set_mtime(&context.handle, from_filetime(last_write_time))
                .map_err(fsp)?
        } else {
            self.vfs.info(&context.handle)
        };
        self.fill(&e, file_info);
        Ok(())
    }

    fn set_delete(
        &self,
        context: &Context,
        _file_name: &U16CStr,
        delete_file: bool,
    ) -> winfsp::Result<()> {
        if delete_file {
            self.vfs.check_delete(&context.handle).map_err(fsp)?;
        }
        Ok(())
    }

    fn set_file_size(
        &self,
        context: &Context,
        new_size: u64,
        set_allocation_size: bool,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        let e = self
            .vfs
            .set_size(&context.handle, new_size, set_allocation_size)
            .map_err(fsp)?;
        self.fill(&e, file_info);
        Ok(())
    }

    fn read(&self, context: &Context, buffer: &mut [u8], offset: u64) -> winfsp::Result<u32> {
        if offset >= self.vfs.info(&context.handle).size {
            return Err(FspError::NTSTATUS(STATUS_END_OF_FILE));
        }
        let n = self
            .vfs
            .read(&context.handle, offset, buffer)
            .map_err(fsp)?;
        Ok(n as u32)
    }

    fn write(
        &self,
        context: &Context,
        buffer: &[u8],
        offset: u64,
        write_to_eof: bool,
        constrained_io: bool,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<u32> {
        let (n, e) = self
            .vfs
            .write(
                &context.handle,
                offset,
                buffer,
                write_to_eof,
                constrained_io,
            )
            .map_err(fsp)?;
        self.fill(&e, file_info);
        Ok(n as u32)
    }

    fn get_volume_info(&self, out_volume_info: &mut VolumeInfo) -> winfsp::Result<()> {
        let q = self.vfs.quota();
        let total = q
            .total
            .or_else(|| Some(q.used? + q.free?))
            .unwrap_or(UNKNOWN_CAPACITY);
        let free = q
            .free
            .or_else(|| Some(total.saturating_sub(q.used?)))
            .unwrap_or(total);
        out_volume_info.total_size = total;
        out_volume_info.free_size = free.min(total);
        out_volume_info.set_volume_label(&self.label);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filetime_roundtrip() {
        let t = UNIX_EPOCH + Duration::from_secs(1_700_000_000) + Duration::from_nanos(123_400);
        assert_eq!(from_filetime(filetime(t)), t);
        assert_eq!(filetime(UNIX_EPOCH), FILETIME_UNIX_EPOCH);
    }
}
