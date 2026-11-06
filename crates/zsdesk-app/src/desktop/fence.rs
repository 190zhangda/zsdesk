//! One desktop fence: a translucent, per-pixel-alpha layered window that sits
//! just above the desktop. Geometry and hit-testing are pure; painting uses
//! the shared Direct2D target and is presented with `UpdateLayeredWindow`.

use std::collections::HashSet;

use windows::Win32::{
    Foundation::{HWND, POINT, SIZE, RECT},
    Graphics::{Direct2D::ID2D1Bitmap, Gdi::{GetDC, ReleaseDC, AC_SRC_ALPHA, AC_SRC_OVER, BLENDFUNCTION}},
    UI::WindowsAndMessaging::{GetWindowRect, UpdateLayeredWindow, ULW_ALPHA},
};
use zsdesk_core::model::desktop::{Fence, FenceView, TITLE_H};
use zsdesk_ui::{chrome::{scale, Rect}, theme::{glyph, FencePalette}};
use zsdesk_windows::{desktop::ShellEntry, gfx::{Align, Canvas, Color, Gfx, RectF, Surface, TextStyle}, shell::notify::ChangeNotify};

pub const EDGE: i32 = 6;

/// What the pointer is over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FenceHit { None, Title, More, Back, Edge(Edges), Item(usize), Empty, Scrollbar }

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Edges { pub left: bool, pub top: bool, pub right: bool, pub bottom: bool }

/// Pointer interaction in progress.
#[derive(Clone, Debug)]
pub enum Interaction {
    Move { cursor: POINT, rect: RECT },
    Resize { edges: Edges, cursor: POINT, rect: RECT },
    Press { index: usize, cursor: POINT },
    Rubber { start: POINT, current: POINT, base: HashSet<String> },
    Thumb { cursor_y: i32, scroll: i32 },
}

#[derive(Clone, Copy, Debug)]
pub struct Cell { pub cell: Rect, pub icon: Rect, pub label: Rect }

#[derive(Clone, Debug, Default)]
pub struct Geometry {
    pub width: i32,
    pub height: i32,
    pub title: Rect,
    pub more: Rect,
    pub back: Rect,
    pub content: Rect,
    pub cells: Vec<Cell>,
    pub content_height: i32,
    pub max_scroll: i32,
    pub thumb: Rect,
    pub icon_px: i32,
}

impl Geometry {
    pub fn new(width: i32, height: i32, dpi: u32, fence: &Fence, count: usize, scroll: i32, browsing: bool) -> Self {
        let s = |v: i32| scale(v, dpi);
        let title_h = s(TITLE_H);
        let mut g = Geometry { width, height, ..Default::default() };
        g.title = Rect::new(0, 0, width, title_h.min(height));
        g.more = Rect::new(width - s(32), s(3), s(28), title_h - s(6));
        g.back = if browsing { Rect::new(s(4), s(3), s(28), title_h - s(6)) } else { Rect::default() };
        g.content = Rect::new(s(4), title_h, (width - s(8)).max(0), (height - title_h - s(4)).max(0));
        g.icon_px = s(fence.icon_size.pixels());
        if fence.collapsed || g.content.height <= 0 { g.content.height = 0; return g; }
        let pad = s(6);
        let inner_w = (g.content.width - pad * 2).max(1);
        match fence.view {
            FenceView::Icons => {
                let icon = g.icon_px;
                let min_cell = icon + s(36);
                let columns = (inner_w / min_cell).max(1);
                let cell_w = inner_w / columns;
                let label_h = s(34);
                let cell_h = icon + s(10) + label_h;
                for i in 0..count {
                    let (c, r) = ((i as i32) % columns, (i as i32) / columns);
                    let x = g.content.x + pad + c * cell_w;
                    let y = g.content.y + pad + r * cell_h - scroll;
                    let cell = Rect::new(x + s(2), y, cell_w - s(4), cell_h - s(2));
                    let icon_rect = Rect::new(x + (cell_w - icon) / 2, y + s(4), icon, icon);
                    let label = Rect::new(x + s(4), icon_rect.bottom() + s(3), cell_w - s(8), label_h - s(2));
                    g.cells.push(Cell { cell, icon: icon_rect, label });
                }
                let rows = (count as i32 + columns - 1) / columns;
                g.content_height = rows * cell_h + pad * 2;
            }
            FenceView::List => {
                let row_h = s(26);
                let icon = s(20);
                for i in 0..count {
                    let y = g.content.y + pad + i as i32 * row_h - scroll;
                    let cell = Rect::new(g.content.x + pad, y, inner_w, row_h - s(2));
                    let icon_rect = Rect::new(cell.x + s(4), y + (row_h - s(2) - icon) / 2, icon, icon);
                    let label = Rect::new(icon_rect.right() + s(8), y, (cell.right() - icon_rect.right() - s(12)).max(0), row_h - s(2));
                    g.cells.push(Cell { cell, icon: icon_rect, label });
                }
                g.content_height = count as i32 * row_h + pad * 2;
            }
        }
        g.max_scroll = (g.content_height - g.content.height).max(0);
        if g.max_scroll > 0 {
            let track = g.content.height - s(8);
            let thumb_h = ((track as i64 * g.content.height as i64 / g.content_height.max(1) as i64) as i32).max(s(24)).min(track);
            let y = g.content.y + s(4) + ((track - thumb_h) as i64 * scroll as i64 / g.max_scroll.max(1) as i64) as i32;
            g.thumb = Rect::new(width - s(9), y, s(4), thumb_h);
        }
        g
    }

