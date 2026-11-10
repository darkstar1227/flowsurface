//! Drawings on the heatmap, shared with the kline chart of the same ticker.
//!
//! Drawings are anchored by time and price (`DrawingX::Time` and a min-tick `YUnit` on the
//! main panel), so a drawing lands on the same time and price in both charts. This layer is
//! painted on its own canvas frame above the shader and the overlay, and never feeds back into
//! the camera, the colour scale or the instances. Pointer input is only taken while a tool is
//! armed or when a drawing is hit; everything else (pan, zoom) takes its usual path.

use super::Message;
use super::view::TimeView;
use crate::theme::ThemeAccents;
use crate::widget::chart::kline::drawing::{
    DRAWING_HANDLE_HIT_RADIUS_PX, DRAWING_HANDLE_RADIUS_PX, DRAWING_HIT_TOLERANCE_PX,
    DrawingAnchor, DrawingDragOperation, DrawingDragTarget, DrawingHandleKind, DrawingId,
    DrawingObject, DrawingSnapshot, DrawingStyle, DrawingTool, DrawingX, FIB_FILL_ALPHA,
    KlineWidgetDrawingEvent, clip_segment_to_bounds, fib_lines_hit, fib_ratio_label, iced_color,
    intersect_bounds, point_segment_distance,
};
use data::chart::kline::composition::PanelId;
use data::chart::kline::drawing::{FIB_LEVELS, fib_level_y_unit};
use data::chart::value::{PanelValueScale, YUnit};
use exchange::UnixMs;
use exchange::unit::{MinTicksize, Price, PriceStep};
use iced::theme::palette::Extended;
use iced::widget::canvas;
use iced::{Event, Point, Rectangle, Size, keyboard, mouse};

/// Maps time and price to pixels inside the heatmap plot, and back.
///
/// Plain numbers only: the axes of the heatmap are linear in both directions, so the camera is
/// reduced to an origin and a scale per axis. A time is placed like the axis places a bucket
/// (the bucket's centre is its start time), but without snapping to whole buckets, so a drawing
/// made at any instant keeps that instant.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Projection {
    interval_ms: f64,
    latest_bucket: f64,
    phase: f64,
    column_world: f64,
    x_origin: f64,
    x_scale: f64,
    base_price: f64,
    step: f64,
    row_h: f64,
    y_origin: f64,
    y_scale: f64,
}

impl Projection {
    pub fn new(
        time: TimeView<'_>,
        size: Size,
        base_price: Price,
        step: PriceStep,
        row_h: f32,
    ) -> Option<Self> {
        let camera = time.camera;
        let x_origin = f64::from(camera.world_to_screen_x(0.0, size.width));
        let x_scale = f64::from(camera.world_to_screen_x(1.0, size.width)) - x_origin;
        let y_origin = f64::from(camera.world_to_screen_y(0.0, size.width, size.height));
        let y_scale = f64::from(camera.world_to_screen_y(1.0, size.width, size.height)) - y_origin;

        let projection = Self {
            interval_ms: time.interval_ms as f64,
            latest_bucket: time.latest_bucket as f64,
            phase: f64::from(time.phase),
            column_world: f64::from(time.column_world),
            x_origin,
            x_scale,
            base_price: base_price.to_f64(),
            step: Price::from_units(step.units).to_f64(),
            row_h: f64::from(row_h),
            y_origin,
            y_scale,
        };
        projection.is_valid().then_some(projection)
    }

    fn is_valid(self) -> bool {
        [
            self.interval_ms,
            self.column_world,
            self.x_scale,
            self.step,
            self.row_h,
            self.y_scale,
        ]
        .into_iter()
        .all(|value| value.is_finite() && value > 0.0)
            && [
                self.latest_bucket,
                self.phase,
                self.x_origin,
                self.base_price,
                self.y_origin,
            ]
            .into_iter()
            .all(f64::is_finite)
    }

    /// Pixel x of `time_ms`.
    pub fn x_at(self, time_ms: f64) -> f64 {
        let bucket = time_ms / self.interval_ms - self.latest_bucket - self.phase;
        self.x_origin + self.x_scale * bucket * self.column_world
    }

