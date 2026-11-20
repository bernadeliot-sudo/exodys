use nsh_core::file_dialog::{FileFilter, OpenDialog, SaveDialog};
use nsh_core::prelude::*;

use super::{Job, SUBTLE, Tone, Ui, action};

pub(super) fn controls(ui: Ui) -> impl IntoElement {
    view()
        .gap(px(8.0))
        .child(
            text("恢复设置将替换本地端口和分流规则；取消选择文件会保留当前设置。")
                .text_color(SUBTLE)
                .text_size(px(13.0)),
        )
        .child(
            view()
                .flex_row()
                .gap(px(8.0))
                .child(action(
                    "导出设置",
                    Tone::Standard,
                    move || ui.busy.get().is_some() || ui.session.with(Option::is_none),
                    move || {
                        if ui.busy.get_untracked().is_some() {
                            return;
                        }
                        ui.busy.set(Some("选择保存位置…"));
                        let request = pick_save_path(
                            SaveDialog::new()
                                .file_name("exodys-settings.json")
                                .filter(FileFilter::new("设置文件", ["json"]))
                                .button_label("导出"),
                        );
                        spawn(async move {
                            let selection = request.await;
                            ui.busy.set(None);
                            match selection {
                                Ok(Some(path)) => {
                                    ui.work("导出设置…", Job::ExportSettings(path), true)
                                }
                                Ok(None) => {}
                                Err(_) => ui.error("无法打开文件保存面板。"),
                            }
                        });
                    },
                ))
                .child(action(
                    "恢复设置",
                    Tone::Standard,
                    move || {
                        ui.busy.get().is_some()
                            || ui.session.with(Option::is_none)
                            || ui.state.with(|state| state.connected)
                    },
                    move || {
                        if ui.busy.get_untracked().is_some()
                            || ui.state.with_untracked(|state| state.connected)
                        {
                            return;
                        }
                        ui.busy.set(Some("选择设置文件…"));
                        let request = pick_paths(
                            OpenDialog::files()
                                .filter(FileFilter::new("设置文件", ["json"]))
                                .button_label("恢复"),
                        );
                        spawn(async move {
                            let selection = request.await;
                            ui.busy.set(None);
                            match selection {
                                Ok(Some(paths)) => {
                                    if let Some(path) = paths.into_iter().next() {
                                        if ui.state.with_untracked(|state| state.connected) {
                                            ui.error("请先断开连接，再恢复设置。");
                                        } else {
                                            ui.work("恢复设置…", Job::RestoreSettings(path), true);
                                        }
                                    }
                                }
                                Ok(None) => {}
                                Err(_) => ui.error("无法打开文件选择器。"),
                            }
                        });
                    },
                )),
        )
}
