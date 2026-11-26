//! Barcode & QR whiteout baking, vector region erasure, and crisp region capture (L6 Engine).
//!
//! When a vector QR code or barcode is moved, replaced, or its whiteout overlay is baked:
//! - vector paths and buried images wholly inside the target region are removed from page and
//!   shared Form XObject streams (copy-on-write, so other pages stay untouched);
//! - paths or images that cross outside the region are clipped out with an even-odd mask so
//!   surrounding table frames and artwork stay intact;
//! - temporary loose `Whiteout.png` `AddedContent::Image` overlays are removed and replaced by a
//!   baked non-editable page stream placed beneath any kept replacement barcode image.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::collections::HashSet;

use pdfcraft_content::{Matrix, Op, Pieces, num, overlaps, serialize_ops};
use pdfcraft_cos::{CosError, DecodeBudget, Dict, Document as CosDoc, ObjRef, Object, Stream};
use pdfcraft_edit::Content as AddedContent;

use crate::{Document, EditError};

const MAX_FORM_DEPTH: usize = 16;
const MAX_WHITEOUT_PIXELS: usize = 2048 * 2048;

fn rect_contained(inner: [f64; 4], outer: [f64; 4], tol: f64) -> bool {
    inner[0] >= outer[0] - tol && inner[1] >= outer[1] - tol && inner[2] <= outer[2] + tol && inner[3] <= outer[3] + tol
}

fn is_whiteout_image_stream(cos: &CosDoc, image_ref: ObjRef) -> bool {
    let Object::Stream(s) = &*cos.get(image_ref) else {
        return false;
    };
    if s.dict.name(b"Subtype") != Some(b"Image") || s.dict.contains(b"SMask") || s.dict.contains(b"Mask") {
        return false;
    }
    let (Some(w), Some(h)) = (s.dict.int(b"Width"), s.dict.int(b"Height")) else {
        return false;
    };
    if w <= 0 || h <= 0 {
        return false;
    }
    let Some(px) = usize::try_from(w).ok().and_then(|wu| usize::try_from(h).ok().and_then(|hu| wu.checked_mul(hu))) else {
        return false;
    };
    if px > MAX_WHITEOUT_PIXELS {
        return false;
    }
    let Ok((_, bytes)) = pdfcraft_create::image_file(cos, image_ref) else {
        return false;
    };
    let opts = craft_image_codecs::DecodeOptions {
        limits: craft_image_codecs::Limits {
            max_pixels: MAX_WHITEOUT_PIXELS as u64,
            max_alloc: (MAX_WHITEOUT_PIXELS as u64) * 4,
            ..Default::default()
        },
        ..Default::default()
    };
    let Ok(img) = craft_image_codecs::decode_with(&bytes, &opts) else {
        return false;
    };
    let rgba = img.to_rgba8();
    !rgba.is_empty() && rgba.as_chunks::<4>().0.iter().all(|p| p[0] >= 250 && p[1] >= 250 && p[2] >= 250 && p[3] >= 250)
}

/// User-space rectangles of all unbaked solid-white overlay images on `page`.
pub fn page_whiteout_rects(doc: &Document, page: usize) -> Vec<[f64; 4]> {
    crate::guard(|| {
        let Some(editor) = doc.editor.as_ref() else {
            return Vec::new();
        };
        let added_imgs: Vec<ObjRef> = doc
            .added()
            .iter()
            .filter(|a| a.page == page)
            .filter_map(|a| match &a.content {
                AddedContent::Image(im) => Some(im.image),
                _ => None,
            })
            .collect();
        if added_imgs.is_empty() {
            return Vec::new();
        }
        let white_refs: HashSet<ObjRef> = added_imgs.into_iter().filter(|&r| is_whiteout_image_stream(&editor.cos, r)).collect();
        if white_refs.is_empty() {
            return Vec::new();
        }
        doc.page_images(page).into_iter().filter_map(|im| im.object.filter(|r| white_refs.contains(r)).map(|_| im.rect)).collect()
    })
    .unwrap_or_default()
}

