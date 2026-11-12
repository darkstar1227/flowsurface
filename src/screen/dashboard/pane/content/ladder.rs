use crate::style;
use data::panel::ladder::{ChaseTracker, Config, GroupedDepth, Side, TradeStore};
use exchange::Trade;
use exchange::unit::qty::Qty;
use exchange::unit::{Price, PriceStep};
use exchange::{TickerInfo, UnixMs, depth::Depth};

use iced::widget::canvas::{self, Canvas, Path, Stroke, Text};
use iced::{
    Alignment, Element, Event, Point, Rectangle, Renderer, Size, Theme, mouse, padding,
    widget::{center, container, text},
};

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

const TEXT_SIZE: f32 = style::text_size::SMALL;
const ROW_HEIGHT: f32 = 16.0;

// Total width ratios must sum to 1.0
/// Uses half of the width for each side of the order quantity columns
const ORDER_QTY_COLS_WIDTH: f32 = 0.60;
/// Uses half of the width for each side of the trade quantity columns
const TRADE_QTY_COLS_WIDTH: f32 = 0.20;

const COL_PADDING: f32 = 4.0;
/// Used for calculating layout with texts inside the price column
const MONO_CHAR_ADVANCE: f32 = 0.62;
/// Minimum padding on each side of the price text inside the price column
const PRICE_TEXT_SIDE_PAD_MIN: f32 = 12.0;

const CHASE_CIRCLE_RADIUS: f32 = 4.0;
/// Maximum interval between chase updates to consider them part of the same chase
const CHASE_MIN_INTERVAL: Duration = Duration::from_millis(200);

#[derive(Debug, Clone, Copy)]
pub enum Message {
    Scrolled(f32),
    ResetScroll,
    /// Command (Ctrl) + left click on a price row. Only asks for a review: a plain click never
    /// does (it resets the scroll).
    PriceClicked(PriceClick),
}

/// The row a command click landed on, as the pane sees it (before the price is rounded to the
/// symbol's tick, which the exchange builder does).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PriceClick {
    pub side: Side,
    pub price: Price,
}

impl Ladder {
    /// The ticker of the pane (the order's symbol and market).
    pub fn ticker(&self) -> exchange::Ticker {
        self.ticker_info.ticker
    }

    pub fn update(&mut self, message: Message) -> Option<PriceClick> {
        match message {
            Message::Scrolled(delta) => self.scroll(delta),
            Message::ResetScroll => self.reset_scroll(),
            Message::PriceClicked(click) => return Some(click),
        }
        None
    }

    fn scroll(&mut self, delta: f32) {
        self.scroll_px += delta;
        Ladder::invalidate(self, Some(Instant::now()));
    }

    fn reset_scroll(&mut self) {
        self.scroll_px = 0.0;
        Ladder::invalidate(self, Some(Instant::now()));
    }

    fn is_empty(&self) -> bool {
        if self.pending_tick_size.is_some() {
            return true;
        }
        self.grouped_asks().is_empty() && self.grouped_bids().is_empty() && self.trades.is_empty()
    }

    pub fn view<'a>(&'a self, _timezone: data::UserTimezone) -> Element<'a, Message> {
        if self.is_empty() {
            return center(text("Waiting for data...").size(crate::style::text_size::TITLE)).into();
        }

        container(
            Canvas::new(self)
                .width(iced::Length::Fill)
                .height(iced::Length::Fill),
        )
        .padding(padding::left(1).right(1).bottom(1))
        .into()
    }
}

pub struct Ladder {
    ticker_info: TickerInfo,
    pub config: Config,
    cache: canvas::Cache,
    last_tick: Instant,
    /// When the order book was last updated (unlike `last_tick`, a redraw does not move it).
    last_depth_at: Option<Instant>,
    step: PriceStep,
    scroll_px: f32,
    last_exchange_ts_ms: Option<UnixMs>,
    orderbook: [GroupedDepth; 2],
    trades: TradeStore,
    pending_tick_size: Option<PriceStep>,
    raw_price_spread: Option<Price>,
}

impl Ladder {
    pub fn new(config: Option<Config>, ticker_info: TickerInfo, step: PriceStep) -> Self {
        Self {
            trades: TradeStore::new(),
            config: config.unwrap_or_default(),
            ticker_info,
            cache: canvas::Cache::default(),
            last_tick: Instant::now(),
            last_depth_at: None,
            step,
            scroll_px: 0.0,
            last_exchange_ts_ms: None,
            orderbook: [GroupedDepth::new(), GroupedDepth::new()],
            raw_price_spread: None,
            pending_tick_size: None,
        }
    }

    pub fn insert_trades(&mut self, buffer: &[Trade]) {
        self.trades.insert_trades(buffer, self.step);
    }

