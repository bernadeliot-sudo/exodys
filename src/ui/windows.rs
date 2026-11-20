use nsh_core::element::input;

use super::*;
use crate::catalog::Catalog;

pub(super) const SIDEBAR: AdaptiveColor = AdaptiveColor::rgb(0xf3f5f7, 0x27292e);
const ACCENT: AdaptiveColor = AdaptiveColor::rgb(0x005fb8, 0x84c6ff);
const ACCENT_INK: AdaptiveColor = AdaptiveColor::rgb(0xffffff, 0x041b30);
const HOVER: AdaptiveColor = AdaptiveColor::rgb(0xe8eef5, 0x373e48);
const SELECTED: AdaptiveColor = AdaptiveColor::rgb(0xdbeafe, 0x243f59);

pub(super) fn button<T: Into<String>>(
    label: impl Fn() -> T + 'static,
    disabled: impl Fn() -> bool + 'static,
    tone: Tone,
    press: impl Fn() + 'static,
) -> View {
    let (background, foreground, border) = match tone {
        Tone::Primary => (ACCENT, ACCENT_INK, ACCENT),
        Tone::Standard => (SURFACE, INK, LINE),
        Tone::Danger => (SURFACE, ERROR, LINE),
    };
    press_target(disabled, press)
        .justify_center()
        .items_center()
        .px(px(8.0))
        .min_h(px(28.0))
        .rounded(px(6.0))
        .bg(background)
        .border_1()
        .border_color(border)
        .hover(|style| style.opacity(0.86))
        .child(
            text(move || label().into())
                .text_size(px(12.0))
                .text_color(foreground),
        )
}

pub(super) fn icon_button(
    label: &'static str,
    symbol: &'static str,
    disabled: impl Fn() -> bool + 'static,
    press: impl Fn() + 'static,
) -> View {
    let fallback = match symbol {
        "plus" => "导入",
        "gearshape" => "设置",
        "waveform.path.ecg" => "测试",
        "doc.on.doc" => "复制",
        _ => label,
    };
    button(move || fallback, disabled, Tone::Standard, press)
        .h(px(30.0))
        .flex_shrink_0()
        .label(label)
}

fn press_target(disabled: impl Fn() -> bool + 'static, press: impl Fn() + 'static) -> View {
    let disabled = computed(disabled);
    view()
        .role(AccessibilityRole::Button)
        .disabled(disabled)
        .focusable(move || !disabled.get())
        .cursor_pointer()
        .focus_visible(|style| style.border_color(ACCENT))
        .disabled_style(|style| style.opacity(0.54))
        .on_press(move || {
            if !disabled.get_untracked() {
                press();
            }
        })
}

fn field(label: &'static str, id: &'static str, value: Signal<String>) -> Input {
    input()
        .key(id)
        .test_id(id)
        .label(label)
        .placeholder(label)
        .value(value)
        .on_change(move |text: &str| value.set(text.to_owned()))
        .px(px(8.0))
        .py(px(4.0))
        .text_size(px(13.0))
        .text_color(INK)
        .bg(SURFACE)
        .rounded(px(6.0))
        .border_1()
        .border_color(LINE)
        .focus_visible(|style| style.border_color(ACCENT))
}

pub(super) fn editor(
    component: &'static str,
    value: Signal<String>,
    label: &'static str,
    id: &'static str,
    submit: impl Fn() + 'static,
    options: EditorOptions,
) -> Input {
    let mut input = field(label, id, value)
        .multiline(component == "exodys/text-area")
        .auto_focus(options.auto_focus)
        .on_submit(move |text: &str| {
            if !options.readonly.is_some_and(|read| read.get_untracked()) {
                value.set(text.to_owned());
                submit();
            }
        });
    if let Some(limit) = options.max_length {
        input = input.max_length(limit);
    }
    if let Some(readonly) = options.readonly {
        input = input.readonly(readonly);
    }
    input
}

pub(super) fn search_field(ui: Ui) -> Input {
    field("搜索节点名称、协议或地址", "node-search", ui.filter)
}

fn list_row(
    selected: impl Fn() -> bool + 'static,
    disabled: impl Fn() -> bool + 'static,
    press: impl Fn() + 'static,
) -> View {
    let selected = computed(selected);
    press_target(disabled, press)
        .accessibility_state(move || AccessibilityState {
            selected: selected.get(),
            ..Default::default()
        })
        .bg(move || {
            if selected.get() {
                SELECTED
            } else {
                AdaptiveColor::TRANSPARENT
            }
        })
        .hover(|style| style.bg(HOVER))
        .border_1()
        .border_color(AdaptiveColor::TRANSPARENT)
        .rounded(px(6.0))
        .h(px(44.0))
        .px(px(8.0))
        .justify_center()
}

