use anyhow::{Context, Result};
use lopdf::{dictionary, Document, Object, ObjectId, Stream};
use std::collections::BTreeMap;
use std::path::Path;

use crate::detect::Band;

const PAGE_W: f32 = 595.0;
const PAGE_H: f32 = 842.0;
const MARGIN: f32 = 36.0;
const MIN_GAP: f32 = 10.0; // minimum gap between strips when justifying

/// Write one output PDF per instrument.
/// `header`: region above the first system on page 0 (title/composer area).
pub fn write_parts(
    src_path: &Path,
    all_entries: &[Vec<(u32, Band)>; 4],
    header: Option<&Band>,
    part_names: &[&str],
    output_dir: &Path,
) -> Result<()> {
    let src = Document::load(src_path).context("failed to load source PDF")?;
    let src_pages: Vec<ObjectId> = src.page_iter().collect();

    let avail_w = PAGE_W - 2.0 * MARGIN;

    // Header height (from page 0)
    let header_h = header.and_then(|hdr| {
        let (page_idx, _) = all_entries[0][0];
        let src_page_id = *src_pages.get(page_idx as usize)?;
        let (pw, _) = page_dims(&src, src_page_id);
        let h = (hdr.y_top - hdr.y_bot) * (avail_w / pw);
        if h > 0.0 { Some(h) } else { None }
    });

    for (inst_idx, entries) in all_entries.iter().enumerate() {
        // Compute page breaks per instrument using its own strip heights
        let strip_h: Vec<f32> = entries.iter().map(|&(page_idx, band)| {
            let src_page_id = match src_pages.get(page_idx as usize) {
                Some(&id) => id,
                None => return 0.0,
            };
            let (pw, _) = page_dims(&src, src_page_id);
            ((band.y_top - band.y_bot) * (avail_w / pw)).max(0.0)
        }).collect();

        let page_breaks = compute_page_breaks(&strip_h, header_h);
        let out_path = output_dir.join(format!("{}.pdf", part_names[inst_idx]));
        let n = write_one_part(&src, &src_pages, entries, header, &page_breaks, &out_path)?;
        println!("Wrote {} ({} pages)", out_path.display(), n);
    }

    Ok(())
}

/// Returns a bool per system: true = this system starts a new output page.
fn compute_page_breaks(strip_heights: &[f32], header_h: Option<f32>) -> Vec<bool> {
    let avail = PAGE_H - 2.0 * MARGIN;
    let mut breaks = vec![false; strip_heights.len()];
    let mut cur_h = header_h.map(|h| h + MIN_GAP).unwrap_or(0.0);

    for (i, &h) in strip_heights.iter().enumerate() {
        if i == 0 {
            breaks[0] = true;
            cur_h += h;
            continue;
        }
        if cur_h + MIN_GAP + h > avail {
            breaks[i] = true;
            cur_h = h;
        } else {
            cur_h += MIN_GAP + h;
        }
    }
    breaks
}