    pub fn insert_depth(&mut self, depth: &Depth, update_t: UnixMs) {
        if let Some(next) = self.pending_tick_size.take() {
            self.step = next;
            self.trades.rebuild_grouped(self.step);
        }

        let raw_best_bid = depth.bids.last_key_value().map(|(p, _)| *p);
        let raw_best_ask = depth.asks.first_key_value().map(|(p, _)| *p);
        self.raw_price_spread = match (raw_best_bid, raw_best_ask) {
            (Some(bid), Some(ask)) => Some(ask - bid),
            _ => None,
        };

        if self.config.show_chase_tracker {
            let max_int = CHASE_MIN_INTERVAL;
            self.chase_tracker_mut(Side::Bid)
                .update(raw_best_bid, true, update_t, max_int);
            self.chase_tracker_mut(Side::Ask)
                .update(raw_best_ask, false, update_t, max_int);
        } else {
            self.chase_tracker_mut(Side::Bid).reset();
            self.chase_tracker_mut(Side::Ask).reset();
        }

        if self
            .trades
            .maybe_cleanup(update_t, self.config.trade_retention, self.step)
        {
            self.invalidate(Some(Instant::now()));
        }

        self.regroup_from_depth(depth);
        self.last_exchange_ts_ms = Some(update_t);
        self.last_depth_at = Some(Instant::now());
    }

    fn trade_qty_at(&self, price: Price) -> (Qty, Qty) {
        self.trades.trade_qty_at(price)
    }

    pub fn last_update(&self) -> Instant {
        self.last_tick
    }

    fn grouped_asks(&self) -> &BTreeMap<Price, Qty> {
        &self.orderbook[Side::Ask.idx()].orders
    }

    fn grouped_bids(&self) -> &BTreeMap<Price, Qty> {
        &self.orderbook[Side::Bid.idx()].orders
    }

    fn chase_tracker(&self, side: Side) -> &ChaseTracker {
        &self.orderbook[side.idx()].chase
    }

    fn chase_tracker_mut(&mut self, side: Side) -> &mut ChaseTracker {
        &mut self.orderbook[side.idx()].chase
    }

    fn best_price(&self, side: Side) -> Option<Price> {
        self.orderbook[side.idx()].best_price(side)
    }

    pub fn min_tick_size(&self) -> f32 {
        self.ticker_info.min_ticksize.into()
    }

    pub fn tick_size(&self) -> PriceStep {
        self.pending_tick_size.unwrap_or(self.step)
    }

    pub fn set_tick_size(&mut self, step: PriceStep) {
        self.pending_tick_size = Some(step);
        self.invalidate(Some(Instant::now()));
    }

    pub fn set_show_chase_tracker(&mut self, enabled: bool) {
        if self.config.show_chase_tracker != enabled {
            self.config.show_chase_tracker = enabled;
            if !enabled {
                self.chase_tracker_mut(Side::Bid).reset();
                self.chase_tracker_mut(Side::Ask).reset();
            }

            self.invalidate(Some(Instant::now()));
        }
    }

    fn regroup_from_depth(&mut self, depth: &Depth) {
        let step = self.step;

        self.orderbook[Side::Ask.idx()].regroup_from_raw(&depth.asks, Side::Ask, step);
        self.orderbook[Side::Bid.idx()].regroup_from_raw(&depth.bids, Side::Bid, step);
    }

    pub fn invalidate(&mut self, now: Option<Instant>) {
        self.cache.clear();
        if let Some(now) = now {
            self.last_tick = now;
        }
    }

    fn format_price(&self, price: Price) -> String {
        let precision = self.ticker_info.min_ticksize;
        price.to_string(precision)
    }

    fn format_quantity(&self, qty: Qty) -> String {
        data::util::abbr_large_numbers(qty.to_f64())
    }
}

/// What the canvas remembers between events: the modifier keys held now.
#[derive(Debug, Default)]
pub struct Interaction {
    modifiers: iced::keyboard::Modifiers,
}

/// The row under a y position (relative to the canvas), with its side and price. Row 0 is the
/// spread / divider row and has no price. `scroll` and `height` are the ones the rows are drawn
/// with, so the click lands on the row the user sees; a moved best price is in `grid`.
fn row_at(y: f32, scroll: f32, height: f32, grid: &PriceGrid) -> Option<(Side, Price)> {
    if !(0.0..height).contains(&y) {
        return None;
    }
    let mid = height * 0.5;
    // Inverse of `mid + top_y(idx) - scroll`, the top edge of a row.
    let idx = ((y - mid + scroll + ROW_HEIGHT * 0.5) / ROW_HEIGHT).floor() as i32;
    let price = grid.index_to_price(idx)?;
    let side = if idx > 0 { Side::Bid } else { Side::Ask };
    Some((side, price))
}

