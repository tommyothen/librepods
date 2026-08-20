use crate::bluetooth::aacp::{
    AACPManager, BatteryComponent, BatteryInfo, BatteryStatus, ControlCommandIdentifiers,
    EarDetectionStatus,
};
use iced::widget::{
    Space, button, column, container, progress_bar, row, rule, text, text_input, toggler,
};
use iced::{Background, Border, Center, Color, Element, Length, Padding, Theme};
use std::collections::HashMap;
use std::sync::Arc;
use std::thread;
use tokio::runtime::Runtime;
// use crate::bluetooth::att::ATTManager;
use crate::devices::enums::{
    AirPodsNoiseControlMode, AirPodsState, DeviceData, DeviceInformation, DeviceState,
};
use crate::ui::window::Message;
use crate::utils::scramble;

// Shared tones for the flat look: hairline separators and secondary text both
// derive from the theme's text color, so every theme (including Custom) works.
pub fn hairline(theme: &Theme) -> Color {
    theme.palette().text.scale_alpha(0.10)
}

pub fn muted(theme: &Theme) -> Color {
    theme.palette().text.scale_alpha(0.55)
}

pub fn separator<'a>() -> Element<'a, Message> {
    rule::horizontal(1)
        .style(|theme: &Theme| rule::Style {
            color: hairline(theme),
            radius: 0.into(),
            fill_mode: iced::widget::rule::FillMode::Full,
            snap: true,
        })
        .into()
}

// Fixed height on purpose: a Fill-height rule would inflate its whole row.
const BAND_HEIGHT: f32 = 128.0;

fn band_divider<'a>() -> Element<'a, Message> {
    container(Space::new().width(1).height(BAND_HEIGHT))
        .style(|theme: &Theme| container::Style {
            background: Some(Background::Color(hairline(theme))),
            ..container::Style::default()
        })
        .into()
}

// Approximated gaussian blur, modeled on a CSS blur(2px) at ~12px text
// (sigma = 0.18 x font size): iced has no filter effects, so the scrambled
// stand-in is drawn as a stack of low-alpha copies, horizontally weighted the
// way small monospace text tolerates best. Per-copy alpha is tuned for
// source-over compositing (coverage = 1 - prod(1 - a), targeting ~0.7 of the
// muted text tone). The characters underneath are already fake (see
// utils::scramble); the smear is purely the visual language for "hidden".
pub fn blurred_text<'a, M: 'a>(content: String, size: f32) -> Element<'a, M> {
    let spread = (size * 0.36).round(); // ~2 sigma at sigma = 0.18 x size
    let step = spread / 3.0;
    let mut layers = iced::widget::Stack::new();
    let offsets: [(f32, f32, f32); 9] = [
        (0.0, 0.0, 0.22),
        (-step, 0.0, 0.15),
        (step, 0.0, 0.15),
        (-2.0 * step, 0.0, 0.12),
        (2.0 * step, 0.0, 0.12),
        (-spread, 0.0, 0.08),
        (spread, 0.0, 0.08),
        (0.0, -1.0, 0.10),
        (0.0, 1.0, 0.10),
    ];
    for (dx, dy, alpha) in offsets {
        layers = layers.push(
            container(text(content.clone()).size(size).style(move |theme: &Theme| {
                text::Style {
                    color: Some(theme.palette().text.scale_alpha(alpha)),
                }
            }))
            .padding(Padding {
                top: 1.0 + dy,
                bottom: 1.0 - dy,
                left: spread + dx,
                right: spread - dx,
            }),
        );
    }
    layers.into()
}

// Small uppercase section label, e.g. "NOISE CONTROL".
fn section_header<'a>(label: &'a str) -> Element<'a, Message> {
    text(label)
        .size(11)
        .style(|theme: &Theme| text::Style {
            color: Some(muted(theme)),
        })
        .into()
}

