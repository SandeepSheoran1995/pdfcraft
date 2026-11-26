//! Headless acceptance test for barcode/QR overlay replacement logic.
//! Asserts that replacement barcode/QR raster stretches to 100% of user-space bounds
//! without shrinking or aspect ratio distortion.

use pdfcraft_engine::{CodeKind, Document, Edit, ScannedCode, Session};
use pdfcraft_render::{PageRenderer, RenderRequest, RequestKind};
use std::sync::Arc;

#[test]
fn test_barcode_overlay_exact_dimensions() {
    let mut session = Session::new();
    let blank = session.create_blank(200.0, 200.0, 1).expect("create blank page");
    let id = session.open_new("barcode_overlay_test.pdf", blank).expect("open new");

    // 1. Synthetic PDF page with a 100x100 point square at [50.0, 50.0, 150.0, 150.0]
    let rect = [50.0, 50.0, 150.0, 150.0];
    let code = ScannedCode { id: 0, page: 0, kind: CodeKind::Code128, text: "EXACT".into(), rect, pixel_size: [100, 100], image_index: None };

    // 2. Generate replacement barcode image matching exact dimensions
    let orig_w = (code.rect[2] - code.rect[0]).abs().round() as u32;
    let orig_h = (code.rect[3] - code.rect[1]).abs().round() as u32;
    let pw = if code.pixel_size[0] > 0 { code.pixel_size[0] } else { orig_w };
    let ph = if code.pixel_size[1] > 0 { code.pixel_size[1] } else { orig_h };
    assert_eq!(pw, 100);
    assert_eq!(ph, 100);

    let png_bytes = Document::generate_replacement_code(code.kind, &code.text, pw, ph).expect("generate code");

    // 3. Apply replacement edit
    session
        .apply(id, Edit::AddImage { page: 0, rect: Some(code.rect), name: "UpdatedCode.png".into(), bytes: Arc::new(png_bytes) })
        .expect("apply add image");

    // 4. Re-render page to RGBA buffer at scale 1.0 (72 DPI, 1 pt = 1 px)
    let doc = session.get(id).expect("get doc");
    let mut renderer = PageRenderer::new(doc.display.clone(), Default::default());
    let rendered = renderer.render(RenderRequest { page: 0, kind: RequestKind::Pixels, scale: 1.0, ..Default::default() });

    assert!(rendered.error.is_none(), "render failed: {:?}", rendered.error);
    assert_eq!(rendered.width, 200);
    assert_eq!(rendered.height, 200);

    // 5. Assert black pixels of the new barcode start exactly at (50, 50) and end at (150, 150)
    let mut min_x = usize::MAX;
    let mut max_x = 0usize;
    let mut min_y = usize::MAX;
    let mut max_y = 0usize;

    for y in 0..rendered.height as usize {
        for x in 0..rendered.width as usize {
            let idx = (y * rendered.width as usize + x) * 4;
            let r = rendered.rgba[idx];
            let g = rendered.rgba[idx + 1];
            let b = rendered.rgba[idx + 2];
            // Black/dark barcode bar pixel
            if r < 50 && g < 50 && b < 50 {
                min_x = min_x.min(x);
                max_x = max_x.max(x);
                min_y = min_y.min(y);
                max_y = max_y.max(y);
            }
        }
    }

    assert_eq!(min_x, 50, "barcode black pixels should start at x=50, found {min_x}");
    assert!(max_x == 149 || max_x == 150, "barcode black pixels should end at x=149/150, found {max_x}");
    assert_eq!(min_y, 50, "barcode black pixels should start at y=50, found {min_y}");
    assert!(max_y == 149 || max_y == 150, "barcode black pixels should end at y=149/150, found {max_y}");
}