/// How old the order book may be for a click to ask for an order. A quiet book still updates
/// within seconds; older than this, the rows may not be where the market is.
const DEPTH_MAX_AGE: Duration = Duration::from_secs(5);

/// A click may ask for an order only on a book that has both sides and was updated recently.
/// Without a book the grid is made from trade prices, and its bid/ask split is a guess.
fn depth_is_usable(has_bid: bool, has_ask: bool, age: Option<Duration>) -> bool {
    has_bid && has_ask && age.is_some_and(|a| a <= DEPTH_MAX_AGE)
}

/// What a mouse press does. Command (Ctrl) + left on a price row asks for a limit order; on a
/// row with no price it does nothing; every other press keeps its old effect (reset scroll). A
/// plain click never asks for an order.
fn press_message(
    button: mouse::Button,
    modifiers: iced::keyboard::Modifiers,
    row: Option<(Side, Price)>,
) -> Option<Message> {
    if button == mouse::Button::Left && crate::widget::is_command_modifier(modifiers) {
        return row.map(|(side, price)| Message::PriceClicked(PriceClick { side, price }));
    }
    Some(Message::ResetScroll)
}

impl canvas::Program<Message> for Ladder {
    type State = Interaction;

    fn update(
        &self,
        state: &mut Self::State,
        event: &iced::Event,
        bounds: iced::Rectangle,
        cursor: iced_core::mouse::Cursor,
    ) -> Option<canvas::Action<Message>> {
        // Followed wherever the cursor is, so the key is known when the click comes.
        if let Event::Keyboard(iced::keyboard::Event::ModifiersChanged(modifiers)) = event {
            state.modifiers = *modifiers;
            return None;
        }

        let cursor_position = cursor.position_in(bounds)?;

        match event {
            Event::Mouse(mouse::Event::ButtonPressed(
                button @ (mouse::Button::Middle | mouse::Button::Left | mouse::Button::Right),
            )) => {
                let usable = depth_is_usable(
                    self.best_price(Side::Bid).is_some(),
                    self.best_price(Side::Ask).is_some(),
                    self.last_depth_at.map(|t| t.elapsed()),
                );
                let row = self.build_price_grid().filter(|_| usable).and_then(|grid| {
                    row_at(cursor_position.y, self.scroll_px, bounds.height, &grid)
                });
                match press_message(*button, state.modifiers, row) {
                    Some(message) => Some(canvas::Action::publish(message).and_capture()),
                    None => Some(canvas::Action::capture()),
                }
            }
            Event::Mouse(mouse::Event::WheelScrolled { delta }) => {
                let scroll_amount = match delta {
                    mouse::ScrollDelta::Lines { y, .. } => -(*y) * ROW_HEIGHT,
                    mouse::ScrollDelta::Pixels { y, .. } => -*y,
                };

                Some(canvas::Action::publish(Message::Scrolled(scroll_amount)).and_capture())
            }
            _ => None,
        }
    }

