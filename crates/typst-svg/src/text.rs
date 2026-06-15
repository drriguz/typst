use ecow::EcoString;
use ttf_parser::GlyphId;
use typst_library::layout::{Abs, Ratio, Size, Transform};
use typst_library::text::TextItem;
use typst_library::text::color::{
    GlyphFrame, GlyphFrameItem, glyph_frame, should_outline,
};
use typst_library::text::FontFlags;
use typst_library::visualize::{FillRule, Paint, RelativeTo};

use crate::path::SvgPathBuilder;
use crate::write::{SvgElem, SvgIdRef, SvgTransform};
use crate::{DedupId, SVGRenderer, State};

/// Represents a glyph to be rendered.
#[derive(Clone)]
pub enum RenderedGlyph {
    /// A frame that contains an image glpyh.
    Frame(GlyphFrame),
    /// A path is a sequence of drawing commands.
    ///
    /// It is in the format of `M x y L x y C x1 y1 x2 y2 x y Z`.
    Path(EcoString),
}

impl SVGRenderer<'_> {
    /// Render a text item. The text is rendered as a group of glyphs. We will
    /// try to render the text as SVG first, then bitmap, then outline. If none
    /// of them works, we will skip the text.
    pub(super) fn render_text(
        &mut self,
        svg: &mut SvgElem,
        state: &State,
        text: &TextItem,
    ) {
        // Math fonts must always be rendered as shapes (<use> with paths),
        // not as <text> elements. Browsers typically don't have math fonts
        // installed, so <text> with math font-family won't render correctly.
        let is_math_font = text.font.info().flags.contains(FontFlags::MATH);

        // Check if all glyphs can be rendered as outline glyphs (selectable text)
        let all_outline = text.glyphs.iter().all(|g| {
            should_outline(&text.font, GlyphId(g.id))
        });

        if !is_math_font && all_outline && !text.glyphs.is_empty() {
            // Render as <text> elements for selectable text
            self.render_text_as_svg_text(svg, state, text);
        } else {
            // Fall back to <use> elements for color/image glyphs or math fonts
            self.render_text_as_glyphs(svg, state, text);
        }
    }

    /// Render text as SVG <text> elements with <tspan> children.
    /// This makes the text selectable and searchable in browsers.
    fn render_text_as_svg_text(
        &mut self,
        svg: &mut SvgElem,
        state: &State,
        text: &TextItem,
    ) {
        let svg = &mut svg.elem("g");

        // For text elements, we don't flip Y because that would flip the
        // characters upside down. Instead, we negate Y positions manually.
        svg.attr("transform", SvgTransform(state.transform));

        // Get font information
        let font_family = &text.font.info().family;
        let font_size = text.size.to_pt();
        let font_style = match text.font.info().variant.style {
            typst_library::text::FontStyle::Normal => "normal",
            typst_library::text::FontStyle::Italic => "italic",
            typst_library::text::FontStyle::Oblique => "oblique",
        };
        let font_weight = text.font.info().variant.weight.to_number();
        let font_stretch = (text.font.info().variant.stretch.to_ratio().get() * 100.0) as u16;

        // Build the text element
        let mut text_elem = svg.elem("text");
        text_elem
            .attr("font-family", font_family.as_str())
            .attr("font-size", font_size)
            .attr("font-style", font_style)
            .attr("font-weight", font_weight as f64)
            .attr("font-stretch", font_stretch as f64);

        // Apply fill color
        self.write_fill_for_text(&mut text_elem, &text.fill);

        // Emit each glyph as a <tspan> with positioning
        // Note: Y is negated because we're not using the Y-axis flip
        let mut x = Abs::pt(0.0);
        let mut y = Abs::pt(0.0);

        for glyph in &text.glyphs {
            let glyph_text = self.get_glyph_text(text, glyph);
            if glyph_text.is_empty() {
                // Skip empty glyphs (e.g., default ignorables)
                x += glyph.x_advance.at(text.size);
                y += glyph.y_advance.at(text.size);
                continue;
            }

            let x_offset = x + glyph.x_offset.at(text.size);
            let y_offset = y + glyph.y_offset.at(text.size);

            let mut tspan = text_elem.elem("tspan");
            tspan
                .attr("x", x_offset.to_pt())
                .attr("y", -y_offset.to_pt()); // Negate Y

            // Write the text content (escaped for XML)
            let escaped = escape_xml(&glyph_text);
            tspan.text(&escaped);

            x += glyph.x_advance.at(text.size);
            y += glyph.y_advance.at(text.size);
        }
    }

    /// Get the text content for a glyph from the original text.
    fn get_glyph_text(&self, text: &TextItem, glyph: &typst_library::text::Glyph) -> EcoString {
        let range = glyph.range();
        if range.start < range.end && range.end <= text.text.len() {
            // Trim default ignorables
            text.text[range]
                .trim_matches(typst_library::text::is_default_ignorable)
                .into()
        } else {
            EcoString::new()
        }
    }

    /// Write fill attributes for text elements.
    fn write_fill_for_text(&self, elem: &mut SvgElem, paint: &Paint) {
        match paint {
            Paint::Solid(color) => {
                let rgb = color.to_rgb();
                let r = (rgb.red * 255.0) as u8;
                let g = (rgb.green * 255.0) as u8;
                let b = (rgb.blue * 255.0) as u8;
                let fill_color = format!("#{:02x}{:02x}{:02x}", r, g, b);
                elem.attr("fill", fill_color.as_str());
                if rgb.alpha < 1.0 {
                    elem.attr("fill-opacity", rgb.alpha as f64);
                }
            }
            Paint::Gradient(_) | Paint::Tiling(_) => {
                // For gradients/tilings, we need to use the <use> approach
                // This is a fallback case - we'll handle it in render_text_as_glyphs
                elem.attr("fill", "black");
            }
        }
    }

    /// Render text using the traditional <use> glyph approach (fallback).
    fn render_text_as_glyphs(
        &mut self,
        svg: &mut SvgElem,
        state: &State,
        text: &TextItem,
    ) {
        let svg = &mut svg.elem("g");

        // Flip the transform since fonts use a Y-Up coordinate system.
        let state = state.pre_concat(Transform::scale(Ratio::one(), -Ratio::one()));
        svg.attr("transform", SvgTransform(state.transform));

        let mut x = Abs::pt(0.0);
        let mut y = Abs::pt(0.0);
        for glyph in &text.glyphs {
            let id = GlyphId(glyph.id);
            let x_offset = x + glyph.x_offset.at(text.size);
            let y_offset = y + glyph.y_offset.at(text.size);

            self.render_glyph(svg, &state, text, id, x_offset, y_offset);

            x += glyph.x_advance.at(text.size);
            y += glyph.y_advance.at(text.size);
        }
    }

    fn render_glyph(
        &mut self,
        svg: &mut SvgElem,
        state: &State,
        text: &TextItem,
        glyph_id: GlyphId,
        x_offset: Abs,
        y_offset: Abs,
    ) {
        if should_outline(&text.font, glyph_id) {
            // Pre-scale outlined glyphs, so strokes and fill patterns don't
            // need to consider text size glyph scaling.
            let scale = Ratio::new(text.size.to_pt() / text.font.units_per_em());
            let key = (&text.font, glyph_id, scale);
            let (id, path) = self.glyphs.insert_with_val(key, || {
                let mut builder = SvgPathBuilder::with_scale(scale);
                text.font.ttf().outline_glyph(glyph_id, &mut builder)?;
                Some(RenderedGlyph::Path(builder.finsish()))
            });

            if path.is_some() {
                self.render_path_glyph(svg, state, text, glyph_id, x_offset, y_offset, id)
            }
        } else {
            // Image glyphs apply a `scale` at use site, since colr, svg-, and
            // bitmap glyph images are usually quite large, and having one glyph
            // per text size is a bit of a waste.
            let key = (&text.font, glyph_id);
            let (id, frame) = self.glyphs.insert_with_val(key, || {
                let frame = glyph_frame(&text.font, glyph_id.0)?;
                Some(RenderedGlyph::Frame(frame))
            });

            if frame.is_some() {
                self.render_image_glyph(svg, x_offset, y_offset, text, id);
            }
        }
    }

    /// Write a reference to an image glyph that is stored in font units.
    fn render_image_glyph(
        &mut self,
        svg: &mut SvgElem,
        x_offset: Abs,
        y_offset: Abs,
        text: &TextItem,
        id: DedupId,
    ) {
        let scale = Ratio::new(text.size.to_pt() / text.font.units_per_em());
        // Flip the transform again, since images are drawn Y-Down.
        let ts = Transform::translate(x_offset, y_offset)
            .pre_concat(Transform::scale(scale, -scale));

        svg.elem("use")
            .attr("xlink:href", SvgIdRef(id))
            .attr("transform", SvgTransform(ts));
    }

    /// Render a pre-scaled path glyph defined by an outline.
    #[allow(clippy::too_many_arguments)]
    fn render_path_glyph(
        &mut self,
        svg: &mut SvgElem,
        state: &State,
        text: &TextItem,
        glyph_id: GlyphId,
        x_offset: Abs,
        y_offset: Abs,
        id: DedupId,
    ) {
        // Apply the transform here, because the state transform is used to draw
        // strokes and fills with gradients and tilings.
        let state = state.pre_concat(Transform::translate(x_offset, y_offset));

        let Some(glyph_size) = text.font.ttf().glyph_bounding_box(glyph_id) else {
            // This shouldn't happen, because the glyph has been successfully
            // outlined to create the path.
            return;
        };

        let aspect_ratio = Size::new(
            Abs::pt(glyph_size.width() as f64),
            Abs::pt(glyph_size.height() as f64),
        )
        .aspect_ratio();

        let mut use_ = svg.elem("use");
        use_.attr("xlink:href", SvgIdRef(id))
            .attr("x", x_offset.to_pt())
            .attr("y", y_offset.to_pt());

        self.write_fill(
            &mut use_,
            &text.fill,
            FillRule::default(),
            aspect_ratio,
            self.text_paint_transform(&state, &text.fill),
        );
        if let Some(stroke) = &text.stroke {
            self.write_stroke(
                &mut use_,
                stroke,
                aspect_ratio,
                self.text_paint_transform(&state, &stroke.paint),
            );
        }
    }

    fn text_paint_transform(&self, state: &State, paint: &Paint) -> Transform {
        match paint {
            Paint::Solid(_) => Transform::identity(),
            Paint::Gradient(gradient) => match gradient.unwrap_relative(true) {
                RelativeTo::Self_ => Transform::identity(),
                RelativeTo::Parent => Transform::scale(
                    Ratio::new(state.size.x.to_pt()),
                    Ratio::new(state.size.y.to_pt()),
                )
                .post_concat(state.transform.invert().unwrap()),
            },
            Paint::Tiling(tiling) => match tiling.unwrap_relative(true) {
                RelativeTo::Self_ => Transform::identity(),
                RelativeTo::Parent => state.transform.invert().unwrap(),
            },
        }
    }

    /// Build the glyph definitions.
    pub(super) fn write_glyph_defs(&mut self, svg: &mut SvgElem) {
        if self.glyphs.iter().all(|(_, g)| g.is_none()) {
            return;
        }

        let mut defs = svg.elem("defs");
        let glyphs = std::mem::take(&mut self.glyphs);
        for (id, glyph) in glyphs.iter() {
            let Some(glyph) = glyph else { continue };

            let mut symbol = defs.elem("symbol");
            symbol.attr("id", id);
            symbol.attr("overflow", "visible");

            match glyph {
                RenderedGlyph::Frame(frame) => {
                    let state = State::new(frame.size()).pre_translate(frame.item.pos());
                    match &frame.item {
                        GlyphFrameItem::Tofu(_, shape) => {
                            self.render_shape(&mut symbol, &state, shape);
                        }
                        GlyphFrameItem::Image(_, image, size) => {
                            self.render_image(&mut symbol, &state, image, size);
                        }
                    }
                }
                RenderedGlyph::Path(path) => {
                    symbol.elem("path").attr("d", path);
                }
            }
        }

        // The glyphs have been taken above, there shouldn't be any new glyphs
        // produced from writing the glyph definitions.
        assert!(self.glyphs.is_empty());
    }
}

/// Escape special XML characters in text content.
fn escape_xml(text: &str) -> EcoString {
    let mut result = EcoString::new();
    for c in text.chars() {
        match c {
            '&' => result.push_str("&amp;"),
            '<' => result.push_str("&lt;"),
            '>' => result.push_str("&gt;"),
            '"' => result.push_str("&quot;"),
            '\'' => result.push_str("&apos;"),
            _ => result.push(c),
        }
    }
    result
}
