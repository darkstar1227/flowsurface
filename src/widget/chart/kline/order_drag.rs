//! Dragging a stop or target line to another price (f3a2): hit testing, the price under the
//! pointer and the preview. All of it is memory-only widget state; nothing is saved, and the
//! drawings library never sees it. Releasing a drag only publishes
//! `KlineWidgetEvent::OrderLineMoved`: the order is read again and checked before anything is
//! built (see `replace_flow` and `main::start_replace_request`).

use super::painting::PaintInputs;
use super::scene::{CursorTarget, Scene};
use super::{CHAR_W, TEXT_SIZE};
use crate::order_lines::{Grab, LineKind, OrderLine};
use crate::replace_flow::{DragBlock, price_at_y};
use iced::theme::palette::Extended;
use iced::widget::canvas;
use iced::{Point, Rectangle};

/// How close (in pixels) the pointer must be to a line to grab it.
const GRAB_RADIUS: f32 = 5.0;

/// A press and release that moved less than this (in pixels) is a click, not a drag.
pub(super) const CLICK_SLOP: f32 = 4.0;

/// A drag in progress.
#[derive(Debug, Clone)]
pub(super) struct OrderDrag {
    pub(super) order_id: String,
    /// Where the pointer was when the line was grabbed (widget coordinates).
    pub(super) start_y: f32,
    /// Where the pointer is now.
    pub(super) y: f32,
    /// The price under the pointer.
    pub(super) price: f32,
}

/// A line that can be grabbed, or whose grab is explained: a stop or a target.
fn is_candidate(line: &OrderLine) -> bool {
    matches!(line.kind, LineKind::Stop | LineKind::Target) && line.grab != Grab::No
}

/// The primary price scale is plain (linear): a position on it maps back to a price. Percent
/// and log scales do not.
pub(super) fn plain_scale(scene: &Scene) -> bool {
    !scene
        .primary
        .mode
        .uses_display_transform(scene.can_use_log_primary_scale())
}

/// What the line allows on this chart: a draggable line is blocked on a percent or log scale.
pub(super) fn grab_of(line: &OrderLine, scene: &Scene) -> Grab {
    match &line.grab {
        Grab::Yes(_) if !plain_scale(scene) => Grab::Blocked(DragBlock::Scale),
        other => other.clone(),
    }
}

/// The widget y of a price on the primary panel, as the lines are painted; `None` outside the
/// primary panel.
fn line_y(scene: &Scene, price: f32) -> Option<f32> {
    let plot = scene.plot_rect();
    let anchor = scene
        .primary
        .series_percent_anchors
        .first()
        .copied()
        .flatten();
    let y = plot.y + scene.map_primary_plot_with_anchor_f32(price, anchor);
    let panel = scene.primary_plot();
    let (top, bottom) = (plot.y + panel.y, plot.y + panel.y + panel.height);
    (y.is_finite() && y >= top && y <= bottom).then_some(y)
}

/// The index of the stop or target line under `local` (widget coordinates), the nearest one.
pub(super) fn hit(lines: &[OrderLine], scene: &Scene, local: Point) -> Option<usize> {
    // A legend icon, panel control, drawing or study object under the pointer comes first.
    if !matches!(scene.interaction.target, CursorTarget::Plot(_)) {
        return None;
    }
    let plot = scene.plot_rect();
    if local.x < plot.x || local.x > plot.x + plot.width {
        return None;
    }
    lines
        .iter()
        .enumerate()
        .filter(|(_, l)| is_candidate(l))
        .filter_map(|(i, l)| {
            let y = line_y(scene, l.price_f32()?)?;
            Some((i, (y - local.y).abs()))
        })
        .filter(|(_, d)| *d <= GRAB_RADIUS)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(i, _)| i)
}

/// The price under `local` on a plain primary scale; `None` outside the primary plot, on a
/// percent or log scale, or on a degenerate range. Dragging out of the plot is how a drag is
/// cancelled.
pub(super) fn price_at(scene: &Scene, local: Point) -> Option<f32> {
    if !plain_scale(scene) {
        return None;
    }
    let plot_local = scene.layout.plot_local_point(local)?;
    let panel = scene.primary_plot();
    if plot_local.x < 0.0 || plot_local.x > panel.width {
        return None;
    }
    let (min, max) = scene
        .primary
        .value_scale
        .values_f32(scene.primary.domain.active());
    price_at_y(
        plot_local.y - panel.y,
        0.0,
        panel.height,
        min,
        max,
        scene.primary.inverted,
    )
}