    pub fn hit(&self, x: i32, y: i32, dpi: u32, locked: bool, collapsed: bool) -> FenceHit {
        let edge = scale(EDGE, dpi);
        if !locked {
            let edges = Edges {
                left: x < edge, right: x >= self.width - edge,
                top: y < edge / 2, bottom: !collapsed && y >= self.height - edge,
            };
            if edges.left || edges.right || edges.top || edges.bottom {
                if !(collapsed && (edges.top || edges.bottom) && !(edges.left || edges.right)) { return FenceHit::Edge(edges); }
            }
        }
        if self.title.contains(x, y) {
            if self.more.contains(x, y) { return FenceHit::More; }
            if !self.back.is_empty() && self.back.contains(x, y) { return FenceHit::Back; }
            return FenceHit::Title;
        }
        if !self.thumb.is_empty() && x >= self.thumb.x - scale(4, dpi) && self.content.contains(x, y) { return FenceHit::Scrollbar; }
        if self.content.contains(x, y) {
            if let Some(i) = self.cells.iter().position(|c| c.cell.contains(x, y)) { return FenceHit::Item(i); }
            return FenceHit::Empty;
        }
        FenceHit::None
    }

    /// Item indices intersecting a rubber-band rectangle.
    pub fn in_rect(&self, a: POINT, b: POINT) -> Vec<usize> {
        let (l, r) = (a.x.min(b.x), a.x.max(b.x));
        let (t, bo) = (a.y.min(b.y), a.y.max(b.y));
        self.cells.iter().enumerate().filter(|(_, c)| c.cell.x < r && c.cell.right() > l && c.cell.y < bo && c.cell.bottom() > t).map(|(i, _)| i).collect()
    }

    /// Insertion index for a drop at `(x, y)` (manual ordering).
    pub fn insertion(&self, x: i32, y: i32) -> usize {
        self.cells.iter().position(|c| y < c.cell.y || (y < c.cell.bottom() && x < c.cell.x + c.cell.width / 2)).unwrap_or(self.cells.len())
    }

    /// Columns in icon view (for keyboard navigation).
    pub fn columns(&self) -> usize {
        let first_y = self.cells.first().map(|c| c.cell.y);
        self.cells.iter().take_while(|c| Some(c.cell.y) == first_y).count().max(1)
    }
}

/// Runtime state of one fence window.
pub struct FenceWin {
    pub id: u64,
    pub hwnd: HWND,
    pub dpi: u32,
    pub entries: Vec<ShellEntry>,
    pub selection: HashSet<String>,
    pub anchor: Option<usize>,
    pub focus: Option<usize>,
    pub hover: FenceHit,
    pub scroll: i32,
    pub interaction: Option<Interaction>,
    pub surface: Option<Surface>,
    pub notify: Option<ChangeNotify>,
    pub notify_path: Option<std::path::PathBuf>,
    pub drop: Option<zsdesk_windows::ole::DropRegistration>,
    /// Item under a drag (folder target) or `usize::MAX` for the whole fence.
    pub drop_hover: Option<usize>,
    pub rename: Option<(HWND, RenameTarget)>,
    pub message: Option<String>,
    pub tracking: bool,
    /// A collapsed fence temporarily rolled down under the pointer.
    pub peek: bool,
    /// Explicitly collapsed under the pointer: no peek until it leaves.
    pub peek_blocked: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RenameTarget { Item(String), Title }

impl FenceWin {
    pub fn new(id: u64) -> Self {
        Self { id, hwnd: HWND::default(), dpi: 96, entries: Vec::new(), selection: HashSet::new(), anchor: None, focus: None,
            hover: FenceHit::None, scroll: 0, interaction: None, surface: None, notify: None, notify_path: None, drop: None,
            drop_hover: None, rename: None, message: None, tracking: false, peek: false, peek_blocked: false }
    }