// One cell of the battery band: a large figure over a thin fill bar.
fn battery_cell<'a>(label: &'static str, info: Option<&BatteryInfo>) -> Element<'a, Message> {
    let connected = info.map(|b| b.status != BatteryStatus::Disconnected).unwrap_or(false);
    let charging = info.map(|b| b.status == BatteryStatus::Charging).unwrap_or(false);

    let number: Element<'a, Message> = if let (true, Some(b)) = (connected, info) {
        let mut cells = row![
            text(b.level.to_string()).size(40),
            text("%").size(16).style(|theme: &Theme| text::Style {
                color: Some(muted(theme)),
            }),
        ]
        .spacing(2)
        .align_y(iced::Alignment::End);
        if charging {
            cells = cells.push(
                text("charging")
                    .size(11)
                    .style(|theme: &Theme| text::Style {
                        color: Some(theme.palette().primary),
                    }),
            );
        }
        cells.into()
    } else {
        text("\u{2013}")
            .size(40)
            .style(|theme: &Theme| text::Style {
                color: Some(muted(theme)),
            })
            .into()
    };

    let bar: Element<'a, Message> = if let (true, Some(b)) = (connected, info) {
        progress_bar(0.0..=100.0, b.level as f32)
            .length(Length::Fill)
            .girth(3)
            .style(|theme: &Theme| progress_bar::Style {
                background: Background::Color(hairline(theme)),
                bar: Background::Color(theme.palette().text),
                border: Border::default(),
            })
            .into()
    } else {
        container(Space::new().width(Length::Fill).height(3))
            .style(|theme: &Theme| container::Style {
                background: Some(Background::Color(hairline(theme))),
                ..container::Style::default()
            })
            .into()
    };

    column![
        number,
        text(label).size(12).style(|theme: &Theme| text::Style {
            color: Some(muted(theme)),
        }),
        Space::new().height(10),
        bar,
    ]
    .spacing(2)
    .width(Length::Fill)
    .height(BAND_HEIGHT)
    .padding(Padding {
        top: 18.0,
        bottom: 18.0,
        left: 20.0,
        right: 20.0,
    })
    .into()
}

// One equal-width segment of the noise control selector.
fn mode_segment<'a>(
    mac: &str,
    state: &AirPodsState,
    aacp_manager: Arc<AACPManager>,
    mode: AirPodsNoiseControlMode,
) -> Element<'a, Message> {
    let selected = state.noise_control_mode.to_byte() == mode.to_byte();
    let label = match mode {
        AirPodsNoiseControlMode::Off => "Off",
        AirPodsNoiseControlMode::NoiseCancellation => "ANC",
        AirPodsNoiseControlMode::Transparency => "Transparency",
        AirPodsNoiseControlMode::Adaptive => "Adaptive",
    };

    let mac = mac.to_string();
    let state = state.clone();
    let on_press = move || {
        let aacp_manager = aacp_manager.clone();
        let mode_byte = mode.to_byte();
        run_async_in_thread(async move {
            aacp_manager
                .send_control_command(ControlCommandIdentifiers::ListeningMode, &[mode_byte])
                .await
                .expect("Failed to send Noise Control Mode command");
        });
        let mut state = state.clone();
        state.noise_control_mode = mode.clone();
        Message::StateChanged(mac.to_string(), DeviceState::AirPods(state))
    };

    button(text(label).size(13).width(Length::Fill).center())
        .padding(Padding {
            top: 11.0,
            bottom: 11.0,
            left: 0.0,
            right: 0.0,
        })
        .width(Length::Fill)
        .style(move |theme: &Theme, _status| {
            let mut style = iced::widget::button::Style::default();
            if selected {
                let pair = theme.extended_palette().primary.base;
                style.background = Some(Background::Color(pair.color));
                style.text_color = pair.text;
            } else {
                style.background = Some(Background::Color(Color::TRANSPARENT));
                style.text_color = muted(theme);
            }
            style
        })
        .on_press_with(on_press)
        .into()
}

