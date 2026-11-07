use ab_glyph::{Font, FontArc, FontVec, GlyphId, Outline, OutlineCurve, Point, Rect, v2};
use ethornell_core::{EthornellError, Result};
use std::collections::BTreeSet;
use std::path::Path;

/// Stateless font loading shared by GPU text, CPU snapshots and text metrics.
/// Keep the returned FontArc in the session, not a process-wide OnceLock.
pub struct FontProvider;

impl FontProvider {
    /// Loads exactly the requested face (TTC/OTC indices are zero-based).
    /// Explicit selection never silently falls back to a different file/face.
    pub fn load_file(path: impl AsRef<Path>, face_index: u32) -> Result<FontArc> {
        let bytes = std::fs::read(path)?;
        Self::load_bytes(bytes, face_index)
    }

    pub fn load_bytes(bytes: Vec<u8>, face_index: u32) -> Result<FontArc> {
        FontVec::try_from_vec_and_index(bytes, face_index)
            .map(FontArc::new)
            .map_err(|_| EthornellError::Other(format!("invalid font or face index {face_index}")))
    }

    /// Unique missing printable codepoints, in deterministic order. This is a
    /// cmap check, not a guarantee of shaping or of coverage of an entire script.
    pub fn missing_glyphs(font: &FontArc, text: &str) -> Vec<char> {
        text.chars()
            .filter(|ch| !ch.is_control() && font.glyph_id(*ch).0 == 0)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    pub fn require_coverage(font: &FontArc, text: &str) -> Result<()> {
        let missing = Self::missing_glyphs(font, text);
        if missing.is_empty() {
            Ok(())
        } else {
            Err(EthornellError::Other(format!(
                "font missing {} required glyphs: {:?}",
                missing.len(),
                missing
            )))
        }
    }

    pub fn load_system_cjk_font() -> Result<FontArc> {
        Self::load_system_for_text("A日本語中文")
    }

    /// Hosts should pass their own required SJIS/GBK repertoire/sample here,
    /// or validate their explicit font with require_coverage before bootstrap.
    pub fn load_system_for_text(required_text: &str) -> Result<FontArc> {
        for path in super::SYSTEM_CJK_FONT_CANDIDATES {
            let Ok(bytes) = std::fs::read(path) else {
                continue;
            };
            // A TTC can contain regional faces. Try each existing face rather
            // than assuming face 0 supports the session's language.
            for face in 0..32 {
                let Ok(font) = Self::load_bytes(bytes.clone(), face) else {
                    break;
                };
                if Self::missing_glyphs(&font, required_text).is_empty() {
                    tracing::debug!(path, face, "loaded system CJK font");
                    return Ok(font);
                }
            }
        }
        Err(EthornellError::Other(
            "no usable system font with required glyph coverage".into(),
        ))
    }
}

/// A font that draws what `primary` has, and the characters it lacks with
/// `fallback`, scaled to `primary`'s em. A game's own font (often a patch's
/// font for its remapped codes) covers the script but rarely the menus,
/// names and symbols the host font has.
///
/// Fallback glyphs are numbered after the primary's; metrics, kerning
/// between primary glyphs, `codepoint_ids` and `font_data` are the
/// primary's.
pub struct FallbackFont {
    primary: FontArc,
    fallback: FontArc,
    offset: u32,
    scale: f32,
}

impl FallbackFont {
    pub fn new(primary: FontArc, fallback: FontArc) -> FontArc {
        let scale = match (primary.units_per_em(), fallback.units_per_em()) {
            (Some(primary), Some(fallback)) if fallback > 0.0 => primary / fallback,
            _ => 1.0,
        };
        let offset = primary.glyph_count() as u32;
        FontArc::new(Self { primary, fallback, offset, scale })
    }

    /// The fallback glyph behind `id`, if it is one.
    fn fallback_id(&self, id: GlyphId) -> Option<GlyphId> {
        (id.0 as u32 >= self.offset).then(|| GlyphId((id.0 as u32 - self.offset) as u16))
    }