    fn draw(
        &self,
        _state: &Self::State,
        renderer: &Renderer,
        theme: &Theme,
        bounds: Rectangle,
        _cursor: iced_core::mouse::Cursor,
    ) -> Vec<iced::widget::canvas::Geometry<Renderer>> {
        let palette = theme.extended_palette();

        let text_color = palette.background.base.text;
        let bid_color = palette.success.base.color;
        let ask_color = palette.danger.base.color;

        let divider_color = style::split_ruler(theme).color;

        let orderbook_visual = self.cache.draw(renderer, bounds.size(), |frame| {
            if let Some(grid) = self.build_price_grid() {
                let layout = self.price_layout_for(bounds.width, &grid);
                let cols = self.column_ranges(bounds.width, layout.price_px);

                let (visible_rows, maxima) = self.visible_rows(bounds, &grid);

                let mut spread_row: Option<(f32, f32)> = None;
                let mut best_bid_y: Option<f32> = None;
                let mut best_ask_y: Option<f32> = None;

                for visible_row in visible_rows.iter() {
                    match visible_row.row {
                        DomRow::Ask { price, .. }
                            if Some(price)
                                == self.grouped_asks().first_key_value().map(|(p, _)| *p) =>
                        {
                            best_ask_y = Some(visible_row.y);
                        }
                        DomRow::Bid { price, .. }
                            if Some(price)
                                == self.grouped_bids().last_key_value().map(|(p, _)| *p) =>
                        {
                            best_bid_y = Some(visible_row.y);
                        }
                        _ => {}
                    }

                    match visible_row.row {
                        DomRow::Ask { price, qty } => {
                            self.draw_row(
                                frame,
                                visible_row.y,
                                price,
                                qty,
                                false,
                                ask_color,
                                text_color,
                                maxima.vis_max_order_qty,
                                visible_row.buy_t,
                                visible_row.sell_t,
                                maxima.vis_max_trade_qty,
                                bid_color,
                                ask_color,
                                &cols,
                            );
                        }
                        DomRow::Bid { price, qty } => {
                            self.draw_row(
                                frame,
                                visible_row.y,
                                price,
                                qty,
                                true,
                                bid_color,
                                text_color,
                                maxima.vis_max_order_qty,
                                visible_row.buy_t,
                                visible_row.sell_t,
                                maxima.vis_max_trade_qty,
                                bid_color,
                                ask_color,
                                &cols,
                            );
                        }
                        DomRow::Spread => {
                            if let Some(spread) = self.raw_price_spread {
                                let min_ticksize = self.ticker_info.min_ticksize;
                                spread_row = Some((visible_row.y, visible_row.y + ROW_HEIGHT));

                                let spread = spread.round_to_min_tick(min_ticksize);
                                let content = format!("Spread: {}", spread.to_string(min_ticksize));
                                frame.fill_text(Text {
                                    content,
                                    position: Point::new(
                                        bounds.width / 2.0,
                                        visible_row.y + ROW_HEIGHT / 2.0,
                                    ),
                                    color: palette.secondary.strong.color,
                                    size: style::text_size::TINY.into(),
                                    font: style::AZERET_MONO,
                                    align_x: Alignment::Center.into(),
                                    align_y: Alignment::Center.into(),
                                    ..Default::default()
                                });
                            }
                        }
                        DomRow::CenterDivider => {
                            let y_mid = visible_row.y + ROW_HEIGHT / 2.0 - 0.5;

                            frame.fill_rectangle(
                                Point::new(0.0, y_mid),
                                Size::new(bounds.width, 1.0),
                                divider_color,
                            );
                        }
                    }
                }

                if self.config.show_chase_tracker {
                    let left_gap_mid_x = cols.sell.1 + (layout.inside_pad_px + COL_PADDING) * 0.5;
                    let right_gap_mid_x = cols.buy.0 - (layout.inside_pad_px + COL_PADDING) * 0.5;

                    self.draw_chase_trail(
                        frame,
                        &grid,
                        bounds,
                        self.chase_tracker(Side::Bid),
                        right_gap_mid_x,
                        best_ask_y.map(|y| y + ROW_HEIGHT / 2.0),
                        palette.success.weak.color,
                        true, // is_bid
                    );
                    self.draw_chase_trail(
                        frame,
                        &grid,
                        bounds,
                        self.chase_tracker(Side::Ask),
                        left_gap_mid_x,
                        best_bid_y.map(|y| y + ROW_HEIGHT / 2.0),
                        palette.danger.weak.color,
                        false,
                    );
                }

                // Price column vertical dividers with a gap over the spread row (if visible)
                let mut draw_vsplit = |x: f32, gap: Option<(f32, f32)>| {
                    let x = x.floor() + 0.5;
                    match gap {
                        Some((top, bottom)) => {
                            if top > 0.0 {
                                frame.fill_rectangle(
                                    Point::new(x, 0.0),
                                    Size::new(1.0, top.max(0.0)),
                                    divider_color,
                                );
                            }
                            if bottom < bounds.height {
                                frame.fill_rectangle(
                                    Point::new(x, bottom),
                                    Size::new(1.0, (bounds.height - bottom).max(0.0)),
                                    divider_color,
                                );
                            }
                        }
                        None => {
                            frame.fill_rectangle(
                                Point::new(x, 0.0),
                                Size::new(1.0, bounds.height),
                                divider_color,
                            );
                        }
                    }
                };
                draw_vsplit(cols.sell.1, spread_row);
                draw_vsplit(cols.buy.0, spread_row);

                if let Some((top, bottom)) = spread_row {
                    let y_top: f32 = top.floor() + 0.5;
                    let y_bot = bottom.floor() + 0.5;

                    frame.fill_rectangle(
                        Point::new(0.0, y_top),
                        Size::new(cols.sell.1, 1.0),
                        divider_color,
                    );
                    frame.fill_rectangle(
                        Point::new(0.0, y_bot),
                        Size::new(cols.sell.1, 1.0),
                        divider_color,
                    );

                    frame.fill_rectangle(
                        Point::new(cols.buy.0, y_top),
                        Size::new(bounds.width - cols.buy.0, 1.0),
                        divider_color,
                    );
                    frame.fill_rectangle(
                        Point::new(cols.buy.0, y_bot),
                        Size::new(bounds.width - cols.buy.0, 1.0),
                        divider_color,
                    );
                }
            }
        });

        vec![orderbook_visual]
    }
}

#[derive(Default)]
struct Maxima {
    vis_max_order_qty: f32,
    vis_max_trade_qty: f32,
}

struct VisibleRow {
    row: DomRow,
    y: f32,
    buy_t: Qty,
    sell_t: Qty,
}