#[test]
fn test_barcode_overlay_off_center_y_placement() {
    let mut session = Session::new();
    let blank = session.create_blank(300.0, 400.0, 1).expect("create blank page");
    let id = session.open_new("barcode_offcenter_test.pdf", blank).expect("open new");

    // Off-center user-space rectangle near the top of the page (y=[250, 350] on height=400).
    // In rendered top-down pixels (height=400), black pixels must land in y=[50, 150], not [250, 350].
    let rect = [20.0, 250.0, 140.0, 350.0];
    let png_bytes = Document::generate_replacement_code(CodeKind::Code128, "OFFCTR", 120, 100).expect("generate code");

    session
        .apply(id, Edit::AddImage { page: 0, rect: Some(rect), name: "OffCenter.png".into(), bytes: Arc::new(png_bytes) })
        .expect("apply add image");

    let doc = session.get(id).expect("get doc");
    let mut renderer = PageRenderer::new(doc.display.clone(), Default::default());
    let rendered = renderer.render(RenderRequest { page: 0, kind: RequestKind::Pixels, scale: 1.0, ..Default::default() });
    assert!(rendered.error.is_none(), "render failed: {:?}", rendered.error);

    let mut min_x = usize::MAX;
    let mut max_x = 0usize;
    let mut min_y = usize::MAX;
    let mut max_y = 0usize;
    for y in 0..rendered.height as usize {
        for x in 0..rendered.width as usize {
            let idx = (y * rendered.width as usize + x) * 4;
            if rendered.rgba[idx] < 50 && rendered.rgba[idx + 1] < 50 && rendered.rgba[idx + 2] < 50 {
                min_x = min_x.min(x);
                max_x = max_x.max(x);
                min_y = min_y.min(y);
                max_y = max_y.max(y);
            }
        }
    }
    assert_eq!(min_x, 20, "x start mismatch: {min_x}");
    assert!((139..=140).contains(&max_x), "x end mismatch: {max_x}");
    assert_eq!(min_y, 50, "top-down y start mismatch (expected 50, got {min_y})");
    assert!((149..=150).contains(&max_y), "top-down y end mismatch (expected 149..=150, got {max_y})");
}

#[test]
fn test_qr_code_inside_bordered_cell_keeps_exact_bounds() {
    use pdfcraft_engine::barcode_scan::scan_rgba_buffer;
    use pdfcraft_engine::qr::QrCode;

    let payload = "WQ-1313046-4916-2786";
    let matrix = QrCode::encode_text(payload).expect("encode qr");
    let module_px = 5usize;
    let qr_side = matrix.size * module_px;
    let canvas_w = 400usize;
    let canvas_h = 400usize;
    let qr_x0 = (canvas_w - qr_side) / 2;
    let qr_y0 = (canvas_h - qr_side) / 2;

    let mut rgba = vec![255u8; canvas_w * canvas_h * 4];
    let mut draw_rect = |x0: usize, y0: usize, x1: usize, y1: usize| {
        for y in y0.min(canvas_h)..y1.min(canvas_h) {
            for x in x0.min(canvas_w)..x1.min(canvas_w) {
                let idx = (y * canvas_w + x) * 4;
                rgba[idx] = 0;
                rgba[idx + 1] = 0;
                rgba[idx + 2] = 0;
            }
        }
    };

    let cell_x0 = qr_x0 - 40;
    let cell_x1 = qr_x0 + qr_side + 40;
    let cell_y0 = qr_y0 - 20;
    let cell_y1 = qr_y0 + qr_side + 20;
    draw_rect(cell_x0, cell_y0, cell_x1, cell_y0 + 2);
    draw_rect(cell_x0, cell_y1, cell_x1, cell_y1 + 2);
    draw_rect(cell_x0, cell_y0, cell_x0 + 2, cell_y1 + 2);
    draw_rect(cell_x1, cell_y0, cell_x1 + 2, cell_y1 + 2);

    for r in 0..matrix.size {
        for c in 0..matrix.size {
            if matrix.is_dark(r, c) {
                let x0 = qr_x0 + c * module_px;
                let y0 = qr_y0 + r * module_px;
                draw_rect(x0, y0, x0 + module_px, y0 + module_px);
            }
        }
    }

    let scanned = scan_rgba_buffer(canvas_w, canvas_h, &rgba, 0);
    assert_eq!(scanned.len(), 1);
    assert_eq!(scanned[0].text, payload);
    let r = scanned[0].rect;
    assert!((r[0] - qr_x0 as f64).abs() <= 2.0, "left edge inflated: expected {qr_x0}, got {r:?}");
    assert!((r[1] - qr_y0 as f64).abs() <= 2.0, "top edge inflated: expected {qr_y0}, got {r:?}");
    assert!((r[2] - (qr_x0 + qr_side) as f64).abs() <= 2.0, "right edge inflated: expected {}, got {r:?}", qr_x0 + qr_side);
    assert!((r[3] - (qr_y0 + qr_side) as f64).abs() <= 2.0, "bottom edge inflated: expected {}, got {r:?}", qr_y0 + qr_side);
    assert!(scanned[0].pixel_size[0].abs_diff(qr_side as u32) <= 3, "expected ~{qr_side}px width, got {:?}", scanned[0].pixel_size);
    assert!(scanned[0].pixel_size[1].abs_diff(qr_side as u32) <= 3, "expected ~{qr_side}px height, got {:?}", scanned[0].pixel_size);
}

