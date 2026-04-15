//! Read image data from the macOS clipboard.
//!
//! GPUI's `ClipboardItem` only surfaces text today, so we fall back to
//! NSPasteboard directly. We prefer PNG data (native screenshot format),
//! then TIFF which we convert to PNG via NSBitmapImageRep.
//!
//! Returns `None` when there's no image on the pasteboard or when the
//! conversion fails.

#[cfg(target_os = "macos")]
pub fn read_image_png_bytes() -> Option<Vec<u8>> {
    use cocoa::base::{id, nil};
    use cocoa::foundation::NSString;
    use objc::runtime::Class;
    use objc::{class, msg_send, sel, sel_impl};

    unsafe {
        let pb_cls = class!(NSPasteboard);
        let pb: id = msg_send![pb_cls, generalPasteboard];
        if pb == nil { return None; }

        let png_type: id = NSString::alloc(nil).init_str("public.png");
        let tiff_type: id = NSString::alloc(nil).init_str("public.tiff");

        // Try PNG first — exactly what Cmd-Shift-Ctrl-4 puts on the clipboard.
        let png_data: id = msg_send![pb, dataForType: png_type];
        if png_data != nil {
            return Some(ns_data_to_vec(png_data));
        }

        // Fall back to TIFF: load into NSBitmapImageRep, re-encode as PNG.
        let tiff_data: id = msg_send![pb, dataForType: tiff_type];
        if tiff_data != nil {
            let rep_cls: &Class = class!(NSBitmapImageRep);
            let rep: id = msg_send![rep_cls, imageRepWithData: tiff_data];
            if rep != nil {
                // NSBitmapImageFileTypePNG = 4
                let empty_dict: id = {
                    let dict_cls = class!(NSDictionary);
                    msg_send![dict_cls, dictionary]
                };
                let png: id = msg_send![rep, representationUsingType: 4u64 properties: empty_dict];
                if png != nil {
                    return Some(ns_data_to_vec(png));
                }
            }
        }

        None
    }
}

#[cfg(target_os = "macos")]
unsafe fn ns_data_to_vec(data: cocoa::base::id) -> Vec<u8> {
    use cocoa::foundation::NSData;
    let len = data.length() as usize;
    let bytes_ptr = data.bytes() as *const u8;
    std::slice::from_raw_parts(bytes_ptr, len).to_vec()
}

#[cfg(not(target_os = "macos"))]
pub fn read_image_png_bytes() -> Option<Vec<u8>> {
    None
}

/// Save image bytes to a fresh file under `/tmp/allele-scratch/` and return
/// the absolute path. The directory is created on demand.
pub fn save_clipboard_png(bytes: &[u8]) -> std::io::Result<std::path::PathBuf> {
    let dir = std::path::PathBuf::from("/tmp/allele-scratch");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("img-{}.png", uuid::Uuid::new_v4()));
    std::fs::write(&path, bytes)?;
    Ok(path)
}