    /// Time under pixel `x`, in milliseconds.
    pub fn time_at(self, x: f64) -> f64 {
        let world = (x - self.x_origin) / self.x_scale;
        (world / self.column_world + self.phase + self.latest_bucket) * self.interval_ms
    }

    /// Pixel y of `price`. A price is drawn at the centre of its row, like the axis labels.
    pub fn y_at(self, price: f64) -> f64 {
        let steps = (price - self.base_price) / self.step;
        self.y_origin - self.y_scale * (steps + 0.5) * self.row_h
    }

    /// Price under pixel `y`.
    pub fn price_at(self, y: f64) -> f64 {
        let world = (y - self.y_origin) / self.y_scale;
        self.base_price + (-world / self.row_h - 0.5) * self.step
    }
}

/// What the overlay needs besides the projection to turn drawings into pixels.
#[derive(Clone, Copy)]
pub struct View {
    projection: Projection,
    /// `BaseTickerMinTick`, the unit the kline main panel stores its drawings in.
    scale: PanelValueScale,
    label_precision: MinTicksize,
    plot: Rectangle,
}

impl View {
    pub fn new(
        projection: Projection,
        scale: PanelValueScale,
        label_precision: MinTicksize,
        plot_size: Size,
    ) -> Self {
        Self {
            projection,
            scale,
            label_precision,
            plot: Rectangle::with_size(plot_size),
        }
    }

    fn x_for(&self, x: DrawingX) -> Option<f32> {
        let DrawingX::Time(time) = x else {
            return None;
        };
        finite(self.projection.x_at(time.as_u64() as f64))
    }

    fn y_for(&self, y_unit: YUnit) -> Option<f32> {
        finite(self.projection.y_at(self.scale.unit_to_value(y_unit)))
    }

    fn point_for(&self, anchor: DrawingAnchor) -> Option<Point> {
        if !anchor.panel_id.is_main() {
            return None;
        }
        Some(Point::new(
            self.x_for(anchor.x)?,
            self.y_for(anchor.y_unit)?,
        ))
    }

    /// The anchor under a point of the plot, in the units the kline chart stores.
    pub fn anchor_at(&self, point: Point) -> Option<DrawingAnchor> {
        let time = self.projection.time_at(f64::from(point.x));
        let price = self.projection.price_at(f64::from(point.y));
        if !time.is_finite() || !price.is_finite() {
            return None;
        }
        Some(DrawingAnchor {
            panel_id: PanelId::Main,
            x: DrawingX::Time(UnixMs::new(time.max(0.0).round() as u64)),
            y_unit: self.scale.value_to_unit(price),
        })
    }

    fn price_label(&self, y_unit: YUnit) -> String {
        Price::from_f64(self.scale.unit_to_value(y_unit)).to_string(self.label_precision)
    }

    fn trendline(&self, start: DrawingAnchor, end: DrawingAnchor) -> Option<(Point, Point)> {
        let start = self.point_for(start)?;
        let end = self.point_for(end)?;
        clip_segment_to_bounds(start, end, self.plot)
    }

    /// The box corners, which may lie outside the plot, and the part of it inside.
    fn box_geometry(
        &self,
        start: DrawingAnchor,
        end: DrawingAnchor,
        extend_right: bool,
    ) -> Option<(Rectangle, Rectangle)> {
        let start = self.point_for(start)?;
        let end = self.point_for(end)?;
        let left = start.x.min(end.x);
        let right = if extend_right {
            self.plot.width
        } else {
            start.x.max(end.x)
        };
        let top = start.y.min(end.y);
        let bottom = start.y.max(end.y);
        let full = Rectangle {
            x: left,
            y: top,
            width: (right - left).max(1.0),
            height: (bottom - top).max(1.0),
        };
        Some((full, intersect_bounds(full, self.plot)?))
    }

    fn horizontal_line(&self, y_unit: YUnit, x: DrawingX, ray: bool) -> Option<(f32, f32)> {
        let y = self.y_for(y_unit)?;
        if !(0.0..=self.plot.height).contains(&y) {
            return None;
        }
        let start_x = if ray { self.x_for(x)?.max(0.0) } else { 0.0 };
        (start_x <= self.plot.width).then_some((y, start_x))
    }

