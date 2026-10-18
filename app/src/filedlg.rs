//! The native Windows file dialogs (`IFileOpenDialog` / `IFileSaveDialog`). Each call blocks
//! until the dialog closes, so callers run it on a thread of its own.

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

#[cfg(not(windows))]
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