/// The price with as many decimals as the line's own price has (at most eight).
fn price_text(line: &OrderLine, price: f32) -> String {
    let decimals = line
        .price
        .to_plain_string()
        .split_once('.')
        .map_or(0, |(_, frac)| frac.len())
        .min(8);
    format!("{price:.decimals$}")
}

fn line_color(kind: LineKind, palette: &Extended) -> iced::Color {
    match kind {
        LineKind::Target => palette.success.base.color,
        _ => palette.danger.base.color,
    }
}

fn label_rect(plot: Rectangle, y: f32, text: &str) -> Rectangle {
    let height = TEXT_SIZE + 4.0;
    let width = text.chars().count() as f32 * CHAR_W + 8.0;
    Rectangle {
        x: (plot.x + plot.width - width).max(plot.x),
        y: (y - height * 0.5).clamp(plot.y, (plot.y + plot.height - height).max(plot.y)),
        width,
        height,
    }
}

fn fill_label(
    frame: &mut PaintInputs,
    rect: Rectangle,
    text: String,
    fill: iced::Color,
    on: iced::Color,
) {
    frame.fill_rectangle(rect.position(), rect.size(), fill);
    frame.fill_text(canvas::Text {
        content: text,
        position: Point::new(rect.x + 4.0, rect.y + rect.height * 0.5),
        color: on,
        size: TEXT_SIZE.into(),
        align_x: iced::Alignment::Start.into(),
        align_y: iced::Alignment::Center.into(),
        font: crate::style::AZERET_MONO,
        ..Default::default()
    });
}

/// The preview of a drag (a dashed line at the pointer with the target price) and, for a line
/// the pointer is over, what it allows: how to move it, or why it cannot be moved.
pub(super) fn paint(
    frame: &mut PaintInputs,
    scene: &Scene,
    palette: &Extended,
    lines: &[OrderLine],
    hover: Option<usize>,
    drag: Option<&OrderDrag>,
) {
    let plot = scene.plot_rect();
    if let Some(drag) = drag {
        let line = lines
            .iter()
            .find(|l| matches!(&l.grab, Grab::Yes(id) if *id == drag.order_id));
        let Some(line) = line else {
            return;
        };
        let color = line_color(line.kind, palette);
        let y = drag.y.clamp(plot.y, plot.y + plot.height);
        frame.stroke(
            &canvas::Path::line(Point::new(plot.x, y), Point::new(plot.x + plot.width, y)),
            canvas::Stroke {
                width: 1.5,
                line_dash: canvas::LineDash {
                    segments: &[6.0, 4.0],
                    offset: 0,
                },
                ..Default::default()
            }
            .with_color(color),
        );
        let text = format!("{} → {}", line.kind.label(), price_text(line, drag.price));
        let rect = label_rect(plot, y, &text);
        let on = if data::config::theme::is_dark(color) {
            iced::Color::WHITE
        } else {
            iced::Color::BLACK
        };
        fill_label(frame, rect, text, color, on);
        return;
    }
    let Some(line) = hover.and_then(|i| lines.get(i)) else {
        return;
    };
    let Some(y) = line.price_f32().and_then(|p| line_y(scene, p)) else {
        return;
    };
    let (text, color) = match grab_of(line, scene) {
        Grab::Yes(_) => (
            "Drag to move · Esc cancels".to_string(),
            palette.secondary.strong.color,
        ),
        Grab::Blocked(why) => (why.text().to_string(), palette.secondary.weak.color),
        Grab::No => return,
    };
    let height = TEXT_SIZE + 4.0;
    let width = text.chars().count() as f32 * CHAR_W + 8.0;
    let rect = Rectangle {
        x: plot.x + 8.0,
        y: (y - height - 3.0).max(plot.y),
        width,
        height,
    };
    let on = if data::config::theme::is_dark(color) {
        iced::Color::WHITE
    } else {
        iced::Color::BLACK
    };
    fill_label(frame, rect, text, color, on);
}
