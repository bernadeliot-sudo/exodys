use nsh_core::element::create_root;

use super::*;
use crate::catalog::{PolicyGroup, PolicyMode};

pub(super) fn page(ui: Ui) -> impl IntoElement {
    let (owner, ()) = create_root(|| ());
    cleanup(move || owner.dispose());
    owner.run(move || {
        let selected = signal(None::<String>);
        let confirming = signal(None::<String>);
        let disabled = move || ui.busy.get().is_some() || ui.state.with(|s| s.connected);
        let create = move || {
            let name = ui.draft.get_untracked().trim().to_owned();
            if !name.is_empty() && !disabled() {
                selected.set(Some(name.clone()));
                ui.work("创建策略组…", Job::CreatePolicy(name), false);
            }
        };
        view().flex_1().min_h(px(0.0)).gap(px(10.0))
            .child(text("策略组").text_size(px(20.0)).font_weight(FontWeight::SEMIBOLD))
            .child(text("在分流规则中使用 GROUP:组名指定出口。自动组通过成员向 Cloudflare 发送探测请求。").text_size(px(13.0)).text_color(SUBTLE))
            .child(view().flex_row().items_center().gap(px(8.0))
                .child(editor("exodys/text-field", ui.draft, "名称使用小写字母、数字或连字符", "policy-name", create, EditorOptions { max_length: Some(32), readonly: Some(computed(disabled)), ..EditorOptions::default() })
                    .h(px(28.0)).flex_1().min_w(px(0.0)))
                .child(action("创建策略组", Tone::Primary, disabled, create)))
            .child(scroll().flex_1().min_h(px(0.0)).child(dynamic(move || {
                let snapshot = ui.state.get();
                let groups = &snapshot.preferences.policy_groups;
                let Some(active) = selected.get().as_ref()
                    .and_then(|name| groups.iter().find(|group| &group.name == name))
                    .or_else(|| groups.first()) else {
                    return text("创建一个策略组，然后将所选节点加入其中。").text_color(SUBTLE).into_element();
                };
                let names = view().gap(px(4.0)).children(groups.iter().map(|group| {
                    let name = group.name.clone();
                    let label = if group.name == active.name { format!("✓ {}", group.name) } else { group.name.clone() };
                    native_button(move || label.clone(), move || ui.busy.get().is_some(), Tone::Standard, move || {
                        selected.set(Some(name.clone()));
                        confirming.set(None);
                    })
                }));
                view().flex_row().gap(px(12.0))
                    .child(names.w(px(125.0)))
                    .child(detail(ui, &snapshot, active, confirming, disabled).flex_1().min_w(px(0.0)))
                    .into_element()
            })))
            .child(view().flex_row().justify_end().child(action("返回节点", Tone::Standard,
                move || ui.busy.get().is_some(), move || ui.page.set(Page::Nodes))))
    })
}

fn detail(
    ui: Ui,
    snapshot: &Snapshot,
    group: &PolicyGroup,
    confirming: Signal<Option<String>>,
    disabled: impl Fn() -> bool + Copy + 'static,
) -> View {
    let current = snapshot.preferences.selected.clone();
    let can_add = current
        .as_ref()
        .is_some_and(|member| !group.members.contains(member))
        && group.members.len() < crate::catalog::MAX_AUTO_MEMBERS;
    let name = group.name.clone();
    let automatic = group.mode == PolicyMode::Automatic;
    let some_members = !group.members.is_empty();
    let mut editor = view()
        .gap(px(8.0))
        .child(
            text(format!("{} · {} 个节点", group.name, group.members.len()))
                .font_weight(FontWeight::SEMIBOLD)
                .truncate(),
        )
        .child(
            text(match &group.mode {
                PolicyMode::Automatic => "自动选择出口",
                PolicyMode::Manual(Some(_)) => "已指定手动出口",
                PolicyMode::Manual(None) => "请选择组内出口",
            })
            .text_color(SUBTLE),
        )
        .child(
            view()
                .gap(px(4.0))
                .children(group.members.iter().map(|member| {
                    let label = snapshot
                        .catalog
                        .profile(member.profile)
                        .and_then(|profile| {
                            profile
                                .node(&member.node)
                                .map(|node| format!("{} · {}", profile.name, node.name))
                        })
                        .unwrap_or_else(|| "节点已失效".into());
                    let checked = group.mode == PolicyMode::Manual(Some(member.clone()));
                    let choice = member.clone();
                    let removed = member.clone();
                    let choose_group = group.name.clone();
                    let remove_group = group.name.clone();
                    view().gap(px(4.0)).child(text(label).truncate()).child(
                        view()
                            .flex_row()
                            .gap(px(8.0))
                            .child(action(
                                if checked {
                                    "当前手动出口"
                                } else {
                                    "设为手动出口"
                                },
                                Tone::Standard,
                                move || disabled() || checked,
                                move || {
                                    ui.work(
                                        "设置组出口…",
                                        Job::SetPolicyMode(
                                            choose_group.clone(),
                                            PolicyMode::Manual(Some(choice.clone())),
                                        ),
                                        false,
                                    )
                                },
                            ))
                            .child(action(
                                "移除成员",
                                Tone::Standard,
                                disabled,
                                move || {
                                    ui.work(
                                        "移除策略组成员…",
                                        Job::SetPolicyMember(
                                            remove_group.clone(),
                                            removed.clone(),
                                            false,
                                        ),
                                        false,
                                    )
                                },
                            )),
                    )
                })),
        )
        .child(action(
            "启用组内自动选择",
            Tone::Standard,
            move || disabled() || automatic || !some_members,
            move || {
                ui.work(
                    "启用组内自动选择…",
                    Job::SetPolicyMode(name.clone(), PolicyMode::Automatic),
                    false,
                )
            },
        ));
    if let (Some(node), Some(member)) = (snapshot.node(), current) {
        let name = group.name.clone();
        let hint = format!("所选节点 {}", node.name);
        editor = editor
            .child(text(hint).text_color(SUBTLE).truncate())
            .child(action(
                "加入所选节点",
                Tone::Standard,
                move || disabled() || !can_add,
                move || {
                    ui.work(
                        "加入策略组成员…",
                        Job::SetPolicyMember(name.clone(), member.clone(), true),
                        false,
                    )
                },
            ));
    }
    let remove = group.name.clone();
    let pending = group.name.clone();
    let target = group.name.clone();
    editor
        .child(action(
            "删除策略组…",
            Tone::Standard,
            disabled,
            move || confirming.set(Some(remove.clone())),
        ))
        .child(when(
            move || confirming.with(|name| name.as_ref() == Some(&pending)),
            move || {
                let target = target.clone();
                view()
                    .flex_row()
                    .gap(px(8.0))
                    .child(action(
                        "取消删除",
                        Tone::Standard,
                        disabled,
                        move || confirming.set(None),
                    ))
                    .child(action("确认删除", Tone::Danger, disabled, move || {
                        confirming.set(None);
                        ui.work("删除策略组…", Job::RemovePolicy(target.clone()), false);
                    }))
            },
        ))
}
