use std::path::PathBuf;

use nsh_core::element::create_root;
use nsh_core::file_dialog::{FileFilter, OpenDialog, SaveDialog};

use super::*;

#[derive(Clone, PartialEq)]
enum Step {
    Choose,
    ConfirmRestore(PathBuf),
    Passphrase { path: PathBuf, restore: bool },
}

pub(super) fn page(ui: Ui) -> impl IntoElement {
    let (owner, ()) = create_root(|| ());
    cleanup(move || owner.dispose());
    owner.run(move || {
        let step = signal(Step::Choose);
        view()
            .flex_1()
            .gap(px(12.0))
            .child(text("完整备份").text_size(px(20.0)).font_weight(FontWeight::SEMIBOLD))
            .child(text("完整导出会创建经过身份验证的加密快照，包含整个工作区。请妥善保管口令；恢复时需要相同的口令。")
                .text_size(px(13.0)).text_color(SUBTLE))
            .child(text("完整备份适合单次迁移和手动存档。").text_size(px(13.0)).text_color(SUBTLE))
            .child(view().flex_row().child(action("自动同步…", Tone::Standard,
                move || ui.busy.get().is_some(), move || ui.page.set(Page::Sync))))
            .child(dynamic(move || match step.get() {
                Step::Choose => view().gap(px(8.0))
                    .child(view().flex_row().gap(px(8.0))
                        .child(action("完整导出…", Tone::Standard,
                            move || ui.busy.get().is_some() || ui.session.with(Option::is_none),
                            move || {
                                if ui.busy.get_untracked().is_some() { return; }
                                ui.busy.set(Some("选择备份保存位置…"));
                                let request = pick_save_path(SaveDialog::new()
                                    .file_name("exodys.exodysbackup")
                                    .filter(FileFilter::new("Exodys 完整备份", ["exodysbackup"]))
                                    .button_label("选择"));
                                spawn(async move {
                                    let result = request.await;
                                    ui.busy.set(None);
                                    if ui.page.get_untracked() != Page::Backup { return; }
                                    match result {
                                        Ok(Some(path)) => step.set(Step::Passphrase { path, restore: false }),
                                        Ok(None) => {},
                                        Err(_) => ui.error("无法打开文件保存面板。"),
                                    }
                                });
                            }))
                        .child(action("完整恢复…", Tone::Standard,
                            move || ui.busy.get().is_some() || ui.session.with(Option::is_none) || ui.state.with(|s| s.connected),
                            move || {
                                if ui.busy.get_untracked().is_some() || ui.state.with_untracked(|s| s.connected) { return; }
                                ui.busy.set(Some("选择完整备份…"));
                                let request = pick_paths(OpenDialog::files()
                                    .filter(FileFilter::new("Exodys 完整备份", ["exodysbackup"]))
                                    .button_label("选择"));
                                spawn(async move {
                                    let result = request.await;
                                    ui.busy.set(None);
                                    if ui.page.get_untracked() != Page::Backup { return; }
                                    match result {
                                        Ok(Some(paths)) => if let Some(path) = paths.into_iter().next() { step.set(Step::ConfirmRestore(path)); },
                                        Ok(None) => {},
                                        Err(_) => ui.error("无法打开文件选择器。"),
                                    }
                                });
                            })))
                    .child(text("完整恢复前请先断开连接。").text_size(px(13.0)).text_color(SUBTLE))
                    .into_element(),
                Step::ConfirmRestore(path) => view().gap(px(8.0))
                    .child(text("完整恢复将替换当前设备上的整个工作区。确定继续吗？").text_size(px(13.0)))
                    .child(text(path.to_string_lossy().into_owned()).text_size(px(13.0)).text_color(SUBTLE))
                    .child(view().flex_row().gap(px(8.0))
                        .child(action("取消", Tone::Standard, || false, move || step.set(Step::Choose)))
                        .child(action("确认替换工作区", Tone::Danger,
                            move || ui.busy.get().is_some() || ui.state.with(|s| s.connected),
                            move || {
                                if !ui.state.with_untracked(|s| s.connected) {
                                    step.set(Step::Passphrase { path: path.clone(), restore: true });
                                }
                            })))
                    .into_element(),
                Step::Passphrase { path, restore } => view().gap(px(8.0))
                    .child(text(if restore { "输入此完整备份的口令，然后按回车开始完整恢复。" } else { "设置至少 12 个字符的口令，然后按回车开始完整导出。" }).text_size(px(13.0)))
                    .child(text(path.to_string_lossy().into_owned()).text_size(px(13.0)).text_color(SUBTLE))
                    .child(native("exodys/secure-text-field")
                        .key("backup-passphrase")
                        .test_id("backup-passphrase")
                        .h(px(28.0))
                        .on_native_event(move |event: &NativeEvent| {
                            if &*event.name != "submit" { return; }
                            let Some(passphrase) = event.data.as_u64().and_then(crate::macos_native::take_secure_input) else { return; };
                            if ui.busy.get_untracked().is_some() || ui.session.with_untracked(Option::is_none) { return; }
                            if restore && ui.state.with_untracked(|s| s.connected) {
                                ui.error("请先断开连接，再完整恢复。");
                                return;
                            }
                            if !restore && passphrase.chars().count() < 12 {
                                ui.error("完整导出的口令至少需要 12 个字符，请重新输入。");
                                return;
                            }
                            if restore && passphrase.is_empty() {
                                ui.error("请输入完整备份的口令。");
                                return;
                            }
                            let job = if restore { Job::RestoreBackup(path.clone(), passphrase) } else { Job::ExportBackup(path.clone(), passphrase) };
                            if !restore {
                                step.set(Step::Choose);
                            }
                            ui.work(if restore { "完整恢复…" } else { "完整导出…" }, job, restore);
                        }))
                    .child(view().flex_row().child(action("取消", Tone::Standard, || false, move || step.set(Step::Choose))))
                    .into_element(),
            }))
            .child(view().flex_1())
            .child(view().flex_row().justify_end().child(action("返回设置", Tone::Standard,
                move || ui.busy.get().is_some(), move || ui.page.set(Page::Settings))))
    })
}
