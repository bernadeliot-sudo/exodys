use std::{cell::RefCell, rc::Rc};

use nsh_platform_macos::{
    ComponentContext, Components, NativeComponent,
    objc2::{
        DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, rc::Retained,
        runtime::ProtocolObject,
    },
    objc2_app_kit::{
        NSAccessibility, NSAutoresizingMaskOptions, NSColor, NSControlTextEditingDelegate, NSFont,
        NSFontWeightMedium, NSLineBreakMode, NSScrollView, NSTableCellView, NSTableColumn,
        NSTableColumnResizingOptions, NSTableView, NSTableViewColumnAutoresizingStyle,
        NSTableViewDataSource, NSTableViewDelegate, NSTableViewStyle, NSTextAlignment, NSTextField,
        NSUserInterfaceItemIdentification, NSView,
    },
    objc2_foundation::{
        NSIndexSet, NSInteger, NSNotification, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize,
        NSString,
    },
};
use serde_json::Value;

const PROFILE_TABLE: &str = "exodys/profile-table";
const NODE_TABLE: &str = "exodys/node-table";

pub(super) fn register(components: &Components) {
    components.register(
        PROFILE_TABLE,
        Rc::new(|context| Box::new(Table::new(context, TableKind::Profiles))),
    );
    components.register(
        NODE_TABLE,
        Rc::new(|context| Box::new(Table::new(context, TableKind::Nodes))),
    );
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TableKind {
    Profiles,
    Nodes,
}

#[derive(Clone, Debug)]
struct ProfileRow {
    id: u64,
    name: String,
    count: usize,
}

#[derive(Clone, Debug)]
struct NodeRow {
    id: String,
    name: String,
    protocol: String,
    endpoint: String,
}

#[derive(Default)]
struct TableData {
    profiles: Vec<ProfileRow>,
    nodes: Vec<NodeRow>,
    selected: Option<String>,
    suppress_selection_event: std::cell::Cell<bool>,
}

struct TargetState {
    context: ComponentContext,
    kind: TableKind,
    data: Rc<RefCell<TableData>>,
    table: RefCell<Option<Retained<NSTableView>>>,
}

struct Table {
    scroll: Retained<NSScrollView>,
    table: Retained<NSTableView>,
    _target: Retained<TableTarget>,
    kind: TableKind,
    data: Rc<RefCell<TableData>>,
}

impl Table {
    fn new(context: ComponentContext, kind: TableKind) -> Self {
        let mtm =
            MainThreadMarker::new().expect("native components are created on the main thread");
        let table = NSTableView::new(mtm);
        table.setAllowsMultipleSelection(false);
        table.setAllowsEmptySelection(true);
        table.setStyle(match kind {
            TableKind::Profiles => NSTableViewStyle::Plain,
            TableKind::Nodes => NSTableViewStyle::Inset,
        });
        table.setRowHeight(42.0);
        table.setIntercellSpacing(NSSize::new(0.0, 2.0));
        table.setColumnAutoresizingStyle(
            NSTableViewColumnAutoresizingStyle::FirstColumnOnlyAutoresizingStyle,
        );
        table.setAccessibilityIdentifier(Some(&NSString::from_str(match kind {
            TableKind::Profiles => "native-profile-table",
            TableKind::Nodes => "native-node-table",
        })));
        table.setAccessibilityLabel(Some(&NSString::from_str(match kind {
            TableKind::Profiles => "配置列表",
            TableKind::Nodes => "节点列表",
        })));

        let column = NSTableColumn::initWithIdentifier(
            NSTableColumn::alloc(mtm),
            &NSString::from_str("entry"),
        );
        column.setMinWidth(0.0);
        column.setResizingMask(NSTableColumnResizingOptions::AutoresizingMask);
        table.addTableColumn(&column);
        table.setHeaderView(None);
        table.setBackgroundColor(&NSColor::clearColor());

        let data = Rc::new(RefCell::new(TableData::default()));
        let target: Retained<TableTarget> = unsafe {
            msg_send![
                super(TableTarget::alloc(mtm).set_ivars(TargetState {
                    context,
                    kind,
                    data: data.clone(),
                    table: RefCell::new(None),
                })),
                init
            ]
        };
        unsafe {
            let data_source: &ProtocolObject<dyn NSTableViewDataSource> =
                ProtocolObject::from_ref(&*target);
            let delegate: &ProtocolObject<dyn NSTableViewDelegate> =
                ProtocolObject::from_ref(&*target);
            table.setDataSource(Some(data_source));
            table.setDelegate(Some(delegate));
        }
        *target.ivars().table.borrow_mut() = Some(table.clone());

        let scroll = NSScrollView::new(mtm);
        scroll.setHasVerticalScroller(true);
        scroll.setHasHorizontalScroller(false);
        scroll.setAutohidesScrollers(true);
        scroll.setDrawsBackground(false);
        scroll.setDocumentView(Some(&table));

        Self {
            scroll,
            table,
            _target: target,
            kind,
            data,
        }
    }

    fn set_rows(&mut self, value: Option<&Value>) {
        let rows = value.and_then(Value::as_array);
        let mut data = self.data.borrow_mut();
        match self.kind {
            TableKind::Profiles => {
                data.profiles = rows
                    .into_iter()
                    .flat_map(|rows| rows.iter())
                    .filter_map(|row| {
                        Some(ProfileRow {
                            id: row.get("id")?.as_u64()?,
                            name: row.get("name")?.as_str()?.to_owned(),
                            count: row.get("count")?.as_u64()? as usize,
                        })
                    })
                    .collect();
            }
            TableKind::Nodes => {
                data.nodes = rows
                    .into_iter()
                    .flat_map(|rows| rows.iter())
                    .filter_map(|row| {
                        Some(NodeRow {
                            id: row.get("id")?.as_str()?.to_owned(),
                            name: row.get("name")?.as_str()?.to_owned(),
                            protocol: row.get("protocol")?.as_str()?.to_owned(),
                            endpoint: row.get("endpoint")?.as_str()?.to_owned(),
                        })
                    })
                    .collect();
            }
        }
        data.suppress_selection_event.set(true);
        drop(data);
        self.table.reloadData();
        self.sync_selection();
    }

    fn set_selected(&mut self, value: Option<&Value>) {
        let mut data = self.data.borrow_mut();
        data.selected = match self.kind {
            TableKind::Profiles => value.and_then(Value::as_u64).map(|id| id.to_string()),
            TableKind::Nodes => value.and_then(Value::as_str).map(str::to_owned),
        };
        drop(data);
        self.sync_selection();
    }

    fn sync_selection(&self) {
        let selected = self.data.borrow().selected.clone();
        let row = selected.and_then(|selected| match self.kind {
            TableKind::Profiles => self
                .data
                .borrow()
                .profiles
                .iter()
                .position(|profile| profile.id.to_string() == selected),
            TableKind::Nodes => self
                .data
                .borrow()
                .nodes
                .iter()
                .position(|node| node.id == selected),
        });
        self.data.borrow().suppress_selection_event.set(true);
        if let Some(row) = row {
            let indexes = NSIndexSet::indexSetWithIndex(row);
            self.table
                .selectRowIndexes_byExtendingSelection(&indexes, false);
        } else if self.table.selectedRow() >= 0 {
            self.table.deselectRow(self.table.selectedRow());
        }
        self.data.borrow().suppress_selection_event.set(false);
    }
}

impl NativeComponent for Table {
    fn view(&self) -> Retained<NSView> {
        Retained::into_super(self.scroll.clone())
    }

    fn set_prop(&mut self, name: &str, value: Option<&Value>) {
        match name {
            "rows" => self.set_rows(value),
            "selected" => self.set_selected(value),
            "disabled" => {
                self.table
                    .setEnabled(!value.and_then(Value::as_bool).unwrap_or(false));
                self.sync_selection();
            }
            _ => {}
        }
    }
}

struct RowContent {
    title: String,
    detail: String,
    accessory: Option<String>,
}

struct RowLabels {
    title: Retained<NSTextField>,
    detail: Retained<NSTextField>,
    accessory: Option<Retained<NSTextField>>,
}

define_class!(
    #[unsafe(super(
        NSTableCellView,
        NSView,
        nsh_platform_macos::objc2_app_kit::NSResponder,
        NSObject
    ))]
    #[thread_kind = MainThreadOnly]
    #[ivars = RowLabels]
    #[name = "ExodysListCell"]
    struct ListCell;
);

