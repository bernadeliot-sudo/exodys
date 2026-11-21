use std::path::PathBuf;

use nsh_core::element::create_root;

use crate::session::SyncConflict;

use super::*;

#[derive(Clone, Default, PartialEq)]
enum Step {
    #[default]
    Idle,
    Unlock(PathBuf),
    Resolve {
        conflict: SyncConflict,
        keep_local: bool,
    },
}

pub(super) fn page(ui: Ui) -> impl IntoElement {
    let (owner, ()) = create_root(|| ());
    cleanup(move || owner.dispose());
    owner.run(move || {
        let step = signal(Step::Idle);
        view()
            .flex_1()
            .gap(px(12.0))
            .child(text("自动同步").text_size(px(20.0)).font_weight(FontWeight::SEMIBOLD))
            .child(text("在每台 Mac 上选择同一个共享文件夹并输入相同口令。应用打开且断开代理时，会定期交换加密版本。口令在关闭应用后清除。")
                .text_size(px(13.0)).text_color(SUBTLE))
            .child(dynamic(move || {
                let status = ui.state.with(|state| state.sync.clone());
                let folder = status.directory.as_ref().map(|path| path.to_string_lossy().into_owned());
                let mut content = view().gap(px(8.0));
                if let Some(folder) = folder {
                    content = content.child(text(folder).text_size(px(13.0)).text_color(SUBTLE));
                }
                if let Some(problem) = status.problem {
                    content = content.child(text(problem).text_size(px(13.0)).text_color(ERROR));
                }
                if status.unlocked {
                    if let Some(active) = status.conflict.as_ref()
                        && let Step::Resolve { conflict, keep_local } = step.get()
                        && *active == conflict
                    {
                        return content.child(resolve(ui, step, conflict, keep_local));
                    }
                    return content.child(sync_controls(ui, step, status.conflict));
                }
                if let Step::Unlock(path) = step.get() {
                    return content.child(unlock(ui, step, path));
                }
                content.child(view().flex_row().gap(px(8.0))
                    .child(action("选择共享文件夹…", Tone::Standard, move || unavailable(ui), move || choose_folder(ui, step)))
                    .children(status.directory.into_iter().map(move |path| action(
                        "解锁自动同步", Tone::Standard, move || unavailable(ui),
                        move || step.set(Step::Unlock(path.clone())),
                    ))))
            }))
            .child(view().flex_1())
            .child(view().flex_row().justify_end().child(action("返回备份", Tone::Standard,
                move || ui.busy.get().is_some(), move || ui.page.set(Page::Backup))))
    })
}

fn unavailable(ui: Ui) -> bool {
    ui.busy.get().is_some()
        || ui.state.with(|state| state.connected)
        || ui.session.with(Option::is_none)
}

fn choose_folder(ui: Ui, step: Signal<Step>) {
    if ui.busy.get_untracked().is_some() {
        return;
    }
    ui.busy.set(Some("选择同步文件夹…"));
    let request = pick_paths(OpenDialog::folders().button_label("选择"));
    spawn(async move {
        let result = request.await;
        ui.busy.set(None);
        if ui.page.get_untracked() != Page::Sync {
            return;
        }
        match result {
            Ok(Some(paths)) => {
                if let Some(path) = paths.into_iter().next() {
                    step.set(Step::Unlock(path));
                }
            }
            Ok(None) => {}
            Err(_) => ui.error("无法打开同步文件夹选择器。"),
        }
    });
}

fn unlock(ui: Ui, step: Signal<Step>, path: PathBuf) -> impl IntoElement {
    view()
        .gap(px(8.0))
        .child(
            text(path.to_string_lossy().into_owned())
                .text_size(px(13.0))
                .text_color(SUBTLE),
        )
        .child(text("输入至少 12 个字符的同步口令，然后按回车。").text_size(px(13.0)))
        .child(
            native("exodys/secure-text-field")
                .key("sync-passphrase")
                .test_id("sync-passphrase")
                .h(px(28.0))
                .on_native_event(move |event: &NativeEvent| {
                    if &*event.name != "submit" {
                        return;
                    }
                    let Some(passphrase) = event
                        .data
                        .as_u64()
                        .and_then(crate::macos_native::take_secure_input)
                    else {
                        return;
                    };
                    ui.work(
                        "解锁自动同步…",
                        Job::StartSync(path.clone(), passphrase),
                        false,
                    );
                }),
        )
        .child(view().flex_row().child(action(
            "取消",
            Tone::Standard,
            move || ui.busy.get().is_some(),
            move || step.set(Step::Idle),
        )))
}

fn sync_controls(ui: Ui, step: Signal<Step>, conflict: Option<SyncConflict>) -> impl IntoElement {
    let mut content = view().gap(px(8.0));
    if let Some(conflict) = conflict {
        let local = conflict.clone();
        content = content
            .child(
                text(conflict.detail.clone())
                    .text_size(px(13.0))
                    .text_color(ERROR),
            )
            .child(
                view()
                    .flex_row()
                    .gap(px(8.0))
                    .child(action(
                        "保留此设备…",
                        Tone::Standard,
                        move || unavailable(ui),
                        move || {
                            step.set(Step::Resolve {
                                conflict: local.clone(),
                                keep_local: true,
                            })
                        },
                    ))
                    .child(action(
                        "采用共享版本…",
                        Tone::Danger,
                        move || unavailable(ui),
                        move || {
                            step.set(Step::Resolve {
                                conflict: conflict.clone(),
                                keep_local: false,
                            })
                        },
                    )),
            );
    }
    content.child(
        view()
            .flex_row()
            .gap(px(8.0))
            .child(action(
                "立即同步",
                Tone::Standard,
                move || unavailable(ui),
                move || ui.work("正在同步…", Job::SyncTick, false),
            ))
            .child(action(
                "暂停同步",
                Tone::Standard,
                move || ui.busy.get().is_some(),
                move || {
                    step.set(Step::Idle);
                    ui.work("暂停自动同步…", Job::PauseSync, false);
                },
            )),
    )
}

fn resolve(
    ui: Ui,
    step: Signal<Step>,
    conflict: SyncConflict,
    keep_local: bool,
) -> impl IntoElement {
    let message = if keep_local {
        "将此设备的配置发布为新的同步版本。"
    } else {
        "用共享版本替换此设备上的配置。"
    };
    view()
        .gap(px(8.0))
        .child(text(message).text_size(px(13.0)))
        .child(
            view()
                .flex_row()
                .gap(px(8.0))
                .child(action(
                    "取消",
                    Tone::Standard,
                    || false,
                    move || step.set(Step::Idle),
                ))
                .child(action(
                    "确认同步版本",
                    Tone::Danger,
                    move || unavailable(ui),
                    move || {
                        step.set(Step::Idle);
                        ui.work(
                            "保存同步版本…",
                            Job::ResolveSync {
                                conflict: conflict.clone(),
                                keep_local,
                            },
                            false,
                        );
                    },
                )),
        )
}
