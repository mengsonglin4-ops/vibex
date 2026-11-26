use super::{Result, UNSAFE};
use std::fs::File;
use std::path::Path;

pub(super) fn open(root: &Path, relative: &str) -> Result<Option<File>> {
    Ok(pin(root, relative, false)?.and_then(|mut handles| handles.pop()))
}

pub(super) fn pin_directory(root: &Path, relative: &str) -> Result<Option<Vec<File>>> {
    pin(root, relative, true)
}

fn pin(root: &Path, relative: &str, directory: bool) -> Result<Option<Vec<File>>> {
    use std::fs::OpenOptions;
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    use std::os::windows::io::AsRawHandle;
    use std::path::{Component, PathBuf, Prefix};

    let full = root.join(relative);
    let mut prefix = PathBuf::new();
    let mut pinned = Vec::new();
    let mut components = full.components().peekable();
    while let Some(component) = components.next() {
        prefix.push(component.as_os_str());
        match component {
            Component::Prefix(prefix)
                if matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_)) =>
            {
                continue;
            }
            Component::RootDir => continue,
            Component::Normal(_) => {}
            _ => return Err(UNSAFE),
        }
        let leaf = components.peek().is_none();
        let mut options = OpenOptions::new();
        options
            .share_mode(1)
            .custom_flags(0x0020_0000 | 0x0200_0000);
        if leaf && !directory {
            options.read(true);
        } else {
            options.access_mode(0x80);
        }
        let file = match options.open(&prefix) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(UNSAFE),
        };
        let metadata = file.metadata().map_err(|_| UNSAFE)?;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err(UNSAFE);
        }
        if leaf && !directory {
            if !metadata.is_file() {
                return Err(UNSAFE);
            }
            #[repr(C)]
            #[derive(Default)]
            struct StandardInfo {
                allocation: i64,
                size: i64,
                links: u32,
                deleting: u8,
                directory: u8,
            }
            #[link(name = "kernel32")]
            unsafe extern "system" {
                fn GetFileInformationByHandleEx(
                    handle: *mut std::ffi::c_void,
                    class: i32,
                    buffer: *mut StandardInfo,
                    size: u32,
                ) -> i32;
            }
            let mut info = StandardInfo::default();
            // FileStandardInfo is written synchronously into this live, ABI-sized buffer.
            let ok = unsafe {
                GetFileInformationByHandleEx(
                    file.as_raw_handle(),
                    1,
                    &mut info,
                    std::mem::size_of::<StandardInfo>() as u32,
                )
            };
            if ok == 0 || info.links != 1 || info.deleting != 0 {
                return Err(UNSAFE);
            }
            pinned.push(file);
            return Ok(Some(pinned));
        }
        if !metadata.is_dir() {
            return Err(UNSAFE);
        }
        pinned.push(file);
        if leaf {
            return Ok(Some(pinned));
        }
    }
    Err(UNSAFE)
}
