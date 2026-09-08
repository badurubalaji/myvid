//! Icons drawn as vector paths on a 24x24 grid.
//!
//! Drawn rather than shipped as a font: an icon font would be another asset to
//! bundle, and these scale and recolour for free.

use iced::mouse;
use iced::widget::canvas::{self, Frame, Geometry, Path, Stroke};
use iced::widget::Canvas;
use iced::{Color, Element, Point, Rectangle, Renderer, Size, Theme};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Glyph {
    Play,
    Pause,
    Back10,
    Forward10,
    Volume,
    Mute,
    Fullscreen,
    ExitFullscreen,
    Folder,
    Sliders,
    Scissors,
}

#[derive(Debug)]
pub struct Icon {
    glyph: Glyph,
    color: Color,
}

/// An icon sized in logical pixels.
pub fn icon<'a, Message: 'a>(glyph: Glyph, size: f32, color: Color) -> Element<'a, Message> {
    Canvas::new(Icon { glyph, color })
        .width(size)
        .height(size)
        .into()
}

impl<Message> canvas::Program<Message> for Icon {
    type State = ();

    fn draw(
        &self,
        _state: &Self::State,
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let mut frame = Frame::new(renderer, bounds.size());
        frame.scale(bounds.width.min(bounds.height) / 24.0);

        let stroke = || {
            Stroke::default()
                .with_width(1.7)
                .with_color(self.color)
                .with_line_cap(canvas::LineCap::Round)
                .with_line_join(canvas::LineJoin::Round)
        };

        match self.glyph {
            Glyph::Play => {
                let path = Path::new(|b| {
                    b.move_to(Point::new(8.0, 4.8));
                    b.line_to(Point::new(19.0, 12.0));
                    b.line_to(Point::new(8.0, 19.2));
                    b.close();
                });
                frame.fill(&path, self.color);
            }
            Glyph::Pause => {
                for x in [6.8_f32, 13.2] {
                    let bar = Path::new(|b| {
                        b.rounded_rectangle(
                            Point::new(x, 4.8),
                            Size::new(4.0, 14.4),
                            1.0.into(),
                        );
                    });
                    frame.fill(&bar, self.color);
                }
            }
            Glyph::Back10 => {
                frame.stroke(&chevrons(-1.0), stroke());
            }
            Glyph::Forward10 => {
                frame.stroke(&chevrons(1.0), stroke());
            }
            Glyph::Volume | Glyph::Mute => {
                let speaker = Path::new(|b| {
                    b.move_to(Point::new(4.0, 9.4));
                    b.line_to(Point::new(7.4, 9.4));
                    b.line_to(Point::new(12.0, 5.4));
                    b.line_to(Point::new(12.0, 18.6));
                    b.line_to(Point::new(7.4, 14.6));
                    b.line_to(Point::new(4.0, 14.6));
                    b.close();
                });
                frame.fill(&speaker, self.color);

                if self.glyph == Glyph::Volume {
                    let waves = Path::new(|b| {
                        b.move_to(Point::new(15.4, 9.2));
                        b.quadratic_curve_to(Point::new(17.2, 12.0), Point::new(15.4, 14.8));
                        b.move_to(Point::new(18.2, 6.8));
                        b.quadratic_curve_to(Point::new(21.2, 12.0), Point::new(18.2, 17.2));
                    });
                    frame.stroke(&waves, stroke());
                } else {
                    let cross = Path::new(|b| {
                        b.move_to(Point::new(15.6, 9.4));
                        b.line_to(Point::new(20.4, 14.6));
                        b.move_to(Point::new(20.4, 9.4));
                        b.line_to(Point::new(15.6, 14.6));
                    });
                    frame.stroke(&cross, stroke());
                }
            }
            Glyph::Fullscreen => {
                frame.stroke(&corners(false), stroke());
            }
            Glyph::ExitFullscreen => {
                frame.stroke(&corners(true), stroke());
            }
            Glyph::Sliders => {
                let rails = Path::new(|b| {
                    for y in [7.0_f32, 12.0, 17.0] {
                        b.move_to(Point::new(3.5, y));
                        b.line_to(Point::new(20.5, y));
                    }
                });
                frame.stroke(&rails, stroke());
                // Knobs sit at different positions, which is what makes the
                // icon read as controls rather than a list.
                for (x, y) in [(8.0_f32, 7.0_f32), (15.0, 12.0), (10.5, 17.0)] {
                    let knob = Path::new(|b| b.circle(Point::new(x, y), 2.4));
                    frame.fill(&knob, self.color);
                }
            }
            Glyph::Scissors => {
                let blades = Path::new(|b| {
                    b.move_to(Point::new(7.4, 8.2));
                    b.line_to(Point::new(19.5, 18.6));
                    b.move_to(Point::new(19.5, 5.4));
                    b.line_to(Point::new(7.4, 15.8));
                });
                frame.stroke(&blades, stroke());
                for y in [6.0_f32, 18.0] {
                    let ring = Path::new(|b| b.circle(Point::new(5.4, y), 2.5));
                    frame.stroke(&ring, stroke());
                }
            }
            Glyph::Folder => {
                let folder = Path::new(|b| {
                    b.move_to(Point::new(3.2, 18.4));
                    b.line_to(Point::new(3.2, 6.4));
                    b.line_to(Point::new(9.2, 6.4));
                    b.line_to(Point::new(11.0, 8.8));
                    b.line_to(Point::new(20.8, 8.8));
                    b.line_to(Point::new(20.8, 18.4));
                    b.close();
                });
                frame.stroke(&folder, stroke());
            }
        }

        vec![frame.into_geometry()]
    }
}

/// Two chevrons pointing left (`dir` = -1) or right (`dir` = 1).
fn chevrons(dir: f32) -> Path {
    Path::new(|b| {
        for offset in [0.0_f32, 6.4] {
            let tip = 8.4 + offset;
            let tail = 13.0 + offset;
            let (x0, x1) = if dir > 0.0 {
                (tip, tail)
            } else {
                (24.0 - tip, 24.0 - tail)
            };
            b.move_to(Point::new(x1, 6.0));
            b.line_to(Point::new(x0, 12.0));
            b.line_to(Point::new(x1, 18.0));
        }
    })
}

/// Four corner brackets, pointing outwards to enter fullscreen and inwards to
/// leave it.
fn corners(inward: bool) -> Path {
    Path::new(|b| {
        let (near, far) = if inward { (9.0, 3.6) } else { (3.6, 9.0) };
        // top-left
        b.move_to(Point::new(near, far));
        b.line_to(Point::new(near, near));
        b.line_to(Point::new(far, near));
        // top-right
        b.move_to(Point::new(24.0 - near, far));
        b.line_to(Point::new(24.0 - near, near));
        b.line_to(Point::new(24.0 - far, near));
        // bottom-right
        b.move_to(Point::new(24.0 - near, 24.0 - far));
        b.line_to(Point::new(24.0 - near, 24.0 - near));
        b.line_to(Point::new(24.0 - far, 24.0 - near));
        // bottom-left
        b.move_to(Point::new(near, 24.0 - far));
        b.line_to(Point::new(near, 24.0 - near));
        b.line_to(Point::new(far, 24.0 - near));
    })
}