    fn vertical_line(&self, x: DrawingX) -> Option<f32> {
        let x = self.x_for(x)?;
        (0.0..=self.plot.width).contains(&x).then_some(x)
    }

    /// Rows of every Fib level, and the span from the earlier anchor to the later one.
    fn fib(&self, start: DrawingAnchor, end: DrawingAnchor) -> Option<FibGeometry> {
        let start_x = self.x_for(start.x)?;
        let end_x = self.x_for(end.x)?;
        let left = start_x.min(end_x).max(0.0);
        let right = start_x.max(end_x).min(self.plot.width);
        if right < left {
            return None;
        }
        let levels = FIB_LEVELS
            .into_iter()
            .map(|ratio| {
                let y_unit = fib_level_y_unit(start.y_unit, end.y_unit, ratio);
                Some(FibLevel {
                    ratio,
                    y_unit,
                    y: self.y_for(y_unit)?,
                })
            })
            .collect::<Option<Vec<_>>>()?;
        Some(FibGeometry {
            left,
            right,
            levels,
        })
    }

    fn handles(&self, object: &DrawingObject) -> Vec<(DrawingHandleKind, Point)> {
        match object {
            DrawingObject::Trendline { start, end }
            | DrawingObject::FibRetracement { start, end } => [
                (DrawingHandleKind::TrendlineStart, *start),
                (DrawingHandleKind::TrendlineEnd, *end),
            ]
            .into_iter()
            .filter_map(|(kind, anchor)| Some((kind, self.point_for(anchor)?)))
            .collect(),
            DrawingObject::Box {
                start,
                end,
                extend_right,
            } => {
                let (Some(start), Some(end)) = (self.point_for(*start), self.point_for(*end))
                else {
                    return Vec::new();
                };
                let (left, right) = (start.x.min(end.x), start.x.max(end.x));
                let (top, bottom) = (start.y.min(end.y), start.y.max(end.y));
                if *extend_right {
                    return vec![
                        (DrawingHandleKind::BoxTopLeft, Point::new(left, top)),
                        (DrawingHandleKind::BoxBottomLeft, Point::new(left, bottom)),
                    ];
                }
                vec![
                    (DrawingHandleKind::BoxTopLeft, Point::new(left, top)),
                    (DrawingHandleKind::BoxTopRight, Point::new(right, top)),
                    (DrawingHandleKind::BoxBottomRight, Point::new(right, bottom)),
                    (DrawingHandleKind::BoxBottomLeft, Point::new(left, bottom)),
                ]
            }
            DrawingObject::HorizontalLine {
                panel_id,
                y_unit,
                x,
                ray: true,
            } => self
                .point_for(DrawingAnchor {
                    panel_id: *panel_id,
                    x: *x,
                    y_unit: *y_unit,
                })
                .map(|point| (DrawingHandleKind::HorizontalLineStart, point))
                .into_iter()
                .collect(),
            DrawingObject::HorizontalLine { .. } | DrawingObject::VerticalLine { .. } => Vec::new(),
        }
    }

