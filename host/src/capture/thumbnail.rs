//! One-shot, low-res thumbnail snapshots for the target-picker UI — not
//! part of the streaming pipeline. Static snapshots only (matching
//! Discord's actual picker behavior, not continuously-live video), so
//! browsing the picker doesn't add ongoing capture/encode load.
//!
//! Plain GDI (`BitBlt`/`StretchBlt`/`PrintWindow`) rather than DXGI/D3D11:
//! a thumbnail doesn't need zero-copy GPU performance, and GDI avoids
//! standing up a whole D3D11 device just to grab one still frame.

use anyhow::anyhow;
use image::{ImageBuffer, Rgba};
use windows::Win32::Foundation::{HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleBitmap, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC,
    GetDIBits, GetObjectW, ReleaseDC, SelectObject, SetStretchBltMode, StretchBlt, BITMAP,
    BITMAPINFO, DIB_RGB_COLORS, HALFTONE, HDC, HGDIOBJ, SRCCOPY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DrawIconEx, GetClassLongPtrW, GetIconInfo, SendMessageW, DI_NORMAL, GCLP_HICON, HICON,
    ICONINFO, ICON_BIG, ICON_SMALL2, WM_GETICON,
};

/// Downscales whatever's in `src_dc`'s `src_rect` to fit within
/// `max_dim` x `max_dim` and reads it back as tightly-packed RGBA rows.
fn stretch_and_read(src_dc: HDC, src_rect: RECT, max_dim: u32) -> anyhow::Result<(u32, u32, Vec<u8>)> {
    let src_w = (src_rect.right - src_rect.left).max(1) as u32;
    let src_h = (src_rect.bottom - src_rect.top).max(1) as u32;
    let scale = (max_dim as f32 / src_w.max(src_h) as f32).min(1.0);
    let dst_w = ((src_w as f32 * scale) as u32).max(1);
    let dst_h = ((src_h as f32 * scale) as u32).max(1);

    unsafe {
        let mem_dc = CreateCompatibleDC(Some(src_dc));
        if mem_dc.is_invalid() {
            anyhow::bail!("CreateCompatibleDC failed");
        }
        let bitmap = CreateCompatibleBitmap(src_dc, dst_w as i32, dst_h as i32);
        if bitmap.is_invalid() {
            let _ = DeleteDC(mem_dc);
            anyhow::bail!("CreateCompatibleBitmap failed");
        }
        let old = SelectObject(mem_dc, HGDIOBJ(bitmap.0));

        SetStretchBltMode(mem_dc, HALFTONE);
        let stretch_ok = StretchBlt(
            mem_dc,
            0,
            0,
            dst_w as i32,
            dst_h as i32,
            Some(src_dc),
            src_rect.left,
            src_rect.top,
            src_w as i32,
            src_h as i32,
            SRCCOPY,
        );

        let mut info = BITMAPINFO::default();
        info.bmiHeader.biSize = std::mem::size_of_val(&info.bmiHeader) as u32;
        info.bmiHeader.biWidth = dst_w as i32;
        info.bmiHeader.biHeight = -(dst_h as i32); // negative: top-down DIB
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 32;

        let mut buf = vec![0u8; (dst_w * dst_h * 4) as usize];
        let lines = GetDIBits(
            mem_dc,
            bitmap,
            0,
            dst_h,
            Some(buf.as_mut_ptr() as *mut _),
            &mut info,
            DIB_RGB_COLORS,
        );

        SelectObject(mem_dc, old);
        let _ = DeleteObject(HGDIOBJ(bitmap.0));
        let _ = DeleteDC(mem_dc);

        if !stretch_ok.as_bool() || lines == 0 {
            anyhow::bail!("failed to capture screen content");
        }

        // GDI's 32bpp DIB is BGRA; the `image` crate wants RGBA.
        for px in buf.chunks_exact_mut(4) {
            px.swap(0, 2);
        }

        Ok((dst_w, dst_h, buf))
    }
}

fn encode_png(width: u32, height: u32, rgba: Vec<u8>) -> anyhow::Result<Vec<u8>> {
    let img: ImageBuffer<Rgba<u8>, _> =
        ImageBuffer::from_raw(width, height, rgba).ok_or_else(|| anyhow!("invalid thumbnail buffer"))?;
    let mut out = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)?;
    Ok(out)
}

/// Captures a downscaled PNG snapshot of an entire monitor.
pub fn capture_monitor_thumbnail(adapter_index: u32, output_index: u32, max_dim: u32) -> anyhow::Result<Vec<u8>> {
    let (width, height, rgba) = unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let adapter: IDXGIAdapter1 = factory.EnumAdapters1(adapter_index)?;
        let output = adapter.EnumOutputs(output_index)?;
        let rect = output.GetDesc()?.DesktopCoordinates;

        let screen_dc = GetDC(None);
        if screen_dc.is_invalid() {
            anyhow::bail!("GetDC(None) failed");
        }
        let result = stretch_and_read(screen_dc, rect, max_dim);
        ReleaseDC(None, screen_dc);
        result?
    };
    encode_png(width, height, rgba)
}