    pub fn window_rect(&self) -> RECT {
        let mut r = RECT::default();
        let _ = unsafe { GetWindowRect(self.hwnd, &mut r) };
        r
    }

    pub fn geometry(&self, fence: &Fence) -> Geometry {
        let r = self.window_rect();
        if self.peek {
            let mut open = fence.clone();
            open.collapsed = false;
            return Geometry::new(r.right - r.left, r.bottom - r.top, self.dpi, &open, self.entries.len(), self.scroll, fence.browse.is_some());
        }
        Geometry::new(r.right - r.left, r.bottom - r.top, self.dpi, fence, self.entries.len(), self.scroll, fence.browse.is_some())
    }

    /// Collapsed as displayed right now (a peeking fence behaves as expanded).
    pub fn collapsed(&self, fence: &Fence) -> bool { fence.collapsed && !self.peek }

    pub fn clamp_scroll(&mut self, fence: &Fence) {
        let g = self.geometry(fence);
        self.scroll = self.scroll.clamp(0, g.max_scroll);
    }

    pub fn selected_entries(&self) -> Vec<&ShellEntry> {
        self.entries.iter().filter(|e| self.selection.contains(&e.key)).collect()
    }

    /// Paints and presents the fence. `icon` resolves item bitmaps.
    pub fn render(&mut self, gfx: &Gfx, fence: &Fence, palette: &FencePalette, mut icon: impl FnMut(&ShellEntry, i32) -> Option<ID2D1Bitmap>) {
        if self.hwnd.0.is_null() { return; }
        let rect = self.window_rect();
        let (w, h) = (rect.right - rect.left, rect.bottom - rect.top);
        if w <= 0 || h <= 0 { return; }
        let g = self.geometry(fence);
        let bitmaps: Vec<Option<ID2D1Bitmap>> = g.cells.iter().enumerate().map(|(i, c)| {
            let visible = c.cell.bottom() >= g.content.y && c.cell.y <= g.content.bottom();
            if visible { icon(&self.entries[i], if fence.view == FenceView::List { scale(20, self.dpi) } else { g.icon_px }) } else { None }
        }).collect();
        if Surface::ensure(&mut self.surface, w, h).is_none() { return; }
        let Some(surface) = self.surface.as_ref() else { return };
        let dpi = self.dpi;
        let sf = |v: f32| v * dpi as f32 / 96.0;
        let col = |c: zsdesk_ui::theme::Color| Color::rgba8(c.r, c.g, c.b, c.a);
        let _ = gfx.draw(surface, |cv: &Canvas| {
            let outer = RectF::new(0.5, 0.5, w as f32 - 1.0, h as f32 - 1.0);
            let radius = sf(8.0);
            let drop_all = self.drop_hover == Some(usize::MAX);
            cv.round(outer, radius, col(palette.background));
            // Title bar.
            let title = RectF::new(0.0, 0.0, w as f32, g.title.height as f32);
            cv.push_clip(title);
            cv.round(outer, radius, col(palette.title));
            cv.pop_clip();
            let hovered = !matches!(self.hover, FenceHit::None);
            let title_style = TextStyle::ui(sf(12.5)).weight(600).align(Align::Center);
            let count = format!("{}", self.entries.len());
            let side = sf(36.0);
            let mut text = fence.title.clone();
            if fence.locked { text = format!("{text} \u{1F512}"); }
            if !matches!(self.rename, Some((_, RenameTarget::Title))) {
                cv.text(&text, RectF::new(side, 0.0, (w as f32 - side * 2.0).max(0.0), title.h), &title_style, col(palette.text));
            }
            let small = TextStyle::ui(sf(11.0)).align(Align::End);
            if hovered && !g.more.is_empty() {
                let more = rf(g.more);
                if self.hover == FenceHit::More { cv.round(more, sf(4.0), col(palette.hover)); }
                cv.glyph(glyph::MORE, more, sf(13.0), col(palette.text));
            } else {
                cv.text(&count, RectF::new(w as f32 - sf(40.0), 0.0, sf(30.0), title.h), &small, col(palette.text_secondary));
            }
            if !g.back.is_empty() {
                let back = rf(g.back);
                if self.hover == FenceHit::Back { cv.round(back, sf(4.0), col(palette.hover)); }
                cv.glyph("\u{E72B}", back, sf(12.0), col(palette.text));
            }
            if g.content.height > 0 {
                let content = rf(g.content);
                cv.push_clip(content);
                if self.entries.is_empty() {
                    let hint = self.message.clone().unwrap_or_else(|| if fence.is_desktop() { "将桌面图标拖到这里".into() } else { "此文件夹为空".into() });
                    cv.text(&hint, content, &TextStyle::ui(sf(12.0)).align(Align::Center), col(palette.text_secondary));
                }
                let label_style = if fence.view == FenceView::List { TextStyle::ui(sf(12.0)) } else { TextStyle::ui(sf(12.0)).align(Align::Center).top().wrapped() };
                for (i, cell) in g.cells.iter().enumerate() {
                    if cell.cell.bottom() < g.content.y || cell.cell.y > g.content.bottom() { continue; }
                    let entry = &self.entries[i];
                    let selected = self.selection.contains(&entry.key);
                    let r = rf(cell.cell);
                    if self.drop_hover == Some(i) {
                        cv.round(r, sf(4.0), col(palette.selected));
                        cv.stroke_round(r, sf(4.0), col(palette.accent), sf(1.5));
                    } else if selected {
                        cv.round(r, sf(4.0), col(palette.selected));
                        cv.stroke_round(r, sf(4.0), col(palette.selected_border), 1.0);
                    } else if self.hover == FenceHit::Item(i) {
                        cv.round(r, sf(4.0), col(palette.hover));
                    }
                    if self.focus == Some(i) && selected { cv.stroke_round(r, sf(4.0), col(palette.selected_border), 1.0); }
                    let icon_rect = rf(cell.icon);
                    let opacity = if entry.ghosted { 0.5 } else { 1.0 };
                    match &bitmaps[i] {
                        Some(bitmap) => {
                            let (bw, bh) = bitmap_size(bitmap);
                            cv.image(bitmap, fit(icon_rect, bw, bh), opacity);
                        }
                        None => cv.glyph(if entry.is_container() { glyph::FOLDER } else { "\u{E8A5}" }, icon_rect, icon_rect.h * 0.7, col(palette.text_secondary)),
                    }
                    if entry.is_link && fence.view == FenceView::Icons {
                        // Shortcut overlay arrow, like the desktop.
                        let a = RectF::new(icon_rect.x, icon_rect.bottom() - sf(14.0), sf(14.0), sf(14.0));
                        cv.round(a, sf(2.0), Color::rgba8(0xFF, 0xFF, 0xFF, 0xF0));
                        cv.glyph("\u{E8A7}", a, sf(9.0), Color::rgba8(0x00, 0x5F, 0xB8, 0xFF));
                    }
                    if let Some((_, RenameTarget::Item(key))) = &self.rename { if *key == entry.key { continue; } }
                    let label = rf(cell.label);
                    let shadow = col(palette.text_shadow);
                    if shadow.a > 0.0 && !selected {
                        cv.text(&entry.name, RectF::new(label.x + 1.0, label.y + 1.0, label.w, label.h), &label_style, shadow);
                    }
                    cv.text(&entry.name, label, &label_style, col(palette.text));
                }
                if let Some(Interaction::Rubber { start, current, .. }) = &self.interaction {
                    let band = RectF::new(start.x.min(current.x) as f32, start.y.min(current.y) as f32,
                        (start.x - current.x).abs() as f32, (start.y - current.y).abs() as f32);
                    cv.fill(band, col(palette.selected));
                    cv.stroke_round(band, 0.0, col(palette.selected_border), 1.0);
                }
                cv.pop_clip();
                if !g.thumb.is_empty() && (hovered || matches!(self.interaction, Some(Interaction::Thumb { .. }))) {
                    cv.round(rf(g.thumb), sf(2.0), col(palette.text_secondary).with_alpha(0.6));
                }
            }
            let border = if drop_all { col(palette.accent) } else { col(palette.border) };
            cv.stroke_round(outer, radius, border, if drop_all { sf(2.0) } else { 1.0 });
        });
        let screen = unsafe { GetDC(None) };
        let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: 255, AlphaFormat: AC_SRC_ALPHA as u8 };
        let position = POINT { x: rect.left, y: rect.top };
        let size = SIZE { cx: w, cy: h };
        let origin = POINT::default();
        let _ = unsafe { UpdateLayeredWindow(self.hwnd, Some(screen), Some(&position), Some(&size), Some(surface.dc), Some(&origin),
            Default::default(), Some(&blend), ULW_ALPHA) };
        unsafe { ReleaseDC(None, screen); }
    }
}