    fn draw_object(
        &self,
        frame: &mut canvas::Frame,
        object: &DrawingObject,
        style: DrawingStyle,
        selected: bool,
        look: &Look<'_>,
    ) {
        let width = style.normalized_stroke_width() + if selected { 0.6 } else { 0.0 };
        let color = iced_color(style.stroke_color, look.palette, look.accents);
        let stroke = canvas::Stroke::default()
            .with_color(color)
            .with_width(width);

        match object {
            DrawingObject::Trendline { start, end } => {
                if let Some((from, to)) = self.trendline(*start, *end) {
                    frame.stroke(&canvas::Path::line(from, to), stroke);
                }
            }
            DrawingObject::Box {
                start,
                end,
                extend_right,
            } => {
                let Some((full, visible)) = self.box_geometry(*start, *end, *extend_right) else {
                    return;
                };
                if let Some(fill) = style.fill_color {
                    frame.fill_rectangle(
                        visible.position(),
                        visible.size(),
                        iced_color(fill, look.palette, look.accents),
                    );
                }
                let (left, right) = (full.x, full.x + full.width);
                let (top, bottom) = (full.y, full.y + full.height);
                for (from, to) in [
                    (Point::new(left, top), Point::new(right, top)),
                    (Point::new(right, top), Point::new(right, bottom)),
                    (Point::new(right, bottom), Point::new(left, bottom)),
                    (Point::new(left, bottom), Point::new(left, top)),
                ] {
                    if let Some((from, to)) = clip_segment_to_bounds(from, to, self.plot) {
                        frame.stroke(&canvas::Path::line(from, to), stroke);
                    }
                }
            }
            DrawingObject::HorizontalLine { y_unit, x, ray, .. } => {
                if let Some((y, start_x)) = self.horizontal_line(*y_unit, *x, *ray) {
                    frame.stroke(
                        &canvas::Path::line(Point::new(start_x, y), Point::new(self.plot.width, y)),
                        stroke,
                    );
                }
            }
            DrawingObject::VerticalLine { x } => {
                if let Some(x) = self.vertical_line(*x) {
                    frame.stroke(
                        &canvas::Path::line(Point::new(x, 0.0), Point::new(x, self.plot.height)),
                        stroke,
                    );
                }
            }
            DrawingObject::FibRetracement { start, end } => {
                let Some(fib) = self.fib(*start, *end) else {
                    return;
                };
                frame.with_clip(self.plot, |frame| {
                    for pair in fib.levels.windows(2) {
                        let (top, bottom) = (pair[0].y.min(pair[1].y), pair[0].y.max(pair[1].y));
                        frame.fill_rectangle(
                            Point::new(fib.left, top),
                            Size::new((fib.right - fib.left).max(0.0), bottom - top),
                            color.scale_alpha(FIB_FILL_ALPHA),
                        );
                    }
                    for level in &fib.levels {
                        frame.stroke(
                            &canvas::Path::line(
                                Point::new(fib.left, level.y),
                                Point::new(fib.right, level.y),
                            ),
                            stroke,
                        );
                        frame.fill_text(canvas::Text {
                            content: format!(
                                "{} ({})",
                                fib_ratio_label(level.ratio),
                                self.price_label(level.y_unit)
                            ),
                            position: Point::new(fib.left + 4.0, level.y - 2.0),
                            color,
                            size: crate::style::text_size::SMALL.into(),
                            align_y: iced::alignment::Vertical::Bottom,
                            font: crate::style::AZERET_MONO,
                            ..Default::default()
                        });
                    }
                });
            }
        }
    }

    fn draw_handles(&self, frame: &mut canvas::Frame, object: &DrawingObject, look: &Look<'_>) {
        let handles = self.handles(object);
        if handles.is_empty() {
            return;
        }
        let fill = look.palette.background.strongest.color;
        let outline = look.palette.primary.base.color.scale_alpha(0.92);
        frame.with_clip(self.plot, |frame| {
            for (_, point) in handles {
                let circle = canvas::Path::circle(point, DRAWING_HANDLE_RADIUS_PX);
                frame.fill(&circle, fill);
                frame.stroke(
                    &circle,
                    canvas::Stroke::default()
                        .with_color(outline)
                        .with_width(1.2),
                );
            }
        });
    }

    /// Paints every drawing, the selection handles and the unfinished drawing.
    pub fn draw(&self, frame: &mut canvas::Frame, snapshot: &DrawingSnapshot<'_>, look: &Look<'_>) {
        for drawing in snapshot.entities.iter().filter(|drawing| drawing.visible) {
            self.draw_object(
                frame,
                &drawing.object,
                drawing.style,
                snapshot.selected_drawings.contains(&drawing.id),
                look,
            );
        }
        for drawing in snapshot.selected_visible_drawings() {
            self.draw_handles(frame, &drawing.object, look);
        }
        if let Some(draft) = snapshot.drawing_draft {
            let mut style = draft.style();
            style.stroke_color = style.stroke_color.scale_alpha(0.9);
            style.fill_color = style.fill_color.map(|fill| fill.scale_alpha(0.5));
            let preview = draft.preview_object();
            self.draw_object(frame, &preview, style, false, look);
            self.draw_handles(frame, &preview, look);
        }
    }