/// Render `user_rect` on `page` at high resolution and return `(width, height, rgba)`.
pub fn capture_region_rgba(doc: &Document, page: usize, user_rect: [f64; 4]) -> Result<(u32, u32, Vec<u8>), String> {
    let Some(page_info) = doc.info.pages.get(page) else {
        return Err("target page does not exist".into());
    };
    const MAX_PIXELS: f32 = 16_000_000.0;
    let max_scale = (MAX_PIXELS / (page_info.width * page_info.height).max(1.0)).sqrt();
    let eff_scale = 4.0f32.min(max_scale).max(1.0);
    let mut renderer = pdfcraft_render::PageRenderer::new(doc.display.clone(), doc.config.clone());
    let rendered = renderer.render(pdfcraft_render::RenderRequest { page, scale: eff_scale, ..Default::default() });
    if rendered.error.is_some() || rendered.width == 0 || rendered.height == 0 || rendered.rgba.is_empty() {
        return Err("failed to render page region".into());
    }
    let rw = rendered.width as usize;
    let rh = rendered.height as usize;
    let p0 = page_info.user_to_view(user_rect[0] as f32, user_rect[3] as f32);
    let p1 = page_info.user_to_view(user_rect[2] as f32, user_rect[1] as f32);
    let rx0 = ((p0[0].min(p1[0]) * eff_scale).floor().max(0.0) as usize).min(rw);
    let rx1 = ((p0[0].max(p1[0]) * eff_scale).ceil().max(0.0) as usize).min(rw);
    let ry0 = ((p0[1].min(p1[1]) * eff_scale).floor().max(0.0) as usize).min(rh);
    let ry1 = ((p0[1].max(p1[1]) * eff_scale).ceil().max(0.0) as usize).min(rh);
    let cw = rx1.saturating_sub(rx0);
    let ch = ry1.saturating_sub(ry0);
    if cw < 4 || ch < 4 {
        return Err("captured barcode region is too small".into());
    }
    let mut rgba = vec![255u8; cw.saturating_mul(ch).saturating_mul(4)];
    for row in 0..ch {
        let src_start = ((ry0 + row) * rw + rx0) * 4;
        let dst_start = row * cw * 4;
        let len = cw * 4;
        if let (Some(src), Some(dst)) = (rendered.rgba.get(src_start..src_start + len), rgba.get_mut(dst_start..dst_start + len)) {
            dst.copy_from_slice(src);
        }
    }
    Ok((cw as u32, ch as u32, rgba))
}

/// Render `user_rect` on `page` at high resolution and encode it as a PNG.
pub fn capture_region_png(doc: &Document, page: usize, user_rect: [f64; 4]) -> Result<Vec<u8>, String> {
    let (w, h, rgba) = capture_region_rgba(doc, page, user_rect)?;
    let img = craft_image_codecs::Image::from_u8(w, h, craft_image_codecs::ChannelLayout::Rgba, rgba).map_err(|e| e.to_string())?;
    craft_image_codecs::encode(&img, craft_image_codecs::Format::Png, &craft_image_codecs::EncodeOptions::default()).map_err(|e| e.to_string())
}

fn page_content_refs(cos: &CosDoc, page_dict: &Dict) -> Vec<Object> {
    match page_dict.get(b"Contents").map(|c| cos.resolve(c)) {
        Some(obj) => match &*obj {
            Object::Array(arr) => arr.clone(),
            _ => page_dict.get(b"Contents").cloned().into_iter().collect(),
        },
        None => Vec::new(),
    }
}

fn res_dict(cos: &CosDoc, resources: &Dict, key: &[u8]) -> Dict {
    resources.get(key).map(|o| cos.resolve(o)).and_then(|o| o.as_dict().cloned()).unwrap_or_default()
}