fn rf(r: Rect) -> RectF { RectF::new(r.x as f32, r.y as f32, r.width as f32, r.height as f32) }

fn bitmap_size(bitmap: &ID2D1Bitmap) -> (f32, f32) {
    let size = unsafe { bitmap.GetPixelSize() };
    (size.width as f32, size.height as f32)
}

/// Fits a bitmap into `rect` keeping its aspect ratio (thumbnails are not square).
fn fit(rect: RectF, w: f32, h: f32) -> RectF {
    if w <= 0.0 || h <= 0.0 { return rect; }
    let scale = (rect.w / w).min(rect.h / h).min(4.0);
    let (fw, fh) = (w * scale, h * scale);
    RectF::new(rect.x + (rect.w - fw) / 2.0, rect.y + (rect.h - fh), fw, fh)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zsdesk_core::model::desktop::{FenceSource, Placement};

    fn fence() -> Fence { Fence::new(1, "t", FenceSource::Desktop, Placement::default()) }

    #[test]
    fn icon_grid_wraps_and_scrolls() {
        let g = Geometry::new(340, 250, 96, &fence(), 30, 0, false);
        assert_eq!(g.cells.len(), 30);
        assert!(g.columns() >= 3);
        assert!(g.max_scroll > 0);
        assert!(g.cells.iter().all(|c| c.cell.x >= g.content.x && c.cell.right() <= g.content.right()));
        let scrolled = Geometry::new(340, 250, 96, &fence(), 30, 40, false);
        assert_eq!(scrolled.cells[0].cell.y, g.cells[0].cell.y - 40);
    }

    #[test]
    fn hit_testing_prefers_edges_then_title_then_items() {
        let g = Geometry::new(300, 200, 96, &fence(), 3, 0, false);
        assert!(matches!(g.hit(1, 100, 96, false, false), FenceHit::Edge(Edges { left: true, .. })));
        assert_eq!(g.hit(1, 100, 96, true, false), FenceHit::None);
        assert_eq!(g.hit(150, 10, 96, false, false), FenceHit::Title);
        let c = g.cells[1].cell;
        assert_eq!(g.hit(c.x + 3, c.y + 3, 96, false, false), FenceHit::Item(1));
        assert_eq!(g.hit(150, 190, 96, true, false), FenceHit::Empty);
    }

    #[test]
    fn collapsed_fence_has_no_content_or_vertical_resize() {
        let mut f = fence();
        f.collapsed = true;
        let g = Geometry::new(300, TITLE_H, 96, &f, 10, 0, false);
        assert!(g.cells.is_empty());
        assert_eq!(g.hit(150, TITLE_H - 1, 96, false, true), FenceHit::Title);
    }

    #[test]
    fn insertion_index_follows_reading_order() {
        let g = Geometry::new(340, 400, 96, &fence(), 6, 0, false);
        let c = g.cells[1].cell;
        assert_eq!(g.insertion(c.x + 2, c.y + 5), 1);
        assert_eq!(g.insertion(c.right() - 2, c.y + 5), 2);
        assert_eq!(g.insertion(5, 399), 6);
    }

    #[test]
    fn thumbnails_keep_aspect_ratio() {
        let r = fit(RectF::new(0.0, 0.0, 48.0, 48.0), 96.0, 48.0);
        assert_eq!((r.w, r.h), (48.0, 24.0));
        assert_eq!(r.bottom(), 48.0);
    }
}