    fn hit_object(&self, object: &DrawingObject, style: DrawingStyle, point: Point) -> bool {
        let tolerance = DRAWING_HIT_TOLERANCE_PX;
        match object {
            DrawingObject::Trendline { start, end } => self
                .trendline(*start, *end)
                .is_some_and(|(from, to)| point_segment_distance(point, from, to) <= tolerance),
            DrawingObject::Box {
                start,
                end,
                extend_right,
            } => {
                let Some((full, visible)) = self.box_geometry(*start, *end, *extend_right) else {
                    return false;
                };
                if !visible.expand(tolerance).contains(point) {
                    return false;
                }
                let (left, right) = (full.x, full.x + full.width);
                let (top, bottom) = (full.y, full.y + full.height);
                let near_edge = [
                    (Point::new(left, top), Point::new(right, top)),
                    (Point::new(right, top), Point::new(right, bottom)),
                    (Point::new(right, bottom), Point::new(left, bottom)),
                    (Point::new(left, bottom), Point::new(left, top)),
                ]
                .into_iter()
                .filter_map(|(from, to)| clip_segment_to_bounds(from, to, self.plot))
                .any(|(from, to)| point_segment_distance(point, from, to) <= tolerance);
                near_edge || (style.fill_color.is_some() && visible.contains(point))
            }
            DrawingObject::HorizontalLine { y_unit, x, ray, .. } => self
                .horizontal_line(*y_unit, *x, *ray)
                .is_some_and(|(y, start_x)| {
                    point.x >= start_x - tolerance
                        && point.x <= self.plot.width + tolerance
                        && (point.y - y).abs() <= tolerance
                }),
            DrawingObject::FibRetracement { start, end } => {
                self.fib(*start, *end).is_some_and(|fib| {
                    let rows: Vec<f32> = fib.levels.iter().map(|level| level.y).collect();
                    fib_lines_hit(point, &rows, fib.left, fib.right, tolerance)
                })
            }
            DrawingObject::VerticalLine { x } => self.vertical_line(*x).is_some_and(|x| {
                (point.x - x).abs() <= tolerance
                    && point.y >= -tolerance
                    && point.y <= self.plot.height + tolerance
            }),
        }
    }

    fn hit_handle(&self, snapshot: &DrawingSnapshot<'_>, point: Point) -> Option<Hit> {
        let radius_sq = DRAWING_HANDLE_HIT_RADIUS_PX * DRAWING_HANDLE_HIT_RADIUS_PX;
        snapshot
            .selected_visible_drawings()
            .filter(|drawing| !drawing.locked)
            .flat_map(|drawing| {
                self.handles(&drawing.object)
                    .into_iter()
                    .map(move |(kind, handle)| (drawing.id, kind, handle))
            })
            .filter_map(|(id, kind, handle)| {
                let dist_sq = (point.x - handle.x).powi(2) + (point.y - handle.y).powi(2);
                (dist_sq <= radius_sq).then_some((dist_sq, id, kind))
            })
            .min_by(|a, b| a.0.total_cmp(&b.0))
            .map(|(_, id, kind)| Hit::Handle { id, kind })
    }

    fn hit_body(&self, snapshot: &DrawingSnapshot<'_>, point: Point) -> Option<Hit> {
        snapshot
            .entities
            .iter()
            .rev()
            .filter(|drawing| drawing.visible)
            .find(|drawing| self.hit_object(&drawing.object, drawing.style, point))
            .map(|drawing| Hit::Body {
                id: drawing.id,
                selected: snapshot.selected_drawings.contains(&drawing.id),
            })
    }

    /// The handle or drawing under `point`, topmost first.
    pub fn hit(&self, snapshot: &DrawingSnapshot<'_>, point: Point) -> Option<Hit> {
        if !self.plot.contains(point) {
            return None;
        }
        self.hit_handle(snapshot, point)
            .or_else(|| self.hit_body(snapshot, point))
    }
}

fn finite(value: f64) -> Option<f32> {
    let value = value as f32;
    value.is_finite().then_some(value)
}

struct FibLevel {
    ratio: f64,
    y_unit: YUnit,
    y: f32,
}

struct FibGeometry {
    left: f32,
    right: f32,
    levels: Vec<FibLevel>,
}

