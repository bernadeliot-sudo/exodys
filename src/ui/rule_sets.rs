use std::path::PathBuf;

use nsh_core::element::create_root;

use super::*;
use crate::rule_sets::Source as RuleSetSource;

#[derive(Clone, Copy, PartialEq)]
enum SourceKind {
    File,
    Url,
}

pub(super) fn page(ui: Ui) -> impl IntoElement {
    // The page can be rebuilt by the parent's dynamic region. Keep its drafts
    // and their bindings under an owner that is disposed with that region.
    let (owner, ()) = create_root(|| ());
    cleanup(move || owner.dispose());
    owner.run(move || {
        let name = signal(String::new());
        let url = signal(String::new());
        let file = signal(None::<PathBuf>);
        let kind = signal(SourceKind::File);
        let replacing = signal(false);
        let removing = signal(None::<String>);

        let submit = move |replace: bool| {
            if ui.busy.get_untracked().is_some() || ui.state.with_untracked(|s| s.connected) {
                return;
            }
            let name_value = name.get_untracked().trim().to_owned();
            if name_value.is_empty() {
                ui.error("请输入规则集名称（不含 .srs）。");
                return;
            }
            let exists = ui.state.with_untracked(|s| s.rule_sets.contains(&name_value));
            if exists != replace {
                ui.error(if replace { "该规则集不存在，请使用导入。" } else { "同名规则集已存在；请使用替换操作。" });
                return;
            }
            let source = match kind.get_untracked() {
                SourceKind::File => match file.get_untracked() {
                    Some(path) if path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("srs")) && path.is_file() => RuleSetSource::File(path),
                    _ => { ui.error("请选择本地 .srs 普通文件。"); return; }
                },
                SourceKind::Url => {
                    let value = url.get_untracked().trim().to_owned();
                    if !url::Url::parse(&value).is_ok_and(|url| url.scheme() == "https" && url.host_str().is_some()) {
                        ui.error("请输入有效的 HTTPS 地址。");
                        return;
                    }
                    RuleSetSource::Url(value)
                }
            };
            replacing.set(false);
            ui.work(if replace { "替换规则集…" } else { "导入规则集…" },
                if replace { Job::ReplaceRuleSet(name_value, source) } else { Job::ImportRuleSet(name_value, source) }, false);
        };

        view().flex_1().min_h(px(0.0)).gap(px(10.0))
            .child(text("规则集").text_size(px(20.0)).font_weight(FontWeight::SEMIBOLD))
            .child(text("导入 sing-box 二进制 .srs 并保存本地副本。已安装的规则集可以单独保存 HTTPS 定时更新来源。")
                .text_size(px(13.0)).text_color(SUBTLE))
            .child(text("分流规则示例：RULE-SET,name,DIRECT（也可使用 PROXY 或 REJECT）。GEOIP,cn,DIRECT 使用本地 geoip-cn；GEOSITE,cn,REJECT 使用本地 geosite-cn。")
                .text_size(px(13.0)).text_color(SUBTLE))
            .child(when(move || ui.state.with(|s| s.connected), || text("请先断开连接，再修改规则集。").text_color(SUBTLE)))
            .child(text("名称使用 1–48 个小写字母、数字、连字符或下划线，不含 .srs。")
                .text_size(px(13.0)).text_color(SUBTLE))
            .child(view().flex_row().items_center().gap(px(8.0))
                .child(text("名称"))
                .child(editor("exodys/text-field", name, "名称（不含 .srs）", "rule-set-name", || {}, EditorOptions { readonly: Some(computed(move || ui.busy.get().is_some() || ui.state.with(|s| s.connected))), ..EditorOptions::default() }).h(px(28.0)).flex_1()))
            .child(view().flex_row().gap(px(8.0))
                .child(native_button(move || if kind.get() == SourceKind::File { "✓ 本地 .srs 文件" } else { "本地 .srs 文件" },
                    move || ui.busy.get().is_some() || ui.state.with(|s| s.connected), Tone::Standard,
                    move || { kind.set(SourceKind::File); replacing.set(false); }))
                .child(native_button(move || if kind.get() == SourceKind::Url { "✓ HTTPS 地址" } else { "HTTPS 地址" },
                    move || ui.busy.get().is_some() || ui.state.with(|s| s.connected), Tone::Standard,
                    move || { kind.set(SourceKind::Url); replacing.set(false); }).min_w(px(115.0))))
            .child(dynamic(move || match kind.get() {
                SourceKind::File => view().flex_row().items_center().gap(px(8.0))
                    .child(action("选择 .srs 文件", Tone::Standard, move || ui.busy.get().is_some() || ui.state.with(|s| s.connected), move || {
                        if ui.busy.get_untracked().is_some() || ui.state.with_untracked(|s| s.connected) { return; }
                        ui.busy.set(Some("选择规则集文件…"));
                        let request = pick_paths(OpenDialog::files().filter(FileFilter::new("规则集", ["srs"])).button_label("选择"));
                        spawn(async move {
                            let selected = request.await;
                            ui.busy.set(None);
                            match selected {
                                Ok(Some(paths)) => {
                                    if let Some(path) = paths.into_iter().next()
                                        && !ui.state.with_untracked(|s| s.connected)
                                    {
                                        file.set(Some(path));
                                        replacing.set(false);
                                    }
                                },
                                Ok(None) => (),
                                Err(_) => ui.error("无法打开文件选择器。"),
                            }
                        });
                    }))
                    .child(text(move || file.with(|path| path.as_ref().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|| "尚未选择文件".into()))).flex_1().truncate())
                    .into_element(),
                SourceKind::Url => view().gap(px(4.0))
                    .child(editor("exodys/text-field", url, "https://example.org/geoip-cn.srs", "rule-set-url", || {}, EditorOptions { readonly: Some(computed(move || ui.busy.get().is_some() || ui.state.with(|s| s.connected))), ..EditorOptions::default() }).h(px(28.0)))
                    .child(view().flex_row().items_center().gap(px(8.0))
                        .child(text("应用打开且断开时，每 24 小时更新。失败时保留本地副本。")
                            .flex_1().min_w(px(0.0)).text_size(px(13.0)).text_color(SUBTLE))
                        .child(native_button(|| "保存定时来源", move || {
                            ui.busy.get().is_some() || ui.state.with(|s| s.connected || !s.rule_sets.iter().any(|item| name.with(|n| n.trim() == item)))
                        }, Tone::Standard, move || {
                            ui.work("保存规则集更新来源…", Job::TrackRuleSet(name.get_untracked().trim().to_owned(), Some(url.get_untracked())), false);
                        }).min_w(px(105.0)))).into_element(),
            }))
            .child(view().flex_row().gap(px(8.0))
                .child(action("导入新规则集", Tone::Primary, move || ui.busy.get().is_some() || ui.state.with(|s| s.connected), move || submit(false)))
                .child(action("替换同名规则集…", Tone::Standard, move || ui.busy.get().is_some() || ui.state.with(|s| s.connected), move || replacing.set(true))))
            .child(when(move || replacing.get(), move || view().flex_row().items_center().gap(px(8.0))
                .child(text(move || format!("确认替换本地规则集「{}」？", name.get().trim())).flex_1())
                .child(action("取消替换", Tone::Standard, move || ui.busy.get().is_some(), move || replacing.set(false)))
                .child(action("确认替换", Tone::Danger, move || ui.busy.get().is_some() || ui.state.with(|s| s.connected), move || submit(true)))))
            .child(text("本地规则集").font_weight(FontWeight::SEMIBOLD))
            .child(scroll().flex_1().min_h(px(0.0)).child(dynamic(move || {
                let state = ui.state.get();
                view().gap(px(6.0)).children(state.rule_sets.iter().map(|item| {
                    let selected = item.clone();
                    let to_remove = item.clone();
                    let removal_match = item.clone();
                    let tracked = state.preferences.rule_set_updates.contains_key(item);
                    let refresh = item.clone();
                    let detach = item.clone();
                    let label = if tracked { format!("{item} · 定时更新") } else { item.clone() };
                    view().flex_row().items_center().gap(px(8.0))
                        .child(text(label).flex_1().min_w(px(0.0)).truncate())
                        .child(when(move || tracked, move || {
                            let refresh = refresh.clone();
                            let detach = detach.clone();
                            view().flex_row().gap(px(8.0))
                                .child(action("立即更新", Tone::Standard, move || ui.busy.get().is_some() || ui.state.with(|s| s.connected), move || {
                                    ui.work("更新规则集…", Job::RefreshRuleSet(refresh.clone()), false);
                                }))
                                .child(native_button(|| "停止定时更新", move || ui.busy.get().is_some() || ui.state.with(|s| s.connected), Tone::Standard, move || {
                                    ui.work("停止规则集定时更新…", Job::TrackRuleSet(detach.clone(), None), false);
                                }).min_w(px(105.0)))
                        }))
                        .child(action("移除…", Tone::Standard, move || ui.busy.get().is_some() || ui.state.with(|s| s.connected), move || removing.set(Some(selected.clone()))))
                        .child(when(move || removing.with(|v| v.as_ref() == Some(&removal_match)), {
                            let item = to_remove.clone();
                            move || {
                                let item = item.clone();
                                view().flex_row().gap(px(8.0))
                                    .child(text("确认移除？"))
                                    .child(action("取消移除", Tone::Standard, move || ui.busy.get().is_some(), move || removing.set(None)))
                                    .child(action("确认移除", Tone::Danger, move || ui.busy.get().is_some() || ui.state.with(|s| s.connected), move || {
                                        if !ui.state.with_untracked(|s| s.connected) {
                                            ui.work("移除规则集…", Job::RemoveRuleSet(item.clone()), false);
                                            removing.set(None);
                                        }
                                    }))
                            }
                        }))
                }))
            })))
            .child(view().flex_row().justify_end().child(cancel(ui)))
    })
}