fn write_one_part(
    src: &Document,
    src_pages: &[ObjectId],
    entries: &[(u32, Band)],
    header: Option<&Band>,
    page_breaks: &[bool],
    output_path: &Path,
) -> Result<usize> {
    let mut out = Document::with_version("1.5");
    let pages_id = out.new_object_id();
    let avail_w = PAGE_W - 2.0 * MARGIN;
    let avail_h = PAGE_H - 2.0 * MARGIN;

    let mut xobj_cache: BTreeMap<u32, (ObjectId, f32)> = BTreeMap::new();

    // Pre-fetch XObject for a page, caching results
    let mut get_xobj = |page_idx: u32, out: &mut Document| -> Result<(ObjectId, f32)> {
        if let Some(&c) = xobj_cache.get(&page_idx) {
            return Ok(c);
        }
        let src_page_id = *src_pages.get(page_idx as usize).context("page out of range")?;
        let id = embed_page_as_xobject(src, src_page_id, out)?;
        let (pw, _) = page_dims(src, src_page_id);
        xobj_cache.insert(page_idx, (id, pw));
        Ok((id, pw))
    };

    // A strip to render: source page xobject + band in source coords + scale
    struct Strip {
        xobj_id: ObjectId,
        band: Band,
        scale: f32,
        height: f32, // in output pts
    }

    // Group systems into pages
    // Each group: optional header strip (page 0 only) + system strips
    struct PageGroup {
        header: Option<Strip>,
        systems: Vec<Strip>,
    }

    let mut pages: Vec<PageGroup> = Vec::new();
    let mut cur_group: Option<PageGroup> = None;

    for (sys_idx, &(page_idx, band)) in entries.iter().enumerate() {
        let (xobj_id, pw) = get_xobj(page_idx, &mut out)?;
        let scale = avail_w / pw;
        let h = (band.y_top - band.y_bot) * scale;

        if page_breaks[sys_idx] {
            if let Some(g) = cur_group.take() {
                pages.push(g);
            }
            // New page group
            let hdr_strip = if sys_idx == 0 {
                header.and_then(|hdr| {
                    let h = (hdr.y_top - hdr.y_bot) * scale;
                    if h > 0.0 {
                        Some(Strip { xobj_id, band: *hdr, scale, height: h })
                    } else {
                        None
                    }
                })
            } else {
                None
            };
            cur_group = Some(PageGroup { header: hdr_strip, systems: Vec::new() });
        }

        if h > 0.0 {
            cur_group.as_mut().unwrap().systems.push(Strip { xobj_id, band, scale, height: h });
        }
    }
    if let Some(g) = cur_group {
        pages.push(g);
    }

    // Render each page group with justified spacing
    let mut page_ids: Vec<ObjectId> = Vec::new();
    let mut xobj_counter = 0usize;

    for group in &pages {
        let all_strips: Vec<&Strip> = group.header.iter().chain(group.systems.iter()).collect();
        let n = all_strips.len();
        if n == 0 {
            continue;
        }

        let total_h: f32 = all_strips.iter().map(|s| s.height).sum();
        let num_gaps = if n > 1 { n - 1 } else { 0 };
        // Justify spacing, but cap the gap so sparse pages don't look too spread out.
        let max_gap = 2.5 * MIN_GAP;
        let gap = if num_gaps > 0 {
            let justified = (avail_h - total_h) / num_gaps as f32;
            justified.clamp(MIN_GAP, max_gap)
        } else {
            0.0
        };

        let mut streams: Vec<String> = Vec::new();
        let mut xobjs: Vec<(String, ObjectId)> = Vec::new();
        let mut cur_y = PAGE_H - MARGIN;

        for strip in &all_strips {
            let h = strip.height;
            let dest_y_top = cur_y;
            let dest_y_bot = dest_y_top - h;
            let tx = MARGIN;
            let ty = dest_y_bot - strip.band.y_bot * strip.scale;

            let name = format!("X{}", xobj_counter);
            xobj_counter += 1;
            xobjs.push((name.clone(), strip.xobj_id));
            streams.push(format!(
                "q\n{x1} {y1} {w} {h} re W n\n{a} 0 0 {d} {tx} {ty} cm\n/{name} Do\nQ",
                x1 = MARGIN, y1 = dest_y_bot, w = avail_w, h = h,
                a = strip.scale, d = strip.scale, tx = tx, ty = ty, name = name,
            ));
            cur_y = dest_y_bot - gap;
        }

        let content_id = out.add_object(Stream::new(
            lopdf::Dictionary::new(),
            streams.join("\n").into_bytes(),
        ));
        let mut xobj_dict = lopdf::Dictionary::new();
        for (name, id) in &xobjs {
            xobj_dict.set(name.as_bytes(), Object::Reference(*id));
        }
        let mut resources = lopdf::Dictionary::new();
        resources.set(b"XObject", Object::Dictionary(xobj_dict));
        let pid = out.add_object(dictionary! {
            b"Type" => Object::Name(b"Page".to_vec()),
            b"Parent" => Object::Reference(pages_id),
            b"MediaBox" => Object::Array(vec![
                Object::Integer(0), Object::Integer(0),
                Object::Real(PAGE_W), Object::Real(PAGE_H),
            ]),
            b"Resources" => Object::Dictionary(resources),
            b"Contents" => Object::Reference(content_id),
        });
        page_ids.push(pid);
    }

    let num_pages = page_ids.len();
    let kids: Vec<Object> = page_ids.iter().map(|&id| Object::Reference(id)).collect();
    out.objects.insert(pages_id, Object::Dictionary(dictionary! {
        b"Type" => Object::Name(b"Pages".to_vec()),
        b"Kids" => Object::Array(kids),
        b"Count" => Object::Integer(num_pages as i64),
    }));
    let catalog_id = out.add_object(dictionary! {
        b"Type" => Object::Name(b"Catalog".to_vec()),
        b"Pages" => Object::Reference(pages_id),
    });
    out.trailer.set(b"Root", Object::Reference(catalog_id));
    out.save(output_path)
        .with_context(|| format!("save {}", output_path.display()))?;

    Ok(num_pages)
}