fn clip_out(rects: &[[f64; 4]], ctm: &Matrix, around: [f64; 4]) -> Option<Vec<Op>> {
    let inv = ctm.invert()?;
    let mut ops = Vec::new();
    let big = [around[0] - 10.0, around[1] - 10.0, around[2] + 10.0, around[3] + 10.0];
    for r in rects.iter().filter(|r| overlaps(**r, around, 0.0)) {
        let outer = [(big[0], big[1]), (big[2], big[1]), (big[2], big[3]), (big[0], big[3])];
        let inner = [(r[0], r[1]), (r[0], r[3]), (r[2], r[3]), (r[2], r[1])];
        for poly in [outer, inner] {
            for (k, (x, y)) in poly.iter().enumerate() {
                let (u, v) = inv.apply(*x, *y);
                ops.push(Op::new(if k == 0 { "m" } else { "l" }, vec![num(u), num(v)]));
            }
            ops.push(Op::new("h", vec![]));
        }
        ops.push(Op::new("W*", vec![]));
        ops.push(Op::new("n", vec![]));
    }
    (!ops.is_empty()).then_some(ops)
}

#[derive(Clone)]
struct GsState {
    ctm: Matrix,
    line_width: f64,
    miter_limit: f64,
    line_join: i64,
    line_cap: i64,
}

impl GsState {
    fn new(ctm: Matrix) -> Self {
        Self { ctm, line_width: 1.0, miter_limit: 10.0, line_join: 0, line_cap: 0 }
    }
}

struct ScopeOut {
    streams: Vec<Option<Vec<u8>>>,
    updated_xobjects: Vec<(Vec<u8>, ObjRef)>,
    changed_any: bool,
}

fn split_into_pieces(joined: &Pieces, ops: Vec<Op>, count: usize) -> (Vec<Vec<Op>>, Vec<bool>) {
    let mut pieces: Vec<Vec<Op>> = (0..count).map(|_| Vec::new()).collect();
    let mut rewrite: Vec<bool> = vec![false; count];
    for op in ops {
        let (first, last) = joined.pieces_of(&op);
        if first < last {
            for r in rewrite.iter_mut().take(last + 1).skip(first) {
                *r = true;
            }
        }
        if let Some(p) = pieces.get_mut(last) {
            p.push(op);
        }
    }
    (pieces, rewrite)
}

fn piece_bytes(ops: &[Op], (comment_in, comment_out): (bool, bool)) -> Vec<u8> {
    let mut bytes = if comment_in { b"\n".to_vec() } else { Vec::new() };
    bytes.extend(serialize_ops(ops));
    if comment_out {
        bytes.push(b'%');
    }
    bytes
}

struct WalkCtx<'a> {
    rects: &'a [[f64; 4]],
    gs: GsState,
    stack: Vec<GsState>,
    ext_gstates: Dict,
    xobjects: Dict,
    path: Vec<Op>,
    path_box: Option<[f64; 4]>,
    clip: bool,
    pending: Vec<Op>,
    updated_xobjects: Vec<(Vec<u8>, ObjRef)>,
    form_cache: std::collections::HashMap<(Vec<u8>, [i64; 6]), ObjRef>,
    depth: usize,
    budget: &'a mut DecodeBudget,
}

impl<'a> WalkCtx<'a> {
    fn new(cos: &CosDoc, resources: &Dict, rects: &'a [[f64; 4]], ctm: Matrix, depth: usize, budget: &'a mut DecodeBudget) -> Self {
        Self {
            rects,
            gs: GsState::new(ctm),
            stack: Vec::new(),
            ext_gstates: res_dict(cos, resources, b"ExtGState"),
            xobjects: res_dict(cos, resources, b"XObject"),
            path: Vec::new(),
            path_box: None,
            clip: false,
            pending: Vec::new(),
            updated_xobjects: Vec::new(),
            form_cache: std::collections::HashMap::new(),
            depth,
            budget,
        }
    }

    fn construct_path(&mut self, op: Op) {
        let o = op.op.as_slice();
        let pts: Vec<(f64, f64)> = match o {
            b"re" => op.nums::<4>().map(|[x, y, w, h]| vec![(x, y), (x + w, y), (x, y + h), (x + w, y + h)]).unwrap_or_default(),
            b"m" | b"l" => op.nums::<2>().map(|[x, y]| vec![(x, y)]).unwrap_or_default(),
            b"c" => op.nums::<6>().map(|[x1, y1, x2, y2, x3, y3]| vec![(x1, y1), (x2, y2), (x3, y3)]).unwrap_or_default(),
            b"v" | b"y" => op.nums::<4>().map(|[x1, y1, x2, y2]| vec![(x1, y1), (x2, y2)]).unwrap_or_default(),
            _ => Vec::new(),
        };
        for (x, y) in pts {
            let (u, v) = self.gs.ctm.apply(x, y);
            self.path_box = Some(match self.path_box {
                None => [u, v, u, v],
                Some(b) => [b[0].min(u), b[1].min(v), b[2].max(u), b[3].max(v)],
            });
        }
        self.clip |= matches!(o, b"W" | b"W*");
        self.path.push(op);
    }