#[test]
fn test_vector_qr_edit_never_duplicates_and_bake_removes_vector_paths() {
    use pdfcraft_cos::{Dict, Object, SaveOptions, Stream, write_full};
    use pdfcraft_engine::qr::QrCode;

    // Build a PDF page containing a vector outer border + a vector QR code (multiple `re f` paths).
    let orig_payload = "WQ-1313046-5017-3120";
    let qr = QrCode::encode_text(orig_payload).expect("encode vector qr");
    let qr_x0 = 120.0;
    let qr_y0 = 120.0;
    let mod_pt = 2.25;
    let qr_side = qr.size as f64 * mod_pt;

    let mut content = String::new();
    // Outer frame well outside QR rect
    content.push_str("1 w 0 0 0 RG 20 20 260 260 re S\n");
    content.push_str("0 0 0 rg\n");
    for r in 0..qr.size {
        for c in 0..qr.size {
            if qr.is_dark(r, c) {
                let x = qr_x0 + (c as f64) * mod_pt;
                let y = qr_y0 + ((qr.size - 1 - r) as f64) * mod_pt;
                content.push_str(&format!("{x:.3} {y:.3} {mod_pt:.3} {mod_pt:.3} re f\n"));
            }
        }
    }

    let mut cos = pdfcraft_cos::Document::new_empty();
    let pages = cos.get(cos.root().unwrap()).as_dict().unwrap().reference(b"Pages").unwrap();
    let stream_ref = cos.add(Object::Stream(Stream::from_raw(Dict::new(), content.into_bytes())));
    let mut page = Dict::new();
    page.set(b"Type".to_vec(), Object::name("Page"));
    page.set(b"Parent".to_vec(), Object::Ref(pages));
    page.set(b"MediaBox".to_vec(), Object::Array(vec![0.into(), 0.into(), 300.into(), 300.into()]));
    page.set(b"Contents".to_vec(), Object::Ref(stream_ref));
    let page_ref = cos.add(page);
    cos.update_dict(pages, |d| {
        d.set(b"Kids".to_vec(), Object::Array(vec![Object::Ref(page_ref)]));
        d.set(b"Count".to_vec(), Object::Int(1));
    })
    .unwrap();
    let pdf_bytes = Arc::new(write_full(&cos, &SaveOptions::default()).unwrap());

    let mut session = Session::new();
    let id = session.open("vec_qr.pdf", None, pdf_bytes, None).unwrap();

    let detected0 = session.get(id).unwrap().scan_page_codes(0);
    assert_eq!(detected0.len(), 1, "initial vector page must detect 1 QR code: {detected0:?}");
    assert_eq!(detected0[0].text, orig_payload);
    assert_eq!(detected0[0].image_index, None, "vector QR should initially have no image_index");
    let rect = detected0[0].rect;
    assert!((rect[0] - qr_x0).abs() < 3.0 && (rect[2] - (qr_x0 + qr_side)).abs() < 3.0, "unexpected vector bounds: {rect:?}");

    // Replace vector QR with whiteout + UpdatedCode (1st edit -> still 1 code, now backed by image_index).
    let whiteout = Document::generate_whiteout(detected0[0].pixel_size[0], detected0[0].pixel_size[1]).unwrap();
    let first_edit_png = Document::generate_replacement_code(CodeKind::QrCode, "FIRST-EDIT-001", 80, 80).unwrap();
    let w_rect = [rect[0] - 1.5, rect[1] - 1.5, rect[2] + 1.5, rect[3] + 1.5];
    session
        .apply(
            id,
            Edit::Batch {
                label: "Replace code".into(),
                edits: vec![
                    Edit::AddImage { page: 0, rect: Some(w_rect), name: "Whiteout.png".into(), bytes: Arc::new(whiteout) },
                    Edit::AddImage { page: 0, rect: Some(rect), name: "UpdatedCode.png".into(), bytes: Arc::new(first_edit_png) },
                ],
            },
        )
        .unwrap();

    let detected1 = session.get(id).unwrap().scan_page_codes(0);
    assert_eq!(detected1.len(), 1, "after 1st edit must still detect exactly 1 code: {detected1:?}");
    assert_eq!(detected1[0].text, "FIRST-EDIT-001");
    let img_idx = detected1[0].image_index.expect("replacement QR must be recognized as standalone PageImage");
    assert!(!session.get(id).unwrap().page_whiteout_rects(0).is_empty(), "whiteout overlay must be reported before bake");

    // 2nd edit replaces in-place (or even if another overlay was stacked, occlusion keeps count = 1).
    let second_edit_png = Document::generate_replacement_code(CodeKind::QrCode, "SECOND-EDIT-002", 80, 80).unwrap();
    session
        .apply(
            id,
            Edit::EditPageImage {
                page: 0,
                index: img_idx,
                change: pdfcraft_engine::ImageEdit::Replace { name: "UpdatedCode.png".into(), bytes: Arc::new(second_edit_png) },
            },
        )
        .unwrap();

    let detected2 = session.get(id).unwrap().scan_page_codes(0);
    assert_eq!(detected2.len(), 1, "after 2nd edit must still detect exactly 1 code: {detected2:?}");
    assert_eq!(detected2[0].text, "SECOND-EDIT-002");

    // Bake white layer -> removes whiteout overlay & underlying vector paths while keeping replacement QR & outer border.
    session.apply(id, Edit::BakeBarcodeWhiteout { page: 0, rects: Vec::new() }).unwrap();
    let doc_after_bake = session.get(id).unwrap();
    assert!(doc_after_bake.page_whiteout_rects(0).is_empty(), "bake must remove loose whiteout overlays");
    let detected3 = doc_after_bake.scan_page_codes(0);
    assert_eq!(detected3.len(), 1, "baked page must keep 1 replacement QR: {detected3:?}");
    assert_eq!(detected3[0].text, "SECOND-EDIT-002");
    assert!(detected3[0].image_index.is_some(), "replacement QR must stay selectable after bake");

    // Even if the baked replacement QR is deleted, the old vector QR must NOT come back (because vector paths were removed)!
    let final_idx = detected3[0].image_index.unwrap();
    session.apply(id, Edit::EditPageImage { page: 0, index: final_idx, change: pdfcraft_engine::ImageEdit::Delete }).unwrap();
    let detected4 = session.get(id).unwrap().scan_page_codes(0);
    assert!(detected4.is_empty(), "after deleting baked QR, old vector QR must be permanently gone: {detected4:?}");
}

