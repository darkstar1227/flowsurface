//! A row of tabs, one per layout, above the dashboard.

use crate::modal::layout_manager::{LayoutManager, Message};
use crate::style::{self, Icon, icon_text};
use crate::widget::tooltip;
use crate::window::MULTI_WINDOW_SUPPORTED;

use iced::widget::scrollable::{Direction, Scrollbar};
use iced::widget::tooltip::Position as TooltipPosition;
use iced::widget::{button, container, mouse_area, row, scrollable, text};
use iced::{Alignment, Element, Length};

const SELECT_SHORTCUTS: [&str; 9] = ["⌘1", "⌘2", "⌘3", "⌘4", "⌘5", "⌘6", "⌘7", "⌘8", "⌘9"];

pub fn view(manager: &LayoutManager) -> Element<'_, Message> {
    let active = manager.active_layout_id().map(|layout| layout.unique);
    let closing = manager.closing();
    let tab_count = manager.tab_layouts().count();
    let can_close = tab_count > 1;
    let can_detach = MULTI_WINDOW_SUPPORTED && tab_count > 1;

    let mut tabs = row![].spacing(4).align_y(Alignment::Center);

    for (index, layout) in manager.tab_layouts().enumerate() {
        let id = layout.id.unique;
        let is_active = Some(id) == active;

        let label = button(text(layout.id.name.as_str()).size(style::text_size::BODY))
            .padding([2, 10])
            .style(move |theme, status| style::button::bordered_toggle(theme, status, is_active))
            .on_press_maybe((!is_active).then_some(Message::SelectActive(id)));

        let label = tooltip(
            label,
            SELECT_SHORTCUTS.get(index).copied(),
            TooltipPosition::Bottom,
        );

        let mut tab = row![label].spacing(2).align_y(Alignment::Center);

        if can_detach {
            // The label is a button and swallows presses, so the drag starts on this grip.
            let grip = mouse_area(container(icon_text(Icon::Popout, 10)).padding([2, 4]))
                .on_press(Message::TabGrabbed(id))
                .interaction(iced::mouse::Interaction::Grab);
            tab = tab.push(tooltip(
                grip,
                Some("Click, or drag out of the tab bar, to open in its own window"),
                TooltipPosition::Bottom,
            ));
        }

        if can_close {
            let close: Element<'_, Message> = if closing == Some(id) {
                button(text("Delete?").size(style::text_size::SMALL))
                    .padding([2, 6])
                    .style(|theme, status| style::button::cancel(theme, status, true))
                    .on_press(Message::RequestClose(id))
                    .into()
            } else {
                tooltip(
                    button(icon_text(Icon::Close, 10))
                        .padding([2, 4])
                        .style(|theme, status| style::button::transparent(theme, status, false))
                        .on_press(Message::RequestClose(id)),
                    Some("Delete layout (⌘W)"),
                    TooltipPosition::Bottom,
                )
            };
            tab = tab.push(close);
        }

        let tab: Element<'_, Message> = if can_detach {
            mouse_area(tab)
                .on_right_press(Message::DetachTab(id))
                .into()
        } else {
            tab.into()
        };

        tabs = tabs.push(tab);
    }

    let add = tooltip(
        button(text("+").size(style::text_size::SECTION))
            .padding([0, 8])
            .style(|theme, status| style::button::transparent(theme, status, false))
            .on_press(Message::AddLayout),
        Some("New layout (⌘T)"),
        TooltipPosition::Bottom,
    );
    tabs = tabs.push(add);

    container(
        scrollable(tabs)
            .direction(Direction::Horizontal(
                Scrollbar::new().width(4).scroller_width(4),
            ))
            .width(Length::Fill),
    )
    .padding([0, 8])
    .into()
}
