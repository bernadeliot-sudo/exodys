use std::sync::{Arc, Mutex};
use std::time::Duration;

use nsh_base::{button, input};
use nsh_core::desktop::WindowAttributes;
use nsh_core::element::desktop::current_window;
use nsh_core::file_dialog::{FileFilter, OpenDialog};
use nsh_core::prelude::*;

use crate::catalog::{Matches, NodeRef, Profile, default_directory};
use crate::engine::address;
use crate::import::{MAX_BYTES, Source};
use crate::session::{Job, Session, Snapshot};

const PAPER: AdaptiveColor = AdaptiveColor::rgb(0xf4f7f7, 0x121b1d);
const SIDEBAR: AdaptiveColor = AdaptiveColor::rgb(0xe9efef, 0x192426);
const SURFACE: AdaptiveColor = AdaptiveColor::rgb(0xffffff, 0x1b282a);
const INK: AdaptiveColor = AdaptiveColor::rgb(0x162b2c, 0xf0f7f6);
const SUBTLE: AdaptiveColor = AdaptiveColor::rgb(0x496263, 0xa9bfbe);
const LINE: AdaptiveColor = AdaptiveColor::rgb(0xc8d6d5, 0x36494c);
const TEAL: AdaptiveColor = AdaptiveColor::rgb(0x10635c, 0x91dccf);
const ON_TEAL: AdaptiveColor = AdaptiveColor::rgb(0xffffff, 0x15302c);
const HIGHLIGHT: AdaptiveColor = AdaptiveColor::rgb(0xdbefea, 0x25403e);
const ERROR: AdaptiveColor = AdaptiveColor::rgb(0x9b2d24, 0xffb5ac);

#[component]
pub fn App() -> impl IntoElement {
    let mut window = WindowAttributes::default();
    window.min_size = Some(Some(size(px(740.0), px(560.0))));
    current_window().set(window);
    let ui = Ui::new();
    ui.load();
    cleanup(move || {
        if let Some(session) = ui.session.get_untracked()
            && let Ok(mut session) = session.try_lock()
        {
            session.disconnect();
        }
    });
    let connected = computed(move || ui.state.with(|state| state.connected));
    effect(move || {
        if connected.get() {
            set_interval(Duration::from_secs(1), move || ui.check_connection());
        }
    });

    view()
        .flex_1()
        .flex_row()
        .bg(PAPER)
        .text_color(INK)
        .child(sidebar(ui))
        .child(
            view()
                .flex_1()
                .min_w(px(0.0))
                .gap(px(18.0))
                .p(px(24.0))
                .child(connection(ui))
                .child(feedback(ui))
                .child(dynamic(move || match ui.page.get() {
                    Page::Nodes => nodes(ui).into_element(),
                    Page::Import => import_form(ui).into_element(),
                    Page::Rename(id) => rename_form(ui, id).into_element(),
                    Page::Remove(id) => remove_form(ui, id).into_element(),
                    Page::Settings => settings_form(ui).into_element(),
                })),
        )
}

#[derive(Clone, Copy, PartialEq)]
enum Page {
    Nodes,
    Import,
    Rename(u64),
    Remove(u64),
    Settings,
}

#[derive(Clone, PartialEq)]
struct Notice {
    message: String,
    error: bool,
}

#[derive(Clone, Copy)]
struct Ui {
    session: Signal<Option<Arc<Mutex<Session>>>>,
    state: Signal<Snapshot>,
    focus: Signal<Option<u64>>,
    filter: Signal<String>,
    draft: Signal<String>,
    page: Signal<Page>,
    busy: Signal<Option<&'static str>>,
    notice: Signal<Option<Notice>>,
}

impl Ui {
    fn new() -> Self {
        Self {
            session: signal_with(None, SignalEquals::Never),
            state: signal(Snapshot::default()),
            focus: signal(None),
            filter: signal(String::new()),
            draft: signal(String::new()),
            page: signal(Page::Nodes),
            busy: signal(None),
            notice: signal(None),
        }
    }

