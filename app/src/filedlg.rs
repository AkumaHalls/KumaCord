//! The native file dialogs: Windows `IFileOpenDialog` / `IFileSaveDialog`, macOS the standard
//! panels through `osascript`. Each call blocks until the dialog closes, so callers run it on a
//! thread of its own.

use std::path::PathBuf;

/// Image files the app reads (profile pictures, chat images).
pub const IMAGE_FILTER: (&str, &str) = ("Imagens (PNG, JPEG, WebP, GIF, BMP)", "*.png;*.jpg;*.jpeg;*.jfif;*.webp;*.gif;*.bmp");

#[cfg(windows)]
mod imp {
    use std::path::PathBuf;

    use windows::core::{HSTRING, PCWSTR};
    use windows::Win32::Foundation::{ERROR_CANCELLED, HWND};
    use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED};
    use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
    use windows::Win32::UI::Shell::{
        FileOpenDialog, FileSaveDialog, IFileOpenDialog, IFileSaveDialog, IShellItem, FOS_ALLOWMULTISELECT, FOS_FILEMUSTEXIST, FOS_FORCEFILESYSTEM, FOS_OVERWRITEPROMPT,
        FOS_PATHMUSTEXIST, SIGDN_FILESYSPATH,
    };

    fn err(what: &str, e: windows::core::Error) -> String {
        format!("{what}: {e}")
    }

    /// COM on this thread for the duration of `f`.
    fn with_com<T>(f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
        // SAFETY: plain COM initialisation on this thread, undone below when it succeeded.
        let init = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        let r = f();
        if init.is_ok() {
            // SAFETY: balanced with the successful CoInitializeEx above.
            unsafe { CoUninitialize() };
        }
        r
    }

    /// SAFETY: `item` is a live shell item.
    unsafe fn path_of(item: &IShellItem) -> Result<PathBuf, String> {
        let name = unsafe { item.GetDisplayName(SIGDN_FILESYSPATH) }.map_err(|e| err("file dialog", e))?;
        let path = unsafe { name.to_string() }.map_err(|e| format!("file dialog: {e}"));
        // SAFETY: the string was allocated by the shell with CoTaskMemAlloc.
        unsafe { CoTaskMemFree(Some(name.as_ptr() as *const std::ffi::c_void)) };
        path.map(PathBuf::from)
    }

    pub fn open(owner: Option<isize>, title: &str, filter: (&str, &str), multi: bool) -> Result<Vec<PathBuf>, String> {
        with_com(|| {
            // SAFETY: COM calls on this thread; every interface comes from COM and is released by
            // its Drop; strings outlive the calls that read them.
            unsafe {
                let dlg: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).map_err(|e| err("file dialog", e))?;
                let mut opts = dlg.GetOptions().map_err(|e| err("file dialog", e))? | FOS_FORCEFILESYSTEM | FOS_FILEMUSTEXIST | FOS_PATHMUSTEXIST;
                if multi {
                    opts |= FOS_ALLOWMULTISELECT;
                }
                dlg.SetOptions(opts).map_err(|e| err("file dialog", e))?;
                let (name, spec, all, star) = (HSTRING::from(filter.0), HSTRING::from(filter.1), HSTRING::from("Todos os arquivos"), HSTRING::from("*.*"));
                let filters = [
                    COMDLG_FILTERSPEC { pszName: PCWSTR(name.as_ptr()), pszSpec: PCWSTR(spec.as_ptr()) },
                    COMDLG_FILTERSPEC { pszName: PCWSTR(all.as_ptr()), pszSpec: PCWSTR(star.as_ptr()) },
                ];
                dlg.SetFileTypes(&filters).map_err(|e| err("file dialog", e))?;
                dlg.SetTitle(&HSTRING::from(title)).map_err(|e| err("file dialog", e))?;
                let parent = owner.map(|h| HWND(h as *mut std::ffi::c_void));
                if let Err(e) = dlg.Show(parent) {
                    if e.code() == ERROR_CANCELLED.to_hresult() {
                        return Ok(Vec::new());
                    }
                    return Err(err("file dialog", e));
                }
                let items = dlg.GetResults().map_err(|e| err("file dialog", e))?;
                let n = items.GetCount().map_err(|e| err("file dialog", e))?;
                let mut out = Vec::new();
                for i in 0..n {
                    let item = items.GetItemAt(i).map_err(|e| err("file dialog", e))?;
                    out.push(path_of(&item)?);
                }
                Ok(out)
            }
        })
    }

    pub fn save(owner: Option<isize>, title: &str, file_name: &str, filter: (&str, &str), ext: &str) -> Result<Option<PathBuf>, String> {
        with_com(|| {
            // SAFETY: as in `open`.
            unsafe {
                let dlg: IFileSaveDialog = CoCreateInstance(&FileSaveDialog, None, CLSCTX_INPROC_SERVER).map_err(|e| err("save dialog", e))?;
                let opts = dlg.GetOptions().map_err(|e| err("save dialog", e))? | FOS_FORCEFILESYSTEM | FOS_PATHMUSTEXIST | FOS_OVERWRITEPROMPT;
                dlg.SetOptions(opts).map_err(|e| err("save dialog", e))?;
                let (name, spec) = (HSTRING::from(filter.0), HSTRING::from(filter.1));
                let filters = [COMDLG_FILTERSPEC { pszName: PCWSTR(name.as_ptr()), pszSpec: PCWSTR(spec.as_ptr()) }];
                dlg.SetFileTypes(&filters).map_err(|e| err("save dialog", e))?;
                dlg.SetDefaultExtension(&HSTRING::from(ext)).map_err(|e| err("save dialog", e))?;
                dlg.SetFileName(&HSTRING::from(file_name)).map_err(|e| err("save dialog", e))?;
                dlg.SetTitle(&HSTRING::from(title)).map_err(|e| err("save dialog", e))?;
                let parent = owner.map(|h| HWND(h as *mut std::ffi::c_void));
                if let Err(e) = dlg.Show(parent) {
                    if e.code() == ERROR_CANCELLED.to_hresult() {
                        return Ok(None);
                    }
                    return Err(err("save dialog", e));
                }
                let item = dlg.GetResult().map_err(|e| err("save dialog", e))?;
                path_of(&item).map(Some)
            }
        })
    }
}

