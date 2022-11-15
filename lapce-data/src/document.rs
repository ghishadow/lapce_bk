use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
};

use druid::{
    piet::{
        PietText, PietTextLayout, Text, TextAttribute, TextLayout, TextLayoutBuilder,
    },
    Color, ExtEventSink, FontFamily, Point, Size, Target, Vec2, WidgetId,
};
use itertools::Itertools;
use lapce_core::{
    buffer::{Buffer, DiffLines, InvalLines},
    command::{EditCommand, MultiSelectionCommand},
    cursor::{ColPosition, Cursor, CursorMode},
    editor::{EditType, Editor},
    language::LapceLanguage,
    mode::{Mode, MotionMode},
    movement::{LinePosition, Movement},
    register::{Clipboard, Register, RegisterData},
    selection::{SelRegion, Selection},
    style::line_styles,
    syntax::edit::SyntaxEdit,
    syntax::{util::matching_pair_direction, Syntax},
    word::WordCursor,
};
use lapce_rpc::{
    buffer::BufferId,
    plugin::PluginId,
    proxy::ProxyResponse,
    style::{LineStyle, LineStyles, Style},
};
use lapce_xi_rope::{
    spans::{Spans, SpansBuilder},
    Interval, Rope, RopeDelta, Transformer,
};
use lsp_types::{
    CodeActionOrCommand, CodeActionResponse, DiagnosticSeverity, InlayHint,
    InlayHintLabel,
};
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

use crate::{
    atomic_soft_tabs::{snap_to_soft_tab, snap_to_soft_tab_line_col, SnapDirection},
    command::{InitBufferContentCb, LapceUICommand, LAPCE_UI_COMMAND},
    config::{LapceConfig, LapceTheme},
    data::{EditorDiagnostic, EditorView},
    editor::{EditorLocation, EditorPosition},
    find::{Find, FindProgress},
    history::DocumentHistory,
    proxy::LapceProxy,
    selection_range::{SelectionRangeDirection, SyntaxSelectionRanges},
};

pub struct SystemClipboard {}

impl SystemClipboard {
    fn clipboard() -> druid::Clipboard {
        druid::Application::global().clipboard()
    }
}

impl Clipboard for SystemClipboard {
    fn get_string(&self) -> Option<String> {
        Self::clipboard().get_string()
    }

    fn put_string(&mut self, s: impl AsRef<str>) {
        Self::clipboard().put_string(s)
    }
}

#[derive(Clone)]
pub struct LineExtraStyle {
    pub bg_color: Option<Color>,
    pub under_line: Option<Color>,
}

#[derive(Clone)]
pub struct TextLayoutLine {
    /// Extra styling that should be applied to the text
    /// (x0, x1 or line display end, style)
    pub extra_style: Vec<(f64, Option<f64>, LineExtraStyle)>,
    pub text: PietTextLayout,
    pub whitespaces: Option<Vec<(char, (f64, f64))>>,
    pub indent: f64,
}

#[derive(Clone, Default)]
pub struct TextLayoutCache {
    config_id: u64,
    pub layouts: HashMap<usize, HashMap<usize, Arc<TextLayoutLine>>>,
    pub max_width: f64,
}

impl TextLayoutCache {
    pub fn new() -> Self {
        Self {
            config_id: 0,
            layouts: HashMap::new(),
            max_width: 0.0,
        }
    }

    fn clear(&mut self) {
        self.layouts.clear();
    }

    pub fn check_attributes(&mut self, config_id: u64) {
        if self.config_id != config_id {
            self.clear();
            self.config_id = config_id;
        }
    }
}

#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Debug)]
pub enum LocalBufferKind {
    Empty,
    Palette,
    Search,
    SourceControl,
    FilePicker,
    Keymap,
    Settings,
    PathName,
    Rename,
    PluginSeach,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BufferContent {
    File(PathBuf),
    Local(LocalBufferKind),
    /// A setting input; with its name.
    SettingsValue(String),
    Scratch(BufferId, String),
}

impl BufferContent {
    pub fn path(&self) -> Option<&Path> {
        if let BufferContent::File(p) = self {
            Some(p)
        } else {
            None
        }
    }

    pub fn is_file(&self) -> bool {
        matches!(self, BufferContent::File(_))
    }

    pub fn is_special(&self) -> bool {
        match self {
            BufferContent::File(_) => false,
            BufferContent::Local(local) => match local {
                LocalBufferKind::Search
                | LocalBufferKind::Palette
                | LocalBufferKind::SourceControl
                | LocalBufferKind::FilePicker
                | LocalBufferKind::Settings
                | LocalBufferKind::Keymap
                | LocalBufferKind::PathName
                | LocalBufferKind::PluginSeach
                | LocalBufferKind::Rename => true,
                LocalBufferKind::Empty => false,
            },
            BufferContent::SettingsValue(..) => true,
            BufferContent::Scratch(..) => false,
        }
    }

    pub fn is_input(&self) -> bool {
        match self {
            BufferContent::File(_) => false,
            BufferContent::Local(local) => match local {
                LocalBufferKind::Search
                | LocalBufferKind::Palette
                | LocalBufferKind::FilePicker
                | LocalBufferKind::Settings
                | LocalBufferKind::Keymap
                | LocalBufferKind::PathName
                | LocalBufferKind::PluginSeach
                | LocalBufferKind::Rename => true,
                LocalBufferKind::Empty | LocalBufferKind::SourceControl => false,
            },
            BufferContent::SettingsValue(..) => true,
            BufferContent::Scratch(..) => false,
        }
    }

    pub fn is_palette(&self) -> bool {
        match self {
            BufferContent::File(_) => false,
            BufferContent::SettingsValue(..) => false,
            BufferContent::Scratch(..) => false,
            BufferContent::Local(local) => matches!(local, LocalBufferKind::Palette),
        }
    }

    pub fn is_search(&self) -> bool {
        match self {
            BufferContent::File(_) => false,
            BufferContent::SettingsValue(..) => false,
            BufferContent::Scratch(..) => false,
            BufferContent::Local(local) => matches!(local, LocalBufferKind::Search),
        }
    }

    pub fn is_settings(&self) -> bool {
        match self {
            BufferContent::File(_) => false,
            BufferContent::SettingsValue(..) => true,
            BufferContent::Local(_) => false,
            BufferContent::Scratch(..) => false,
        }
    }

    pub fn file_name(&self) -> &str {
        match self {
            BufferContent::File(p) => {
                p.file_name().and_then(|f| f.to_str()).unwrap_or("")
            }
            BufferContent::Scratch(_, scratch_doc_name) => scratch_doc_name,
            _ => "",
        }
    }
}

pub struct PhantomText {
    kind: PhantomTextKind,
    col: usize,
    text: String,
    font_size: Option<usize>,
    font_family: Option<FontFamily>,
    fg: Option<Color>,
    bg: Option<Color>,
    under_line: Option<Color>,
}

#[derive(Ord, Eq, PartialEq, PartialOrd)]
pub enum PhantomTextKind {
    Ime,
    InlayHint,
    Diagnostic,
}

#[derive(Default)]
pub struct PhantomTextLine {
    text: SmallVec<[PhantomText; 6]>,
    max_severity: Option<DiagnosticSeverity>,
}

impl PhantomTextLine {
    /// Translate a column position into the text into what it would be after combining
    pub fn col_at(&self, pre_col: usize) -> usize {
        let mut last = pre_col;
        for (col_shift, size, col, _) in self.offset_size_iter() {
            if pre_col >= col {
                last = pre_col + col_shift + size;
            }
        }

        last
    }

    /// Translate a column position into the text into what it would be after combining
    /// If before_cursor is false and the cursor is right at the start then it will stay there
    pub fn col_after(&self, pre_col: usize, before_cursor: bool) -> usize {
        let mut last = pre_col;
        for (col_shift, size, col, _) in self.offset_size_iter() {
            if pre_col > col || (pre_col == col && before_cursor) {
                last = pre_col + col_shift + size;
            }
        }

        last
    }

    /// Translate a column position into the position it would be before combining
    pub fn before_col(&self, col: usize) -> usize {
        let mut last = col;
        for (col_shift, size, hint_col, _) in self.offset_size_iter() {
            let shifted_start = hint_col + col_shift;
            let shifted_end = shifted_start + size;
            if col >= shifted_start {
                if col >= shifted_end {
                    last = col - col_shift - size;
                } else {
                    last = hint_col;
                }
            }
        }
        last
    }

    /// Insert the hints at their positions in the text
    pub fn combine_with_text(&self, text: String) -> String {
        let mut text = text;
        let mut col_shift = 0;

        for phantom in self.text.iter() {
            let location = phantom.col + col_shift;

            // Stop iterating if the location is bad
            if text.get(location..).is_none() {
                return text;
            }

            text.insert_str(location, &phantom.text);
            col_shift += phantom.text.len();
        }

        text
    }

    /// Iterator over (col_shift, size, hint, pre_column)
    /// Note that this only iterates over the ordered text, since those depend on the text for where
    /// they'll be positioned
    pub fn offset_size_iter(
        &self,
    ) -> impl Iterator<Item = (usize, usize, usize, &PhantomText)> + '_ {
        let mut col_shift = 0;

        self.text.iter().map(move |phantom| {
            let pre_col_shift = col_shift;
            col_shift += phantom.text.len();
            (
                pre_col_shift,
                col_shift - pre_col_shift,
                phantom.col,
                phantom,
            )
        })
    }
}

#[derive(Clone)]
pub struct Document {
    id: BufferId,
    pub tab_id: WidgetId,
    buffer: Buffer,
    content: BufferContent,
    syntax: Option<Syntax>,
    line_styles: Rc<RefCell<LineStyles>>,
    semantic_styles: Option<Arc<Spans<Style>>>,
    pub text_layouts: Rc<RefCell<TextLayoutCache>>,
    pub sticky_headers: Rc<RefCell<HashMap<usize, Option<Vec<usize>>>>>,
    load_started: Rc<RefCell<bool>>,
    loaded: bool,
    histories: im::HashMap<String, DocumentHistory>,
    pub cursor_offset: usize,
    pub scroll_offset: Vec2,
    pub code_actions: im::HashMap<usize, (PluginId, CodeActionResponse)>,
    pub inlay_hints: Option<Spans<InlayHint>>,
    pub diagnostics: Option<Arc<Vec<EditorDiagnostic>>>,
    ime_text: Option<Arc<String>>,
    ime_pos: (usize, usize, usize),
    pub syntax_selection_range: Option<SyntaxSelectionRanges>,
    pub find: Rc<RefCell<Find>>,
    find_progress: Rc<RefCell<FindProgress>>,
    pub event_sink: ExtEventSink,
    pub proxy: Arc<LapceProxy>,
}

