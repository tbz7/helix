use std::cmp::min;

use helix_core::doc_formatter::{DocumentFormatter, GraphemeSource, TextFormat};
use helix_core::graphemes::Grapheme;
use helix_core::str_utils::char_to_byte_idx;
use helix_core::syntax::Highlight;
use helix_core::syntax::HighlightEvent;
use helix_core::text_annotations::TextAnnotations;
use helix_core::{visual_offset_from_block, Position, RopeSlice};
use helix_stdx::rope::RopeSliceExt;
use helix_view::editor::WhitespaceFeature;
use helix_view::graphics::Rect;
use helix_view::theme::Style;
use helix_view::view::ViewPosition;
use helix_view::Document;
use helix_view::Theme;
use tui::buffer::Buffer as Surface;

use super::trailing_whitespace::{TrailingWhitespaceTracker, WhitespaceKind};

pub trait LineDecoration {
    fn render_background(&mut self, _renderer: &mut TextRenderer, _pos: LinePos) {}
    fn render_foreground(
        &mut self,
        _renderer: &mut TextRenderer,
        _pos: LinePos,
        _end_char_idx: usize,
    ) {
    }
}

impl<F: FnMut(&mut TextRenderer, LinePos)> LineDecoration for F {
    fn render_background(&mut self, renderer: &mut TextRenderer, pos: LinePos) {
        self(renderer, pos)
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum StyleIterKind {
    /// base highlights (usually emitted by TS), byte indices (potentially not codepoint aligned)
    BaseHighlights,
    /// overlay highlights (emitted by custom code from selections), char indices
    Overlay,
}

/// A wrapper around a HighlightIterator
/// that merges the layered highlights to create the final text style
/// and yields the active text style and the char_idx where the active
/// style will have to be recomputed.
///
/// TODO(ropey2): hopefully one day helix and ropey will operate entirely
/// on byte ranges and we can remove this
struct StyleIter<'a, H: Iterator<Item = HighlightEvent>> {
    text_style: Style,
    active_highlights: Vec<Highlight>,
    highlight_iter: H,
    kind: StyleIterKind,
    text: RopeSlice<'a>,
    theme: &'a Theme,
}

impl<H: Iterator<Item = HighlightEvent>> Iterator for StyleIter<'_, H> {
    type Item = (Style, usize);
    fn next(&mut self) -> Option<(Style, usize)> {
        while let Some(event) = self.highlight_iter.next() {
            match event {
                HighlightEvent::HighlightStart(highlights) => {
                    self.active_highlights.push(highlights)
                }
                HighlightEvent::HighlightEnd => {
                    self.active_highlights.pop();
                }
                HighlightEvent::Source { start, mut end } => {
                    if start == end {
                        continue;
                    }
                    let style = self
                        .active_highlights
                        .iter()
                        .fold(self.text_style, |acc, span| {
                            acc.patch(self.theme.highlight(span.0))
                        });
                    if self.kind == StyleIterKind::BaseHighlights {
                        end = self.text.byte_to_next_char(end);
                    }
                    return Some((style, end));
                }
            }
        }
        None
    }
}

#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub struct LinePos {
    /// Indicates whether the given visual line
    /// is the first visual line of the given document line
    pub first_visual_line: bool,
    /// The line index of the document line that contains the given visual line
    pub doc_line: usize,
    /// Vertical offset from the top of the inner view area
    pub visual_line: u16,
    /// The first char index of this visual line.
    /// Note that if the visual line is entirely filled by
    /// a very long inline virtual text then this index will point
    /// at the next (non-virtual) char after this visual line
    pub start_char_idx: usize,
}

pub type TranslatedPosition<'a> = (usize, Box<dyn FnMut(&mut TextRenderer, Position) + 'a>);