/// macOS: AppleScript's standard file panels through `osascript` (blocks until closed, like the
/// Windows dialogs). Cancelling answers error -128: an empty result, not an error.
#[cfg(target_os = "macos")]
mod imp {
    use std::path::PathBuf;

    /// An AppleScript string literal.
    fn quoted(s: &str) -> String {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    }

    /// `*.png;*.jpg` -> `{"png", "jpg"}`.
    pub(super) fn types(patterns: &str) -> String {
        let exts: Vec<String> = patterns.split(';').filter_map(|p| p.trim().strip_prefix("*.")).filter(|e| !e.is_empty() && *e != "*").map(quoted).collect();
        format!("{{{}}}", exts.join(", "))
    }

    /// Run `script`; `Ok(None)` when the user cancelled.
    fn run(script: &str) -> Result<Option<String>, String> {
        let out = std::process::Command::new("/usr/bin/osascript").arg("-e").arg(script).output().map_err(|e| format!("osascript: {e}"))?;
        if out.status.success() {
            return Ok(Some(String::from_utf8_lossy(&out.stdout).trim_end_matches('\n').to_string()));
        }
        let err = String::from_utf8_lossy(&out.stderr);
        if err.contains("-128") {
            return Ok(None);
        }
        Err(format!("osascript: {}", err.trim()))
    }

    pub fn open(_owner: Option<isize>, title: &str, filter: (&str, &str), multi: bool) -> Result<Vec<PathBuf>, String> {
        let pick = format!("choose file with prompt {} of type {}{}", quoted(title), types(filter.1), if multi { " with multiple selections allowed" } else { "" });
        let script = format!(
            "set fs to {pick}\nif class of fs is not list then set fs to {{fs}}\nset out to \"\"\nrepeat with f in fs\nset out to out & POSIX path of f & linefeed\nend repeat\nreturn out"
        );
        Ok(run(&script)?.map(|t| t.lines().filter(|l| !l.is_empty()).map(PathBuf::from).collect()).unwrap_or_default())
    }

    pub fn save(_owner: Option<isize>, title: &str, file_name: &str, _filter: (&str, &str), ext: &str) -> Result<Option<PathBuf>, String> {
        let script = format!("POSIX path of (choose file name with prompt {} default name {})", quoted(title), quoted(file_name));
        Ok(run(&script)?.map(|p| {
            let mut p = PathBuf::from(p);
            if p.extension().is_none() && !ext.is_empty() {
                p.set_extension(ext);
            }
            p
        }))
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod imp {
    use std::path::PathBuf;

    pub fn open(_owner: Option<isize>, _title: &str, _filter: (&str, &str), _multi: bool) -> Result<Vec<PathBuf>, String> {
        Err("Escolher arquivo só está disponível no Windows.".into())
    }

    pub fn save(_owner: Option<isize>, _title: &str, _file_name: &str, _filter: (&str, &str), _ext: &str) -> Result<Option<PathBuf>, String> {
        Err("Salvar arquivo só está disponível no Windows.".into())
    }
}

/// "Abrir": the chosen files (empty: cancelled). `owner` is the parent window.
pub fn open(owner: Option<isize>, title: &str, filter: (&str, &str), multi: bool) -> Result<Vec<PathBuf>, String> {
    imp::open(owner, title, filter, multi)
}

/// "Salvar como": the chosen path (`None`: cancelled). `ext` without the dot.
pub fn save(owner: Option<isize>, title: &str, file_name: &str, filter: (&str, &str), ext: &str) -> Result<Option<PathBuf>, String> {
    imp::save(owner, title, file_name, filter, ext)
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    #[test]
    fn filters_become_applescript_type_lists() {
        assert_eq!(super::imp::types("*.png;*.jpg"), "{\"png\", \"jpg\"}");
        assert_eq!(super::imp::types("*.*"), "{}");
    }
}