    fn scaled(&self, point: Point) -> Point {
        Point { x: point.x * self.scale, y: point.y * self.scale }
    }
}

impl Font for FallbackFont {
    fn units_per_em(&self) -> Option<f32> {
        self.primary.units_per_em()
    }
    fn ascent_unscaled(&self) -> f32 {
        self.primary.ascent_unscaled()
    }
    fn descent_unscaled(&self) -> f32 {
        self.primary.descent_unscaled()
    }
    fn line_gap_unscaled(&self) -> f32 {
        self.primary.line_gap_unscaled()
    }
    fn italic_angle(&self) -> f32 {
        self.primary.italic_angle()
    }
    fn glyph_id(&self, c: char) -> GlyphId {
        let id = self.primary.glyph_id(c);
        if id.0 != 0 {
            return id;
        }
        let id = self.fallback.glyph_id(c);
        match u16::try_from(self.offset + id.0 as u32) {
            Ok(combined) if id.0 != 0 => GlyphId(combined),
            _ => GlyphId(0),
        }
    }
    fn h_advance_unscaled(&self, id: GlyphId) -> f32 {
        match self.fallback_id(id) {
            Some(id) => self.fallback.h_advance_unscaled(id) * self.scale,
            None => self.primary.h_advance_unscaled(id),
        }
    }
    fn h_side_bearing_unscaled(&self, id: GlyphId) -> f32 {
        match self.fallback_id(id) {
            Some(id) => self.fallback.h_side_bearing_unscaled(id) * self.scale,
            None => self.primary.h_side_bearing_unscaled(id),
        }
    }
    fn v_advance_unscaled(&self, id: GlyphId) -> f32 {
        match self.fallback_id(id) {
            Some(id) => self.fallback.v_advance_unscaled(id) * self.scale,
            None => self.primary.v_advance_unscaled(id),
        }
    }
    fn v_side_bearing_unscaled(&self, id: GlyphId) -> f32 {
        match self.fallback_id(id) {
            Some(id) => self.fallback.v_side_bearing_unscaled(id) * self.scale,
            None => self.primary.v_side_bearing_unscaled(id),
        }
    }
    fn kern_unscaled(&self, first: GlyphId, second: GlyphId) -> f32 {
        match (self.fallback_id(first), self.fallback_id(second)) {
            (None, None) => self.primary.kern_unscaled(first, second),
            (Some(first), Some(second)) => self.fallback.kern_unscaled(first, second) * self.scale,
            _ => 0.0,
        }
    }
    fn outline(&self, id: GlyphId) -> Option<Outline> {
        let Some(id) = self.fallback_id(id) else {
            return self.primary.outline(id);
        };
        let outline = self.fallback.outline(id)?;
        Some(Outline {
            bounds: Rect { min: self.scaled(outline.bounds.min), max: self.scaled(outline.bounds.max) },
            curves: outline
                .curves
                .into_iter()
                .map(|curve| match curve {
                    OutlineCurve::Line(a, b) => OutlineCurve::Line(self.scaled(a), self.scaled(b)),
                    OutlineCurve::Quad(a, b, c) => {
                        OutlineCurve::Quad(self.scaled(a), self.scaled(b), self.scaled(c))
                    }
                    OutlineCurve::Cubic(a, b, c, d) => OutlineCurve::Cubic(
                        self.scaled(a),
                        self.scaled(b),
                        self.scaled(c),
                        self.scaled(d),
                    ),
                })
                .collect(),
        })
    }
    fn glyph_count(&self) -> usize {
        (self.offset as usize + self.fallback.glyph_count()).min(u16::MAX as usize + 1)
    }
    fn codepoint_ids(&self) -> ab_glyph::CodepointIdIter<'_> {
        self.primary.codepoint_ids()
    }
    fn glyph_raster_image2(&self, id: GlyphId, pixel_size: u16) -> Option<v2::GlyphImage<'_>> {
        match self.fallback_id(id) {
            Some(id) => self.fallback.glyph_raster_image2(id, pixel_size),
            None => self.primary.glyph_raster_image2(id, pixel_size),
        }
    }
    fn font_data(&self) -> &[u8] {
        self.primary.font_data()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_font_draws_what_the_primary_lacks() {
        let path = super::super::SYSTEM_CJK_FONT_CANDIDATES
            .iter()
            .find(|path| Path::new(path).is_file())
            .expect("system font file");
        let system = FontProvider::load_file(path, 0).unwrap();
        // Every character the primary has is drawn with the primary.
        let font = FallbackFont::new(system.clone(), system.clone());
        let a = font.glyph_id('A');
        assert_eq!(a, system.glyph_id('A'));
        assert_eq!(font.h_advance_unscaled(a), system.h_advance_unscaled(a));
        assert_eq!(font.glyph_id('\u{10ffff}'), GlyphId(0));

        let wrapper = FallbackFont {
            primary: system.clone(),
            fallback: system.clone(),
            offset: 0,
            scale: 0.5,
        };
        // With offset 0 every glyph is a fallback glyph at half size.
        let id = system.glyph_id('中');
        assert_eq!(wrapper.h_advance_unscaled(id), system.h_advance_unscaled(id) * 0.5);
        let outline = wrapper.outline(id).unwrap();
        let original = system.outline(id).unwrap();
        assert_eq!(outline.bounds.max.x, original.bounds.max.x * 0.5);
        assert_eq!(outline.curves.len(), original.curves.len());
    }

    #[test]
    fn invalid_font_and_face_fail_explicitly() {
        assert!(FontProvider::load_bytes(vec![0; 64], 0).is_err());
        assert!(FontProvider::load_file("/nonexistent/ethornell-font.ttf", 0).is_err());
    }

    #[test]
    fn coverage_reports_missing_without_process_cache() {
        let font = FontProvider::load_system_cjk_font().expect("system CJK font");
        assert!(FontProvider::missing_glyphs(&font, "A日本語中文\n").is_empty());
        assert_eq!(
            FontProvider::missing_glyphs(&font, "\u{10ffff}\u{10ffff}"),
            vec!['\u{10ffff}']
        );
        assert!(FontProvider::require_coverage(&font, "\u{10ffff}").is_err());
        assert!(FontProvider::require_coverage(&font, "日本語").is_ok());
    }

    #[test]
    fn explicit_ttc_face_and_invalid_index_are_respected() {
        let path = super::super::SYSTEM_CJK_FONT_CANDIDATES
            .iter()
            .find(|path| Path::new(path).is_file())
            .expect("system font file");
        let bytes = std::fs::read(path).unwrap();
        assert!(FontProvider::load_bytes(bytes.clone(), 0).is_ok());
        assert!(FontProvider::load_bytes(bytes, u32::MAX).is_err());
    }
}