#[test]
fn test_vector_qr_56pt_in_cell_keeps_exact_1to1_size_after_replacement() {
    use pdfcraft_cos::{Dict, Object, SaveOptions, Stream, write_full};
    use pdfcraft_engine::qr::QrCode;

    let orig_payload = "WQ-1313046-5031-3060";
    let qr = QrCode::encode_text(orig_payload).expect("encode qr");
    let mod_pt = 56.25 / (qr.size as f64);
    let qr_x0 = 110.0;
    let qr_y0 = 70.0;
    let qr_side = qr.size as f64 * mod_pt;

    let mut content = String::new();
    // Table cell lines closely around QR (~11pt gap top/bottom)
    content.push_str("1 w 0 0 0 RG 20 58 240 80 re S\n0 0 0 rg\n");
    for r in 0..qr.size {
        for c in 0..qr.size {
            if qr.is_dark(r, c) {
                let x = qr_x0 + (c as f64) * mod_pt;
                let y = qr_y0 + ((qr.size - 1 - r) as f64) * mod_pt;
                content.push_str(&format!("{x:.3} {y:.3} {mod_pt:.3} {mod_pt:.3} re f\n"));
            }
        }
    }

    let mut cos = pdfcraft_cos::Document::new_empty();
    let pages = cos.get(cos.root().unwrap()).as_dict().unwrap().reference(b"Pages").unwrap();
    let stream_ref = cos.add(Object::Stream(Stream::from_raw(Dict::new(), content.into_bytes())));
    let mut page = Dict::new();
    page.set(b"Type".to_vec(), Object::name("Page"));
    page.set(b"Parent".to_vec(), Object::Ref(pages));
    page.set(b"MediaBox".to_vec(), Object::Array(vec![0.into(), 0.into(), 280.into(), 320.into()]));
    page.set(b"Contents".to_vec(), Object::Ref(stream_ref));
    let page_ref = cos.add(page);
    cos.update_dict(pages, |d| {
        d.set(b"Kids".to_vec(), Object::Array(vec![Object::Ref(page_ref)]));
        d.set(b"Count".to_vec(), Object::Int(1));
    })
    .unwrap();
    let pdf_bytes = Arc::new(write_full(&cos, &SaveOptions::default()).unwrap());

    let mut session = Session::new();
    let id = session.open("sticker56.pdf", None, pdf_bytes, None).unwrap();
    let detected = session.get(id).unwrap().scan_page_codes(0);
    assert_eq!(detected.len(), 1);
    let w = detected[0].rect[2] - detected[0].rect[0];
    let h = detected[0].rect[3] - detected[0].rect[1];
    assert!((w - qr_side).abs() <= 2.5 && (h - qr_side).abs() <= 2.5, "expected ~{qr_side:.2}pt, got {w:.2}x{h:.2}pt ({:?})", detected[0]);

    let png = Document::generate_replacement_code(CodeKind::QrCode, "WQ-1313046-5031-3061", 113, 113).unwrap();
    session.apply(id, Edit::AddImage { page: 0, rect: Some(detected[0].rect), name: "UpdatedCode.png".into(), bytes: Arc::new(png) }).unwrap();
    let detected2 = session.get(id).unwrap().scan_page_codes(0);
    assert_eq!(detected2.len(), 1);
    let w2 = detected2[0].rect[2] - detected2[0].rect[0];
    let h2 = detected2[0].rect[3] - detected2[0].rect[1];
    assert!(
        (w2 - qr_side).abs() <= 2.5 && (h2 - qr_side).abs() <= 2.5,
        "replacement grew! expected ~{qr_side:.2}pt, got {w2:.2}x{h2:.2}pt ({:?})",
        detected2[0]
    );
}