// Flat label + switch line.
fn toggle_row<'a>(
    label: &'static str,
    value: bool,
    on_toggle: impl Fn(bool) -> Message + 'a,
) -> Element<'a, Message> {
    row![
        text(label).size(14).width(Length::Fill),
        toggler(value).on_toggle(on_toggle).spacing(0).size(20),
    ]
    .align_y(Center)
    .padding(Padding {
        top: 9.0,
        bottom: 9.0,
        left: 0.0,
        right: 0.0,
    })
    .into()
}

// Flat key/value line; the value is click-to-copy when `copy` is set.
// Sensitive values render scrambled while hidden, and clicking one reveals
// everything instead of copying masked garbage.
fn info_row<'a>(
    label: &'static str,
    value: String,
    copy: bool,
    sensitive: bool,
    hidden: bool,
) -> Element<'a, Message> {
    let masked = sensitive && hidden;
    let value_el: Element<'a, Message> = if masked {
        button(blurred_text(scramble(&value), 13.0))
            .style(|_theme: &Theme, _status| {
                let mut style = iced::widget::button::Style::default();
                style.background = Some(Background::Color(Color::TRANSPARENT));
                style
            })
            .padding(0)
            .on_press(Message::ToggleSensitive)
            .into()
    } else if copy {
        button(text(value.clone()).size(13))
            .style(|theme: &Theme, _status| {
                let mut style = iced::widget::button::Style::default();
                style.text_color = theme.palette().text;
                style.background = Some(Background::Color(Color::TRANSPARENT));
                style
            })
            .padding(0)
            .on_press(Message::CopyToClipboard(value))
            .into()
    } else {
        text(value).size(13).into()
    };

    row![
        text(label)
            .size(13)
            .width(130)
            .style(|theme: &Theme| text::Style {
                color: Some(muted(theme)),
            }),
        value_el,
    ]
    .align_y(Center)
    .padding(Padding {
        top: 5.0,
        bottom: 5.0,
        left: 0.0,
        right: 0.0,
    })
    .into()
}

fn status_line(state: &AirPodsState) -> String {
    let in_ear = state
        .ear_detection
        .iter()
        .filter(|s| **s == EarDetectionStatus::InEar)
        .count();
    let in_case = state
        .ear_detection
        .iter()
        .filter(|s| **s == EarDetectionStatus::InCase)
        .count();
    match (in_ear, in_case) {
        (2, _) => "Connected, both in ear".to_string(),
        (1, _) => "Connected, one in ear".to_string(),
        (0, n) if n > 0 => "Connected, in case".to_string(),
        _ => "Connected".to_string(),
    }
}