    fn stroke_box(&self, b: [f64; 4]) -> [f64; 4] {
        let m = self.gs.ctm.0;
        let scale = (m[0] * m[0] + m[1] * m[1] + m[2] * m[2] + m[3] * m[3]).sqrt();
        let joins = self.path.iter().filter(|op| matches!(op.op.as_slice(), b"l" | b"c" | b"v" | b"y" | b"h" | b"re")).count() >= 2
            || self.path.iter().any(|op| op.op.as_slice() == b"re");
        let miter = if joins && self.gs.line_join == 0 { self.gs.miter_limit.max(1.0) } else { 1.0 };
        let reach = if self.gs.line_cap == 2 { miter.max(std::f64::consts::SQRT_2) } else { miter };
        let pad = (self.gs.line_width.max(1.0) / 2.0 * scale).max(0.5) * reach;
        [b[0] - pad, b[1] - pad, b[2] + pad, b[3] + pad]
    }

    fn paint_path(&mut self, op: Op, out_ops: &mut Vec<Op>, changed: &mut bool) {
        let strokes = matches!(op.op.as_slice(), b"S" | b"s" | b"B" | b"B*" | b"b" | b"b*");
        let path_box = self.path_box.take();
        let bbox = path_box.map(|b| if strokes { self.stroke_box(b) } else { b });
        let body = std::mem::take(&mut self.path);
        let clips = std::mem::replace(&mut self.clip, false);
        let pending = std::mem::take(&mut self.pending);
        let hits = op.op.as_slice() != b"n" && bbox.is_some_and(|b| self.rects.iter().any(|r| overlaps(*r, b, 0.0)));
        if !hits || clips {
            out_ops.extend(body);
            out_ops.extend(pending);
            out_ops.push(op);
            return;
        }
        let b = bbox.unwrap_or_default();
        if self.rects.iter().any(|r| rect_contained(b, *r, 1.0)) {
            out_ops.extend(pending);
            *changed = true;
            return;
        }
        if let Some(clip_ops) = clip_out(self.rects, &self.gs.ctm, b) {
            out_ops.push(Op::new("q", vec![]));
            out_ops.extend(clip_ops);
            out_ops.extend(body);
            out_ops.extend(pending);
            out_ops.push(op);
            out_ops.push(Op::new("Q", vec![]));
            *changed = true;
        } else {
            out_ops.extend(body);
            out_ops.extend(pending);
            out_ops.push(op);
        }
    }

    fn graphics_state(&mut self, cos: &CosDoc, op: &Op) {
        let last = op.nums::<1>().map(|[v]| v);
        match op.op.as_slice() {
            b"q" => self.stack.push(self.gs.clone()),
            b"Q" => {
                if let Some(prev) = self.stack.pop() {
                    self.gs = prev;
                }
            }
            b"cm" => {
                if let Some(m) = op.nums::<6>() {
                    self.gs.ctm = Matrix(m).then(&self.gs.ctm);
                }
            }
            b"w" => self.gs.line_width = last.unwrap_or(self.gs.line_width),
            b"M" => self.gs.miter_limit = last.unwrap_or(self.gs.miter_limit),
            b"j" => self.gs.line_join = last.map_or(self.gs.line_join, |j| j as i64),
            b"J" => self.gs.line_cap = last.map_or(self.gs.line_cap, |j| j as i64),
            b"gs" => {
                if let Some(state) = op.name(0).and_then(|n| self.ext_gstates.get(n)).and_then(|s| cos.resolve(s).as_dict().cloned()) {
                    let num_val = |key: &[u8]| state.get(key).and_then(|v| cos.resolve(v).as_f64());
                    self.gs.line_width = num_val(b"LW").unwrap_or(self.gs.line_width);
                    self.gs.miter_limit = num_val(b"ML").unwrap_or(self.gs.miter_limit);
                    self.gs.line_join = num_val(b"LJ").map_or(self.gs.line_join, |j| j as i64);
                    self.gs.line_cap = num_val(b"LC").map_or(self.gs.line_cap, |j| j as i64);
                }
            }
            _ => {}
        }
    }