impl ListCell {
    fn new(table: &NSTableView, column: &NSTableColumn, kind: TableKind) -> Retained<Self> {
        let mtm = table.mtm();
        let title = row_label(mtm, 13.0, false);
        title.setFont(Some(&NSFont::systemFontOfSize_weight(13.0, unsafe {
            NSFontWeightMedium
        })));
        let detail = row_label(mtm, 11.0, true);
        let accessory = (kind == TableKind::Nodes).then(|| {
            let label = row_label(mtm, 11.0, true);
            label.setAlignment(NSTextAlignment::Right);
            label
        });
        let frame = NSRect::new(
            NSPoint::ZERO,
            NSSize::new(column.width(), table.rowHeight()),
        );
        let cell: Retained<Self> = unsafe {
            msg_send![super(Self::alloc(mtm).set_ivars(RowLabels { title, detail, accessory })), initWithFrame: frame]
        };
        cell.setIdentifier(Some(&column.identifier()));
        let labels = cell.ivars();
        let inset = 12.0;
        let accessory_width = if labels.accessory.is_some() {
            82.0
        } else {
            0.0
        };
        let available = (frame.size.width - 2.0 * inset - accessory_width).max(0.0);
        for (label, y) in [(&labels.title, 21.0), (&labels.detail, 6.0)] {
            label.setFrame(NSRect::new(
                NSPoint::new(inset, y),
                NSSize::new(available, label.intrinsicContentSize().height),
            ));
            label.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
            cell.addSubview(label);
        }
        unsafe { cell.setTextField(Some(&labels.title)) };
        if let Some(label) = &labels.accessory {
            let height = label.intrinsicContentSize().height;
            label.setFrame(NSRect::new(
                NSPoint::new(
                    frame.size.width - inset - accessory_width + 12.0,
                    (frame.size.height - height) * 0.5,
                ),
                NSSize::new(accessory_width - 12.0, height),
            ));
            label.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMinXMargin);
            cell.addSubview(label);
        }
        cell
    }

    fn update(&self, row: RowContent) {
        let labels = self.ivars();
        labels.title.setStringValue(&NSString::from_str(&row.title));
        labels
            .detail
            .setStringValue(&NSString::from_str(&row.detail));
        if let Some(label) = &labels.accessory {
            label.setStringValue(&NSString::from_str(
                row.accessory.as_deref().unwrap_or_default(),
            ));
        }
    }
}