/// Theme inputs for painting.
pub struct Look<'a> {
    pub palette: &'a Extended,
    pub accents: &'a [iced::Color; data::config::theme::ACCENT_COUNT],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    Handle {
        id: DrawingId,
        kind: DrawingHandleKind,
    },
    Body {
        id: DrawingId,
        selected: bool,
    },
}

impl Hit {
    pub fn cursor(self) -> mouse::Interaction {
        match self {
            Self::Handle { .. } | Self::Body { selected: true, .. } => mouse::Interaction::Grab,
            Self::Body { .. } => mouse::Interaction::Pointer,
        }
    }
}

/// What the heatmap widget hands the layer when drawings are shown.
pub struct Layer<'a> {
    pub snapshot: DrawingSnapshot<'a>,
    pub accents: ThemeAccents,
    /// Whether the drawing history has an edit to undo or redo; without one, Cmd+Z is left
    /// to the rest of the app instead of being swallowed here.
    pub can_undo: bool,
    pub can_redo: bool,
}

impl Layer<'_> {
    pub fn armed(&self) -> bool {
        self.snapshot.active_tool != DrawingTool::Cursor
    }
}

/// The request Cmd+Z (or Cmd+Shift+Z) makes, or `None` when there is nothing to undo or redo.
fn undo_redo_request(shift: bool, can_undo: bool, can_redo: bool) -> Option<Message> {
    match (shift, can_undo, can_redo) {
        (true, _, true) => Some(Message::DrawingRedoRequested),
        (false, true, _) => Some(Message::DrawingUndoRequested),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy)]
struct ActiveDrag {
    id: DrawingId,
    target: DrawingDragTarget,
}

/// Pointer state kept by the widget between events.
#[derive(Debug, Default)]
pub struct PointerState {
    drag: Option<ActiveDrag>,
    modifiers: keyboard::Modifiers,
}

impl PointerState {
    pub fn is_dragging(&self) -> bool {
        self.drag.is_some()
    }