    fn handle_do(&mut self, cos: &mut CosDoc, resources: &Dict, op: Op, out_ops: &mut Vec<Op>, changed: &mut bool) -> Result<(), CosError> {
        let Some(name) = op.name(0) else {
            out_ops.push(op);
            return Ok(());
        };
        let name_vec = name.to_vec();
        let Some(xo_obj) = self.xobjects.get(name).cloned() else {
            out_ops.push(op);
            return Ok(());
        };
        let resolved = cos.resolve(&xo_obj);
        let Object::Stream(stream) = &*resolved else {
            out_ops.push(op);
            return Ok(());
        };
        let subtype = stream.dict.name(b"Subtype");
        if subtype == Some(b"Image") {
            let b = self.gs.ctm.bbox([0.0, 0.0, 1.0, 1.0]);
            if !self.rects.iter().any(|r| overlaps(*r, b, 0.0)) {
                out_ops.push(op);
                return Ok(());
            }
            if self.rects.iter().any(|r| rect_contained(b, *r, 1.0)) {
                *changed = true;
                return Ok(());
            }
            if let Some(clip_ops) = clip_out(self.rects, &self.gs.ctm, b) {
                out_ops.push(Op::new("q", vec![]));
                out_ops.extend(clip_ops);
                out_ops.push(op);
                out_ops.push(Op::new("Q", vec![]));
                *changed = true;
                return Ok(());
            }
            out_ops.push(op);
            return Ok(());
        }
        if subtype == Some(b"Form") {
            let Some(bbox) = form_bbox(cos, &stream.dict) else {
                out_ops.push(op);
                return Ok(());
            };
            let fmat = form_matrix(cos, &stream.dict).then(&self.gs.ctm);
            let world_box = fmat.bbox(bbox);
            if !self.rects.iter().any(|r| overlaps(*r, world_box, 0.0)) {
                out_ops.push(op);
                return Ok(());
            }
            if self.rects.iter().any(|r| rect_contained(world_box, *r, 1.0)) {
                *changed = true;
                return Ok(());
            }
            if self.depth >= MAX_FORM_DEPTH || !self.budget.enter_form() {
                if let Some(clip_ops) = clip_out(self.rects, &self.gs.ctm, world_box) {
                    out_ops.push(Op::new("q", vec![]));
                    out_ops.extend(clip_ops);
                    out_ops.push(op);
                    out_ops.push(Op::new("Q", vec![]));
                    *changed = true;
                } else {
                    out_ops.push(op);
                }
                return Ok(());
            }
            let ctm_key = fmat.0.map(|v| (v * 1000.0).round() as i64);
            if let Some(&existing_ref) = self.form_cache.get(&(name_vec.clone(), ctm_key)) {
                let mut new_name = name_vec.clone();
                if let Some(entry) = self.updated_xobjects.iter().find(|(_, r)| *r == existing_ref) {
                    new_name = entry.0.clone();
                }
                out_ops.push(Op::new("Do", vec![Object::Name(new_name)]));
                *changed = true;
                return Ok(());
            }
            let form_res =
                stream.dict.get(b"Resources").map(|r| cos.resolve(r)).and_then(|r| r.as_dict().cloned()).unwrap_or_else(|| resources.clone());
            let Ok(form_data) = self.budget.decode_strict(cos, stream) else {
                out_ops.push(op);
                return Ok(());
            };
            let mut form_dict = stream.dict.clone();
            drop(resolved);
            let sub_out = erase_streams(cos, &[form_data], &form_res, self.rects, fmat, self.depth + 1, self.budget)?;
            if !sub_out.changed_any {
                out_ops.push(op);
                return Ok(());
            }
            let Some(Some(new_bytes)) = sub_out.streams.into_iter().next() else {
                out_ops.push(op);
                return Ok(());
            };
            if !sub_out.updated_xobjects.is_empty() {
                let mut sub_res = form_res;
                let mut sub_xo = sub_res.get(b"XObject").and_then(|x| cos.resolve(x).as_dict().cloned()).unwrap_or_default();
                for (n, r) in sub_out.updated_xobjects {
                    sub_xo.set(n, Object::Ref(r));
                }
                sub_res.set(b"XObject".to_vec(), Object::Dict(sub_xo));
                form_dict.set(b"Resources".to_vec(), Object::Dict(sub_res));
            }
            form_dict.remove(b"Length");
            let new_ref = cos.add(Object::Stream(Stream::flate(form_dict, &new_bytes)));
            let mut copy_name = Vec::with_capacity(name_vec.len() + 12);
            copy_name.extend_from_slice(&name_vec);
            copy_name.extend_from_slice(format!("_pcbake{}", new_ref.num).as_bytes());
            self.updated_xobjects.push((copy_name.clone(), new_ref));
            self.form_cache.insert((name_vec, ctm_key), new_ref);
            out_ops.push(Op::new("Do", vec![Object::Name(copy_name)]));
            *changed = true;
            return Ok(());
        }
        out_ops.push(op);
        Ok(())
    }
}

