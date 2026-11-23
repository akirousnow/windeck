use anyhow::{Context, Result};
use windows::Win32::Foundation::{HWND, RECT, SIZE};
use windows::Win32::Graphics::Dwm::{
    DWM_THUMBNAIL_PROPERTIES, DWM_TNP_OPACITY, DWM_TNP_RECTDESTINATION, DWM_TNP_RECTSOURCE,
    DWM_TNP_SOURCECLIENTAREAONLY, DWM_TNP_VISIBLE, DwmQueryThumbnailSourceSize,
    DwmRegisterThumbnail, DwmUnregisterThumbnail, DwmUpdateThumbnailProperties,
};

/// RAII handle for a DWM live thumbnail relationship.
/// Automatically calls `DwmUnregisterThumbnail` on drop.
pub struct DwmThumbnail {
    handle: isize,
    src_hwnd: isize,
}

impl DwmThumbnail {
    pub fn register(dest: HWND, src: HWND) -> Result<Self> {
        let handle = unsafe { DwmRegisterThumbnail(dest, src) }
            .with_context(|| format!("DwmRegisterThumbnail(dest={:?}, src={:?})", dest, src))?;
        Ok(Self {
            handle,
            src_hwnd: src.0 as isize,
        })
    }

    pub fn src_hwnd(&self) -> isize {
        self.src_hwnd
    }

    pub fn query_source_size(&self) -> Result<(i32, i32)> {
        let size: SIZE = unsafe { DwmQueryThumbnailSourceSize(self.handle) }
            .context("DwmQueryThumbnailSourceSize")?;
        Ok((size.cx, size.cy))
    }

    pub fn update(
        &self,
        dest_rect: RECT,
        src_rect: Option<RECT>,
        opacity: u8,
        visible: bool,
        source_client_only: bool,
    ) -> Result<()> {
        let mut flags =
            DWM_TNP_RECTDESTINATION | DWM_TNP_VISIBLE | DWM_TNP_OPACITY | DWM_TNP_SOURCECLIENTAREAONLY;
        let rc_source = match src_rect {
            Some(r) => {
                flags |= DWM_TNP_RECTSOURCE;
                r
            }
            None => RECT::default(),
        };
        let props = DWM_THUMBNAIL_PROPERTIES {
            dwFlags: flags,
            rcDestination: dest_rect,
            rcSource: rc_source,
            opacity,
            fVisible: visible.into(),
            fSourceClientAreaOnly: source_client_only.into(),
        };
        unsafe { DwmUpdateThumbnailProperties(self.handle, &props) }
            .context("DwmUpdateThumbnailProperties")?;
        Ok(())
    }
}

impl Drop for DwmThumbnail {
    fn drop(&mut self) {
        if self.handle != 0 {
            let _ = unsafe { DwmUnregisterThumbnail(self.handle) };
            self.handle = 0;
        }
    }
}