pub fn airpods_view<'a>(
    mac: &'a str,
    devices_list: &HashMap<String, DeviceData>,
    state: &'a AirPodsState,
    aacp_manager: Arc<AACPManager>,
    stem_control: bool,
    hide_sensitive: bool,
    // att_manager: Arc<ATTManager>
) -> iced::widget::Container<'a, Message> {
    let mac = mac.to_string();

    // Hero: the device name doubles as the rename input, MAC sits at the far edge.
    let aacp_manager_for_rename = aacp_manager.clone();
    let title = text_input("Name", &state.device_name)
        .size(24)
        .padding(0)
        .style(|theme: &Theme, _status| text_input::Style {
            background: Background::Color(Color::TRANSPARENT),
            border: Border::default(),
            icon: Default::default(),
            placeholder: muted(theme),
            value: theme.palette().text,
            selection: theme.palette().primary.scale_alpha(0.4),
        })
        .on_input({
            let mac = mac.clone();
            let state = state.clone();
            move |new_name| {
                let aacp_manager = aacp_manager_for_rename.clone();
                run_async_in_thread({
                    let new_name = new_name.clone();
                    async move {
                        aacp_manager
                            .send_rename_packet(&new_name)
                            .await
                            .expect("Failed to send rename packet");
                    }
                });
                let mut state = state.clone();
                state.device_name = new_name.clone();
                Message::StateChanged(mac.to_string(), DeviceState::AirPods(state))
            }
        });

    let mac_element: Element<'_, Message> = if hide_sensitive {
        blurred_text(scramble(&mac), 12.0)
    } else {
        text(mac.clone())
            .size(12)
            .style(|theme: &Theme| text::Style {
                color: Some(muted(theme)),
            })
            .into()
    };
    // Global privacy toggle: an eye in the top right corner of the page.
    let eye = button(
        text(if hide_sensitive { "\u{1002ED}" } else { "\u{1002EF}" }).size(15).style(
            |theme: &Theme| text::Style {
                color: Some(muted(theme)),
            },
        ),
    )
    .style(|_theme: &Theme, _status| {
        let mut style = iced::widget::button::Style::default();
        style.background = Some(Background::Color(Color::TRANSPARENT));
        style
    })
    .padding(0)
    .on_press(Message::ToggleSensitive);
    let hero = column![
        row![title, mac_element, eye].spacing(16).align_y(Center),
        Space::new().height(4),
        text(status_line(state))
            .size(12)
            .style(|theme: &Theme| text::Style {
                color: Some(muted(theme)),
            }),
        Space::new().height(20),
        separator(),
    ];

    // Battery band: one bordered strip, cells split by vertical hairlines.
    // A single Headphone component (AirPods Max) or Left/Right/Case.
    let find = |component: BatteryComponent| state.battery.iter().find(|b| b.component == component);
    let battery_cells = if let Some(headphone) = find(BatteryComponent::Headphone) {
        row![battery_cell("Headphones", Some(headphone))]
    } else {
        row![
            battery_cell("Left", find(BatteryComponent::Left)),
            band_divider(),
            battery_cell("Right", find(BatteryComponent::Right)),
            band_divider(),
            battery_cell("Case", find(BatteryComponent::Case)),
        ]
    };
    let battery_band = container(battery_cells).style(|theme: &Theme| container::Style {
        border: Border {
            width: 1.0,
            color: hairline(theme),
            radius: 0.into(),
        },
        ..container::Style::default()
    });

    // Noise control: equal-width segments inside one hairline outline.
    let mut modes = vec![
        AirPodsNoiseControlMode::Transparency,
        AirPodsNoiseControlMode::NoiseCancellation,
        AirPodsNoiseControlMode::Adaptive,
    ];
    if state.allow_off_mode {
        modes.insert(0, AirPodsNoiseControlMode::Off);
    }
    let mut segments = row![].spacing(0);
    for mode in modes {
        segments = segments.push(mode_segment(&mac, state, aacp_manager.clone(), mode));
    }
    let noise_control = container(segments).style(|theme: &Theme| container::Style {
        border: Border {
            width: 1.0,
            color: hairline(theme),
            radius: 0.into(),
        },
        ..container::Style::default()
    });

    let left_col = column![
        section_header("BATTERY"),
        Space::new().height(14),
        battery_band,
        Space::new().height(28),
        section_header("NOISE CONTROL"),
        Space::new().height(14),
        noise_control,
    ]
    .width(Length::Fill);

    // Audio toggles, flat rows. Stem press lives here too: it configures the
    // AirPods themselves, so it belongs with the device, not an app settings page.
    let audio_rows = {
        let pv_manager = aacp_manager.clone();
        let pv_mac = mac.clone();
        let pv_state = state.clone();
        let ca_manager = aacp_manager.clone();
        let ca_mac = mac.clone();
        let ca_state = state.clone();
        let off_manager = aacp_manager.clone();
        let off_mac = mac.clone();
        let off_state = state.clone();
        let stem_manager = aacp_manager.clone();
        column![
            toggle_row("Personalized volume", state.personalized_volume_enabled, move |is_enabled| {
                let aacp_manager = pv_manager.clone();
                run_async_in_thread(async move {
                    aacp_manager
                        .send_control_command(
                            ControlCommandIdentifiers::AdaptiveVolumeConfig,
                            if is_enabled { &[0x01] } else { &[0x02] },
                        )
                        .await
                        .expect("Failed to send Personalized Volume command");
                });
                let mut state = pv_state.clone();
                state.personalized_volume_enabled = is_enabled;
                Message::StateChanged(pv_mac.clone(), DeviceState::AirPods(state))
            }),
            toggle_row("Conversation awareness", state.conversation_awareness_enabled, move |is_enabled| {
                let aacp_manager = ca_manager.clone();
                run_async_in_thread(async move {
                    aacp_manager
                        .send_control_command(
                            ControlCommandIdentifiers::ConversationDetectConfig,
                            if is_enabled { &[0x01] } else { &[0x02] },
                        )
                        .await
                        .expect("Failed to send Conversation Awareness command");
                });
                let mut state = ca_state.clone();
                state.conversation_awareness_enabled = is_enabled;
                Message::StateChanged(ca_mac.clone(), DeviceState::AirPods(state))
            }),
            toggle_row("Off listening mode", state.allow_off_mode, move |is_enabled| {
                let aacp_manager = off_manager.clone();
                run_async_in_thread(async move {
                    aacp_manager
                        .send_control_command(
                            ControlCommandIdentifiers::AllowOffOption,
                            if is_enabled { &[0x01] } else { &[0x02] },
                        )
                        .await
                        .expect("Failed to send Off Listening Mode command");
                });
                let mut state = off_state.clone();
                state.allow_off_mode = is_enabled;
                Message::StateChanged(off_mac.clone(), DeviceState::AirPods(state))
            }),
            toggle_row("Stem press track control", stem_control, move |is_enabled| {
                // Bitmask: double press = 0x02, triple = 0x04. Applied live and
                // persisted so reconnects re-apply it at connect time.
                let aacp_manager = stem_manager.clone();
                run_async_in_thread(async move {
                    aacp_manager
                        .send_control_command(
                            ControlCommandIdentifiers::StemConfig,
                            if is_enabled { &[0x06] } else { &[0x00] },
                        )
                        .await
                        .expect("Failed to send Stem Config command");
                });
                Message::StemControlChanged(is_enabled)
            }),
        ]
    };

    // Device information, when the daemon has captured it.
    let mut information = column![];
    if let Some(device) = devices_list.get(mac.as_str())
        && let Some(DeviceInformation::AirPods(ref info)) = device.information
    {
        information = column![
            Space::new().height(28),
            section_header("DEVICE"),
            Space::new().height(8),
            info_row("Model", info.model_number.clone(), false, false, hide_sensitive),
            info_row("Serial", info.serial_number.clone(), true, true, hide_sensitive),
            info_row("Left serial", info.left_serial_number.clone(), true, true, hide_sensitive),
            info_row("Right serial", info.right_serial_number.clone(), true, true, hide_sensitive),
            info_row("Firmware", info.version1.clone(), false, false, hide_sensitive),
        ];
    }

    let right_col = column![
        section_header("AUDIO"),
        Space::new().height(4),
        audio_rows,
        information,
    ]
    .width(Length::Fill);

    let cols = row![left_col, right_col].spacing(44);

    let content = column![
        hero,
        Space::new().height(24),
        cols,
        Space::new().height(24),
    ]
    .width(Length::Fill);

    container(iced::widget::scrollable(content).height(Length::Fill))
        .padding(Padding {
            top: 28.0,
            bottom: 0.0,
            left: 36.0,
            right: 36.0,
        })
        .width(Length::Fill)
        .height(Length::Fill)
}

fn run_async_in_thread<F>(fut: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    thread::spawn(move || {
        let rt = Runtime::new().unwrap();
        rt.block_on(fut);
    });
}
