//! The Modern direction's palette and widget styles, in one place.

use iced::widget::{button, container, slider};
use iced::{Background, Border, Color, Shadow, Vector};

pub const SURFACE: Color = rgb(0x0a, 0x0a, 0x09);
pub const GLASS: Color = rgba(0x13, 0x12, 0x10, 0.72);
pub const GLASS_BORDER: Color = rgba(0xff, 0xff, 0xff, 0.09);
pub const HOVER: Color = rgba(0xff, 0xff, 0xff, 0.11);

pub const TEXT: Color = rgb(0xf1, 0xee, 0xe8);
pub const MUTED: Color = rgb(0x8d, 0x87, 0x79);
pub const FAINT: Color = rgb(0x6d, 0x68, 0x5e);

pub const ACCENT: Color = rgb(0xe0, 0xa3, 0x4a);
pub const DANGER: Color = rgb(0xd6, 0x48, 0x3a);
pub const OK: Color = rgb(0x7f, 0xb0, 0x69);

pub const TRACK: Color = rgba(0xff, 0xff, 0xff, 0.15);

const fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color {
        r: r as f32 / 255.0,
        g: g as f32 / 255.0,
        b: b as f32 / 255.0,
        a: 1.0,
    }
}

const fn rgba(r: u8, g: u8, b: u8, a: f32) -> Color {
    Color {
        r: r as f32 / 255.0,
        g: g as f32 / 255.0,
        b: b as f32 / 255.0,
        a,
    }
}

/// The video ground. Also what letterbox bars are made of — the shader simply
/// does not draw there.
pub fn stage(_theme: &iced::Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(SURFACE)),
        ..container::Style::default()
    }
}

/// The floating control bar.
pub fn glass(_theme: &iced::Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(GLASS)),
        border: Border {
            color: GLASS_BORDER,
            width: 1.0,
            radius: 18.0.into(),
        },
        shadow: Shadow {
            color: rgba(0, 0, 0, 0.45),
            offset: Vector::new(0.0, 10.0),
            blur_radius: 40.0,
        },
        ..container::Style::default()
    }
}

/// Backing for subtitle text. iced has no text outline, and a dark plate is
/// more legible over a bright scene than a drop shadow would be anyway.
pub fn caption(_theme: &iced::Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(rgba(0x00, 0x00, 0x00, 0.55))),
        border: Border {
            radius: 5.0.into(),
            ..Border::default()
        },
        ..container::Style::default()
    }
}

pub fn error_chip(_theme: &iced::Theme) -> container::Style {
    container::Style {
        text_color: Some(TEXT),
        background: Some(Background::Color(rgba(0x2a, 0x14, 0x11, 0.94))),
        border: Border {
            color: rgba(0xd6, 0x48, 0x3a, 0.45),
            width: 1.0,
            radius: 9.0.into(),
        },
        ..container::Style::default()
    }
}

/// Icon buttons: invisible until hovered, so the chrome stays quiet.
pub fn ghost_button(_theme: &iced::Theme, status: button::Status) -> button::Style {
    let background = match status {
        button::Status::Hovered | button::Status::Pressed => Some(Background::Color(HOVER)),
        _ => None,
    };

    button::Style {
        background,
        text_color: TEXT,
        border: Border {
            radius: 8.0.into(),
            ..Border::default()
        },
        ..button::Style::default()
    }
}

/// The play/pause button: always visible, always the biggest target.
pub fn primary_button(_theme: &iced::Theme, status: button::Status) -> button::Style {
    let background = match status {
        button::Status::Hovered | button::Status::Pressed => rgba(0xff, 0xff, 0xff, 0.18),
        _ => rgba(0xff, 0xff, 0xff, 0.11),
    };

    button::Style {
        background: Some(Background::Color(background)),
        text_color: TEXT,
        border: Border {
            color: GLASS_BORDER,
            width: 1.0,
            radius: 21.0.into(),
        },
        ..button::Style::default()
    }
}

pub fn accent_button(_theme: &iced::Theme, status: button::Status) -> button::Style {
    let background = match status {
        button::Status::Hovered | button::Status::Pressed => Color {
            a: 0.85,
            ..ACCENT
        },
        _ => ACCENT,
    };

    button::Style {
        background: Some(Background::Color(background)),
        text_color: rgb(0x14, 0x12, 0x0f),
        border: Border {
            radius: 8.0.into(),
            ..Border::default()
        },
        ..button::Style::default()
    }
}

/// The URL field.
pub fn field(_theme: &iced::Theme, status: iced::widget::text_input::Status) -> iced::widget::text_input::Style {
    let border = match status {
        iced::widget::text_input::Status::Focused { .. } => ACCENT,
        _ => GLASS_BORDER,
    };

    iced::widget::text_input::Style {
        background: Background::Color(rgba(0xff, 0xff, 0xff, 0.06)),
        border: Border {
            color: border,
            width: 1.0,
            radius: 8.0.into(),
        },
        icon: MUTED,
        placeholder: FAINT,
        value: TEXT,
        selection: Color { a: 0.35, ..ACCENT },
    }
}

/// The scrub bar: accent behind the handle, faint track ahead of it.
pub fn scrub(_theme: &iced::Theme, status: slider::Status) -> slider::Style {
    let handle_radius = match status {
        slider::Status::Hovered | slider::Status::Dragged => 8.0,
        _ => 7.0,
    };

    slider::Style {
        rail: slider::Rail {
            backgrounds: (Background::Color(ACCENT), Background::Color(TRACK)),
            width: 7.0,
            border: Border {
                radius: 4.0.into(),
                ..Border::default()
            },
        },
        handle: slider::Handle {
            shape: slider::HandleShape::Circle {
                radius: handle_radius,
            },
            background: Background::Color(Color::WHITE),
            border_width: 0.0,
            border_color: Color::TRANSPARENT,
        },
    }
}

/// The volume slider: same shape, quieter colours.
pub fn volume(_theme: &iced::Theme, _status: slider::Status) -> slider::Style {
    slider::Style {
        rail: slider::Rail {
            backgrounds: (
                Background::Color(rgba(0xdd, 0xd7, 0xcb, 1.0)),
                Background::Color(rgba(0xff, 0xff, 0xff, 0.16)),
            ),
            width: 4.0,
            border: Border {
                radius: 2.0.into(),
                ..Border::default()
            },
        },
        handle: slider::Handle {
            shape: slider::HandleShape::Circle { radius: 5.0 },
            background: Background::Color(Color::WHITE),
            border_width: 0.0,
            border_color: Color::TRANSPARENT,
        },
    }
}