struct ColumnRanges {
    bid_order: (f32, f32),
    sell: (f32, f32),
    price: (f32, f32),
    buy: (f32, f32),
    ask_order: (f32, f32),
}

struct PriceLayout {
    price_px: f32,
    inside_pad_px: f32,
}

impl Ladder {
    // [BidOrderQty][SellQty][ Price ][BuyQty][AskOrderQty]
    const NUMBER_OF_COLUMN_GAPS: f32 = 4.0;

    fn price_sample_text(&self, grid: &PriceGrid) -> String {
        let a = self.format_price(grid.best_ask);
        let b = self.format_price(grid.best_bid);
        if a.len() >= b.len() { a } else { b }
    }

    fn mono_text_width_px(text_len: usize) -> f32 {
        (text_len as f32) * TEXT_SIZE * MONO_CHAR_ADVANCE
    }

    fn price_layout_for(&self, total_width: f32, grid: &PriceGrid) -> PriceLayout {
        let sample = self.price_sample_text(grid);
        let text_px = Self::mono_text_width_px(sample.len());

        let desired_total_gap = CHASE_CIRCLE_RADIUS * 2.0 + 4.0;
        let inside_pad_px = PRICE_TEXT_SIDE_PAD_MIN
            .max(desired_total_gap - COL_PADDING)
            .max(0.0);

        let price_px = (text_px + 2.0 * inside_pad_px).min(total_width.max(0.0));

        PriceLayout {
            price_px,
            inside_pad_px,
        }
    }

    fn column_ranges(&self, width: f32, price_px: f32) -> ColumnRanges {
        let total_gutter_width = COL_PADDING * Self::NUMBER_OF_COLUMN_GAPS;
        let usable_width = (width - total_gutter_width).max(0.0);

        let price_width = price_px.min(usable_width);

        let rest = (usable_width - price_width).max(0.0);
        let rest_ratio = ORDER_QTY_COLS_WIDTH + TRADE_QTY_COLS_WIDTH; // 0.80

        let order_share = if rest_ratio > 0.0 {
            (ORDER_QTY_COLS_WIDTH / rest_ratio) * rest
        } else {
            0.0
        };
        let trade_share = if rest_ratio > 0.0 {
            (TRADE_QTY_COLS_WIDTH / rest_ratio) * rest
        } else {
            0.0
        };

        let bid_order_width = order_share * 0.5;
        let sell_trades_width = trade_share * 0.5;
        let buy_trades_width = trade_share * 0.5;
        let ask_order_width = order_share * 0.5;

        let mut cursor_x = 0.0;

        let bid_order_end = cursor_x + bid_order_width;
        let bid_order_range = (cursor_x, bid_order_end);
        cursor_x = bid_order_end + COL_PADDING;

        let sell_trades_end = cursor_x + sell_trades_width;
        let sell_trades_range = (cursor_x, sell_trades_end);
        cursor_x = sell_trades_end + COL_PADDING;

        let price_end = cursor_x + price_width;
        let price_range = (cursor_x, price_end);
        cursor_x = price_end + COL_PADDING;

        let buy_trades_end = cursor_x + buy_trades_width;
        let buy_trades_range = (cursor_x, buy_trades_end);
        cursor_x = buy_trades_end + COL_PADDING;

        let ask_order_end = cursor_x + ask_order_width;
        let ask_order_range = (cursor_x, ask_order_end);

        ColumnRanges {
            bid_order: bid_order_range,
            sell: sell_trades_range,
            price: price_range,
            buy: buy_trades_range,
            ask_order: ask_order_range,
        }
    }