    /// Handles one event. Returns `true` when the event was taken and must not reach the
    /// heatmap's own pan and zoom handling. `plot` is in window coordinates, `cursor_in_plot`
    /// the cursor relative to it when inside.
    pub fn handle(
        &mut self,
        layer: &Layer<'_>,
        view: Option<&View>,
        event: &Event,
        plot: Rectangle,
        cursor_in_plot: Option<Point>,
        blocked: bool,
        publish: &mut impl FnMut(Message),
    ) -> bool {
        match event {
            Event::Keyboard(keyboard::Event::ModifiersChanged(modifiers)) => {
                self.modifiers = *modifiers;
                false
            }
            Event::Keyboard(keyboard::Event::KeyReleased { modifiers, .. }) => {
                self.modifiers = *modifiers;
                false
            }
            Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. }) => {
                self.modifiers = *modifiers;
                if cursor_in_plot.is_none() {
                    return false;
                }
                match key.as_ref() {
                    keyboard::Key::Character(character)
                        if character.eq_ignore_ascii_case("z")
                            && modifiers.contains(keyboard::Modifiers::COMMAND) =>
                    {
                        match undo_redo_request(modifiers.shift(), layer.can_undo, layer.can_redo) {
                            Some(message) => {
                                publish(message);
                                true
                            }
                            None => false,
                        }
                    }
                    keyboard::Key::Named(
                        keyboard::key::Named::Backspace | keyboard::key::Named::Delete,
                    ) if !layer.snapshot.selected_drawings.is_empty() => {
                        publish(Message::DrawingDeleteRequested);
                        true
                    }
                    _ => false,
                }
            }
            Event::Mouse(mouse::Event::CursorMoved { position }) => {
                let Some(view) = view else {
                    return false;
                };
                let local = Point::new(position.x - plot.x, position.y - plot.y);
                if let Some(drag) = self.drag {
                    if let Some(anchor) = view.anchor_at(local) {
                        publish(Message::Drawing(KlineWidgetDrawingEvent::DragMoved {
                            id: drag.id,
                            target: drag.target,
                            anchor,
                        }));
                    }
                    return true;
                }
                if layer.snapshot.drawing_draft.is_some()
                    && cursor_in_plot.is_some()
                    && let Some(anchor) = view.anchor_at(local)
                {
                    publish(Message::Drawing(KlineWidgetDrawingEvent::AnchorMoved(
                        anchor,
                    )));
                }
                false
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                let Some(drag) = self.drag.take() else {
                    return false;
                };
                publish(Message::Drawing(KlineWidgetDrawingEvent::DragFinished {
                    id: drag.id,
                }));
                true
            }
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Right)) => {
                if layer.snapshot.drawing_draft.is_some() && cursor_in_plot.is_some() {
                    publish(Message::Drawing(KlineWidgetDrawingEvent::DraftCanceled));
                    return true;
                }
                false
            }
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                let (Some(view), Some(local), false) = (view, cursor_in_plot, blocked) else {
                    return false;
                };
                self.press(layer, view, local, publish)
            }
            _ => false,
        }
    }

    fn press(
        &mut self,
        layer: &Layer<'_>,
        view: &View,
        local: Point,
        publish: &mut impl FnMut(Message),
    ) -> bool {
        let snapshot = &layer.snapshot;
        let anchor = view.anchor_at(local);

        if layer.armed() {
            let Some(anchor) = anchor else {
                return false;
            };
            publish(Message::Drawing(KlineWidgetDrawingEvent::AnchorPressed(
                anchor,
            )));
            return true;
        }

        let copy = self.modifiers.command();
        let hit = view.hit(snapshot, local);
        let Some(anchor) = anchor else {
            return false;
        };

        let (id, target, operation) = match hit {
            Some(Hit::Handle { id, kind }) if !copy => (
                id,
                DrawingDragTarget::Handle(kind),
                DrawingDragOperation::Move,
            ),
            Some(Hit::Handle { .. } | Hit::Body { .. }) => {
                // The drawing body, also when a handle is hit while copying.
                let Some(Hit::Body { id, selected }) = view.hit_body(snapshot, local) else {
                    return false;
                };
                if !selected {
                    publish(Message::Drawing(KlineWidgetDrawingEvent::Selected(Some(
                        id,
                    ))));
                }
                if snapshot
                    .entities
                    .iter()
                    .any(|drawing| drawing.id == id && drawing.locked)
                {
                    // A locked drawing can be selected but not moved or copied.
                    return true;
                }
                let operation = if copy {
                    DrawingDragOperation::Copy
                } else {
                    DrawingDragOperation::Move
                };
                (id, DrawingDragTarget::Translate, operation)
            }
            None => {
                // A click on empty space only clears the selection; the press still pans.
                if !snapshot.selected_drawings.is_empty() {
                    publish(Message::Drawing(KlineWidgetDrawingEvent::Selected(None)));
                }
                return false;
            }
        };

        publish(Message::Drawing(KlineWidgetDrawingEvent::DragStarted {
            id,
            target,
            operation,
            anchor,
        }));
        self.drag = Some(ActiveDrag { id, target });
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn undo_redo_only_requested_when_available() {
        assert!(matches!(
            undo_redo_request(false, true, false),
            Some(Message::DrawingUndoRequested)
        ));
        assert!(undo_redo_request(false, false, true).is_none());
        assert!(matches!(
            undo_redo_request(true, false, true),
            Some(Message::DrawingRedoRequested)
        ));
        assert!(undo_redo_request(true, true, false).is_none());
    }

    /// 1 s buckets, latest bucket 1000, 20 px per bucket, one px per world unit; rows of
    /// 10 px, price step 0.5, base price 100 at the centre row of a 400 px tall plot.
    fn projection() -> Projection {
        Projection {
            interval_ms: 1_000.0,
            latest_bucket: 1_000.0,
            phase: 0.25,
            column_world: 20.0,
            x_origin: 300.0,
            x_scale: 1.0,
            base_price: 100.0,
            step: 0.5,
            row_h: 10.0,
            y_origin: 200.0,
            y_scale: 1.0,
        }
    }

    #[test]
    fn time_maps_to_x_and_back() {
        let projection = projection();
        let latest_ms = 1_000.0 * 1_000.0;
        // The latest bucket sits `phase` columns left of the origin.
        assert!((projection.x_at(latest_ms) - (300.0 - 0.25 * 20.0)).abs() < 1e-9);
        // One bucket later is one column to the right.
        let next = projection.x_at(latest_ms + 1_000.0);
        assert!((next - projection.x_at(latest_ms) - 20.0).abs() < 1e-9);
        for time in [latest_ms - 90_000.0, latest_ms, latest_ms + 12_345.0] {
            let back = projection.time_at(projection.x_at(time));
            assert!((back - time).abs() < 1e-6, "{back} != {time}");
        }
    }

    #[test]
    fn times_between_buckets_are_not_snapped() {
        let projection = projection();
        let half = projection.x_at(1_000_500.0) - projection.x_at(1_000_000.0);
        assert!((half - 10.0).abs() < 1e-9);
    }

    #[test]
    fn price_maps_to_row_centres_and_back() {
        let projection = projection();
        // Base price is the centre of row 0, whose centre is half a row above the origin.
        assert!((projection.y_at(100.0) - (200.0 - 5.0)).abs() < 1e-9);
        // One step up is one row higher on screen.
        assert!((projection.y_at(100.5) - (200.0 - 15.0)).abs() < 1e-9);
        assert!((projection.y_at(99.5) - (200.0 + 5.0)).abs() < 1e-9);
        for price in [95.25, 100.0, 103.7] {
            let back = projection.price_at(projection.y_at(price));
            assert!((back - price).abs() < 1e-9, "{back} != {price}");
        }
    }

    #[test]
    fn panning_and_zooming_move_drawings_with_the_axes() {
        let near = projection();
        let panned = Projection {
            x_origin: near.x_origin - 50.0,
            y_origin: near.y_origin + 30.0,
            ..near
        };
        let time = 1_010_000.0;
        assert!((near.x_at(time) - panned.x_at(time) - 50.0).abs() < 1e-9);
        assert!((panned.y_at(101.0) - near.y_at(101.0) - 30.0).abs() < 1e-9);

        let zoomed = Projection {
            x_scale: 2.0,
            y_scale: 2.0,
            ..near
        };
        assert!(
            ((zoomed.x_at(time) - zoomed.x_origin) - 2.0 * (near.x_at(time) - near.x_origin)).abs()
                < 1e-9
        );
        let back = zoomed.price_at(zoomed.y_at(101.25));
        assert!((back - 101.25).abs() < 1e-9);
    }

    #[test]
    fn projection_matches_the_axis_placement_of_the_camera() {
        use crate::widget::chart::heatmap::scene::camera::Camera;

        let mut camera = Camera::default();
        camera.offset = [-3.5, 2.0];
        let size = Size::new(800.0, 500.0);
        let time = TimeView {
            camera: &camera,
            latest_bucket: 5_000,
            interval_ms: 500,
            column_world: 4.0,
            phase: 0.3,
        };
        let step = PriceStep {
            units: Price::from_f64(0.5).units,
        };
        let base = Price::from_f64(100.0);
        let row_h = 2.0_f32;
        let projection = Projection::new(time, size, base, step, row_h).expect("valid axes");

        // A bucket start lands where the time axis puts that bucket.
        let bucket_time = UnixMs::new(4_990 * 500);
        let expected = f64::from(time.screen_x(bucket_time, size.width));
        assert!((projection.x_at(4_990.0 * 500.0) - expected).abs() < 1e-3);

        // A price lands at the centre of its row, `step` per row, like the Y axis.
        let step_f = Price::from_units(step.units).to_f64();
        for rows in [-7.0_f64, 0.0, 3.0, 12.0] {
            let price = base.to_f64() + rows * step_f;
            let world_y = -(rows as f32 + 0.5) * row_h;
            let expected = f64::from(camera.world_to_screen_y(world_y, size.width, size.height));
            assert!((projection.y_at(price) - expected).abs() < 1e-3, "{rows}");
        }
    }

    #[test]
    fn invalid_axes_are_rejected() {
        assert!(projection().is_valid());
        assert!(
            !Projection {
                step: 0.0,
                ..projection()
            }
            .is_valid()
        );
        assert!(
            !Projection {
                x_scale: f64::NAN,
                ..projection()
            }
            .is_valid()
        );
    }
}