fn row_label(mtm: MainThreadMarker, size: f64, secondary: bool) -> Retained<NSTextField> {
    let label = NSTextField::labelWithString(&NSString::new(), mtm);
    label.setFont(Some(&NSFont::systemFontOfSize(size)));
    label.setMaximumNumberOfLines(1);
    label.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
    if secondary {
        label.setTextColor(Some(&NSColor::secondaryLabelColor()));
    }
    label
}

impl TargetState {
    fn row_count(&self) -> usize {
        let data = self.data.borrow();
        match self.kind {
            TableKind::Profiles => data.profiles.len(),
            TableKind::Nodes => data.nodes.len(),
        }
    }

    fn row_content(&self, row: NSInteger) -> Option<RowContent> {
        let row = usize::try_from(row).ok()?;
        let data = self.data.borrow();
        Some(match self.kind {
            TableKind::Profiles => {
                let profile = data.profiles.get(row)?;
                RowContent {
                    title: profile.name.clone(),
                    detail: format!("{} 个节点", profile.count),
                    accessory: None,
                }
            }
            TableKind::Nodes => {
                let node = data.nodes.get(row)?;
                RowContent {
                    title: node.name.clone(),
                    detail: node.endpoint.clone(),
                    accessory: Some(node.protocol.clone()),
                }
            }
        })
    }

    fn cell(
        &self,
        table: &NSTableView,
        column: &NSTableColumn,
        row: NSInteger,
    ) -> Option<Retained<NSView>> {
        let content = self.row_content(row)?;
        let cell = unsafe { table.makeViewWithIdentifier_owner(&column.identifier(), None) }
            .and_then(|view| view.downcast::<ListCell>().ok())
            .unwrap_or_else(|| ListCell::new(table, column, self.kind));
        cell.update(content);
        Some(Retained::into_super(Retained::into_super(cell)))
    }

    fn select(&self, row: NSInteger) {
        if self.data.borrow().suppress_selection_event.get() {
            return;
        }
        let Some(row) = usize::try_from(row).ok() else {
            return;
        };
        let data = self.data.borrow();
        match self.kind {
            TableKind::Profiles => {
                let Some(profile) = data.profiles.get(row) else {
                    return;
                };
                if data.selected.as_deref() == Some(&profile.id.to_string()) {
                    return;
                }
                self.context.emit("select", Value::from(profile.id));
            }
            TableKind::Nodes => {
                let Some(node) = data.nodes.get(row) else {
                    return;
                };
                if data.selected.as_deref() == Some(node.id.as_str()) {
                    return;
                }
                self.context.emit("select", Value::String(node.id.clone()));
            }
        }
    }
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = TargetState]
    #[name = "ExodysTableTarget"]
    struct TableTarget;

    unsafe impl NSObjectProtocol for TableTarget {}
    unsafe impl NSControlTextEditingDelegate for TableTarget {}
    unsafe impl NSTableViewDataSource for TableTarget {}
    unsafe impl NSTableViewDelegate for TableTarget {}

    impl TableTarget {
        #[unsafe(method(numberOfRowsInTableView:))]
        fn number_of_rows(&self, _table: &NSTableView) -> NSInteger {
            self.ivars().row_count() as NSInteger
        }

        #[unsafe(method_id(tableView:viewForTableColumn:row:))]
        fn cell_view(&self, table: &NSTableView, column: Option<&NSTableColumn>, row: NSInteger) -> Option<Retained<NSView>> {
            column.and_then(|column| self.ivars().cell(table, column, row))
        }

        #[unsafe(method(tableViewSelectionDidChange:))]
        fn selection_changed(&self, _notification: &NSNotification) {
            let Some(table) = self.ivars().table.borrow().as_ref().cloned() else {
                return;
            };
            self.ivars().select(table.selectedRow());
        }
    }
);