    fn load(self) {
        if self.busy.get_untracked().is_some() {
            return;
        }
        self.busy.set(Some("打开配置…"));
        self.notice.set(None);
        spawn(async move {
            let opened = blocking(|| default_directory().and_then(Session::open)).await;
            batch(|| {
                match opened {
                    Ok(Ok(session)) => {
                        let state = session.snapshot();
                        self.focus.set(
                            state
                                .preferences
                                .selected
                                .as_ref()
                                .map(|selection| selection.profile),
                        );
                        self.state.set(state);
                        self.session.set(Some(Arc::new(Mutex::new(session))));
                    }
                    Ok(Err(message)) => self.error(message),
                    Err(_) => self.error("无法打开配置。请重试。"),
                }
                self.busy.set(None);
            });
        });
    }

    fn error(self, message: impl Into<String>) {
        self.notice.set(Some(Notice {
            message: message.into(),
            error: true,
        }));
    }

    fn navigate(self, id: Option<u64>) {
        batch(|| {
            self.focus.set(id);
            self.filter.set(String::new());
            self.page.set(Page::Nodes);
        });
    }

    fn form(self, page: Page, value: impl Into<String>) {
        batch(|| {
            self.draft.set(value.into());
            self.notice.set(None);
            self.page.set(page);
        });
    }

    fn work(self, label: &'static str, job: Job, close_form: bool) {
        if self.busy.get_untracked().is_some() {
            return;
        }
        let Some(session) = self.session.get_untracked() else {
            return;
        };
        batch(|| {
            self.notice.set(None);
            self.busy.set(Some(label));
        });
        spawn(async move {
            let result = blocking(move || {
                let mut session = session
                    .lock()
                    .map_err(|_| "无法完成操作。请重新打开 Exodys。".to_owned())?;
                let outcome = session.run(job);
                Ok::<_, String>((session.snapshot(), outcome))
            })
            .await;
            batch(|| {
                match result {
                    Ok(Ok((state, outcome))) => {
                        let focus = self
                            .focus
                            .get_untracked()
                            .filter(|id| state.catalog.profile(*id).is_some());
                        let fallback = state
                            .preferences
                            .selected
                            .as_ref()
                            .map(|selected| selected.profile);
                        self.state.set(state);
                        match outcome {
                            Ok(outcome) => {
                                if close_form {
                                    self.draft.set(String::new());
                                    self.navigate(outcome.focus.or(focus).or(fallback));
                                }
                                self.notice.set(outcome.message.map(|message| Notice {
                                    message,
                                    error: false,
                                }));
                            }
                            Err(message) => self.error(message),
                        }
                    }
                    Ok(Err(message)) => self.error(message),
                    Err(_) => self.error("后台操作意外结束。请重试。"),
                }
                self.busy.set(None);
            });
        });
    }

    fn check_connection(self) {
        if !self.state.with_untracked(|state| state.connected)
            || self.busy.get_untracked().is_some()
        {
            return;
        }
        let Some(session) = self.session.get_untracked() else {
            return;
        };
        if let Ok(mut session) = session.try_lock()
            && session.check_connection()
        {
            batch(|| {
                self.state.set(session.snapshot());
                self.error("sing-box 已退出。本地代理已断开，请重新连接。");
            });
        };
    }

    fn pick_file(self) {
        if self.busy.get_untracked().is_some() {
            return;
        }
        self.busy.set(Some("选择文件…"));
        let request = pick_paths(
            OpenDialog::files()
                .filter(FileFilter::new(
                    "代理配置",
                    ["yaml", "yml", "json", "txt", "conf"],
                ))
                .button_label("导入"),
        );
        spawn(async move {
            let selection = request.await;
            self.busy.set(None);
            match selection {
                Ok(Some(paths)) => {
                    if let Some(path) = paths.into_iter().next() {
                        self.work("读取配置…", Job::Import(Source::File(path)), true);
                    }
                }
                Ok(None) => {}
                Err(_) => self.error("无法打开文件选择器。你也可以粘贴配置内容。"),
            }
        });
    }
}