pub(super) fn profile_list(ui: Ui, profiles: Memo<Arc<Catalog>>) -> VirtualList {
    virtual_list(
        move || profiles.with(|c| c.profiles.len()),
        move |at| {
            let profile = computed(move || profiles.with(|c| c.profiles.get(at.index()).cloned()));
            list_row(
                move || profile.with(|p| p.as_ref().is_some_and(|p| Some(p.id) == ui.focus.get())),
                move || ui.busy.get().is_some(),
                move || {
                    if let Some(id) = profile.with_untracked(|p| p.as_ref().map(|p| p.id)) {
                        ui.navigate(Some(id));
                    }
                },
            )
            .child(
                text(move || {
                    profile.with(|p| p.as_ref().map_or_else(String::new, |p| p.name.clone()))
                })
                .text_size(px(13.0))
                .font_weight(FontWeight::MEDIUM)
                .truncate(),
            )
            .child(
                text(move || {
                    profile.with(|p| {
                        p.as_ref()
                            .map_or_else(String::new, |p| format!("{} 个节点", p.nodes.len()))
                    })
                })
                .text_size(px(11.0))
                .text_color(SUBTLE)
                .truncate(),
            )
        },
    )
    .keys(move || profiles.with(|c| c.profiles.iter().map(|p| p.id).collect::<Vec<_>>()))
    .test_id("profile-table")
    .flex_1()
    .min_h(px(0.0))
}

#[derive(PartialEq)]
struct NodeRow {
    id: NodeRef,
    title: String,
    endpoint: String,
    protocol: String,
}

pub(super) fn node_list(
    ui: Ui,
    profile: Memo<Option<Arc<Profile>>>,
    matches: Memo<Matches>,
    selected: Memo<Option<NodeRef>>,
    disabled: impl Fn() -> bool + Copy + 'static,
) -> VirtualList {
    virtual_list(
        move || matches.with(Matches::len),
        move |at| {
            let row = computed(move || {
                profile.with(|p| {
                    let p = p.as_ref()?;
                    let index = matches.with(|m| m.at(at.index()))?;
                    let node = p.nodes.get(index)?;
                    Some(NodeRow {
                        id: NodeRef {
                            profile: p.id,
                            node: node.id.clone(),
                        },
                        title: node.name.clone(),
                        endpoint: node.endpoint(),
                        protocol: node.protocol().to_owned(),
                    })
                })
            });
            list_row(
                move || {
                    row.with(|row| {
                        row.as_ref().is_some_and(|row| {
                            selected.with(|selected| selected.as_ref() == Some(&row.id))
                        })
                    })
                },
                disabled,
                move || {
                    if let Some(id) =
                        row.with_untracked(|row| row.as_ref().map(|row| row.id.clone()))
                    {
                        ui.work("应用节点选择…", Job::Select(id), false);
                    }
                },
            )
            .child(
                view()
                    .flex_row()
                    .items_center()
                    .gap(px(12.0))
                    .child(
                        view()
                            .flex_1()
                            .min_w(px(0.0))
                            .child(
                                text(move || {
                                    row.with(|row| {
                                        row.as_ref()
                                            .map_or_else(String::new, |row| row.title.clone())
                                    })
                                })
                                .text_size(px(13.0))
                                .font_weight(FontWeight::MEDIUM)
                                .truncate(),
                            )
                            .child(
                                text(move || {
                                    row.with(|row| {
                                        row.as_ref()
                                            .map_or_else(String::new, |row| row.endpoint.clone())
                                    })
                                })
                                .text_size(px(11.0))
                                .text_color(SUBTLE)
                                .truncate(),
                            ),
                    )
                    .child(
                        text(move || {
                            row.with(|row| {
                                row.as_ref()
                                    .map_or_else(String::new, |row| row.protocol.clone())
                            })
                        })
                        .text_size(px(11.0))
                        .text_color(SUBTLE),
                    ),
            )
        },
    )
    .keys(move || {
        profile.with(|p| {
            matches.with(|m| {
                let Some(p) = p else {
                    return Vec::new();
                };
                (0..m.len())
                    .filter_map(|i| m.at(i).and_then(|i| p.nodes.get(i)))
                    .map(|node| (p.id, node.id.clone()))
                    .collect::<Vec<_>>()
            })
        })
    })
    .test_id("node-table")
    .flex_1()
    .min_h(px(0.0))
}