    fn draw_row(
        &self,
        frame: &mut iced::widget::canvas::Frame,
        y: f32,
        price: Price,
        order_qty: Qty,
        is_bid: bool,
        side_color: iced::Color,
        text_color: iced::Color,
        max_order_qty: f32,
        trade_buy_qty: Qty,
        trade_sell_qty: Qty,
        max_trade_qty: f32,
        trade_buy_color: iced::Color,
        trade_sell_color: iced::Color,
        cols: &ColumnRanges,
    ) {
        let order_qty_f32 = order_qty.to_f32_lossy();
        let trade_buy_qty_f32 = trade_buy_qty.to_f32_lossy();
        let trade_sell_qty_f32 = trade_sell_qty.to_f32_lossy();

        if is_bid {
            Self::fill_bar(
                frame,
                cols.bid_order,
                y,
                ROW_HEIGHT,
                order_qty_f32,
                max_order_qty,
                side_color,
                true,
                0.20,
            );
            let qty_txt = self.format_quantity(order_qty);
            let x_text = cols.bid_order.0 + 6.0;
            Self::draw_cell_text(frame, &qty_txt, x_text, y, text_color, Alignment::Start);
        } else {
            Self::fill_bar(
                frame,
                cols.ask_order,
                y,
                ROW_HEIGHT,
                order_qty_f32,
                max_order_qty,
                side_color,
                false,
                0.20,
            );
            let qty_txt = self.format_quantity(order_qty);
            let x_text = cols.ask_order.1 - 6.0;
            Self::draw_cell_text(frame, &qty_txt, x_text, y, text_color, Alignment::End);
        }

        // Sell trades (right-to-left)
        Self::fill_bar(
            frame,
            cols.sell,
            y,
            ROW_HEIGHT,
            trade_sell_qty_f32,
            max_trade_qty,
            trade_sell_color,
            false,
            0.30,
        );
        let sell_txt = if trade_sell_qty_f32 > 0.0 {
            self.format_quantity(trade_sell_qty)
        } else {
            "".into()
        };
        Self::draw_cell_text(
            frame,
            &sell_txt,
            cols.sell.1 - 6.0,
            y,
            text_color,
            Alignment::End,
        );

        // Buy trades (left-to-right)
        Self::fill_bar(
            frame,
            cols.buy,
            y,
            ROW_HEIGHT,
            trade_buy_qty_f32,
            max_trade_qty,
            trade_buy_color,
            true,
            0.30,
        );
        let buy_txt = if trade_buy_qty_f32 > 0.0 {
            self.format_quantity(trade_buy_qty)
        } else {
            "".into()
        };
        Self::draw_cell_text(
            frame,
            &buy_txt,
            cols.buy.0 + 6.0,
            y,
            text_color,
            Alignment::Start,
        );

        // Price
        let price_text = self.format_price(price);
        let price_x_center = (cols.price.0 + cols.price.1) * 0.5;
        Self::draw_cell_text(
            frame,
            &price_text,
            price_x_center,
            y,
            side_color,
            Alignment::Center,
        );
    }

    fn fill_bar(
        frame: &mut iced::widget::canvas::Frame,
        (x_start, x_end): (f32, f32),
        y: f32,
        height: f32,
        value: f32,
        scale_value_max: f32,
        color: iced::Color,
        from_left: bool,
        alpha: f32,
    ) {
        if scale_value_max <= 0.0 || value <= 0.0 {
            return;
        }
        let col_width = x_end - x_start;

        let mut bar_width = (value / scale_value_max) * col_width.max(1.0);
        bar_width = bar_width.min(col_width);
        let bar_x = if from_left {
            x_start
        } else {
            x_end - bar_width
        };

        frame.fill_rectangle(
            Point::new(bar_x, y),
            Size::new(bar_width, height),
            iced::Color { a: alpha, ..color },
        );
    }

    fn draw_cell_text(
        frame: &mut iced::widget::canvas::Frame,
        text: &str,
        x_anchor: f32,
        y: f32,
        color: iced::Color,
        align: Alignment,
    ) {
        frame.fill_text(Text {
            content: text.to_string(),
            position: Point::new(x_anchor, y + ROW_HEIGHT / 2.0),
            color,
            size: TEXT_SIZE.into(),
            font: style::AZERET_MONO,
            align_x: align.into(),
            align_y: Alignment::Center.into(),
            ..Default::default()
        });
    }

    fn draw_chase_trail(
        &self,
        frame: &mut iced::widget::canvas::Frame,
        grid: &PriceGrid,
        bounds: Rectangle,
        tracker: &ChaseTracker,
        pos_x: f32,
        best_offer_y: Option<f32>,
        color: iced::Color,
        is_bid: bool,
    ) {
        let radius = CHASE_CIRCLE_RADIUS;
        if let Some((start_p_raw, end_p_raw, alpha)) = tracker.segment() {
            let start_p = start_p_raw.round_to_side_step(is_bid, grid.tick);
            let end_p = end_p_raw.round_to_side_step(is_bid, grid.tick);

            let color = color.scale_alpha(alpha);
            let stroke_w = 2.0;
            let pad_to_circle = radius + stroke_w * 0.5;

            let start_y = self.price_to_screen_y(start_p, grid, bounds.height);
            let end_y = self
                .price_to_screen_y(end_p, grid, bounds.height)
                .or(best_offer_y);

            if let Some(end_y) = end_y {
                if let Some(start_y) = start_y {
                    let dy = end_y - start_y;
                    if dy.abs() > pad_to_circle {
                        let line_end_y = end_y - dy.signum() * pad_to_circle;
                        let line_path =
                            Path::line(Point::new(pos_x, start_y), Point::new(pos_x, line_end_y));
                        frame.stroke(
                            &line_path,
                            Stroke::default().with_color(color).with_width(stroke_w),
                        );
                    }
                }

                let circle = &Path::circle(Point::new(pos_x, end_y), radius);
                frame.fill(circle, color);
            }
        }
    }