impl Document {
    pub fn new(
        content: BufferContent,
        tab_id: WidgetId,
        event_sink: ExtEventSink,
        proxy: Arc<LapceProxy>,
    ) -> Self {
        let syntax = match &content {
            BufferContent::File(path) => Syntax::init(path),
            BufferContent::Local(_) => None,
            BufferContent::SettingsValue(..) => None,
            BufferContent::Scratch(..) => None,
        };
        let id = match &content {
            BufferContent::Scratch(id, _) => *id,
            _ => BufferId::next(),
        };

        Self {
            id,
            tab_id,
            buffer: Buffer::new(""),
            content,
            syntax,
            line_styles: Rc::new(RefCell::new(HashMap::new())),
            text_layouts: Rc::new(RefCell::new(TextLayoutCache::new())),
            sticky_headers: Rc::new(RefCell::new(HashMap::new())),
            semantic_styles: None,
            load_started: Rc::new(RefCell::new(false)),
            histories: im::HashMap::new(),
            loaded: false,
            cursor_offset: 0,
            scroll_offset: Vec2::ZERO,
            code_actions: im::HashMap::new(),
            inlay_hints: None,
            diagnostics: None,
            ime_text: None,
            ime_pos: (0, 0, 0),
            find: Rc::new(RefCell::new(Find::new(0))),
            find_progress: Rc::new(RefCell::new(FindProgress::Ready)),
            event_sink,
            proxy,
            syntax_selection_range: None,
        }
    }

    pub fn id(&self) -> BufferId {
        self.id
    }

    pub fn loaded(&self) -> bool {
        self.loaded
    }

    pub fn set_content(&mut self, content: BufferContent) {
        self.content = content;
        self.syntax = match &self.content {
            BufferContent::File(path) => Syntax::init(path),
            BufferContent::Local(_) => None,
            BufferContent::SettingsValue(..) => None,
            BufferContent::Scratch(..) => None,
        };
        self.on_update(None);
    }

    pub fn content(&self) -> &BufferContent {
        &self.content
    }

    pub fn rev(&self) -> u64 {
        self.buffer.rev()
    }

    pub fn init_content(&mut self, content: Rope) {
        self.buffer.init_content(content);
        self.buffer.detect_indent(self.syntax.as_ref());
        self.loaded = true;
        self.on_update(None);
    }

    pub fn set_language(&mut self, language: LapceLanguage) {
        self.syntax = Some(Syntax::from_language(language));
    }

    pub fn set_diagnostics(&mut self, diagnostics: &[EditorDiagnostic]) {
        self.clear_text_layout_cache();
        self.diagnostics = Some(Arc::new(
            diagnostics
                .iter()
                // We discard diagnostics that have bad positions
                .map(|d| EditorDiagnostic {
                    range: (
                        self.buffer.offset_of_position(&d.diagnostic.range.start),
                        self.buffer.offset_of_position(&d.diagnostic.range.end),
                    ),
                    lines: d.lines,
                    diagnostic: d.diagnostic.clone(),
                })
                .collect(),
        ));
    }

    fn update_diagnostics(&mut self, delta: &RopeDelta) {
        if let Some(mut diagnostics) = self.diagnostics.clone() {
            for diagnostic in Arc::make_mut(&mut diagnostics).iter_mut() {
                let mut transformer = Transformer::new(delta);
                let (start, end) = diagnostic.range;
                let (new_start, new_end) = (
                    transformer.transform(start, false),
                    transformer.transform(end, true),
                );

                let new_start_pos = self.buffer().offset_to_position(new_start);

                let new_end_pos = self.buffer().offset_to_position(new_end);

                diagnostic.range = (new_start, new_end);

                diagnostic.diagnostic.range.start = new_start_pos;
                diagnostic.diagnostic.range.end = new_end_pos;
            }
            self.diagnostics = Some(diagnostics);
        }
    }

    pub fn reload(&mut self, content: Rope, set_pristine: bool) {
        self.code_actions.clear();
        self.inlay_hints = None;
        let delta = self.buffer.reload(content, set_pristine);
        self.apply_deltas(&[delta]);
    }

    pub fn handle_file_changed(&mut self, content: Rope) {
        if self.buffer.is_pristine() {
            self.reload(content, true);
        }
    }