/// Returns the window's icon (large app icon, falling back to the small
/// one, falling back to the window class's icon), or `None` if it has
/// none. Unlike a `PrintWindow`/`BitBlt` screenshot, this never goes stale
/// or blank for occluded, GPU-composited, or not-currently-repainted
/// windows — it's just the same icon Alt-Tab and the taskbar show.
unsafe fn window_icon(hwnd: HWND) -> Option<HICON> {
    unsafe {
        let big = SendMessageW(hwnd, WM_GETICON, Some(WPARAM(ICON_BIG as usize)), Some(LPARAM(0)));
        if big.0 != 0 {
            return Some(HICON(big.0 as *mut _));
        }
        let small = SendMessageW(hwnd, WM_GETICON, Some(WPARAM(ICON_SMALL2 as usize)), Some(LPARAM(0)));
        if small.0 != 0 {
            return Some(HICON(small.0 as *mut _));
        }
        let class_icon = GetClassLongPtrW(hwnd, GCLP_HICON);
        if class_icon != 0 {
            return Some(HICON(class_icon as *mut _));
        }
        None
    }
}

/// Renders an `HICON` into a top-down 32bpp RGBA buffer via `DrawIconEx`
/// onto a DIB section (rather than a plain compatible bitmap), so the
/// icon's alpha channel survives the read-back instead of compositing
/// onto an opaque background.
fn render_icon(hicon: HICON, max_dim: u32) -> anyhow::Result<(u32, u32, Vec<u8>)> {
    unsafe {
        let mut info = ICONINFO::default();
        GetIconInfo(hicon, &mut info)?;

        let mut bmp = BITMAP::default();
        let size_source = if !info.hbmColor.is_invalid() {
            info.hbmColor
        } else {
            info.hbmMask
        };
        GetObjectW(
            HGDIOBJ(size_source.0),
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut bmp as *mut _ as *mut _),
        );
        let native_h = if !info.hbmColor.is_invalid() {
            bmp.bmHeight
        } else {
            bmp.bmHeight / 2 // color+mask stacked in one bitmap when there's no separate color bitmap
        };
        let native = bmp.bmWidth.max(native_h).max(1) as u32;
        if !info.hbmColor.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(info.hbmColor.0));
        }
        if !info.hbmMask.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(info.hbmMask.0));
        }

        let size = native.min(max_dim).max(16);

        let screen_dc = GetDC(None);
        if screen_dc.is_invalid() {
            anyhow::bail!("GetDC(None) failed");
        }
        let mem_dc = CreateCompatibleDC(Some(screen_dc));

        let mut dib_info = BITMAPINFO::default();
        dib_info.bmiHeader.biSize = std::mem::size_of_val(&dib_info.bmiHeader) as u32;
        dib_info.bmiHeader.biWidth = size as i32;
        dib_info.bmiHeader.biHeight = -(size as i32); // negative: top-down
        dib_info.bmiHeader.biPlanes = 1;
        dib_info.bmiHeader.biBitCount = 32;

        let mut bits_ptr: *mut core::ffi::c_void = std::ptr::null_mut();
        let dib = CreateDIBSection(Some(mem_dc), &dib_info, DIB_RGB_COLORS, &mut bits_ptr, None, 0)?;
        let old = SelectObject(mem_dc, HGDIOBJ(dib.0));

        let draw_result = DrawIconEx(mem_dc, 0, 0, hicon, size as i32, size as i32, 0, None, DI_NORMAL);

        let buf_len = (size * size * 4) as usize;
        let mut buf = vec![0u8; buf_len];
        if draw_result.is_ok() {
            std::ptr::copy_nonoverlapping(bits_ptr as *const u8, buf.as_mut_ptr(), buf_len);
        }

        SelectObject(mem_dc, old);
        let _ = DeleteObject(HGDIOBJ(dib.0));
        let _ = DeleteDC(mem_dc);
        ReleaseDC(None, screen_dc);

        draw_result?;

        // GDI's 32bpp DIB is BGRA; the `image` crate wants RGBA.
        for px in buf.chunks_exact_mut(4) {
            px.swap(0, 2);
        }

        Ok((size, size, buf))
    }
}

/// Returns the window's app icon as a PNG — a simpler and more reliable
/// stand-in for "what is this window" than a live screenshot, which can go
/// stale, blank, or fail outright for occluded/protected/GPU-composited
/// windows. Static either way (matching Discord's actual picker
/// behavior), just backed by a different, sturdier source.
pub fn capture_window_thumbnail(hwnd: isize, max_dim: u32) -> anyhow::Result<Vec<u8>> {
    let hwnd = HWND(hwnd as *mut _);
    // Icons from WM_GETICON / GCLP_HICON are owned by the target window's
    // class, not by us — never DestroyIcon() them (that could make the
    // owning window lose its own icon, or worse).
    let hicon = unsafe { window_icon(hwnd) }.ok_or_else(|| anyhow!("window has no icon"))?;
    let (width, height, rgba) = render_icon(hicon, max_dim)?;
    encode_png(width, height, rgba)
}