fn form_bbox(cos: &CosDoc, dict: &Dict) -> Option<[f64; 4]> {
    let arr = dict.get(b"BBox").map(|v| cos.resolve(v))?;
    let a = arr.as_array()?;
    if a.len() != 4 {
        return None;
    }
    let b = [a[0].as_f64()?, a[1].as_f64()?, a[2].as_f64()?, a[3].as_f64()?];
    (b.iter().all(|v| v.is_finite()) && b[2] > b[0] && b[3] > b[1]).then_some(b)
}

fn form_matrix(cos: &CosDoc, dict: &Dict) -> Matrix {
    let Some(v) = dict.get(b"Matrix").map(|m| cos.resolve(m)) else {
        return Matrix::IDENTITY;
    };
    v.as_array().and_then(|a| Matrix::from_operands(a)).unwrap_or(Matrix::IDENTITY)
}

fn erase_streams(
    cos: &mut CosDoc,
    streams: &[Vec<u8>],
    resources: &Dict,
    rects: &[[f64; 4]],
    ctm: Matrix,
    depth: usize,
    budget: &mut DecodeBudget,
) -> Result<ScopeOut, CosError> {
    if streams.is_empty() {
        return Ok(ScopeOut { streams: Vec::new(), updated_xobjects: Vec::new(), changed_any: false });
    }
    let slices: Vec<&[u8]> = streams.iter().map(Vec::as_slice).collect();
    let joined = Pieces::join(&slices);
    let parsed = joined.parse();
    let comment_edges = joined.comment_edges(&parsed);
    let (pieces, initial_rewrite) = split_into_pieces(&joined, parsed.ops, streams.len());
    let mut walk = WalkCtx::new(cos, resources, rects, ctm, depth, budget);
    let mut processed = Vec::with_capacity(pieces.len());
    let mut changed_any = initial_rewrite.iter().any(|&c| c);

    for (piece, mut changed) in pieces.into_iter().zip(initial_rewrite) {
        let mut out_ops = Vec::with_capacity(piece.len());
        for op in piece {
            let o = op.op.as_slice();
            if matches!(o, b"m" | b"l" | b"c" | b"v" | b"y" | b"h" | b"re" | b"W" | b"W*") {
                walk.construct_path(op);
                continue;
            }
            if matches!(o, b"S" | b"s" | b"f" | b"F" | b"f*" | b"B" | b"B*" | b"b" | b"b*" | b"n") {
                walk.paint_path(op, &mut out_ops, &mut changed);
                continue;
            }
            if !walk.path.is_empty() && matches!(o, b"w" | b"M" | b"j" | b"J" | b"d" | b"gs") {
                walk.graphics_state(cos, &op);
                walk.pending.push(op);
                continue;
            }
            if !walk.path.is_empty() {
                out_ops.append(&mut walk.path);
                out_ops.append(&mut walk.pending);
                walk.path_box = None;
                walk.clip = false;
            }
            match o {
                b"q" | b"Q" | b"cm" | b"w" | b"M" | b"j" | b"J" | b"gs" => {
                    walk.graphics_state(cos, &op);
                    out_ops.push(op);
                }
                b"BI" => {
                    let b = walk.gs.ctm.bbox([0.0, 0.0, 1.0, 1.0]);
                    if walk.rects.iter().any(|r| overlaps(*r, b, 0.0)) {
                        if walk.rects.iter().any(|r| rect_contained(b, *r, 1.0)) {
                            changed = true;
                        } else if let Some(clip_ops) = clip_out(walk.rects, &walk.gs.ctm, b) {
                            out_ops.push(Op::new("q", vec![]));
                            out_ops.extend(clip_ops);
                            out_ops.push(op);
                            out_ops.push(Op::new("Q", vec![]));
                            changed = true;
                        } else {
                            out_ops.push(op);
                        }
                    } else {
                        out_ops.push(op);
                    }
                }
                b"Do" => walk.handle_do(cos, resources, op, &mut out_ops, &mut changed)?,
                _ => out_ops.push(op),
            }
        }
        if !walk.path.is_empty() {
            out_ops.append(&mut walk.path);
            out_ops.append(&mut walk.pending);
        }
        changed_any |= changed;
        processed.push((out_ops, changed));
    }

    let out_streams = processed.into_iter().zip(comment_edges).map(|((ops, changed), edges)| changed.then(|| piece_bytes(&ops, edges))).collect();
    Ok(ScopeOut { streams: out_streams, updated_xobjects: walk.updated_xobjects, changed_any })
}