#[test]
fn test_framed_qr_with_quiet_zone_keeps_inner_size_when_replaced_in_frame() {
    // Reproduces embedded 74.5x74.5 pt image whose inner QR modules occupy 56.25x56.25 pt
    // (surrounded by a ~9.125 pt white quiet zone on all four sides). Replacing the image stream
    // with generate_replacement_code_in_frame must preserve the ~56.25 pt QR module size rather
    // than stretching modules to 74.5 pt.
    let frame_px = 149u32;
    let inner_x0 = 18u32;
    let inner_y0 = 18u32;
    let inner_side = 113u32;
    let frame_rect = [100.0, 60.0, 174.5, 134.5];
    let inner_rect_px = [inner_x0, inner_y0, inner_x0 + inner_side, inner_y0 + inner_side];

    let initial_png =
        Document::generate_replacement_code_in_frame(CodeKind::QrCode, "WQ-1313046-5031-3060", frame_px, frame_px, inner_rect_px).unwrap();
    let mut cos = pdfcraft_cos::Document::new_empty();
    let pages = cos.get(cos.root().unwrap()).as_dict().unwrap().reference(b"Pages").unwrap();
    let mut page = pdfcraft_cos::Dict::new();
    page.set(b"Type".to_vec(), pdfcraft_cos::Object::name("Page"));
    page.set(b"Parent".to_vec(), pdfcraft_cos::Object::Ref(pages));
    page.set(b"MediaBox".to_vec(), pdfcraft_cos::Object::Array(vec![0.into(), 0.into(), 280.into(), 320.into()]));
    let page_ref = cos.add(page);
    cos.update_dict(pages, |d| {
        d.set(b"Kids".to_vec(), pdfcraft_cos::Object::Array(vec![pdfcraft_cos::Object::Ref(page_ref)]));
        d.set(b"Count".to_vec(), pdfcraft_cos::Object::Int(1));
    })
    .unwrap();
    let pdf_bytes = Arc::new(pdfcraft_cos::write_full(&cos, &pdfcraft_cos::SaveOptions::default()).unwrap());

    let mut session = Session::new();
    let id = session.open("framed.pdf", None, pdf_bytes, None).unwrap();
    session.apply(id, Edit::AddImage { page: 0, rect: Some(frame_rect), name: "FramedQr.png".into(), bytes: Arc::new(initial_png) }).unwrap();

    let detected = session.get(id).unwrap().scan_page_codes(0);
    assert_eq!(detected.len(), 1);
    let orig_w = detected[0].rect[2] - detected[0].rect[0];
    let orig_h = detected[0].rect[3] - detected[0].rect[1];
    assert!((orig_w - 56.5).abs() <= 2.5 && (orig_h - 56.5).abs() <= 2.5, "expected ~56.5pt inner QR, got {orig_w:.2}x{orig_h:.2}pt");
    let img_idx = detected[0].image_index.expect("framed QR must map to embedded image");

    // Map detected PDF rect into frame pixel space and replace in-frame.
    let fw = frame_rect[2] - frame_rect[0];
    let fh = frame_rect[3] - frame_rect[1];
    let px0 = (((detected[0].rect[0] - frame_rect[0]) / fw) * frame_px as f64).round().max(0.0) as u32;
    let px1 = (((detected[0].rect[2] - frame_rect[0]) / fw) * frame_px as f64).round().min(frame_px as f64) as u32;
    // PDF Y increases upward while pixel Y increases downward.
    let py0 = (((frame_rect[3] - detected[0].rect[3]) / fh) * frame_px as f64).round().max(0.0) as u32;
    let py1 = (((frame_rect[3] - detected[0].rect[1]) / fh) * frame_px as f64).round().min(frame_px as f64) as u32;

    let updated_png =
        Document::generate_replacement_code_in_frame(CodeKind::QrCode, "WQ-1313046-5031-3061", frame_px, frame_px, [px0, py0, px1, py1]).unwrap();
    session
        .apply(
            id,
            Edit::EditPageImage {
                page: 0,
                index: img_idx,
                change: pdfcraft_engine::ImageEdit::Replace { name: "UpdatedCode.png".into(), bytes: Arc::new(updated_png) },
            },
        )
        .unwrap();

    let detected2 = session.get(id).unwrap().scan_page_codes(0);
    assert_eq!(detected2.len(), 1);
    assert_eq!(detected2[0].text, "WQ-1313046-5031-3061");
    let new_w = detected2[0].rect[2] - detected2[0].rect[0];
    let new_h = detected2[0].rect[3] - detected2[0].rect[1];
    assert!(
        (new_w - orig_w).abs() <= 2.0 && (new_h - orig_h).abs() <= 2.0,
        "in-frame replacement must stay {orig_w:.2}x{orig_h:.2}pt, got {new_w:.2}x{new_h:.2}pt"
    );
}