    pub fn retrieve_file<P: EditorPosition + Send + 'static>(
        &mut self,
        locations: Vec<(WidgetId, EditorLocation<P>)>,
        unsaved_buffer: Option<Rope>,
        cb: Option<InitBufferContentCb>,
    ) {
        if self.loaded || *self.load_started.borrow() {
            return;
        }

        *self.load_started.borrow_mut() = true;
        if let BufferContent::File(path) = &self.content {
            let id = self.id;
            let tab_id = self.tab_id;
            let path = path.clone();
            let event_sink = self.event_sink.clone();
            let proxy = self.proxy.clone();
            std::thread::spawn(move || {
                proxy.proxy_rpc.new_buffer(id, path.clone(), move |result| {
                    if let Ok(ProxyResponse::NewBufferResponse { content }) = result
                    {
                        let _ = event_sink.submit_command(
                            LAPCE_UI_COMMAND,
                            P::init_buffer_content_cmd(
                                path,
                                Rope::from(content),
                                locations,
                                unsaved_buffer,
                                cb,
                            ),
                            Target::Widget(tab_id),
                        );
                    };
                })
            });
        }

        self.retrieve_history("head");
    }

    pub fn retrieve_history(&mut self, version: &str) {
        if self.histories.contains_key(version) {
            return;
        }

        let history = DocumentHistory::new(version.to_string());
        history.retrieve(self);
        self.histories.insert(version.to_string(), history);
    }

    pub fn reload_history(&self, version: &str) {
        if let Some(history) = self.histories.get(version) {
            history.retrieve(self);
        }
    }

    pub fn load_history(&mut self, version: &str, content: Rope) {
        let mut history = DocumentHistory::new(version.to_string());
        history.load_content(content, self);
        self.histories.insert(version.to_string(), history);
    }

    pub fn get_history(&self, version: &str) -> Option<&DocumentHistory> {
        self.histories.get(version)
    }

    pub fn history_visual_line(&self, version: &str, line: usize) -> usize {
        let mut visual_line = 0;
        if let Some(history) = self.histories.get(version) {
            for (_i, change) in history.changes().iter().enumerate() {
                match change {
                    DiffLines::Left(range) => {
                        visual_line += range.len();
                    }
                    DiffLines::Both(_, r) | DiffLines::Right(r) => {
                        if r.contains(&line) {
                            visual_line += line - r.start;
                            break;
                        }
                        visual_line += r.len();
                    }
                    DiffLines::Skip(_, r) => {
                        if r.contains(&line) {
                            break;
                        }
                        visual_line += 1;
                    }
                }
            }
        }
        visual_line
    }

    pub fn history_actual_line_from_visual(
        &self,
        version: &str,
        visual_line: usize,
    ) -> usize {
        let mut current_visual_line = 0;
        let mut line = 0;
        if let Some(history) = self.histories.get(version) {
            for (i, change) in history.changes().iter().enumerate() {
                match change {
                    DiffLines::Left(range) => {
                        current_visual_line += range.len();
                        if current_visual_line > visual_line {
                            if let Some(change) = history.changes().get(i + 1) {
                                match change {
                                    DiffLines::Left(_) => {}
                                    DiffLines::Both(_, r)
                                    | DiffLines::Skip(_, r)
                                    | DiffLines::Right(r) => {
                                        line = r.start;
                                    }
                                }
                            } else if i > 0 {
                                if let Some(change) = history.changes().get(i - 1) {
                                    match change {
                                        DiffLines::Left(_) => {}
                                        DiffLines::Both(_, r)
                                        | DiffLines::Skip(_, r)
                                        | DiffLines::Right(r) => {
                                            line = r.end - 1;
                                        }
                                    }
                                }
                            }
                            break;
                        }
                    }
                    DiffLines::Skip(_, r) => {
                        current_visual_line += 1;
                        if current_visual_line > visual_line {
                            line = r.end;
                            break;
                        }
                    }
                    DiffLines::Both(_, r) | DiffLines::Right(r) => {
                        current_visual_line += r.len();
                        if current_visual_line > visual_line {
                            line = r.end - (current_visual_line - visual_line);
                            break;
                        }
                    }
                }
            }
        }
        if current_visual_line <= visual_line {
            self.buffer.last_line()
        } else {
            line
        }
    }

    fn trigger_head_change(&self) {
        if let Some(head) = self.histories.get("head") {
            head.trigger_update_change(self);
        }
    }

    pub fn update_history_changes(
        &mut self,
        rev: u64,
        version: &str,
        changes: Arc<Vec<DiffLines>>,
    ) {
        if rev != self.rev() {
            return;
        }
        if let Some(history) = self.histories.get_mut(version) {
            history.update_changes(changes);
        }
    }

    pub fn update_history_styles(
        &mut self,
        version: &str,
        styles: Arc<Spans<Style>>,
    ) {
        if let Some(history) = self.histories.get_mut(version) {
            history.update_styles(styles);
        }
    }

    fn get_semantic_styles(&self) {
        if !self.loaded() {
            return;
        }

        if !self.content().is_file() {
            return;
        }
        if let BufferContent::File(path) = self.content() {
            let tab_id = self.tab_id;
            let path = path.clone();
            let buffer_id = self.id();
            let buffer = self.buffer();
            let rev = buffer.rev();
            let len = buffer.len();
            let event_sink = self.event_sink.clone();
            let syntactic_styles =
                self.syntax().and_then(|s| s.styles.as_ref()).cloned();

            self.proxy
                .proxy_rpc
                .get_semantic_tokens(path.clone(), move |result| {
                    if let Ok(ProxyResponse::GetSemanticTokens { styles }) = result {
                        rayon::spawn(move || {
                            let mut styles_span = SpansBuilder::new(len);
                            for style in styles.styles {
                                styles_span.add_span(
                                    Interval::new(style.start, style.end),
                                    style.style,
                                );
                            }

                            let styles = styles_span.build();

                            let styles =
                                if let Some(syntactic_styles) = syntactic_styles {
                                    syntactic_styles.merge(&styles, |a, b| {
                                        if let Some(b) = b {
                                            return b.clone();
                                        }
                                        a.clone()
                                    })
                                } else {
                                    styles
                                };
                            let styles = Arc::new(styles);

                            let _ = event_sink.submit_command(
                                LAPCE_UI_COMMAND,
                                LapceUICommand::UpdateSemanticStyles(
                                    buffer_id, path, rev, styles,
                                ),
                                Target::Widget(tab_id),
                            );
                        });
                    }
                });
        }
    }

    pub fn get_inlay_hints(&self) {
        if !self.loaded() {
            return;
        }

        if !self.content().is_file() {
            return;
        }

        if let BufferContent::File(path) = self.content() {
            let tab_id = self.tab_id;
            let path = path.clone();
            let buffer = self.buffer().clone();
            let rev = buffer.rev();
            let len = buffer.len();
            let event_sink = self.event_sink.clone();
            self.proxy
                .proxy_rpc
                .get_inlay_hints(path.clone(), move |result| {
                    if let Ok(ProxyResponse::GetInlayHints { mut hints }) = result {
                        // Sort the inlay hints by their position, as the LSP does not guarantee that it will
                        // provide them in the order that they are in within the file
                        // as well, Spans does not iterate in the order that they appear
                        hints.sort_by(|left, right| {
                            left.position.cmp(&right.position)
                        });

                        let mut hints_span = SpansBuilder::new(len);
                        for hint in hints {
                            let offset =
                                buffer.offset_of_position(&hint.position).min(len);
                            hints_span.add_span(
                                Interval::new(offset, (offset + 1).min(len)),
                                hint,
                            );
                        }
                        let hints = hints_span.build();
                        let _ = event_sink.submit_command(
                            LAPCE_UI_COMMAND,
                            LapceUICommand::UpdateInlayHints { path, rev, hints },
                            Target::Widget(tab_id),
                        );
                    }
                });
        }
    }

    fn on_update(&mut self, edits: Option<SmallVec<[SyntaxEdit; 3]>>) {
        self.find.borrow_mut().unset();
        *self.find_progress.borrow_mut() = FindProgress::Started;
        self.get_inlay_hints();
        self.clear_style_cache();
        self.trigger_syntax_change(edits);
        self.get_semantic_styles();
        self.clear_sticky_headers_cache();
        self.trigger_head_change();
        self.notify_special();
    }

    fn notify_special(&self) {
        match &self.content {
            BufferContent::File(_) => {}
            BufferContent::Scratch(..) => {}
            BufferContent::Local(local) => {
                let s = self.buffer.to_string();
                match local {
                    LocalBufferKind::Search => {
                        let _ = self.event_sink.submit_command(
                            LAPCE_UI_COMMAND,
                            LapceUICommand::UpdateSearch(s),
                            Target::Widget(self.tab_id),
                        );
                    }
                    LocalBufferKind::PluginSeach => {}
                    LocalBufferKind::SourceControl => {}
                    LocalBufferKind::Empty => {}
                    LocalBufferKind::Rename => {}
                    LocalBufferKind::Palette => {
                        let _ = self.event_sink.submit_command(
                            LAPCE_UI_COMMAND,
                            LapceUICommand::UpdatePaletteInput(s),
                            Target::Widget(self.tab_id),
                        );
                    }
                    LocalBufferKind::FilePicker => {
                        let pwd = PathBuf::from(s);
                        let _ = self.event_sink.submit_command(
                            LAPCE_UI_COMMAND,
                            LapceUICommand::UpdatePickerPwd(pwd),
                            Target::Widget(self.tab_id),
                        );
                    }
                    LocalBufferKind::Keymap => {
                        let _ = self.event_sink.submit_command(
                            LAPCE_UI_COMMAND,
                            LapceUICommand::UpdateKeymapsFilter(s),
                            Target::Widget(self.tab_id),
                        );
                    }
                    LocalBufferKind::Settings => {
                        let _ = self.event_sink.submit_command(
                            LAPCE_UI_COMMAND,
                            LapceUICommand::UpdateSettingsFilter(s),
                            Target::Widget(self.tab_id),
                        );
                    }
                    LocalBufferKind::PathName => {
                        // TODO: anything to update with this?
                    }
                }
            }
            BufferContent::SettingsValue(..) => {}
        }
    }

    pub fn set_syntax(&mut self, syntax: Option<Syntax>) {
        self.syntax = syntax;
        if self.semantic_styles.is_none() {
            self.clear_style_cache();
        }
        self.clear_sticky_headers_cache();
    }

    fn clear_sticky_headers_cache(&self) {
        self.sticky_headers.borrow_mut().clear();
    }

    pub fn set_semantic_styles(&mut self, styles: Option<Arc<Spans<Style>>>) {
        self.semantic_styles = styles;
        self.clear_style_cache();
    }

    fn clear_style_cache(&self) {
        self.line_styles.borrow_mut().clear();
        self.clear_text_layout_cache();
    }

    fn clear_text_layout_cache(&self) {
        self.text_layouts.borrow_mut().clear();
    }

    pub fn trigger_syntax_change(
        &mut self,
        edits: Option<SmallVec<[SyntaxEdit; 3]>>,
    ) {
        if let Some(syntax) = self.syntax.as_mut() {
            let rev = self.buffer.rev();
            let text = self.buffer.text().clone();

            syntax.parse(rev, text, edits.as_deref());
        }
    }

    /// Update the inlay hints with new ones
    /// Clears any caches that need to be updated after change
    pub fn set_inlay_hints(&mut self, hints: Spans<InlayHint>) {
        self.inlay_hints = Some(hints);
        self.clear_text_layout_cache();
    }

    pub fn buffer(&self) -> &Buffer {
        &self.buffer
    }

    pub fn buffer_mut(&mut self) -> &mut Buffer {
        &mut self.buffer
    }

    pub fn syntax(&self) -> Option<&Syntax> {
        self.syntax.as_ref()
    }

    fn update_styles(&mut self, delta: &RopeDelta) {
        if let Some(styles) = self.semantic_styles.as_mut() {
            Arc::make_mut(styles).apply_shape(delta);
        }
        if let Some(syntax) = self.syntax.as_mut() {
            if let Some(styles) = syntax.styles.as_mut() {
                Arc::make_mut(styles).apply_shape(delta);
            }
        }

        if let Some(syntax) = self.syntax.as_mut() {
            syntax.lens.apply_delta(delta);
        }
    }

    fn update_inlay_hints(&mut self, delta: &RopeDelta) {
        if let Some(hints) = self.inlay_hints.as_mut() {
            hints.apply_shape(delta);
        }
    }

    pub fn set_ime_pos(&mut self, line: usize, col: usize, shift: usize) {
        self.ime_pos = (line, col, shift);
    }

    pub fn ime_text(&self) -> Option<&Arc<String>> {
        self.ime_text.as_ref()
    }

    pub fn ime_pos(&self) -> (usize, usize, usize) {
        self.ime_pos
    }

    pub fn set_ime_text(&mut self, text: String) {
        self.ime_text = Some(Arc::new(text));
        self.clear_text_layout_cache();
    }

    pub fn clear_ime_text(&mut self) {
        if self.ime_text.is_some() {
            self.ime_text = None;
            self.clear_text_layout_cache();
        }
    }

    pub fn line_phantom_text(
        &self,
        config: &LapceConfig,
        line: usize,
    ) -> PhantomTextLine {
        let start_offset = self.buffer.offset_of_line(line);
        let end_offset = self.buffer.offset_of_line(line + 1);

        let hints = config
            .editor
            .enable_inlay_hints
            .then_some(())
            .and_then(|_| {
                self.inlay_hints.as_ref().map(|hints| {
                    let chunks = hints.iter_chunks(start_offset..end_offset);
                    chunks.filter_map(|(interval, inlay_hint)| {
                        let on_line = interval.start >= start_offset
                            && interval.start < end_offset;
                        on_line.then(|| {
                            let (_, col) =
                                self.buffer.offset_to_line_col(interval.start);
                            let text = match &inlay_hint.label {
                                InlayHintLabel::String(label) => label.to_string(),
                                InlayHintLabel::LabelParts(parts) => {
                                    parts.iter().map(|p| &p.value).join("")
                                }
                            };
                            PhantomText {
                                kind: PhantomTextKind::InlayHint,
                                col,
                                text,
                                fg: Some(
                                    config
                                        .get_color_unchecked(
                                            LapceTheme::INLAY_HINT_FOREGROUND,
                                        )
                                        .clone(),
                                ),
                                font_family: Some(
                                    config.editor.inlay_hint_font_family(),
                                ),
                                font_size: Some(
                                    config.editor.inlay_hint_font_size(),
                                ),
                                bg: Some(
                                    config
                                        .get_color_unchecked(
                                            LapceTheme::INLAY_HINT_BACKGROUND,
                                        )
                                        .clone(),
                                ),
                                under_line: None,
                            }
                        })
                    })
                })
            });
        let mut text: SmallVec<[PhantomText; 6]> =
            hints.into_iter().flatten().collect();

        let mut max_severity = None;
        let diag_text =
            config.editor.enable_error_lens.then_some(()).and_then(|_| {
                self.diagnostics.as_ref().map(|diags| {
                    diags
                        .iter()
                        .filter(|diag| {
                            diag.diagnostic.range.end.line as usize == line
                                && diag.diagnostic.severity
                                    < Some(DiagnosticSeverity::HINT)
                        })
                        .map(|diag| {
                            match (diag.diagnostic.severity, max_severity) {
                                (Some(severity), Some(max)) => {
                                    if severity < max {
                                        max_severity = Some(severity);
                                    }
                                }
                                (Some(severity), None) => {
                                    max_severity = Some(severity);
                                }
                                _ => {}
                            }

                            let rope_text = self.buffer.rope_text();
                            let col = rope_text.offset_of_line(line + 1)
                                - rope_text.offset_of_line(line);
                            let fg = {
                                let severity = diag
                                    .diagnostic
                                    .severity
                                    .unwrap_or(DiagnosticSeverity::WARNING);
                                let theme_prop = if severity
                                    == DiagnosticSeverity::ERROR
                                {
                                    LapceTheme::ERROR_LENS_ERROR_FOREGROUND
                                } else if severity == DiagnosticSeverity::WARNING {
                                    LapceTheme::ERROR_LENS_WARNING_FOREGROUND
                                } else {
                                    // information + hint (if we keep that) + things without a severity
                                    LapceTheme::ERROR_LENS_OTHER_FOREGROUND
                                };

                                config.get_color_unchecked(theme_prop).clone()
                            };
                            let text = format!(
                                "    {}",
                                diag.diagnostic.message.lines().join(" ")
                            );
                            PhantomText {
                                kind: PhantomTextKind::Diagnostic,
                                col,
                                text,
                                fg: Some(fg),
                                font_size: Some(
                                    config.editor.error_lens_font_size(),
                                ),
                                font_family: Some(
                                    config.editor.error_lens_font_family(),
                                ),
                                bg: None,
                                under_line: None,
                            }
                        })
                })
            });
        let mut diag_text: SmallVec<[PhantomText; 6]> =
            diag_text.into_iter().flatten().collect();

        text.append(&mut diag_text);

        if let Some(ime_text) = self.ime_text.as_ref() {
            let (ime_line, col, _) = self.ime_pos;
            if line == ime_line {
                text.push(PhantomText {
                    kind: PhantomTextKind::Ime,
                    text: ime_text.to_string(),
                    col,
                    font_size: None,
                    font_family: None,
                    fg: None,
                    bg: None,
                    under_line: Some(
                        config
                            .get_color_unchecked(LapceTheme::EDITOR_FOREGROUND)
                            .clone(),
                    ),
                });
            }
        }

        text.sort_by(|a, b| {
            if a.col == b.col {
                a.kind.cmp(&b.kind)
            } else {
                a.col.cmp(&b.col)
            }
        });

        PhantomTextLine { text, max_severity }
    }

    fn apply_deltas(&mut self, deltas: &[(RopeDelta, InvalLines, SyntaxEdit)]) {
        let rev = self.rev() - deltas.len() as u64;
        for (i, (delta, _, _)) in deltas.iter().enumerate() {
            self.update_styles(delta);
            self.update_inlay_hints(delta);
            self.update_diagnostics(delta);
            if let BufferContent::File(path) = &self.content {
                self.proxy.proxy_rpc.update(
                    path.clone(),
                    delta.clone(),
                    rev + i as u64 + 1,
                );
            }
        }

        // TODO(minor): We could avoid this potential allocation since most apply_delta callers are actually using a Vec
        // which we could reuse.
        // We use a smallvec because there is unlikely to be more than a couple of deltas
        let edits = deltas.iter().map(|(_, _, edits)| edits.clone()).collect();
        self.on_update(Some(edits));
    }

    pub fn do_insert(
        &mut self,
        cursor: &mut Cursor,
        s: &str,
        config: &LapceConfig,
    ) -> Vec<(RopeDelta, InvalLines, SyntaxEdit)> {
        let old_cursor = cursor.mode.clone();
        let deltas = Editor::insert(
            cursor,
            &mut self.buffer,
            s,
            self.syntax.as_ref(),
            config.editor.auto_closing_matching_pairs,
        );
        self.buffer_mut().set_cursor_before(old_cursor);
        self.buffer_mut().set_cursor_after(cursor.mode.clone());
        self.apply_deltas(&deltas);
        deltas
    }

    pub fn do_raw_edit(
        &mut self,
        edits: &[(impl AsRef<Selection>, &str)],
        edit_type: EditType,
    ) -> (RopeDelta, InvalLines, SyntaxEdit) {
        let (delta, inval_lines, edits) = self.buffer.edit(edits, edit_type);
        self.apply_deltas(&[(delta.clone(), inval_lines.clone(), edits.clone())]);
        (delta, inval_lines, edits)
    }

    pub fn do_edit(
        &mut self,
        cursor: &mut Cursor,
        cmd: &EditCommand,
        modal: bool,
        register: &mut Register,
    ) -> Vec<(RopeDelta, InvalLines, SyntaxEdit)> {
        let mut clipboard = SystemClipboard {};
        let old_cursor = cursor.mode.clone();
        let deltas = Editor::do_edit(
            cursor,
            &mut self.buffer,
            cmd,
            self.syntax.as_ref(),
            &mut clipboard,
            modal,
            register,
        );
        self.buffer_mut().set_cursor_before(old_cursor);
        self.buffer_mut().set_cursor_after(cursor.mode.clone());
        self.apply_deltas(&deltas);
        deltas
    }

    pub fn do_multi_selection(
        &self,
        text: &mut PietText,
        cursor: &mut Cursor,
        cmd: &MultiSelectionCommand,
        view: &EditorView,
        config: &LapceConfig,
    ) {
        use MultiSelectionCommand::*;
        match cmd {
            SelectUndo => {
                if let CursorMode::Insert(_) = cursor.mode.clone() {
                    if let Some(selection) =
                        cursor.history_selections.last().cloned()
                    {
                        cursor.mode = CursorMode::Insert(selection);
                    }
                    cursor.history_selections.pop();
                }
            }
            InsertCursorAbove => {
                if let CursorMode::Insert(mut selection) = cursor.mode.clone() {
                    let offset = selection.first().map(|s| s.end).unwrap_or(0);
                    let (new_offset, _) = self.move_offset(
                        text,
                        offset,
                        cursor.horiz.as_ref(),
                        1,
                        &Movement::Up,
                        Mode::Insert,
                        view,
                        config,
                    );
                    if new_offset != offset {
                        selection.add_region(SelRegion::new(
                            new_offset, new_offset, None,
                        ));
                    }
                    cursor.set_insert(selection);
                }
            }
            InsertCursorBelow => {
                if let CursorMode::Insert(mut selection) = cursor.mode.clone() {
                    let offset = selection.last().map(|s| s.end).unwrap_or(0);
                    let (new_offset, _) = self.move_offset(
                        text,
                        offset,
                        cursor.horiz.as_ref(),
                        1,
                        &Movement::Down,
                        Mode::Insert,
                        view,
                        config,
                    );
                    if new_offset != offset {
                        selection.add_region(SelRegion::new(
                            new_offset, new_offset, None,
                        ));
                    }
                    cursor.set_insert(selection);
                }
            }
            InsertCursorEndOfLine => {
                if let CursorMode::Insert(selection) = cursor.mode.clone() {
                    let mut new_selection = Selection::new();
                    for region in selection.regions() {
                        let (start_line, _) =
                            self.buffer.offset_to_line_col(region.min());
                        let (end_line, end_col) =
                            self.buffer.offset_to_line_col(region.max());
                        for line in start_line..end_line + 1 {
                            let offset = if line == end_line {
                                self.buffer.offset_of_line_col(line, end_col)
                            } else {
                                self.buffer.line_end_offset(line, true)
                            };
                            new_selection
                                .add_region(SelRegion::new(offset, offset, None));
                        }
                    }
                    cursor.set_insert(new_selection);
                }
            }
            SelectCurrentLine => {
                if let CursorMode::Insert(selection) = cursor.mode.clone() {
                    let mut new_selection = Selection::new();
                    for region in selection.regions() {
                        let start_line = self.buffer.line_of_offset(region.min());
                        let start = self.buffer.offset_of_line(start_line);
                        let end_line = self.buffer.line_of_offset(region.max());
                        let end = self.buffer.offset_of_line(end_line + 1);
                        new_selection.add_region(SelRegion::new(start, end, None));
                    }
                    cursor.set_insert(selection);
                }
            }
            SelectAllCurrent => {
                if let CursorMode::Insert(mut selection) = cursor.mode.clone() {
                    if !selection.is_empty() {
                        let first = selection.first().unwrap();
                        let (start, end) = if first.is_caret() {
                            self.buffer.select_word(first.start)
                        } else {
                            (first.min(), first.max())
                        };
                        let search_str = self.buffer.slice_to_cow(start..end);
                        let case_sensitive = self.find.borrow().case_sensitive();
                        let multicursor_case_sensitive =
                            config.editor.multicursor_case_sensitive;
                        let case_sensitive =
                            multicursor_case_sensitive || case_sensitive;
                        let search_whole_word =
                            config.editor.multicursor_whole_words;
                        let mut find = Find::new(0);
                        find.set_case_sensitive(case_sensitive);
                        find.set_find(&search_str, false, search_whole_word);
                        let mut offset = 0;
                        while let Some((start, end)) =
                            find.next(self.buffer.text(), offset, false, false)
                        {
                            offset = end;
                            selection.add_region(SelRegion::new(start, end, None));
                        }
                    }
                    cursor.set_insert(selection);
                }
            }
            SelectNextCurrent => {
                if let CursorMode::Insert(mut selection) = cursor.mode.clone() {
                    if !selection.is_empty() {
                        let mut had_caret = false;
                        for region in selection.regions_mut() {
                            if region.is_caret() {
                                had_caret = true;
                                let (start, end) =
                                    self.buffer.select_word(region.start);
                                region.start = start;
                                region.end = end;
                            }
                        }
                        if !had_caret {
                            let r = selection.last_inserted().unwrap();
                            let search_str =
                                self.buffer.slice_to_cow(r.min()..r.max());
                            let case_sensitive = self.find.borrow().case_sensitive();
                            let case_sensitive =
                                config.editor.multicursor_case_sensitive
                                    || case_sensitive;
                            let search_whole_word =
                                config.editor.multicursor_whole_words;
                            let mut find = Find::new(0);
                            find.set_case_sensitive(case_sensitive);
                            find.set_find(&search_str, false, search_whole_word);
                            let mut offset = r.max();
                            let mut seen = HashSet::new();
                            while let Some((start, end)) =
                                find.next(self.buffer.text(), offset, false, true)
                            {
                                if !selection
                                    .regions()
                                    .iter()
                                    .any(|r| r.min() == start && r.max() == end)
                                {
                                    selection.add_region(SelRegion::new(
                                        start, end, None,
                                    ));
                                    break;
                                }
                                if seen.contains(&end) {
                                    break;
                                }
                                offset = end;
                                seen.insert(offset);
                            }
                        }
                    }
                    cursor.set_insert(selection);
                }
            }
            SelectSkipCurrent => {
                if let CursorMode::Insert(mut selection) = cursor.mode.clone() {
                    if !selection.is_empty() {
                        let r = selection.last_inserted().unwrap();
                        if r.is_caret() {
                            let (start, end) = self.buffer.select_word(r.start);
                            selection.replace_last_inserted_region(SelRegion::new(
                                start, end, None,
                            ));
                        } else {
                            let search_str =
                                self.buffer.slice_to_cow(r.min()..r.max());
                            let case_sensitive = self.find.borrow().case_sensitive();
                            let mut find = Find::new(0);
                            find.set_case_sensitive(case_sensitive);
                            find.set_find(&search_str, false, false);
                            let mut offset = r.max();
                            let mut seen = HashSet::new();
                            while let Some((start, end)) =
                                find.next(self.buffer.text(), offset, false, true)
                            {
                                if !selection
                                    .regions()
                                    .iter()
                                    .any(|r| r.min() == start && r.max() == end)
                                {
                                    selection.replace_last_inserted_region(
                                        SelRegion::new(start, end, None),
                                    );
                                    break;
                                }
                                if seen.contains(&end) {
                                    break;
                                }
                                offset = end;
                                seen.insert(offset);
                            }
                        }
                    }
                    cursor.set_insert(selection);
                }
            }
            SelectAll => {
                let new_selection = Selection::region(0, self.buffer.len());
                cursor.set_insert(new_selection);
            }
        }
    }

    pub fn do_motion_mode(
        &mut self,
        cursor: &mut Cursor,
        motion_mode: MotionMode,
        register: &mut Register,
    ) {
        if let Some(m) = &cursor.motion_mode {
            if m == &motion_mode {
                let offset = cursor.offset();
                let deltas = Editor::execute_motion_mode(
                    cursor,
                    &mut self.buffer,
                    motion_mode,
                    offset,
                    offset,
                    true,
                    register,
                );
                self.apply_deltas(&deltas);
            }
            cursor.motion_mode = None;
        } else {
            cursor.motion_mode = Some(motion_mode);
        }
    }

    pub fn do_paste(&mut self, cursor: &mut Cursor, data: &RegisterData) {
        let deltas = Editor::do_paste(cursor, &mut self.buffer, data);
        self.apply_deltas(&deltas)
    }

    pub fn styles(&self) -> Option<&Arc<Spans<Style>>> {
        if let Some(semantic_styles) = self.semantic_styles.as_ref() {
            Some(semantic_styles)
        } else {
            self.syntax().and_then(|s| s.styles.as_ref())
        }
    }

    fn line_style(&self, line: usize) -> Arc<Vec<LineStyle>> {
        if self.line_styles.borrow().get(&line).is_none() {
            let styles = self.styles();

            let line_styles = styles
                .map(|styles| line_styles(self.buffer.text(), line, styles))
                .unwrap_or_default();
            self.line_styles
                .borrow_mut()
                .insert(line, Arc::new(line_styles));
        }
        self.line_styles.borrow().get(&line).cloned().unwrap()
    }

    pub fn line_col_of_point(
        &self,
        text: &mut PietText,
        mode: Mode,
        point: Point,
        view: &EditorView,
        config: &LapceConfig,
    ) -> ((usize, usize), bool) {
        let (line, font_size) = match view {
            EditorView::Diff(version) => {
                if let Some(history) = self.get_history(version) {
                    let line_height = config.editor.line_height();
                    let mut line = 0;
                    let mut lines = 0;
                    for change in history.changes().iter() {
                        match change {
                            DiffLines::Left(l) => {
                                lines += l.len();
                                if (lines * line_height) as f64 > point.y {
                                    break;
                                }
                            }
                            DiffLines::Skip(_l, r) => {
                                lines += 1;
                                if (lines * line_height) as f64 > point.y {
                                    break;
                                }
                                line += r.len();
                            }
                            DiffLines::Both(_, r) | DiffLines::Right(r) => {
                                lines += r.len();
                                if (lines * line_height) as f64 > point.y {
                                    line += ((point.y
                                        - ((lines - r.len()) * line_height) as f64)
                                        / line_height as f64)
                                        .floor()
                                        as usize;
                                    break;
                                }
                                line += r.len();
                            }
                        }
                    }
                    (line, config.editor.font_size)
                } else {
                    (0, config.editor.font_size)
                }
            }
            EditorView::Lens => {
                if let Some(syntax) = self.syntax() {
                    let lens = &syntax.lens;
                    let line = lens.line_of_height(point.y.round() as usize);
                    let line_height =
                        lens.height_of_line(line + 1) - lens.height_of_line(line);
                    let font_size = if line_height < config.editor.line_height() {
                        config.editor.code_lens_font_size
                    } else {
                        config.editor.font_size
                    };
                    (line, font_size)
                } else {
                    (
                        (point.y / config.editor.line_height() as f64).floor()
                            as usize,
                        config.editor.font_size,
                    )
                }
            }
            EditorView::Normal => (
                (point.y / config.editor.line_height() as f64).floor() as usize,
                config.editor.font_size,
            ),
        };

        let line = line.min(self.buffer.last_line());

        let mut x_shift = 0.0;
        if font_size < config.editor.font_size {
            let line_content = self.buffer.line_content(line);
            let mut col = 0usize;
            for ch in line_content.chars() {
                if ch == ' ' || ch == '\t' {
                    col += 1;
                } else {
                    break;
                }
            }

            if col > 0 {
                let normal_text_layout = self.get_text_layout(
                    text,
                    line,
                    config.editor.font_size,
                    config,
                );
                let small_text_layout =
                    self.get_text_layout(text, line, font_size, config);
                x_shift =
                    normal_text_layout.text.hit_test_text_position(col).point.x
                        - small_text_layout.text.hit_test_text_position(col).point.x;
            }
        }

        let text_layout = self.get_text_layout(text, line, font_size, config);
        let hit_point = text_layout
            .text
            .hit_test_point(Point::new(point.x - x_shift, 0.0));
        let phantom_text = self.line_phantom_text(config, line);
        let col = phantom_text.before_col(hit_point.idx);
        let max_col = self.buffer.line_end_col(line, mode != Mode::Normal);
        let mut col = col.min(max_col);

        if config.editor.atomic_soft_tabs && config.editor.tab_width > 1 {
            col = snap_to_soft_tab_line_col(
                &self.buffer,
                line,
                col,
                SnapDirection::Nearest,
                config.editor.tab_width,
            );
        }

        ((line, col), hit_point.is_inside)
    }

    pub fn offset_of_point(
        &self,
        text: &mut PietText,
        mode: Mode,
        point: Point,
        view: &EditorView,
        config: &LapceConfig,
    ) -> (usize, bool) {
        let ((line, col), is_inside) =
            self.line_col_of_point(text, mode, point, view, config);
        (self.buffer.offset_of_line_col(line, col), is_inside)
    }

    pub fn points_of_offset(
        &self,
        text: &mut PietText,
        offset: usize,
        view: &EditorView,
        config: &LapceConfig,
    ) -> (Point, Point) {
        let (line, col) = self.buffer.offset_to_line_col(offset);
        self.points_of_line_col(text, line, col, view, config)
    }

    pub fn points_of_line_col(
        &self,
        text: &mut PietText,
        line: usize,
        col: usize,
        view: &EditorView,
        config: &LapceConfig,
    ) -> (Point, Point) {
        let (y, line_height, font_size) = match view {
            EditorView::Diff(version) => {
                if let Some(history) = self.get_history(version) {
                    let line_height = config.editor.line_height();
                    let mut current_line = 0;
                    let mut y = 0;
                    for change in history.changes().iter() {
                        match change {
                            DiffLines::Left(l) => {
                                y += l.len() * line_height;
                            }
                            DiffLines::Skip(_l, r) => {
                                if current_line + r.len() > line {
                                    break;
                                }
                                y += line_height;
                                current_line += r.len();
                            }
                            DiffLines::Both(_, r) | DiffLines::Right(r) => {
                                if current_line + r.len() > line {
                                    y += line_height * (line - current_line);
                                    break;
                                }
                                y += r.len() * line_height;
                                current_line += r.len();
                            }
                        }
                    }
                    (y, config.editor.line_height(), config.editor.font_size)
                } else {
                    (0, config.editor.line_height(), config.editor.font_size)
                }
            }
            EditorView::Lens => {
                if let Some(syntax) = self.syntax() {
                    let lens = &syntax.lens;
                    let height = lens.height_of_line(line);
                    let line_height =
                        lens.height_of_line(line + 1) - lens.height_of_line(line);
                    let font_size = if line_height < config.editor.line_height() {
                        config.editor.code_lens_font_size
                    } else {
                        config.editor.font_size
                    };
                    (height, line_height, font_size)
                } else {
                    (
                        config.editor.line_height() * line,
                        config.editor.line_height(),
                        config.editor.font_size,
                    )
                }
            }
            EditorView::Normal => (
                config.editor.line_height() * line,
                config.editor.line_height(),
                config.editor.font_size,
            ),
        };

        let line = line.min(self.buffer.last_line());

        let phantom_text = self.line_phantom_text(config, line);
        let col = phantom_text.col_after(col, false);

        let mut x_shift = 0.0;
        if font_size < config.editor.font_size {
            let line_content = self.buffer.line_content(line);
            let mut col = 0usize;
            for ch in line_content.chars() {
                if ch == ' ' || ch == '\t' {
                    col += 1;
                } else {
                    break;
                }
            }

            if col > 0 {
                let normal_text_layout = self.get_text_layout(
                    text,
                    line,
                    config.editor.font_size,
                    config,
                );
                let small_text_layout =
                    self.get_text_layout(text, line, font_size, config);
                x_shift =
                    normal_text_layout.text.hit_test_text_position(col).point.x
                        - small_text_layout.text.hit_test_text_position(col).point.x;
            }
        }

        let x = self
            .line_point_of_line_col(text, line, col, font_size, config)
            .x
            + x_shift;
        (
            Point::new(x, y as f64),
            Point::new(x, (y + line_height) as f64),
        )
    }

    fn diff_cursor_line(&self, version: &str, line: usize) -> usize {
        let mut cursor_line = 0;
        if let Some(history) = self.get_history(version) {
            for (_i, change) in history.changes().iter().enumerate() {
                match change {
                    DiffLines::Left(_range) => {}
                    DiffLines::Both(_, r) | DiffLines::Right(r) => {
                        if r.contains(&line) {
                            cursor_line += line - r.start;
                            break;
                        }
                        cursor_line += r.len();
                    }
                    DiffLines::Skip(_, r) => {
                        if r.contains(&line) {
                            break;
                        }
                    }
                }
            }
        }
        cursor_line
    }

    fn diff_actual_line(&self, version: &str, cursor_line: usize) -> usize {
        let mut current_cursor_line = 0;
        let mut line = 0;
        if let Some(history) = self.get_history(version) {
            for (_i, change) in history.changes().iter().enumerate() {
                match change {
                    DiffLines::Left(_range) => {}
                    DiffLines::Skip(_, _r) => {}
                    DiffLines::Both(_, r) | DiffLines::Right(r) => {
                        current_cursor_line += r.len();
                        if current_cursor_line > cursor_line {
                            line = r.end - (current_cursor_line - cursor_line);
                            break;
                        }
                    }
                }
            }
        }
        if current_cursor_line <= cursor_line {
            self.buffer.last_line()
        } else {
            line
        }
    }

    pub fn line_point_of_offset(
        &self,
        text: &mut PietText,
        offset: usize,
        font_size: usize,
        config: &LapceConfig,
    ) -> Point {
        let (line, col) = self.buffer.offset_to_line_col(offset);
        self.line_point_of_line_col(text, line, col, font_size, config)
    }

    pub fn line_point_of_line_col(
        &self,
        text: &mut PietText,
        line: usize,
        col: usize,
        font_size: usize,
        config: &LapceConfig,
    ) -> Point {
        let text_layout = self.get_text_layout(text, line, font_size, config);
        text_layout.text.hit_test_text_position(col).point
    }

    pub fn get_text_layout(
        &self,
        text: &mut PietText,
        line: usize,
        font_size: usize,
        config: &LapceConfig,
    ) -> Arc<TextLayoutLine> {
        self.text_layouts.borrow_mut().check_attributes(config.id);
        if self.text_layouts.borrow().layouts.get(&font_size).is_none() {
            let mut cache = self.text_layouts.borrow_mut();
            cache.layouts.insert(font_size, HashMap::new());
        }
        let cache_exits = self
            .text_layouts
            .borrow()
            .layouts
            .get(&font_size)
            .unwrap()
            .get(&line)
            .is_some();
        if !cache_exits {
            let text_layout =
                Arc::new(self.new_text_layout(text, line, font_size, config));
            let mut cache = self.text_layouts.borrow_mut();
            let width = text_layout.text.size().width;
            if width > cache.max_width {
                cache.max_width = width;
            }
            cache
                .layouts
                .get_mut(&font_size)
                .unwrap()
                .insert(line, text_layout);
        }
        self.text_layouts
            .borrow()
            .layouts
            .get(&font_size)
            .unwrap()
            .get(&line)
            .cloned()
            .unwrap()
    }

    fn new_text_layout(
        &self,
        text: &mut PietText,
        line: usize,
        font_size: usize,
        config: &LapceConfig,
    ) -> TextLayoutLine {
        let line_content_original = self.buffer.line_content(line);

        let (line_content, line_content_original) =
            if let Some(s) = line_content_original.strip_suffix("\r\n") {
                (
                    format!("{s}  "),
                    &line_content_original[..line_content_original.len() - 2],
                )
            } else if let Some(s) = line_content_original.strip_suffix('\n') {
                (
                    format!("{s} ",),
                    &line_content_original[..line_content_original.len() - 1],
                )
            } else {
                (
                    line_content_original.to_string(),
                    &line_content_original[..],
                )
            };
        let phantom_text = self.line_phantom_text(config, line);
        let line_content = phantom_text.combine_with_text(line_content);

        let tab_width =
            config.tab_width(text, config.editor.font_family(), font_size);

        let font_family = if self.content.is_input() {
            config.ui.font_family()
        } else {
            config.editor.font_family()
        };
        let font_size = if self.content.is_input() {
            config.ui.font_size()
        } else {
            font_size
        };
        let mut layout_builder = text
            .new_text_layout(line_content.to_string())
            .font(font_family, font_size as f64)
            .text_color(
                config
                    .get_color_unchecked(LapceTheme::EDITOR_FOREGROUND)
                    .clone(),
            )
            .set_tab_width(tab_width);

        // Apply various styles to the lines text
        let styles = self.line_style(line);
        for line_style in styles.iter() {
            if let Some(fg_color) = line_style.style.fg_color.as_ref() {
                if let Some(fg_color) = config.get_style_color(fg_color) {
                    let start = phantom_text.col_at(line_style.start);
                    let end = phantom_text.col_at(line_style.end);
                    layout_builder = layout_builder.range_attribute(
                        start..end,
                        TextAttribute::TextColor(fg_color.clone()),
                    );
                }
            }
        }

        for (offset, size, col, phantom) in phantom_text.offset_size_iter() {
            let start = col + offset;
            let end = start + size;

            if let Some(fg) = phantom.fg.clone() {
                layout_builder = layout_builder
                    .range_attribute(start..end, TextAttribute::TextColor(fg));
            }
            if let Some(phantom_font_size) = phantom.font_size {
                layout_builder = layout_builder.range_attribute(
                    start..end,
                    TextAttribute::FontSize(phantom_font_size.min(font_size) as f64),
                );
            }
            if let Some(font_family) = phantom.font_family.clone() {
                layout_builder = layout_builder.range_attribute(
                    start..end,
                    TextAttribute::FontFamily(font_family),
                );
            }
        }

        let text_layout = layout_builder.build().unwrap();
        let mut extra_style = Vec::new();
        for (offset, size, col, phantom) in phantom_text.offset_size_iter() {
            if phantom.bg.is_some() || phantom.under_line.is_some() {
                let start = col + offset;
                let end = start + size;
                let x0 = text_layout.hit_test_text_position(start).point.x;
                let x1 = text_layout.hit_test_text_position(end).point.x;
                extra_style.push((
                    x0,
                    Some(x1),
                    LineExtraStyle {
                        bg_color: phantom.bg.clone(),
                        under_line: phantom.under_line.clone(),
                    },
                ));
            }
        }

        if let Some(max_severity) = phantom_text.max_severity {
            let theme_prop = if max_severity == DiagnosticSeverity::ERROR {
                LapceTheme::ERROR_LENS_ERROR_BACKGROUND
            } else if max_severity == DiagnosticSeverity::WARNING {
                LapceTheme::ERROR_LENS_WARNING_BACKGROUND
            } else {
                LapceTheme::ERROR_LENS_OTHER_BACKGROUND
            };

            let x1 = (!config.editor.error_lens_end_of_line).then(|| {
                text_layout
                    .hit_test_text_position(line_content.len())
                    .point
                    .x
            });

            extra_style.push((
                0.0,
                x1,
                LineExtraStyle {
                    bg_color: Some(config.get_color_unchecked(theme_prop).clone()),
                    under_line: None,
                },
            ));
        }

        let new_whitespaces = Self::new_whitespace_layout(
            line_content_original,
            &text_layout,
            &phantom_text,
            config,
        );

        let indent_line = if line_content_original.trim().is_empty() {
            let offset = self.buffer.offset_of_line(line);
            if let Some(offset) = self
                .syntax
                .as_ref()
                .and_then(|syntax| syntax.parent_offset(offset))
            {
                self.buffer.line_of_offset(offset)
            } else {
                line
            }
        } else {
            line
        };

        let indent = if indent_line != line {
            self.get_text_layout(text, indent_line, font_size, config)
                .indent
                + 1.0
        } else {
            let offset = self.buffer.first_non_blank_character_on_line(indent_line);
            let (_, col) = self.buffer.offset_to_line_col(offset);
            text_layout.hit_test_text_position(col).point.x
        };

        TextLayoutLine {
            text: text_layout,
            extra_style,
            whitespaces: new_whitespaces,
            indent,
        }
    }

    /// Create rendable whitespace layout by creating a new text layout
    /// with invicible spaces and special utf8 characters that display
    /// the different white space characters.
    fn new_whitespace_layout(
        line_content: &str,
        text_layout: &PietTextLayout,
        phantom: &PhantomTextLine,
        config: &LapceConfig,
    ) -> Option<Vec<(char, (f64, f64))>> {
        let mut render_leading = false;
        let mut render_boundary = false;
        let mut render_between = false;

        // TODO: render whitespaces only on highlighted text
        match config.editor.render_whitespace.as_str() {
            "all" => {
                render_leading = true;
                render_boundary = true;
                render_between = true;
            }
            "boundary" => {
                render_leading = true;
                render_boundary = true;
            }
            "trailing" => {} // All configs include rendering trailing whitespace
            _ => return None,
        }

        let mut whitespace_buffer = Vec::new();
        let mut rendered_whitespaces: Vec<(char, (f64, f64))> = Vec::new();
        let mut char_found = false;
        let mut col = 0;
        for c in line_content.chars() {
            match c {
                '\t' => {
                    let col_left = phantom.col_after(col, true);
                    let col_right = phantom.col_after(col + 1, false);
                    let x0 = text_layout.hit_test_text_position(col_left).point.x;
                    let x1 = text_layout.hit_test_text_position(col_right).point.x;
                    whitespace_buffer.push(('\t', (x0, x1)));
                }
                ' ' => {
                    let col_left = phantom.col_after(col, true);
                    let col_right = phantom.col_after(col + 1, false);
                    let x0 = text_layout.hit_test_text_position(col_left).point.x;
                    let x1 = text_layout.hit_test_text_position(col_right).point.x;
                    whitespace_buffer.push((' ', (x0, x1)));
                }
                _ => {
                    if (char_found && render_between)
                        || (char_found
                            && render_boundary
                            && whitespace_buffer.len() > 1)
                        || (!char_found && render_leading)
                    {
                        rendered_whitespaces.extend(whitespace_buffer.iter());
                    } else {
                    }

                    char_found = true;
                    whitespace_buffer.clear();
                }
            }
            col += c.len_utf8();
        }
        rendered_whitespaces.extend(whitespace_buffer.iter());

        Some(rendered_whitespaces)
    }

    pub fn line_horiz_col(
        &self,
        text: &mut PietText,
        line: usize,
        font_size: usize,
        horiz: &ColPosition,
        caret: bool,
        config: &LapceConfig,
    ) -> usize {
        match *horiz {
            ColPosition::Col(x) => {
                let text_layout =
                    self.get_text_layout(text, line, font_size, config);
                let n = text_layout.text.hit_test_point(Point::new(x, 0.0)).idx;
                n.min(self.buffer.line_end_col(line, caret))
            }
            ColPosition::End => self.buffer.line_end_col(line, caret),
            ColPosition::Start => 0,
            ColPosition::FirstNonBlank => {
                self.buffer.first_non_blank_character_on_line(line)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn move_region(
        &self,
        text: &mut PietText,
        region: &SelRegion,
        count: usize,
        modify: bool,
        movement: &Movement,
        mode: Mode,
        view: &EditorView,
        config: &LapceConfig,
    ) -> SelRegion {
        let (end, horiz) = self.move_offset(
            text,
            region.end,
            region.horiz.as_ref(),
            count,
            movement,
            mode,
            view,
            config,
        );
        let start = match modify {
            true => region.start,
            false => end,
        };
        SelRegion::new(start, end, horiz)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn move_cursor(
        &mut self,
        text: &mut PietText,
        cursor: &mut Cursor,
        movement: &Movement,
        count: usize,
        modify: bool,
        view: &EditorView,
        register: &mut Register,
        config: &LapceConfig,
    ) {
        match cursor.mode {
            CursorMode::Normal(offset) => {
                let (new_offset, horiz) = self.move_offset(
                    text,
                    offset,
                    cursor.horiz.as_ref(),
                    count,
                    movement,
                    Mode::Normal,
                    view,
                    config,
                );
                if let Some(motion_mode) = cursor.motion_mode.clone() {
                    let (moved_new_offset, _) = self.move_offset(
                        text,
                        new_offset,
                        None,
                        1,
                        &Movement::Right,
                        Mode::Insert,
                        view,
                        config,
                    );
                    let (start, end) = match movement {
                        Movement::EndOfLine | Movement::WordEndForward => {
                            (offset, moved_new_offset)
                        }
                        Movement::MatchPairs => {
                            if new_offset > offset {
                                (offset, moved_new_offset)
                            } else {
                                (moved_new_offset, new_offset)
                            }
                        }
                        _ => (offset, new_offset),
                    };
                    let deltas = Editor::execute_motion_mode(
                        cursor,
                        &mut self.buffer,
                        motion_mode,
                        start,
                        end,
                        movement.is_vertical(),
                        register,
                    );
                    self.apply_deltas(&deltas);
                    cursor.motion_mode = None;
                } else {
                    cursor.mode = CursorMode::Normal(new_offset);
                    cursor.horiz = horiz;
                }
            }
            CursorMode::Visual { start, end, mode } => {
                let (new_offset, horiz) = self.move_offset(
                    text,
                    end,
                    cursor.horiz.as_ref(),
                    count,
                    movement,
                    Mode::Visual,
                    view,
                    config,
                );
                cursor.mode = CursorMode::Visual {
                    start,
                    end: new_offset,
                    mode,
                };
                cursor.horiz = horiz;
            }
            CursorMode::Insert(ref selection) => {
                let selection = self.move_selection(
                    text,
                    selection,
                    cursor.horiz.as_ref(),
                    count,
                    modify,
                    movement,
                    Mode::Insert,
                    view,
                    config,
                );
                cursor.set_insert(selection);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn move_selection(
        &self,
        text: &mut PietText,
        selection: &Selection,
        _horiz: Option<&ColPosition>,
        count: usize,
        modify: bool,
        movement: &Movement,
        mode: Mode,
        view: &EditorView,
        config: &LapceConfig,
    ) -> Selection {
        let mut new_selection = Selection::new();
        for region in selection.regions() {
            new_selection.add_region(self.move_region(
                text, region, count, modify, movement, mode, view, config,
            ));
        }
        new_selection
    }

    #[allow(clippy::too_many_arguments)]
    pub fn move_offset(
        &self,
        text: &mut PietText,
        offset: usize,
        horiz: Option<&ColPosition>,
        count: usize,
        movement: &Movement,
        mode: Mode,
        view: &EditorView,
        config: &LapceConfig,
    ) -> (usize, Option<ColPosition>) {
        match movement {
            Movement::Left => {
                let mut new_offset = self.buffer.move_left(offset, mode, count);

                if config.editor.atomic_soft_tabs && config.editor.tab_width > 1 {
                    new_offset = snap_to_soft_tab(
                        &self.buffer,
                        new_offset,
                        SnapDirection::Left,
                        config.editor.tab_width,
                    );
                }

                (new_offset, None)
            }
            Movement::Right => {
                let mut new_offset = self.buffer.move_right(offset, mode, count);

                if config.editor.atomic_soft_tabs && config.editor.tab_width > 1 {
                    new_offset = snap_to_soft_tab(
                        &self.buffer,
                        new_offset,
                        SnapDirection::Right,
                        config.editor.tab_width,
                    );
                }

                (new_offset, None)
            }
            Movement::Up => {
                let line = self.buffer.line_of_offset(offset);
                if line == 0 {
                    return (offset, horiz.cloned());
                }

                let (line, font_size) = match view {
                    EditorView::Lens => {
                        if let Some(syntax) = self.syntax() {
                            let lens = &syntax.lens;
                            let line = if count == 1 {
                                let mut line = line - 1;
                                loop {
                                    if line == 0 {
                                        break;
                                    }

                                    let line_height = lens.height_of_line(line + 1)
                                        - lens.height_of_line(line);
                                    if line_height == config.editor.line_height() {
                                        break;
                                    }
                                    line -= 1;
                                }
                                line
                            } else {
                                line.saturating_sub(count)
                            };
                            let line_height = lens.height_of_line(line + 1)
                                - lens.height_of_line(line);
                            let font_size =
                                if line_height == config.editor.line_height() {
                                    config.editor.font_size
                                } else {
                                    config.editor.code_lens_font_size
                                };

                            (line, font_size)
                        } else {
                            (line.saturating_sub(count), config.editor.font_size)
                        }
                    }
                    EditorView::Diff(version) => {
                        let cursor_line = self.diff_cursor_line(version, line);
                        let cursor_line = if cursor_line > count {
                            cursor_line - count
                        } else {
                            0
                        };
                        (
                            self.diff_actual_line(version, cursor_line),
                            config.editor.font_size,
                        )
                    }
                    EditorView::Normal => {
                        (line.saturating_sub(count), config.editor.font_size)
                    }
                };

                let horiz = horiz.cloned().unwrap_or_else(|| {
                    ColPosition::Col(
                        self.line_point_of_offset(text, offset, font_size, config).x,
                    )
                });
                let col = self.line_horiz_col(
                    text,
                    line,
                    font_size,
                    &horiz,
                    mode != Mode::Normal,
                    config,
                );
                let new_offset = self.buffer.offset_of_line_col(line, col);
                (new_offset, Some(horiz))
            }
            Movement::Down => {
                let last_line = self.buffer.last_line();
                let line = self.buffer.line_of_offset(offset);

                let (line, font_size) = match view {
                    EditorView::Lens => {
                        if let Some(syntax) = self.syntax() {
                            let lens = &syntax.lens;
                            let line = if count == 1 {
                                let mut line = (line + 1).min(last_line);
                                loop {
                                    if line == last_line {
                                        break;
                                    }

                                    let line_height = lens.height_of_line(line + 1)
                                        - lens.height_of_line(line);
                                    if line_height == config.editor.line_height() {
                                        break;
                                    }
                                    line += 1;
                                }
                                line
                            } else {
                                line + count
                            };
                            let line_height = lens.height_of_line(line + 1)
                                - lens.height_of_line(line);
                            let font_size =
                                if line_height == config.editor.line_height() {
                                    config.editor.font_size
                                } else {
                                    config.editor.code_lens_font_size
                                };

                            (line, font_size)
                        } else {
                            (line + count, config.editor.font_size)
                        }
                    }
                    EditorView::Diff(version) => {
                        let cursor_line = self.diff_cursor_line(version, line);
                        let cursor_line = cursor_line + count;
                        (
                            self.diff_actual_line(version, cursor_line),
                            config.editor.font_size,
                        )
                    }
                    EditorView::Normal => (line + count, config.editor.font_size),
                };

                let line = line.min(last_line);

                let horiz = horiz.cloned().unwrap_or_else(|| {
                    ColPosition::Col(
                        self.line_point_of_offset(text, offset, font_size, config).x,
                    )
                });
                let col = self.line_horiz_col(
                    text,
                    line,
                    font_size,
                    &horiz,
                    mode != Mode::Normal,
                    config,
                );
                let new_offset = self.buffer.offset_of_line_col(line, col);
                (new_offset, Some(horiz))
            }
            Movement::DocumentStart => (0, Some(ColPosition::Start)),
            Movement::DocumentEnd => {
                let last_offset = self
                    .buffer
                    .offset_line_end(self.buffer.len(), mode != Mode::Normal);
                (last_offset, Some(ColPosition::End))
            }
            Movement::FirstNonBlank => {
                let line = self.buffer.line_of_offset(offset);
                let non_blank_offset =
                    self.buffer.first_non_blank_character_on_line(line);
                let start_line_offset = self.buffer.offset_of_line(line);
                if offset > non_blank_offset {
                    // Jump to the first non-whitespace character if we're strictly after it
                    (non_blank_offset, Some(ColPosition::FirstNonBlank))
                } else {
                    // If we're at the start of the line, also jump to the first not blank
                    if start_line_offset == offset {
                        (non_blank_offset, Some(ColPosition::FirstNonBlank))
                    } else {
                        // Otherwise, jump to the start of the line
                        (start_line_offset, Some(ColPosition::Start))
                    }
                }
            }
            Movement::StartOfLine => {
                let line = self.buffer.line_of_offset(offset);
                let new_offset = self.buffer.offset_of_line(line);
                (new_offset, Some(ColPosition::Start))
            }
            Movement::EndOfLine => {
                let new_offset =
                    self.buffer.offset_line_end(offset, mode != Mode::Normal);
                (new_offset, Some(ColPosition::End))
            }
            Movement::Line(position) => {
                let line = match position {
                    LinePosition::Line(line) => {
                        (line - 1).min(self.buffer.last_line())
                    }
                    LinePosition::First => 0,
                    LinePosition::Last => self.buffer.last_line(),
                };
                let font_size = if let EditorView::Lens = view {
                    if let Some(syntax) = self.syntax() {
                        let lens = &syntax.lens;
                        let line_height = lens.height_of_line(line + 1)
                            - lens.height_of_line(line);

                        if line_height == config.editor.line_height() {
                            config.editor.font_size
                        } else {
                            config.editor.code_lens_font_size
                        }
                    } else {
                        config.editor.font_size
                    }
                } else {
                    config.editor.font_size
                };
                let horiz = horiz.cloned().unwrap_or_else(|| {
                    ColPosition::Col(
                        self.line_point_of_offset(text, offset, font_size, config).x,
                    )
                });
                let col = self.line_horiz_col(
                    text,
                    line,
                    font_size,
                    &horiz,
                    mode != Mode::Normal,
                    config,
                );
                let new_offset = self.buffer.offset_of_line_col(line, col);
                (new_offset, Some(horiz))
            }
            Movement::Offset(offset) => {
                let new_offset = *offset;
                let new_offset = self
                    .buffer
                    .text()
                    .prev_grapheme_offset(new_offset + 1)
                    .unwrap();
                (new_offset, None)
            }
            Movement::WordEndForward => {
                let new_offset = self.buffer.move_n_wordends_forward(
                    offset,
                    count,
                    mode == Mode::Insert,
                );
                (new_offset, None)
            }
            Movement::WordForward => {
                let new_offset = self.buffer.move_n_words_forward(offset, count);
                (new_offset, None)
            }
            Movement::WordBackward => {
                let new_offset =
                    self.buffer.move_n_words_backward(offset, count, mode);
                (new_offset, None)
            }
            Movement::NextUnmatched(c) => {
                if let Some(syntax) = self.syntax.as_ref() {
                    let new_offset = syntax
                        .find_tag(offset, false, &c.to_string())
                        .unwrap_or(offset);
                    (new_offset, None)
                } else {
                    let new_offset = WordCursor::new(self.buffer.text(), offset)
                        .next_unmatched(*c)
                        .map_or(offset, |new| new - 1);
                    (new_offset, None)
                }
            }
            Movement::PreviousUnmatched(c) => {
                if let Some(syntax) = self.syntax.as_ref() {
                    let new_offset = syntax
                        .find_tag(offset, true, &c.to_string())
                        .unwrap_or(offset);
                    (new_offset, None)
                } else {
                    let new_offset = WordCursor::new(self.buffer.text(), offset)
                        .previous_unmatched(*c)
                        .unwrap_or(offset);
                    (new_offset, None)
                }
            }
            Movement::MatchPairs => {
                if let Some(syntax) = self.syntax.as_ref() {
                    let new_offset =
                        syntax.find_matching_pair(offset).unwrap_or(offset);
                    (new_offset, None)
                } else {
                    let new_offset = WordCursor::new(self.buffer.text(), offset)
                        .match_pairs()
                        .unwrap_or(offset);
                    (new_offset, None)
                }
            }
        }
    }

    pub fn code_action_size(
        &self,
        text: &mut PietText,
        offset: usize,
        config: &LapceConfig,
    ) -> Size {
        let prev_offset = self.buffer.prev_code_boundary(offset);
        let empty_vec = Vec::new();
        let code_actions = self
            .code_actions
            .get(&prev_offset)
            .map(|(_plugin_id, code_actions)| code_actions)
            .unwrap_or(&empty_vec);

        let action_text_layouts: Vec<PietTextLayout> = code_actions
            .iter()
            .map(|code_action| {
                let title = match code_action {
                    CodeActionOrCommand::Command(cmd) => cmd.title.to_string(),
                    CodeActionOrCommand::CodeAction(action) => {
                        action.title.to_string()
                    }
                };

                text.new_text_layout(title)
                    .font(config.ui.font_family(), config.ui.font_size() as f64)
                    .build()
                    .unwrap()
            })
            .collect();

        let mut width = 0.0;
        for text_layout in &action_text_layouts {
            let line_width = text_layout.size().width + 10.0;
            if line_width > width {
                width = line_width;
            }
        }
        let line_height = config.editor.line_height() as f64;
        Size::new(width, code_actions.len() as f64 * line_height)
    }

    pub fn reset_find(&self, current_find: &Find) {
        {
            let find = self.find.borrow();
            if find.search_string == current_find.search_string
                && find.case_matching == current_find.case_matching
                && find.regex.as_ref().map(|r| r.as_str())
                    == current_find.regex.as_ref().map(|r| r.as_str())
                && find.whole_words == current_find.whole_words
            {
                return;
            }
        }

        let mut find = self.find.borrow_mut();
        find.unset();
        find.search_string = current_find.search_string.clone();
        find.case_matching = current_find.case_matching;
        find.regex = current_find.regex.clone();
        find.whole_words = current_find.whole_words;
        *self.find_progress.borrow_mut() = FindProgress::Started;
    }

    pub fn update_find(
        &self,
        current_find: &Find,
        start_line: usize,
        end_line: usize,
    ) {
        self.reset_find(current_find);

        let mut find_progress = self.find_progress.borrow_mut();
        let search_range = match &find_progress.clone() {
            FindProgress::Started => {
                // start incremental find on visible region
                let start = self.buffer.offset_of_line(start_line);
                let end = self.buffer.offset_of_line(end_line + 1);
                *find_progress =
                    FindProgress::InProgress(Selection::region(start, end));
                Some((start, end))
            }
            FindProgress::InProgress(searched_range) => {
                if searched_range.regions().len() == 1
                    && searched_range.min_offset() == 0
                    && searched_range.max_offset() >= self.buffer.len()
                {
                    // the entire text has been searched
                    // end find by executing multi-line regex queries on entire text
                    // stop incremental find
                    *find_progress = FindProgress::Ready;
                    Some((0, self.buffer.len()))
                } else {
                    let start = self.buffer.offset_of_line(start_line);
                    let end = self.buffer.offset_of_line(end_line + 1);
                    let mut range = Some((start, end));
                    for region in searched_range.regions() {
                        if region.min() <= start && region.max() >= end {
                            range = None;
                            break;
                        }
                    }
                    if range.is_some() {
                        let mut new_range = searched_range.clone();
                        new_range.add_region(SelRegion::new(start, end, None));
                        *find_progress = FindProgress::InProgress(new_range);
                    }
                    range
                }
            }
            _ => None,
        };

        let mut find = self.find.borrow_mut();
        if let Some((search_range_start, search_range_end)) = search_range {
            if !find.is_multiline_regex() {
                find.update_find(
                    self.buffer.text(),
                    search_range_start,
                    search_range_end,
                    true,
                );
            } else {
                // only execute multi-line regex queries if we are searching the entire text (last step)
                if search_range_start == 0 && search_range_end == self.buffer.len() {
                    find.update_find(
                        self.buffer.text(),
                        search_range_start,
                        search_range_end,
                        true,
                    );
                }
            }
        }
    }

    pub fn sticky_headers(&self, line: usize) -> Option<Vec<usize>> {
        if let Some(lines) = self.sticky_headers.borrow().get(&line) {
            return lines.clone();
        }
        let offset = self.buffer.offset_of_line(line + 1);
        let lines = self.syntax.as_ref()?.sticky_headers(offset).map(|offsets| {
            offsets
                .iter()
                .filter_map(|offset| {
                    let l = self.buffer.line_of_offset(*offset);
                    if l <= line {
                        Some(l)
                    } else {
                        None
                    }
                })
                .dedup()
                .sorted()
                .collect()
        });
        self.sticky_headers.borrow_mut().insert(line, lines.clone());
        lines
    }

    pub fn change_syntax_selection(
        &mut self,
        direction: SelectionRangeDirection,
    ) -> Option<Selection> {
        if let Some(selections) = self.syntax_selection_range.as_mut() {
            match direction {
                SelectionRangeDirection::Next => selections.next_range(),
                SelectionRangeDirection::Previous => selections.previous_range(),
            }
            .map(|range| {
                let start = self.buffer.offset_of_position(&range.start);
                let end = self.buffer.offset_of_position(&range.end);
                selections.last_known_selection = Some((start, end));
                Selection::region(start, end)
            })
        } else {
            None
        }
    }

    pub fn find_enclosing_brackets(&self, offset: usize) -> Option<(usize, usize)> {
        let char_at_cursor = match self.buffer().char_at_offset(offset) {
            Some(c) => c,
            None => return None,
        };

        if let Some(syntax) = self.syntax() {
            if matching_pair_direction(char_at_cursor).is_some() {
                if let Some(new_offset) = syntax.find_matching_pair(offset) {
                    return Some((offset, new_offset));
                }
            } else {
                return syntax.find_enclosing_pair(offset);
            }
        }

        let mut cursor = WordCursor::new(self.buffer.text(), offset);
        if matching_pair_direction(char_at_cursor).is_some() {
            let new_offset = cursor.match_pairs().unwrap_or(offset);
            Some((offset, new_offset))
        } else {
            cursor.find_enclosing_pair()
        }
    }
}
