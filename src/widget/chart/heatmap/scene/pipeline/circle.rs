use data::chart::heatmap::{Config, GroupedTrade, bubble_radius_px};
use exchange::unit::{Price, PriceStep, Qty, qty};

use crate::widget::chart::heatmap::scene::depth_grid::HeatmapPalette;
use crate::widget::chart::heatmap::view::ViewWindow;

pub const CIRCLE_VERTICES: &[[f32; 2]] = &[[-1.0, -1.0], [1.0, -1.0], [1.0, 1.0], [-1.0, 1.0]];
pub const CIRCLE_INDICES: &[u16] = &[0, 1, 2, 2, 3, 0];

#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct CircleInstance {
    pub y_world: f32,
    pub x_bin_rel: i32,
    pub x_frac: f32,
    pub radius_px: f32,
    _pad: f32,
    pub color: [f32; 4],
}

impl CircleInstance {
    pub const R_MIN_PX: f32 = 1.5;
    const ALPHA: f32 = 0.8;

    pub fn from_trade(
        trade: &GroupedTrade,
        bucket: i64,
        ref_bucket: i64,
        base_price: Price,
        step: PriceStep,
        y_anchor: Option<Price>,
        w: &ViewWindow,
        palette: &HeatmapPalette,
        max_trade_qty: Qty,
        config: &Config,
        fallback_radius_px: f32,
    ) -> Self {
        let x_bin_rel = (bucket - ref_bucket).clamp(i32::MIN as i64, i32::MAX as i64) as i32;
        let x_frac = 0.0;

        let y_world = w.y_center_for_price_texture_aligned(trade.price, base_price, step, y_anchor);

        let q = trade.qty.max(qty::Qty::ZERO).to_f64();
        let t = (q / max_trade_qty.to_scale_or_one()).clamp(0.0, 1.0) as f32;
        let radius_px = if let Some(scale_pct) = config.trade_size_scale {
            bubble_radius_px(
                t,
                config.trade_radius_scale,
                Self::R_MIN_PX,
                config.trade_max_radius_px,
                scale_pct,
            )
        } else {
            fallback_radius_px.max(Self::R_MIN_PX)
        };

        let rgba = if trade.is_sell {
            [
                palette.sell_rgb[0],
                palette.sell_rgb[1],
                palette.sell_rgb[2],
                Self::ALPHA,
            ]
        } else {
            [
                palette.buy_rgb[0],
                palette.buy_rgb[1],
                palette.buy_rgb[2],
                Self::ALPHA,
            ]
        };

        Self {
            y_world,
            x_bin_rel,
            x_frac,
            radius_px,
            _pad: 0.0,
            color: rgba,
        }
    }
}