/// Embed a source page as a Form XObject in `out`.
fn embed_page_as_xobject(src: &Document, src_page_id: ObjectId, out: &mut Document) -> Result<ObjectId> {
    let page_dict = src.get_object(src_page_id)?.as_dict()?;
    let content_bytes = src.get_page_content(src_page_id)?;
    let resources = page_dict.get(b"Resources").ok().cloned();
    let (pw, ph) = page_dims(src, src_page_id);

    let mut xobj_dict = dictionary! {
        b"Type" => Object::Name(b"XObject".to_vec()),
        b"Subtype" => Object::Name(b"Form".to_vec()),
        b"FormType" => Object::Integer(1),
        b"BBox" => Object::Array(vec![
            Object::Real(0.0), Object::Real(0.0),
            Object::Real(pw), Object::Real(ph),
        ]),
    };
    if let Some(res) = resources {
        xobj_dict.set(b"Resources", deep_copy_object(src, &res, out));
    }

    Ok(out.add_object(Stream::new(xobj_dict, content_bytes)))
}

/// Deep-copy an Object and all objects it references from src into out.
fn deep_copy_object(src: &Document, obj: &Object, out: &mut Document) -> Object {
    match obj {
        Object::Reference(id) => {
            if let Ok(src_obj) = src.get_object(*id) {
                let copied = deep_copy_object(src, src_obj, out);
                Object::Reference(out.add_object(copied))
            } else {
                obj.clone()
            }
        }
        Object::Dictionary(dict) => {
            let mut new_dict = lopdf::Dictionary::new();
            for (k, v) in dict.iter() {
                new_dict.set(k.clone(), deep_copy_object(src, v, out));
            }
            Object::Dictionary(new_dict)
        }
        Object::Array(arr) => {
            Object::Array(arr.iter().map(|v| deep_copy_object(src, v, out)).collect())
        }
        Object::Stream(stream) => {
            let mut new_dict = lopdf::Dictionary::new();
            for (k, v) in stream.dict.iter() {
                new_dict.set(k.clone(), deep_copy_object(src, v, out));
            }
            Object::Stream(Stream::new(new_dict, stream.content.clone()))
        }
        _ => obj.clone(),
    }
}

fn page_dims(src: &Document, page_id: ObjectId) -> (f32, f32) {
    let arr = src
        .get_object(page_id)
        .and_then(|o| o.as_dict())
        .ok()
        .and_then(|d| d.get(b"MediaBox").ok())
        .and_then(|o| o.as_array().ok())
        .cloned();

    if let Some(arr) = arr {
        let get = |i: usize| match arr.get(i) {
            Some(Object::Integer(v)) => Some(*v as f32),
            Some(Object::Real(v)) => Some(*v as f32),
            _ => None,
        };
        if let (Some(w), Some(h)) = (get(2), get(3)) {
            return (w, h);
        }
    }
    (595.0, 842.0)
}