fn sidebar(ui: Ui) -> impl IntoElement {
    let profiles = computed(move || ui.state.with(|state| state.catalog.clone()));
    view()
        .w(px(208.0))
        .p(px(16.0))
        .gap(px(13.0))
        .border_r_1()
        .border_color(LINE)
        .bg(SIDEBAR)
        .child(
            view()
                .gap(px(3.0))
                .pb(px(9.0))
                .child(
                    text("Exodys")
                        .text_size(px(24.0))
                        .font_weight(FontWeight::BOLD),
                )
                .child(text("本机代理").text_size(px(13.0)).text_color(SUBTLE)),
        )
        .child(action(
            "＋ 导入配置",
            Tone::Primary,
            move || ui.busy.get().is_some() || ui.session.with(Option::is_none),
            move || ui.form(Page::Import, ""),
        ))
        .child(
            view()
                .flex_row()
                .justify_between()
                .items_center()
                .pt(px(8.0))
                .child(
                    text("配置")
                        .text_size(px(13.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(SUBTLE),
                )
                .child(
                    text(move || profiles.with(|c| c.profiles.len().to_string()))
                        .text_size(px(12.0))
                        .text_color(SUBTLE),
                ),
        )
        .child(
            virtual_list(
                move || profiles.with(|c| c.profiles.len()),
                move |at| {
                    let profile =
                        computed(move || profiles.with(|c| c.profiles.get(at.index()).cloned()));
                    button()
                        .h(px(47.0))
                        .rounded(px(8.0))
                        .px(px(9.0))
                        .justify_center()
                        .selected(move || {
                            profile
                                .with(|p| p.as_ref().is_some_and(|p| Some(p.id) == ui.focus.get()))
                        })
                        .selected_style(|s| s.bg(HIGHLIGHT))
                        .hover(|s| s.bg(HIGHLIGHT))
                        .focus(|s| s.border_color(TEAL).border_1())
                        .disabled(move || ui.busy.get().is_some())
                        .on_press(move || {
                            ui.navigate(profile.with_untracked(|p| p.as_ref().map(|p| p.id)))
                        })
                        .child(
                            text(move || {
                                profile.with(|p| {
                                    p.as_ref().map(|p| p.name.clone()).unwrap_or_default()
                                })
                            })
                            .font_weight(FontWeight::MEDIUM)
                            .truncate(),
                        )
                        .child(
                            text(move || {
                                profile.with(|p| {
                                    p.as_ref()
                                        .map(|p| format!("{} 个节点", p.nodes.len()))
                                        .unwrap_or_default()
                                })
                            })
                            .text_size(px(11.0))
                            .text_color(SUBTLE),
                        )
                },
            )
            .estimated_item_size(px(47.0))
            .flex_1()
            .empty(
                text("暂无配置")
                    .text_color(SUBTLE)
                    .text_size(px(13.0))
                    .p(px(9.0)),
            ),
        )
        .child(action(
            "本地端口设置",
            Tone::Quiet,
            move || ui.busy.get().is_some() || ui.session.with(Option::is_none),
            move || {
                ui.form(
                    Page::Settings,
                    ui.state
                        .with_untracked(|state| state.preferences.port.to_string()),
                );
            },
        ))
}

fn connection(ui: Ui) -> impl IntoElement {
    const BACK: u32 = 0x153538;
    const WHITE: u32 = 0xf0f9f7;
    const MUTED: u32 = 0xb9d5d1;
    const GLOW: u32 = 0x9be6d8;
    let connected = computed(move || ui.state.with(|s| s.connected));
    let selected_name = computed(move || ui.state.with(|s| s.node().map(|node| node.name.clone())));
    let socket = computed(move || ui.state.with(|s| address(s.preferences.port).to_string()));
    view()
        .bg(rgb(BACK))
        .rounded(px(14.0))
        .p(px(18.0))
        .gap(px(10.0))
        .child(
            view()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .child(
                    view()
                        .size(px(8.0))
                        .rounded_full()
                        .bg(move || rgb(if connected.get() { GLOW } else { 0xa7b5b5 })),
                )
                .child(
                    text(move || {
                        if connected.get() {
                            "本地代理运行中"
                        } else {
                            "本地代理已断开"
                        }
                    })
                    .text_color(rgb(MUTED))
                    .text_size(px(12.0)),
                ),
        )
        .child(
            view()
                .flex_row()
                .items_center()
                .gap(px(13.0))
                .child(
                    text(move || {
                        selected_name
                            .with(|name| name.as_deref().unwrap_or("未选择节点").to_owned())
                    })
                    .flex_1()
                    .truncate()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(WHITE))
                    .text_size(px(27.0)),
                )
                .child(
                    button()
                        .test_id("connection-toggle")
                        .bg(rgb(GLOW))
                        .rounded(px(8.0))
                        .min_w(px(98.0))
                        .h(px(38.0))
                        .items_center()
                        .justify_center()
                        .disabled(move || {
                            ui.busy.get().is_some() || selected_name.with(Option::is_none)
                        })
                        .disabled_style(|s| s.opacity(0.42))
                        .hover(|s| s.bg(rgb(0xbef4e9)))
                        .focus(|s| s.border_1().border_color(rgb(WHITE)))
                        .on_press(move || {
                            if connected.get_untracked() {
                                ui.work("断开连接…", Job::Disconnect, false);
                            } else {
                                ui.work("启动本地代理…", Job::Connect, false);
                            }
                        })
                        .child(
                            text(move || if connected.get() { "断开" } else { "连接" })
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(rgb(BACK)),
                        ),
                ),
        )
        .child(
            view()
                .flex_row()
                .gap(px(10.0))
                .items_center()
                .child(
                    text(move || format!("{}  HTTP / SOCKS5", socket.get()))
                        .flex_1()
                        .truncate()
                        .text_size(px(12.0))
                        .text_color(rgb(MUTED)),
                )
                .child(panel_button("复制地址", move || {
                    set_clipboard(socket.get_untracked());
                    ui.notice.set(Some(Notice {
                        message: "已复制本地代理地址。".into(),
                        error: false,
                    }));
                }))
                .child(when(
                    move || connected.get(),
                    move || {
                        panel_button("测试代理", move || {
                            ui.work(
                                "通过代理发送测试请求…",
                                Job::Probe("https://cp.cloudflare.com/generate_204".into()),
                                false,
                            );
                        })
                    },
                )),
        )
}

fn panel_button(label: &'static str, execute: impl Fn() + 'static) -> impl IntoElement {
    button()
        .h(px(28.0))
        .px(px(9.0))
        .justify_center()
        .items_center()
        .rounded(px(6.0))
        .border_1()
        .border_color(rgb(0x527673))
        .hover(|s| s.bg(rgb(0x27504d)))
        .focus(|s| s.border_color(rgb(0x9be6d8)))
        .on_press(execute)
        .child(text(label).text_size(px(12.0)).text_color(rgb(0xe0f5f1)))
}

fn feedback(ui: Ui) -> impl IntoElement {
    when(
        move || ui.busy.get().is_some() || ui.notice.with(Option::is_some),
        move || {
            view()
                .flex_row()
                .gap(px(8.0))
                .items_center()
                .px(px(12.0))
                .py(px(8.0))
                .bg(SURFACE)
                .border_1()
                .border_color(LINE)
                .rounded(px(8.0))
                .child(
                    text(move || {
                        if let Some(busy) = ui.busy.get() {
                            return busy.to_owned();
                        }
                        ui.notice.with(|notice| {
                            notice
                                .as_ref()
                                .map(|notice| notice.message.clone())
                                .unwrap_or_default()
                        })
                    })
                    .flex_1()
                    .text_size(px(13.0))
                    .text_color(move || {
                        if ui.notice.with(|n| n.as_ref().is_some_and(|n| n.error)) {
                            ERROR
                        } else {
                            INK
                        }
                    }),
                )
                .child(when(
                    move || ui.busy.get().is_none(),
                    move || {
                        action(
                            "关闭提示",
                            Tone::Quiet,
                            || false,
                            move || ui.notice.set(None),
                        )
                    },
                ))
        },
    )
}

fn nodes(ui: Ui) -> impl IntoElement {
    let current = computed(move || {
        ui.state.with(|state| {
            ui.focus
                .get()
                .and_then(|id| state.catalog.profile(id).cloned())
        })
    });
    view().flex_1().min_h(px(0.0)).child(dynamic(move || {
        if current.with(Option::is_some) {
            node_browser(ui, current).into_element()
        } else {
            empty_library(ui).into_element()
        }
    }))
}

fn empty_library(ui: Ui) -> impl IntoElement {
    view()
        .flex_1()
        .items_center()
        .justify_center()
        .gap(px(12.0))
        .p(px(18.0))
        .rounded(px(10.0))
        .border_1()
        .border_color(LINE)
        .bg(SURFACE)
        .child(
            text(move || {
                if ui.session.with(Option::is_none) {
                    "配置库"
                } else {
                    "导入第一个配置"
                }
            })
            .text_size(px(21.0))
            .font_weight(FontWeight::SEMIBOLD),
        )
        .child(when(
            move || ui.session.with(Option::is_some),
            move || {
                view()
                    .items_center()
                    .gap(px(12.0))
                    .child(
                        text(
                            "粘贴订阅地址或节点链接，也可以打开 Clash YAML 或 sing-box JSON 文件。",
                        )
                        .text_color(SUBTLE)
                        .text_size(px(13.0)),
                    )
                    .child(action(
                        "导入配置",
                        Tone::Primary,
                        move || ui.busy.get().is_some(),
                        move || ui.form(Page::Import, ""),
                    ))
            },
        ))
        .child(when(
            move || ui.session.with(Option::is_none) && ui.busy.get().is_none(),
            move || action("重新加载", Tone::Outline, || false, move || ui.load()),
        ))
}

#[derive(Clone, Default, PartialEq)]
struct Row {
    id: String,
    name: String,
    protocol: String,
    endpoint: String,
}

fn node_browser(ui: Ui, profile: Memo<Option<Arc<Profile>>>) -> impl IntoElement {
    let matches = computed(move || {
        profile.with(|profile| {
            profile
                .as_ref()
                .map(|profile| ui.filter.with(|query| profile.search(query)))
                .unwrap_or(Matches::Empty)
        })
    });
    let profile_id = computed(move || profile.with(|p| p.as_ref().map(|p| p.id)));
    let active = computed(move || {
        ui.state.with(|state| {
            state.connected
                && state
                    .preferences
                    .selected
                    .as_ref()
                    .is_some_and(|node| Some(node.profile) == profile_id.get())
        })
    });
    let selected = computed(move || ui.state.with(|state| state.preferences.selected.clone()));
    view()
        .flex_1()
        .min_h(px(0.0))
        .gap(px(11.0))
        .child(
            view()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .child(
                    view()
                        .flex_1()
                        .min_w(px(0.0))
                        .gap(px(4.0))
                        .child(
                            text(move || {
                                profile.with(|p| {
                                    p.as_ref().map(|p| p.name.clone()).unwrap_or_default()
                                })
                            })
                            .text_size(px(21.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .truncate(),
                        )
                        .child(
                            text(move || {
                                profile.with(|p| {
                                    format!(
                                        "{} / {} 个节点",
                                        matches.with(Matches::len),
                                        p.as_ref().map_or(0, |p| p.nodes.len())
                                    )
                                })
                            })
                            .text_size(px(12.0))
                            .text_color(SUBTLE),
                        ),
                )
                .child(when(
                    move || profile.with(|p| p.as_ref().is_some_and(|p| p.subscription.is_some())),
                    move || {
                        action(
                            "刷新订阅",
                            Tone::Outline,
                            move || ui.busy.get().is_some() || active.get(),
                            move || {
                                if let Some(id) = profile_id.get_untracked() {
                                    ui.work("刷新订阅…", Job::Refresh(id), false);
                                }
                            },
                        )
                    },
                ))
                .child(action(
                    "重命名",
                    Tone::Outline,
                    move || ui.busy.get().is_some(),
                    move || {
                        if let Some(p) = profile.get_untracked() {
                            ui.form(Page::Rename(p.id), p.name.clone());
                        }
                    },
                ))
                .child(action(
                    "删除",
                    Tone::Quiet,
                    move || ui.busy.get().is_some() || active.get(),
                    move || {
                        if let Some(id) = profile_id.get_untracked() {
                            ui.form(Page::Remove(id), "");
                        }
                    },
                )),
        )
        .child(field(ui.filter, "搜索节点名称、协议或地址", "node-search").max_length(180))
        .child(
            view()
                .flex_1()
                .min_h(px(0.0))
                .bg(SURFACE)
                .rounded(px(10.0))
                .border_1()
                .border_color(LINE)
                .child(
                    view()
                        .flex_row()
                        .items_center()
                        .h(px(32.0))
                        .px(px(12.0))
                        .border_b_1()
                        .border_color(LINE)
                        .child(text("节点").flex_1().text_color(SUBTLE).text_size(px(12.0)))
                        .child(
                            text("协议")
                                .w(px(91.0))
                                .text_color(SUBTLE)
                                .text_size(px(12.0)),
                        )
                        .child(
                            text("状态")
                                .w(px(62.0))
                                .text_color(SUBTLE)
                                .text_size(px(12.0)),
                        ),
                )
                .child(
                    virtual_list(
                        move || matches.with(Matches::len),
                        move |at| {
                            let row = computed(move || {
                                profile.with(|profile| {
                                    matches.with(|matches| {
                                        let Some(node) = matches
                                            .at(at.index())
                                            .and_then(|index| profile.as_ref()?.nodes.get(index))
                                        else {
                                            return Row::default();
                                        };
                                        Row {
                                            id: node.id.clone(),
                                            name: node.name.clone(),
                                            protocol: node.protocol().to_owned(),
                                            endpoint: node.endpoint(),
                                        }
                                    })
                                })
                            });
                            let is_selected = move || {
                                selected.with(|selection| {
                                    row.with(|r| {
                                        selection.as_ref().is_some_and(|selection| {
                                            Some(selection.profile) == profile_id.get()
                                                && selection.node == r.id
                                        })
                                    })
                                })
                            };
                            button()
                                .test_id("node-row")
                                .h(px(53.0))
                                .px(px(12.0))
                                .gap(px(8.0))
                                .flex_row()
                                .items_center()
                                .border_b_1()
                                .border_color(LINE)
                                .selected(is_selected)
                                .selected_style(|s| s.bg(HIGHLIGHT))
                                .hover(|s| s.bg(HIGHLIGHT))
                                .focus(|s| s.border_color(TEAL).border_1())
                                .disabled(move || ui.busy.get().is_some())
                                .on_press(move || {
                                    if let Some(id) = profile_id.get_untracked() {
                                        let key = row.with_untracked(|r| r.id.clone());
                                        if !key.is_empty() {
                                            ui.work(
                                                "应用节点选择…",
                                                Job::Select(NodeRef {
                                                    profile: id,
                                                    node: key,
                                                }),
                                                false,
                                            );
                                        }
                                    }
                                })
                                .child(
                                    view()
                                        .flex_1()
                                        .min_w(px(0.0))
                                        .gap(px(3.0))
                                        .child(
                                            text(move || row.with(|r| r.name.clone()))
                                                .font_weight(FontWeight::MEDIUM)
                                                .truncate(),
                                        )
                                        .child(
                                            text(move || row.with(|r| r.endpoint.clone()))
                                                .text_color(SUBTLE)
                                                .text_size(px(11.0))
                                                .truncate(),
                                        ),
                                )
                                .child(
                                    text(move || row.with(|r| r.protocol.clone()))
                                        .w(px(83.0))
                                        .text_size(px(12.0))
                                        .text_color(SUBTLE)
                                        .truncate(),
                                )
                                .child(
                                    text(move || if is_selected() { "已选中" } else { "" })
                                        .w(px(62.0))
                                        .text_size(px(12.0))
                                        .text_color(TEAL),
                                )
                        },
                    )
                    .estimated_item_size(px(53.0))
                    .flex_1()
                    .empty(
                        view()
                            .p(px(20.0))
                            .child(text("没有匹配节点。试试其他搜索词。").text_color(SUBTLE)),
                    ),
                ),
        )
}

fn import_form(ui: Ui) -> impl IntoElement {
    form_panel("导入配置")
        .child(
            text("粘贴订阅地址、节点链接或配置内容，也可以直接导入文件。请确认订阅来源。")
                .text_color(SUBTLE)
                .text_size(px(13.0)),
        )
        .child(
            field(ui.draft, "订阅地址或节点内容", "import-content")
                .multiline(true)
                .auto_focus(true)
                .max_length(MAX_BYTES as u32 + 1)
                .h(px(121.0))
                .px(px(10.0))
                .py(px(9.0)),
        )
        .child(
            view()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .child(action(
                    "从文件导入",
                    Tone::Outline,
                    move || ui.busy.get().is_some(),
                    move || ui.pick_file(),
                ))
                .child(view().flex_1())
                .child(cancel(ui))
                .child(action(
                    "导入",
                    Tone::Primary,
                    move || ui.busy.get().is_some() || ui.draft.with(|d| d.trim().is_empty()),
                    move || {
                        ui.work(
                            "导入配置…",
                            Job::Import(Source::Text(ui.draft.get_untracked())),
                            true,
                        );
                    },
                )),
        )
}

fn rename_form(ui: Ui, id: u64) -> impl IntoElement {
    let save = move || ui.work("保存名称…", Job::Rename(id, ui.draft.get_untracked()), true);
    form_panel("重命名配置")
        .child(
            field(ui.draft, "配置名称", "profile-name")
                .auto_focus(true)
                .max_length(120)
                .on_submit(move |_: &str| save()),
        )
        .child(
            view()
                .flex_row()
                .justify_end()
                .gap(px(8.0))
                .child(cancel(ui))
                .child(action(
                    "保存名称",
                    Tone::Primary,
                    move || ui.busy.get().is_some() || ui.draft.with(|d| d.trim().is_empty()),
                    save,
                )),
        )
}

fn remove_form(ui: Ui, id: u64) -> impl IntoElement {
    form_panel("删除配置？")
        .child(
            text(move || {
                ui.state.with(|state| {
                    state
                        .catalog
                        .profile(id)
                        .map(|profile| {
                            format!(
                                "将从本机移除「{}」及其 {} 个节点。",
                                profile.name,
                                profile.nodes.len()
                            )
                        })
                        .unwrap_or_default()
                })
            })
            .text_color(SUBTLE),
        )
        .child(
            view()
                .flex_row()
                .justify_end()
                .gap(px(8.0))
                .child(cancel(ui))
                .child(action(
                    "删除配置",
                    Tone::Danger,
                    move || ui.busy.get().is_some(),
                    move || ui.work("删除配置…", Job::Remove(id), true),
                )),
        )
}

fn settings_form(ui: Ui) -> impl IntoElement {
    let save = move || match ui.draft.with_untracked(|draft| draft.trim().parse::<u16>()) {
        Ok(port) if port >= 1024 => ui.work("保存端口…", Job::SetPort(port), true),
        _ => ui.error("请输入 1024 至 65535 之间的端口。"),
    };
    form_panel("本地代理设置")
        .child(
            text("在需要代理的应用中使用 127.0.0.1 和下方端口。HTTP 与 SOCKS5 共用此端口。")
                .text_color(SUBTLE)
                .text_size(px(13.0)),
        )
        .child(
            text("本地端口")
                .text_size(px(13.0))
                .font_weight(FontWeight::MEDIUM),
        )
        .child(
            field(ui.draft, "2080", "local-port")
                .max_length(5)
                .w(px(130.0))
                .readonly(move || ui.state.with(|s| s.connected))
                .on_submit(move |_: &str| save()),
        )
        .child(when(
            move || ui.state.with(|s| s.connected),
            || text("断开连接后可修改端口。").text_color(SUBTLE),
        ))
        .child(
            view()
                .flex_row()
                .justify_end()
                .gap(px(8.0))
                .child(cancel(ui))
                .child(action(
                    "保存端口",
                    Tone::Primary,
                    move || ui.busy.get().is_some() || ui.state.with(|s| s.connected),
                    save,
                )),
        )
}

fn form_panel(title: &'static str) -> View {
    view()
        .bg(SURFACE)
        .rounded(px(10.0))
        .border_1()
        .border_color(LINE)
        .p(px(17.0))
        .gap(px(12.0))
        .child(
            text(title)
                .text_size(px(20.0))
                .font_weight(FontWeight::SEMIBOLD),
        )
}

fn cancel(ui: Ui) -> impl IntoElement {
    action(
        "取消",
        Tone::Quiet,
        move || ui.busy.get().is_some(),
        move || {
            ui.draft.set(String::new());
            ui.page.set(Page::Nodes);
        },
    )
}

fn field(value: Signal<String>, label: &'static str, id: &'static str) -> Input {
    input()
        .value(value)
        .on_change(move |text: &str| value.set(text.to_owned()))
        .placeholder(label)
        .label(label)
        .test_id(id)
        .auto_correct(false)
        .auto_capitalize(AutoCapitalize::None)
        .h(px(36.0))
        .px(px(10.0))
        .bg(SURFACE)
        .border_1()
        .border_color(LINE)
        .rounded(px(7.0))
        .focus(|s| s.border_color(TEAL))
        .text_color(INK)
        .text_size(px(14.0))
}

enum Tone {
    Primary,
    Outline,
    Quiet,
    Danger,
}

fn action(
    label: &'static str,
    tone: Tone,
    disabled: impl Fn() -> bool + 'static,
    execute: impl Fn() + 'static,
) -> impl IntoElement {
    let (background, foreground, border) = match tone {
        Tone::Primary => (TEAL, ON_TEAL, TEAL),
        Tone::Outline => (SURFACE, INK, LINE),
        Tone::Quiet => (PAPER, SUBTLE, LINE),
        Tone::Danger => (SURFACE, ERROR, ERROR),
    };
    button()
        .h(px(34.0))
        .px(px(11.0))
        .items_center()
        .justify_center()
        .rounded(px(7.0))
        .bg(background)
        .border_1()
        .border_color(border)
        .disabled(disabled)
        .disabled_style(|s| s.opacity(0.45))
        .hover(|s| s.opacity(0.83))
        .active(|s| s.opacity(0.68))
        .focus(|s| s.border_color(INK))
        .on_press(execute)
        .child(
            text(label)
                .text_color(foreground)
                .text_size(px(13.0))
                .font_weight(FontWeight::MEDIUM),
        )
}