    fn build_price_grid(&self) -> Option<PriceGrid> {
        let best_bid = match (self.best_price(Side::Bid), self.best_price(Side::Ask)) {
            (Some(bb), _) => bb,
            (None, Some(ba)) => ba.checked_add_steps(-1, self.step)?,
            (None, None) => {
                let (min_t, max_t) = self.trades.price_range()?;
                let steps = Price::steps_between_inclusive(min_t, max_t, self.step).unwrap_or(1);
                max_t.checked_add_steps(-(steps as i64 / 2), self.step)?
            }
        };
        let best_ask = best_bid.checked_add_steps(1, self.step)?;

        Some(PriceGrid {
            best_bid,
            best_ask,
            tick: self.step,
        })
    }

    fn visible_rows(&self, bounds: Rectangle, grid: &PriceGrid) -> (Vec<VisibleRow>, Maxima) {
        let asks_grouped = self.grouped_asks();
        let bids_grouped = self.grouped_bids();

        let mut visible: Vec<VisibleRow> = Vec::new();
        let mut maxima = Maxima::default();

        let mid_screen_y = bounds.height * 0.5;
        let scroll = self.scroll_px;

        let y0 = mid_screen_y + PriceGrid::top_y(0) - scroll;
        let idx_top = ((0.0 - y0) / ROW_HEIGHT).floor() as i32;

        let rows_needed = (bounds.height / ROW_HEIGHT).ceil() as i32 + 1;
        let idx_bottom = idx_top + rows_needed;

        for idx in idx_top..=idx_bottom {
            if idx == 0 {
                let top_y_screen = mid_screen_y + PriceGrid::top_y(0) - scroll;
                if top_y_screen < bounds.height && top_y_screen + ROW_HEIGHT > 0.0 {
                    let row = if self.config.show_spread
                        && self.ticker_info.exchange().is_depth_client_aggr()
                    {
                        DomRow::Spread
                    } else {
                        DomRow::CenterDivider
                    };

                    visible.push(VisibleRow {
                        row,
                        y: top_y_screen,
                        buy_t: Qty::default(),
                        sell_t: Qty::default(),
                    });
                }
                continue;
            }

            let Some(price) = grid.index_to_price(idx) else {
                continue;
            };

            let is_bid = idx > 0;
            let order_qty = if is_bid {
                bids_grouped.get(&price).copied().unwrap_or_default()
            } else {
                asks_grouped.get(&price).copied().unwrap_or_default()
            };

            let top_y_screen = mid_screen_y + PriceGrid::top_y(idx) - scroll;
            if top_y_screen >= bounds.height || top_y_screen + ROW_HEIGHT <= 0.0 {
                continue;
            }

            maxima.vis_max_order_qty = maxima.vis_max_order_qty.max(order_qty.to_f32_lossy());
            let (buy_t, sell_t) = self.trade_qty_at(price);
            maxima.vis_max_trade_qty = maxima
                .vis_max_trade_qty
                .max(buy_t.to_f32_lossy().max(sell_t.to_f32_lossy()));

            let row = if is_bid {
                DomRow::Bid {
                    price,
                    qty: order_qty,
                }
            } else {
                DomRow::Ask {
                    price,
                    qty: order_qty,
                }
            };

            visible.push(VisibleRow {
                row,
                y: top_y_screen,
                buy_t,
                sell_t,
            });
        }

        visible.sort_by(|a, b| a.y.total_cmp(&b.y));
        (visible, maxima)
    }

    fn price_to_screen_y(&self, price: Price, grid: &PriceGrid, bounds_height: f32) -> Option<f32> {
        let mid_screen_y = bounds_height * 0.5;
        let scroll = self.scroll_px;

        let idx = if price >= grid.best_ask {
            let steps = Price::steps_between_inclusive(grid.best_ask, price, grid.tick)?;
            -(steps as i32)
        } else if price <= grid.best_bid {
            let steps = Price::steps_between_inclusive(price, grid.best_bid, grid.tick)?;
            steps as i32
        } else {
            return Some(mid_screen_y - scroll);
        };

        let y = mid_screen_y + PriceGrid::top_y(idx) - scroll + ROW_HEIGHT / 2.0;
        Some(y)
    }
}

enum DomRow {
    Ask { price: Price, qty: Qty },
    Spread,
    CenterDivider,
    Bid { price: Price, qty: Qty },
}

struct PriceGrid {
    best_bid: Price,
    best_ask: Price,
    tick: PriceStep,
}

impl PriceGrid {
    /// Returns None for index 0 (spread row)
    fn index_to_price(&self, idx: i32) -> Option<Price> {
        if idx == 0 {
            return None;
        }
        if idx > 0 {
            let off = (idx - 1) as i64; // 1 => best_bid, 2 => best_bid - 1 tick
            self.best_bid.checked_add_steps(-off, self.tick)
        } else {
            let off = (-1 - idx) as i64; // -1 => best_ask, -2 => best_ask + 1 tick
            self.best_ask.checked_add_steps(off, self.tick)
        }
    }