#[allow(clippy::too_many_arguments)]
pub fn render_document(
    surface: &mut Surface,
    viewport: Rect,
    doc: &Document,
    offset: ViewPosition,
    doc_annotations: &TextAnnotations,
    syntax_highlight_iter: impl Iterator<Item = HighlightEvent>,
    overlay_highlight_iter: impl Iterator<Item = HighlightEvent>,
    theme: &Theme,
    line_decoration: &mut [Box<dyn LineDecoration + '_>],
    translated_positions: &mut [TranslatedPosition],
) {
    let mut renderer = TextRenderer::new(surface, doc, theme, offset.horizontal_offset, viewport);
    render_text(
        &mut renderer,
        doc.text().slice(..),
        offset,
        &doc.text_format(viewport.width, Some(theme)),
        doc_annotations,
        syntax_highlight_iter,
        overlay_highlight_iter,
        theme,
        line_decoration,
        translated_positions,
    )
}

fn translate_positions(
    char_pos: usize,
    first_visible_char_idx: usize,
    translated_positions: &mut [TranslatedPosition],
    text_fmt: &TextFormat,
    renderer: &mut TextRenderer,
    pos: Position,
) {
    // check if any positions translated on the fly (like cursor) has been reached
    for (char_idx, callback) in &mut *translated_positions {
        if *char_idx < char_pos && *char_idx >= first_visible_char_idx {
            // by replacing the char_index with usize::MAX large number we ensure
            // that the same position is only translated once
            // text will never reach usize::MAX as rust memory allocations are limited
            // to isize::MAX
            *char_idx = usize::MAX;

            if text_fmt.soft_wrap {
                callback(renderer, pos)
            } else if pos.col >= renderer.col_offset
                && pos.col - renderer.col_offset < renderer.viewport.width as usize
            {
                callback(
                    renderer,
                    Position {
                        row: pos.row,
                        col: pos.col - renderer.col_offset,
                    },
                )
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn render_text<'t>(
    renderer: &mut TextRenderer,
    text: RopeSlice<'t>,
    offset: ViewPosition,
    text_fmt: &TextFormat,
    text_annotations: &TextAnnotations,
    syntax_highlight_iter: impl Iterator<Item = HighlightEvent>,
    overlay_highlight_iter: impl Iterator<Item = HighlightEvent>,
    theme: &Theme,
    line_decorations: &mut [Box<dyn LineDecoration + '_>],
    translated_positions: &mut [TranslatedPosition],
) {
    let (
        Position {
            row: mut row_off, ..
        },
        mut char_pos,
    ) = visual_offset_from_block(
        text,
        offset.anchor,
        offset.anchor,
        text_fmt,
        text_annotations,
    );
    row_off += offset.vertical_offset;

    let (mut formatter, mut first_visible_char_idx) =
        DocumentFormatter::new_at_prev_checkpoint(text, text_fmt, text_annotations, offset.anchor);
    let mut syntax_styles = StyleIter {
        text_style: renderer.text_style,
        active_highlights: Vec::with_capacity(64),
        highlight_iter: syntax_highlight_iter,
        kind: StyleIterKind::BaseHighlights,
        theme,
        text,
    };
    let mut overlay_styles = StyleIter {
        text_style: Style::default(),
        active_highlights: Vec::with_capacity(64),
        highlight_iter: overlay_highlight_iter,
        kind: StyleIterKind::Overlay,
        theme,
        text,
    };

    let mut last_line_pos = LinePos {
        first_visual_line: false,
        doc_line: usize::MAX,
        visual_line: u16::MAX,
        start_char_idx: usize::MAX,
    };
    let mut is_in_indent_area = true;
    let mut last_line_indent_level = 0;
    let mut syntax_style_span = syntax_styles
        .next()
        .unwrap_or_else(|| (Style::default(), usize::MAX));
    let mut overlay_style_span = overlay_styles
        .next()
        .unwrap_or_else(|| (Style::default(), usize::MAX));

    loop {
        // formattter.line_pos returns to line index of the next grapheme
        // so it must be called before formatter.next
        let doc_line = formatter.line_pos();
        let Some((grapheme, mut pos)) = formatter.next() else {
            let mut last_pos = formatter.visual_pos();
            if last_pos.row >= row_off {
                last_pos.col -= 1;
                last_pos.row -= row_off;
                // check if any positions translated on the fly (like cursor) are at the EOF
                translate_positions(
                    char_pos + 1,
                    first_visible_char_idx,
                    translated_positions,
                    text_fmt,
                    renderer,
                    last_pos,
                );
            }
            break;
        };

        // skip any graphemes on visual lines before the block start
        if pos.row < row_off {
            if char_pos >= syntax_style_span.1 {
                syntax_style_span = if let Some(syntax_style_span) = syntax_styles.next() {
                    syntax_style_span
                } else {
                    break;
                }
            }
            if char_pos >= overlay_style_span.1 {
                overlay_style_span = if let Some(overlay_style_span) = overlay_styles.next() {
                    overlay_style_span
                } else {
                    break;
                }
            }
            char_pos += grapheme.doc_chars();
            first_visible_char_idx = char_pos + 1;
            continue;
        }
        pos.row -= row_off;

        // if the end of the viewport is reached stop rendering
        if pos.row as u16 >= renderer.viewport.height {
            break;
        }

        // apply decorations before rendering a new line
        if pos.row as u16 != last_line_pos.visual_line {
            if pos.row > 0 {
                renderer.draw_indent_guides(last_line_indent_level, last_line_pos.visual_line);
                is_in_indent_area = true;
                for line_decoration in &mut *line_decorations {
                    line_decoration.render_foreground(renderer, last_line_pos, char_pos);
                }
            }
            last_line_pos = LinePos {
                first_visual_line: doc_line != last_line_pos.doc_line,
                doc_line,
                visual_line: pos.row as u16,
                start_char_idx: char_pos,
            };
            for line_decoration in &mut *line_decorations {
                line_decoration.render_background(renderer, last_line_pos);
            }
        }

        // acquire the correct grapheme style
        if char_pos >= syntax_style_span.1 {
            syntax_style_span = syntax_styles
                .next()
                .unwrap_or((Style::default(), usize::MAX));
        }
        if char_pos >= overlay_style_span.1 {
            overlay_style_span = overlay_styles
                .next()
                .unwrap_or((Style::default(), usize::MAX));
        }
        char_pos += grapheme.doc_chars();

        // check if any positions translated on the fly (like cursor) has been reached
        translate_positions(
            char_pos,
            first_visible_char_idx,
            translated_positions,
            text_fmt,
            renderer,
            pos,
        );

        let (syntax_style, overlay_style) =
            if let GraphemeSource::VirtualText { highlight } = grapheme.source {
                let mut style = renderer.text_style;
                if let Some(highlight) = highlight {
                    style = style.patch(theme.highlight(highlight.0))
                }
                (style, Style::default())
            } else {
                (syntax_style_span.0, overlay_style_span.0)
            };

        let is_virtual = grapheme.is_virtual();
        renderer.draw_grapheme(
            grapheme.grapheme,
            GraphemeStyle {
                syntax_style,
                overlay_style,
            },
            is_virtual,
            &mut last_line_indent_level,
            &mut is_in_indent_area,
            pos,
        );
    }

    renderer.draw_indent_guides(last_line_indent_level, last_line_pos.visual_line);
    for line_decoration in &mut *line_decorations {
        line_decoration.render_foreground(renderer, last_line_pos, char_pos);
    }
}

#[derive(Debug)]
pub struct TextRenderer<'a> {
    pub surface: &'a mut Surface,
    pub text_style: Style,
    pub whitespace_style: Style,
    pub trailing_whitespace_style: Style,
    pub indent_guide_char: String,
    pub indent_guide_style: Style,
    pub newline: String,
    pub nbsp: String,
    pub nnbsp: String,
    pub space: String,
    pub tab: String,
    pub virtual_tab: String,
    pub indent_width: u16,
    pub starting_indent: usize,
    pub draw_indent_guides: bool,
    pub col_offset: usize,
    pub viewport: Rect,
    pub trailing_whitespace_tracker: TrailingWhitespaceTracker,
}

pub struct GraphemeStyle {
    syntax_style: Style,
    overlay_style: Style,
}

impl<'a> TextRenderer<'a> {
    pub fn new(
        surface: &'a mut Surface,
        doc: &Document,
        theme: &Theme,
        col_offset: usize,
        viewport: Rect,
    ) -> TextRenderer<'a> {
        let editor_config = doc.config.load();

        let tab_width = doc.tab_width();
        let text_style = theme.get("ui.text");
        let indent_width = doc.indent_style.indent_width(tab_width) as u16;

        let ws = &editor_config.whitespace;
        let regular_ws = WhitespaceFeature::Regular.palette(ws, tab_width);
        let trailing_ws = WhitespaceFeature::Trailing.palette(ws, tab_width);
        let trailing_whitespace_tracker = TrailingWhitespaceTracker::new(ws.render, trailing_ws);

        TextRenderer {
            surface,
            indent_guide_char: editor_config.indent_guides.character.into(),
            newline: regular_ws.newline,
            nbsp: regular_ws.nbsp,
            nnbsp: regular_ws.nnbsp,
            space: regular_ws.space,
            tab: regular_ws.tab,
            virtual_tab: regular_ws.virtual_tab,
            whitespace_style: theme.get("ui.virtual.whitespace"),
            trailing_whitespace_style: theme.get("ui.virtual.trailing_whitespace"),
            indent_width,
            starting_indent: col_offset / indent_width as usize
                + (col_offset % indent_width as usize != 0) as usize
                + editor_config.indent_guides.skip_levels as usize,
            indent_guide_style: text_style.patch(
                theme
                    .try_get("ui.virtual.indent-guide")
                    .unwrap_or_else(|| theme.get("ui.virtual.whitespace")),
            ),
            text_style,
            draw_indent_guides: editor_config.indent_guides.render,
            viewport,
            col_offset,
            trailing_whitespace_tracker,
        }
    }

    /// Draws a single `grapheme` at the current render position with a specified `style`.
    pub fn draw_grapheme(
        &mut self,
        grapheme: Grapheme,
        grapheme_style: GraphemeStyle,
        is_virtual: bool,
        last_indent_level: &mut usize,
        is_in_indent_area: &mut bool,
        position: Position,
    ) {
        let cut_off_start = self.col_offset.saturating_sub(position.col);
        let is_whitespace = grapheme.is_whitespace();

        // TODO is it correct to apply the whitespace style to all unicode white spaces?
        let mut style = grapheme_style.syntax_style;
        if is_whitespace {
            style = style.patch(self.whitespace_style);
        }
        style = style.patch(grapheme_style.overlay_style);

        let width = grapheme.width();
        let space = if is_virtual { " " } else { &self.space };
        let nbsp = if is_virtual { " " } else { &self.nbsp };
        let nnbsp = if is_virtual { " " } else { &self.nnbsp };
        let tab = if is_virtual {
            &self.virtual_tab
        } else {
            &self.tab
        };
        let mut whitespace_kind = WhitespaceKind::None;
        let grapheme = match grapheme {
            Grapheme::Tab { width } => {
                whitespace_kind = WhitespaceKind::Tab;
                let grapheme_tab_width = char_to_byte_idx(tab, width);
                &tab[..grapheme_tab_width]
            }
            // TODO special rendering for other whitespaces?
            Grapheme::Other { ref g } if g == " " => {
                whitespace_kind = WhitespaceKind::Space;
                space
            }
            Grapheme::Other { ref g } if g == "\u{00A0}" => {
                whitespace_kind = WhitespaceKind::NonBreakingSpace;
                nbsp
            }
            Grapheme::Other { ref g } if g == "\u{202F}" => {
                whitespace_kind = WhitespaceKind::NarrowNonBreakingSpace;
                nnbsp
            }
            Grapheme::Other { ref g } => g,
            Grapheme::Newline => {
                whitespace_kind = WhitespaceKind::Newline;
                &self.newline
            }
        };

        let viewport_right_edge = self.viewport.width as usize + self.col_offset - 1;
        let in_bounds = self.col_offset <= position.col && position.col <= viewport_right_edge;

        if in_bounds {
            let in_bounds_col = position.col - self.col_offset;
            self.surface.set_string(
                self.viewport.x + in_bounds_col as u16,
                self.viewport.y + position.row as u16,
                grapheme,
                style,
            );

            if self
                .trailing_whitespace_tracker
                .track(in_bounds_col, whitespace_kind)
                || position.col == viewport_right_edge
            {
                self.trailing_whitespace_tracker.render(
                    &mut |trailing_whitespace: &str, from: usize| {
                        self.surface.set_string(
                            self.viewport.x + from as u16,
                            self.viewport.y + position.row as u16,
                            trailing_whitespace,
                            style.patch(self.trailing_whitespace_style),
                        );
                    },
                );
            }
        } else if cut_off_start != 0 && cut_off_start < width {
            // partially on screen
            let rect = Rect::new(
                self.viewport.x,
                self.viewport.y + position.row as u16,
                (width - cut_off_start) as u16,
                1,
            );
            self.surface.set_style(rect, style);
        }

        if *is_in_indent_area && !is_whitespace {
            *last_indent_level = position.col;
            *is_in_indent_area = false;
        }
    }

    /// Overlay indentation guides ontop of a rendered line
    /// The indentation level is computed in `draw_lines`.
    /// Therefore this function must always be called afterwards.
    pub fn draw_indent_guides(&mut self, indent_level: usize, row: u16) {
        if !self.draw_indent_guides {
            return;
        }

        // Don't draw indent guides outside of view
        let end_indent = min(
            indent_level,
            // Add indent_width - 1 to round up, since the first visible
            // indent might be a bit after offset.col
            self.col_offset + self.viewport.width as usize + (self.indent_width as usize - 1),
        ) / self.indent_width as usize;

        for i in self.starting_indent..end_indent {
            let x = (self.viewport.x as usize + (i * self.indent_width as usize) - self.col_offset)
                as u16;
            let y = self.viewport.y + row;
            debug_assert!(self.surface.in_bounds(x, y));
            self.surface
                .set_string(x, y, &self.indent_guide_char, self.indent_guide_style);
        }
    }
}
