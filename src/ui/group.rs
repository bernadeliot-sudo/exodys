use super::*;
use crate::catalog::MAX_AUTO_MEMBERS;

fn selected_in_profile(ui: Ui, profile_id: Memo<Option<u64>>) -> Option<NodeRef> {
    ui.state.with_untracked(|s| {
        s.preferences
            .selected
            .clone()
            .filter(|node| Some(node.profile) == profile_id.get_untracked())
    })
}

pub(super) fn member_button(ui: Ui, profile_id: Memo<Option<u64>>) -> impl IntoElement {
    native_button(
        move || {
            if ui.state.with(|s| {
                s.preferences.selected.as_ref().is_some_and(|node| {
                    Some(node.profile) == profile_id.get()
                        && s.preferences.auto_members.contains(node)
                })
            }) {
                "从自动组移除所选节点"
            } else {
                "将所选节点加入自动组"
            }
        },
        move || {
            ui.busy.get().is_some()
                || ui.state.with(|s| {
                    if s.connected {
                        return true;
                    }
                    let Some(node) = s
                        .preferences
                        .selected
                        .as_ref()
                        .filter(|node| Some(node.profile) == profile_id.get())
                    else {
                        return true;
                    };
                    if s.preferences.auto_members.contains(node) {
                        s.preferences.auto_enabled && s.preferences.auto_members.len() == 1
                    } else {
                        s.preferences.auto_members.len() >= MAX_AUTO_MEMBERS
                    }
                })
        },
        Tone::Standard,
        move || {
            if let Some(node) = selected_in_profile(ui, profile_id) {
                let add = ui
                    .state
                    .with_untracked(|s| !s.preferences.auto_members.contains(&node));
                if !ui.state.with_untracked(|s| s.connected) {
                    ui.work("更新自动组…", Job::SetAutoMember(node, add), false);
                }
            }
        },
    )
    .min_h(px(24.0))
}

pub(super) fn page(ui: Ui) -> impl IntoElement {
    view().flex_1().min_h(px(0.0)).gap(px(12.0))
        .child(text("自动选择组").text_size(px(20.0)).font_weight(FontWeight::SEMIBOLD))
        .child(text(move || ui.state.with(|s| format!("{} · {}/{} 个节点", if s.preferences.auto_enabled { "自动模式" } else { "手动模式" }, s.preferences.auto_members.len(), MAX_AUTO_MEMBERS))).text_size(px(13.0)))
        .child(text("自动模式连接时会通过组内节点向 Cloudflare 发送探测请求：https://www.cloudflare.com/cdn-cgi/trace。请在启用前确认。")
            .text_size(px(13.0)).text_color(SUBTLE))
        .child(text("在各配置的节点列表中选择节点并加入自动组。仅断开连接时可修改组或切换模式；启用自动模式后，移除最后一个节点前请先切换为手动模式。")
            .text_size(px(13.0)).text_color(SUBTLE))
        .child(view().flex_row().child(native_button(
            move || if ui.state.with(|s| s.preferences.auto_enabled) { "切换到手动模式" } else { "启用自动模式" },
            move || ui.busy.get().is_some() || ui.state.with(|s| s.connected || (!s.preferences.auto_enabled && s.preferences.auto_members.is_empty())),
            Tone::Standard,
            move || {
                if !ui.state.with_untracked(|s| s.connected) {
                    ui.work("切换连接模式…", Job::SetAutoEnabled(!ui.state.with_untracked(|s| s.preferences.auto_enabled)), false);
                }
            },
        ).min_h(px(24.0))))
        .child(scroll().flex_1().min_h(px(0.0)).child(dynamic(move || {
            let rows = ui.state.with(|s| s.preferences.auto_members.iter().map(|member| {
                let name = s.catalog.profile(member.profile).map(|profile| {
                    let node = profile.node(&member.node).map(|node| node.name.as_str()).unwrap_or("节点不可用");
                    format!("{} · {}", profile.name, node)
                }).unwrap_or_else(|| format!("配置 {} · 节点不可用", member.profile));
                (member.clone(), name)
            }).collect::<Vec<_>>());
            view().gap(px(6.0)).children(rows.into_iter().map(|(member, name)| {
                view().flex_row().items_center().gap(px(8.0))
                    .child(text(name).flex_1().truncate())
                    .child(action("移除", Tone::Standard,
                        move || ui.busy.get().is_some() || ui.state.with(|s| s.connected || (s.preferences.auto_enabled && s.preferences.auto_members.len() == 1)),
                        move || {
                            if !ui.state.with_untracked(|s| s.connected) {
                                ui.work("更新自动组…", Job::SetAutoMember(member.clone(), false), false);
                            }
                        }))
            }))
        })))
        .child(view().flex_row().justify_end().child(action("返回节点", Tone::Standard, move || ui.busy.get().is_some(), move || ui.page.set(Page::Nodes))))
}