    fn top_y(idx: i32) -> f32 {
        (idx as f32) * ROW_HEIGHT - ROW_HEIGHT * 0.5
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced::keyboard::Modifiers;

    fn grid(best_bid: f32) -> PriceGrid {
        let step = PriceStep {
            units: Price::from_f32(0.5).units,
        };
        let best_bid = Price::from_f32(best_bid).round_to_step(step);
        PriceGrid {
            best_bid,
            best_ask: best_bid.checked_add_steps(1, step).unwrap(),
            tick: step,
        }
    }

    fn price(v: f32) -> Price {
        Price::from_f32(v)
    }

    /// Rows are 16 px; with a 200 px canvas the middle is 100, row 0 spans 92..108.
    #[test]
    fn a_row_is_found_from_its_y_with_the_scroll() {
        let g = grid(100.0);
        // Row 0 (the spread) has no price.
        assert_eq!(row_at(100.0, 0.0, 200.0, &g), None);
        assert_eq!(row_at(92.5, 0.0, 200.0, &g), None);
        // Just below it: row 1 is the best bid; just above: row -1 is the best ask.
        assert_eq!(
            row_at(108.5, 0.0, 200.0, &g),
            Some((Side::Bid, price(100.0)))
        );
        assert_eq!(
            row_at(91.5, 0.0, 200.0, &g),
            Some((Side::Ask, price(100.5)))
        );
        // One row further out.
        assert_eq!(
            row_at(124.5, 0.0, 200.0, &g),
            Some((Side::Bid, price(99.5)))
        );
        assert_eq!(
            row_at(75.5, 0.0, 200.0, &g),
            Some((Side::Ask, price(101.0)))
        );
        // Scrolled down by one row: the content moves up, so the same y is one row deeper.
        assert_eq!(
            row_at(108.5, ROW_HEIGHT, 200.0, &g),
            Some((Side::Bid, price(99.5)))
        );
        // Scrolled so the spread row sits where row 1 was.
        assert_eq!(row_at(108.5, -ROW_HEIGHT, 200.0, &g), None);
        // Outside the canvas.
        assert_eq!(row_at(-1.0, 0.0, 200.0, &g), None);
        assert_eq!(row_at(200.0, 0.0, 200.0, &g), None);
    }

    /// The row follows the best price: the same y gives the new price after the book moves.
    #[test]
    fn a_moved_best_price_moves_the_row_price() {
        assert_eq!(
            row_at(108.5, 0.0, 200.0, &grid(100.0)),
            Some((Side::Bid, price(100.0)))
        );
        assert_eq!(
            row_at(108.5, 0.0, 200.0, &grid(100.5)),
            Some((Side::Bid, price(100.5)))
        );
    }

    /// Command (Ctrl) + left asks for an order; a plain click, Shift and the other buttons never
    /// do.
    #[test]
    fn a_click_needs_a_fresh_book_with_both_sides() {
        let fresh = Some(Duration::from_secs(1));
        assert!(depth_is_usable(true, true, fresh));
        assert!(depth_is_usable(true, true, Some(DEPTH_MAX_AGE)));
        assert!(!depth_is_usable(false, false, fresh));
        assert!(!depth_is_usable(true, false, fresh));
        assert!(!depth_is_usable(false, true, fresh));
        assert!(!depth_is_usable(
            true,
            true,
            Some(DEPTH_MAX_AGE + Duration::from_millis(1))
        ));
        assert!(!depth_is_usable(true, true, None));
    }

    #[test]
    fn only_command_and_left_ask_for_an_order() {
        let row = Some((Side::Bid, price(100.0)));
        let is_order = |m: Option<Message>| matches!(m, Some(Message::PriceClicked(_)));
        assert!(is_order(press_message(
            mouse::Button::Left,
            Modifiers::COMMAND,
            row
        )));
        assert!(!is_order(press_message(
            mouse::Button::Left,
            Modifiers::empty(),
            row
        )));
        assert!(!is_order(press_message(
            mouse::Button::Left,
            Modifiers::SHIFT,
            row
        )));
        assert!(!is_order(press_message(
            mouse::Button::Right,
            Modifiers::COMMAND,
            row
        )));
        assert!(!is_order(press_message(
            mouse::Button::Middle,
            Modifiers::COMMAND,
            row
        )));
        // A plain click keeps its old effect.
        assert!(matches!(
            press_message(mouse::Button::Left, Modifiers::empty(), row),
            Some(Message::ResetScroll)
        ));
        // Command on a row with no price does nothing at all.
        assert!(press_message(mouse::Button::Left, Modifiers::COMMAND, None).is_none());
    }
}