fn white_fill_display_stream(view_inv: Matrix, erase_rects: &[[f64; 4]]) -> Vec<u8> {
    let mut ops = vec![Op::new("q", vec![]), Op::new("rg", vec![num(1.0), num(1.0), num(1.0)])];
    for r in erase_rects {
        let d = view_inv.bbox(*r);
        let w = (d[2] - d[0]).max(0.5);
        let h = (d[3] - d[1]).max(0.5);
        ops.push(Op::new("re", vec![num(d[0]), num(d[1]), num(w), num(h)]));
        ops.push(Op::new("f", vec![]));
    }
    ops.push(Op::new("Q", vec![]));
    serialize_ops(&ops)
}

/// Bake whiteout overlays on `page` into plain non-editable page graphics and remove/clip any
/// underlying vector paths or buried images inside the affected region(s).
pub fn bake_whiteout(cos: &mut CosDoc, page: usize, rects: &[[f64; 4]]) -> Result<(), EditError> {
    let cos_err = |e: CosError| EditError::Edit(pdfcraft_edit::EditError::Cos(e));
    let pages = pdfcraft_model::pages(cos);
    let page_entry = pages.get(page).ok_or(EditError::Edit(pdfcraft_edit::EditError::NoSuchPage(page)))?.clone();
    let added_on_page: Vec<_> = pdfcraft_edit::list_added(cos).into_iter().filter(|a| a.page == page).collect();
    let page_imgs = pdfcraft_edit::page_images(cos, page)?;

    let mut whiteout_indices = Vec::new();
    let mut whiteout_rects = Vec::new();
    for (idx, item) in added_on_page.iter().enumerate() {
        let AddedContent::Image(im) = &item.content else {
            continue;
        };
        if !is_whiteout_image_stream(cos, im.image) {
            continue;
        }
        let user_rect = page_imgs.iter().find(|pi| pi.object == Some(im.image)).map(|pi| pi.rect).unwrap_or(im.rect);
        let selected = rects.is_empty() || rects.iter().any(|r| overlaps(user_rect, *r, 1.0));
        if selected {
            whiteout_indices.push(idx);
            whiteout_rects.push(user_rect);
        }
    }

    let mut erase_rects = rects.to_vec();
    for wr in whiteout_rects {
        if !erase_rects.iter().any(|r| overlaps(*r, wr, 0.25) && rect_contained(wr, *r, 1.0) && rect_contained(*r, wr, 1.0)) {
            erase_rects.push(wr);
        }
    }
    if erase_rects.is_empty() && whiteout_indices.is_empty() {
        return Ok(());
    }

    // Remove the temporary Whiteout AddedContent objects in descending index order so indices don't shift.
    for &idx in whiteout_indices.iter().rev() {
        pdfcraft_edit::delete_content(cos, page, idx)?;
    }

    let kept_added: HashSet<ObjRef> = pdfcraft_edit::list_added(cos).into_iter().filter(|a| a.page == page).map(|a| a.obj).collect();
    let updated_page = pdfcraft_model::pages(cos).swap_remove(page);
    let content_entries = page_content_refs(cos, &updated_page.dict);

    let mut non_kept_positions = Vec::new();
    let mut non_kept_streams = Vec::new();
    let mut budget = DecodeBudget::page();
    for (pos, entry) in content_entries.iter().enumerate() {
        if entry.as_ref().is_some_and(|r| kept_added.contains(&r)) {
            continue;
        }
        let resolved = cos.resolve(entry);
        let Object::Stream(s) = &*resolved else {
            continue;
        };
        let decoded = budget.decode_strict(cos, s).map_err(cos_err)?;
        non_kept_positions.push(pos);
        non_kept_streams.push(decoded);
    }

    let resources = updated_page.dict.get(b"Resources").map(|r| cos.resolve(r)).and_then(|r| r.as_dict().cloned()).unwrap_or_default();
    let erased = erase_streams(cos, &non_kept_streams, &resources, &erase_rects, Matrix::IDENTITY, 0, &mut budget).map_err(cos_err)?;
    if erased.changed_any {
        let mut new_entries = content_entries.clone();
        for (pos, maybe_bytes) in non_kept_positions.into_iter().zip(erased.streams) {
            let Some(bytes) = maybe_bytes else {
                continue;
            };
            let mut dict = match &*cos.resolve(&new_entries[pos]) {
                Object::Stream(s) => s.dict.clone(),
                _ => Dict::new(),
            };
            dict.remove(b"Length");
            let new_ref = cos.add(Object::Stream(Stream::flate(dict, &bytes)));
            new_entries[pos] = Object::Ref(new_ref);
        }
        let mut next_res = resources.clone();
        if !erased.updated_xobjects.is_empty() {
            let mut xo = next_res.get(b"XObject").and_then(|x| cos.resolve(x).as_dict().cloned()).unwrap_or_default();
            for (name, r) in erased.updated_xobjects {
                xo.set(name, Object::Ref(r));
            }
            next_res.set(b"XObject".to_vec(), Object::Dict(xo));
        }
        cos.update_dict(updated_page.obj, |d| {
            d.set(b"Contents".to_vec(), Object::Array(new_entries));
            d.set(b"Resources".to_vec(), Object::Dict(next_res));
        })
        .map_err(cos_err)?;
    }

    // Bake a non-editable white vector rectangle underneath any kept added barcode images.
    if let Some(view_inv) = Matrix(page_entry.view_matrix(cos)).invert() {
        let white_bytes = white_fill_display_stream(view_inv, &erase_rects);
        let baked_ref = pdfcraft_edit::stamp(cos, page, "WhiteoutBake", white_bytes)?;
        let current_entries = page_content_refs(cos, &pdfcraft_model::pages(cos).swap_remove(page).dict);
        if let Some(first_kept_pos) = current_entries.iter().position(|e| e.as_ref().is_some_and(|r| kept_added.contains(&r)))
            && let Some(baked_pos) = current_entries.iter().position(|e| e.as_ref() == Some(baked_ref))
            && baked_pos > first_kept_pos
        {
            let mut reordered = current_entries;
            let baked_obj = reordered.remove(baked_pos);
            reordered.insert(first_kept_pos, baked_obj);
            cos.update_dict(updated_page.obj, |d| {
                d.set(b"Contents".to_vec(), Object::Array(reordered));
            })
            .map_err(cos_err)?;
        }
    }

    Ok(())
}
